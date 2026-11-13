//! `XMLHttpRequest`, `XMLHttpRequestUpload` and `ProgressEvent` (WHATWG XHR),
//! over the transport `fetch` uses ([`super::http`]).
//!
//! ## Why it exists
//!
//! `fetch` has no upload progress. An app that shows a progress bar for a file
//! it PUTs to storage reaches for `XMLHttpRequest`, and without one it has to
//! carry a second code path. This is the real class, with the spec's state
//! machine, events and errors, not a wrapper over `fetch`.
//!
//! A URL reaches what `fetch` would reach with it. `http:` and `https:` go
//! over the transport, with the cookie jar (`withCredentials` is `include`,
//! otherwise `same-origin`) and redirects. `blob:` reads the blob URL store,
//! with `fetch`'s `Range` rules (resolved at `open()`). `data:` URLs and the
//! app's own files (a relative URL) are read locally. ws:, ftp: and the other
//! schemes `fetch` refuses are refused here too.
//!
//! ## Threading
//!
//! `send()` returns at once. The transport reports progress through
//! [`http::Observe`] on a network thread; each callback records what it saw in
//! the request's inbox ([`Shared`]) and, if no delivery is queued yet, queues
//! ONE onto the pump (`web::schedule_native_from`, as `fetch` settles its
//! promise). The delivery ([`tick`]) runs on the JS thread and turns whatever
//! accumulated into events, in the spec's order: upload progress, the
//! response head, body progress, the end. Progress is therefore coalesced per
//! pump turn, which is the spec's "roughly every 50ms" in practice.
//!
//! The `XMLHttpRequest` object is protected from `send()` until the request
//! ends, as a browser keeps an in-flight request alive with no references.
//! A delivery finds its object through [`flights`], by request id, so a
//! delivery queued for a request that `abort()` or `open()` already ended
//! finds nothing and does nothing.
//!
//! ## Chromium, where the spec leaves room
//!
//! - Progress: `progress` fires as body pieces arrive (one per delivery). The
//!   end of the body fires one more only if bytes arrived since the last, and
//!   an empty body fires none, as Chromium does; the spec would always fire
//!   one more.
//! - `upload.onprogress` fires for every delivery including the last, then
//!   `load` and `loadend`.
//! - A response with no `Content-Type` makes a `Blob` with type `""`.
//!
//! ## Divergences
//!
//! The first four are said once on stderr, when they are first met.
//!
//! - **Synchronous requests are refused**: `open(method, url, false)` throws
//!   `InvalidAccessError`. A sync request would freeze the UI thread for the
//!   whole round trip; the pump cannot block.
//! - **No XML**: `responseXML` is `null`, and so is `response` under
//!   `responseType = "document"`.
//! - **The schemes `fetch` refuses** (`ws:`, `ftp:`, `about:`, `javascript:`,
//!   …) are network errors.
//! - **`open()`'s user and password are not sent** (no HTTP authentication).
//! - **No origin, no CORS**: as for `fetch` (this runtime has no web origin),
//!   so every header is readable and no preflight is sent.
//! - During `LOADING`, `responseText` grows only for a body with no
//!   `Content-Encoding`: a coded body is decoded at the end.
//! - The transport's budgets apply as they do to `fetch`: a connect that takes
//!   15 s, or a request on which no byte moves for 60 s, is a network
//!   (`error`) event even with `timeout` 0. A browser leaves both to the OS.
//! - Without `net-h2` the body is reported as one piece once it is complete,
//!   so a download fires one `progress` and `LOADING` is brief. Upload
//!   progress is reported per 64 KiB on both transports.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::util::*;
use super::{blob, cookies, domexception, events, headers, http, request};
use crate::capi;
use crate::glue;

const UNSENT: u8 = 0;
const OPENED: u8 = 1;
const HEADERS_RECEIVED: u8 = 2;
const LOADING: u8 = 3;
const DONE: u8 = 4;

/// The same ceilings `fetch` keeps: a thread per request without `net-h2`, a
/// task with it.
#[cfg(not(feature = "net-h2"))]
const MAX_INFLIGHT: usize = 64;
#[cfg(feature = "net-h2")]
const MAX_INFLIGHT: usize = 4096;
static INFLIGHT: AtomicUsize = AtomicUsize::new(0);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IO_TIMEOUT: Duration = Duration::from_secs(60);

static NEXT_FLIGHT: AtomicU64 = AtomicU64::new(1);

/// The event types an `XMLHttpRequestEventTarget` has handlers for.
const PROGRESS_TYPES: [&str; 7] = ["loadstart", "progress", "abort", "error", "load", "timeout", "loadend"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RType {
    Empty,
    ArrayBuffer,
    Blob,
    Document,
    Json,
    Text,
}

impl RType {
    fn parse(s: &str) -> Option<RType> {
        Some(match s {
            "" => RType::Empty,
            "arraybuffer" => RType::ArrayBuffer,
            "blob" => RType::Blob,
            "document" => RType::Document,
            "json" => RType::Json,
            "text" => RType::Text,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            RType::Empty => "",
            RType::ArrayBuffer => "arraybuffer",
            RType::Blob => "blob",
            RType::Document => "document",
            RType::Json => "json",
            RType::Text => "text",
        }
    }

    fn texty(self) -> bool {
        matches!(self, RType::Empty | RType::Text)
    }
}

/// One `XMLHttpRequest`'s state. A "network error" response is `error`, with
/// status 0, no headers and no body.
pub struct XhrData {
    state: u8,
    send_flag: bool,
    upload_complete: bool,
    upload_listened: bool,
    timed_out: bool,
    method: String,
    url: String,
    /// The URL as `open()` was given it, for a non-network read: `fetch`
    /// reads a relative path off the app's own files, and so does this.
    raw_url: String,
    /// A blob: URL's store entry, resolved at `open()` as the spec says, so
    /// a URL revoked between `open()` and `send()` still loads.
    blob: Option<super::url::BlobEntry>,
    /// Author request headers, names as given, same-name values combined.
    headers: Vec<(String, String)>,
    timeout_ms: u32,
    with_credentials: bool,
    rtype: RType,
    override_mime: Option<String>,
    /// The request body's length, for upload progress.
    req_len: u64,

    error: bool,
    status: u16,
    status_text: String,
    resp_headers: Vec<(String, String)>,
    resp_url: String,
    /// The body so far (a coded body only arrives decoded, at the end).
    body: Vec<u8>,
    has_body: bool,
    /// Bytes received, still content-coded: what progress events count.
    received: u64,
    total: Option<u64>,
    /// `loaded` of the last `progress` fired at the request.
    reported: u64,
    /// `responseText`, decoded, keyed by the body length it was decoded at.
    text: Option<(usize, String)>,

    /// The request in flight (0 for none), and how to stop it.
    flight: u64,
    cancel: Option<http::Cancel>,
    started: Option<Instant>,
    /// Which queued timeout check is the live one (see [`arm_timeout`]).
    timer: u64,
}

impl XhrData {
    fn new() -> XhrData {
        XhrData {
            state: UNSENT,
            send_flag: false,
            upload_complete: false,
            upload_listened: false,
            timed_out: false,
            method: String::new(),
            url: String::new(),
            raw_url: String::new(),
            blob: None,
            headers: Vec::new(),
            timeout_ms: 0,
            with_credentials: false,
            rtype: RType::Empty,
            override_mime: None,
            req_len: 0,
            error: true,
            status: 0,
            status_text: String::new(),
            resp_headers: Vec::new(),
            resp_url: String::new(),
            body: Vec::new(),
            has_body: false,
            received: 0,
            total: None,
            reported: 0,
            text: None,
            flight: 0,
            cancel: None,
            started: None,
            timer: 0,
        }
    }

    /// "Set the response to a network error."
    fn network_error(&mut self) {
        self.error = true;
        self.status = 0;
        self.status_text.clear();
        self.resp_headers.clear();
        self.resp_url.clear();
        self.body = Vec::new();
        self.has_body = false;
        self.received = 0;
        self.total = None;
        self.reported = 0;
        self.text = None;
    }
}

// ---------------------------------------------------------------------------
// Per-context classes and the in-flight registry
// ---------------------------------------------------------------------------

struct XhrClasses {
    xhr_class: capi::JSClassRef,
    xhr: Class,
    upload: Class,
    progress: Class,
    /// Holds the `on<type>` handlers, so an unset one reads back `null`.
    s_handlers: *mut c_void,
    /// An `XMLHttpRequest`'s `upload` object.
    s_upload: *mut c_void,
    /// `response` for `arraybuffer` / `blob` / `json`: the same object on
    /// every read, as the spec says.
    s_response: *mut c_void,
}

unsafe impl Send for XhrClasses {}
unsafe impl Sync for XhrClasses {}

fn classes_reg() -> &'static Mutex<HashMap<usize, &'static XhrClasses>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static XhrClasses>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> &'static XhrClasses {
    classes_reg().lock().unwrap().get(&(g as usize)).copied().expect("webapi::xhr not installed")
}

/// `(context key, request id)` → the protected `XMLHttpRequest` it belongs to.
fn flights() -> &'static Mutex<HashMap<(usize, u64), u64>> {
    static F: OnceLock<Mutex<HashMap<(usize, u64), u64>>> = OnceLock::new();
    F.get_or_init(|| Mutex::new(HashMap::new()))
}

fn flight_obj(g: *mut c_void, flight: u64) -> Option<u64> {
    flights().lock().unwrap().get(&(g as usize, flight)).copied()
}

/// `crate::context` fan-out: the heap is gone, so nothing is unprotected.
pub(crate) fn forget(gone: crate::context::Gone) {
    classes_reg().lock().unwrap().remove(&gone.key);
    flights().lock().unwrap().retain(|(k, _), _| *k != gone.key);
}

unsafe fn xd<'a>(g: *mut c_void, obj: u64) -> Option<&'a mut XhrData> {
    host_data::<XhrData>(g, classes(g).xhr_class, obj)
}

/// Is `flight` still the request `obj` is running? A handler may have called
/// `abort()` or `open()` during any event.
unsafe fn current(g: *mut c_void, obj: u64, flight: u64) -> bool {
    xd(g, obj).is_some_and(|d| d.flight == flight)
}

/// Ends the request in flight, if any: it leaves the registry (so nothing
/// queued for it runs) and, with `cancel`, the transport is told to stop.
/// Returns whether the caller owes the object an unprotect, which it pays
/// after its events, so the object cannot be collected under them.
unsafe fn end_flight(g: *mut c_void, obj: u64, cancel: bool) -> bool {
    let Some(d) = xd(g, obj) else { return false };
    if d.flight == 0 {
        return false;
    }
    flights().lock().unwrap().remove(&(g as usize, d.flight));
    d.flight = 0;
    if let Some(c) = d.cancel.take() {
        if cancel {
            c.abort();
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Advisories
// ---------------------------------------------------------------------------

fn advised() -> &'static Mutex<std::collections::HashSet<String>> {
    static A: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    A.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Says `msg` once per `key`, for the process.
fn advise(key: &str, msg: &str) {
    if advised().lock().unwrap().insert(key.to_string()) {
        eprintln!("[redjs] XMLHttpRequest: {msg}");
    }
}

// ---------------------------------------------------------------------------
// ProgressEvent
// ---------------------------------------------------------------------------

unsafe fn slots(g: *mut c_void, obj: u64) -> Option<u64> {
    let s = get_a(g, obj, state(g).s_event);
    is_cell(s).then_some(s)
}

unsafe fn pe_slot(g: *mut c_void, cf: *mut c_void, key: &str) -> u64 {
    match slots(g, cf_this(cf)) {
        Some(s) => get(g, s, key),
        None => throw_type_error(g, &format!("ProgressEvent.{key} called on a non-ProgressEvent")),
    }
}

#[crate::host_fn]
unsafe fn pe_length_computable(g: *mut c_void, cf: *mut c_void) -> u64 {
    pe_slot(g, cf, "lengthComputable")
}
#[crate::host_fn]
unsafe fn pe_loaded(g: *mut c_void, cf: *mut c_void) -> u64 {
    pe_slot(g, cf, "loaded")
}
#[crate::host_fn]
unsafe fn pe_total(g: *mut c_void, cf: *mut c_void) -> u64 {
    pe_slot(g, cf, "total")
}

/// WebIDL `unsigned long long` from a `ProgressEventInit` member.
unsafe fn ull(g: *mut c_void, init: u64, key: &str) -> Result<f64, ()> {
    if !is_cell(init) {
        return Ok(0.0);
    }
    let v = try_get(g, init, key)?;
    if v == UNDEF {
        return Ok(0.0);
    }
    let n = to_number(g, v)?;
    if !n.is_finite() {
        return Ok(0.0);
    }
    Ok(n.trunc().rem_euclid(18446744073709551616.0))
}

#[crate::host_fn]
unsafe fn progress_event_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    if cf_argc(cf) < 1 {
        return throw_type_error(
            g,
            "Failed to construct 'ProgressEvent': 1 argument required, but only 0 present.",
        );
    }
    let init = cf_arg(cf, 1);
    if init != UNDEF && init != NULL && !blob::is_object(g, init) {
        return throw_type_error(
            g,
            "Failed to construct 'ProgressEvent': The provided value is not of type 'ProgressEventInit'.",
        );
    }
    let obj = ordinary_create(g, cf_new_target(cf), classes(g).progress.proto);
    events::init_event(g, obj, cf_arg(cf, 0), init, false);
    let computable = if is_cell(init) {
        match try_get(g, init, "lengthComputable") {
            Ok(v) => v != UNDEF && glue::rjs_to_boolean(g, v) != 0,
            Err(()) => return UNDEF,
        }
    } else {
        false
    };
    let Ok(loaded) = ull(g, init, "loaded") else { return UNDEF };
    let Ok(total) = ull(g, init, "total") else { return UNDEF };
    if let Some(s) = slots(g, obj) {
        define(g, s, "lengthComputable", enc_bool(computable), 0);
        define(g, s, "loaded", enc_f64(loaded), 0);
        define(g, s, "total", enc_f64(total), 0);
    }
    obj
}

/// A trusted `ProgressEvent` ("fire a progress event named `ty` with
/// `loaded` and `total`": computable when the total is known, i.e. not 0).
unsafe fn new_progress(g: *mut c_void, ty: &str, loaded: u64, total: u64) -> u64 {
    let obj = new_object(g, classes(g).progress.proto);
    events::init_event(g, obj, js_string(g, ty), UNDEF, false);
    if let Some(s) = slots(g, obj) {
        define(g, s, "isTrusted", TRUE, 0);
        define(g, s, "lengthComputable", enc_bool(total != 0), 0);
        define(g, s, "loaded", enc_f64(loaded as f64), 0);
        define(g, s, "total", enc_f64(total as f64), 0);
    }
    obj
}

pub(super) unsafe fn fire_progress(g: *mut c_void, target: u64, ty: &str, loaded: u64, total: u64) {
    let ev = new_progress(g, ty, loaded, total);
    events::dispatch(g, target, ev);
}

unsafe fn fire_readystatechange(g: *mut c_void, target: u64) {
    let ev = events::new_trusted_event(g, "readystatechange");
    events::dispatch(g, target, ev);
}

// ---------------------------------------------------------------------------
// on<event> handler attributes
// ---------------------------------------------------------------------------

unsafe fn handler_bag(g: *mut c_void, this: u64, create: bool) -> u64 {
    let slot = classes(g).s_handlers;
    let bag = get_a(g, this, slot);
    if is_cell(bag) {
        return bag;
    }
    if !create || !is_cell(this) {
        return UNDEF;
    }
    let fresh = new_object(g, UNDEF);
    glue::rjs_define(g, this, slot, fresh, 0);
    fresh
}

macro_rules! handler_accessor {
    ($get_fn:ident, $set_fn:ident, $name:literal) => {
        #[crate::host_fn]
        unsafe fn $get_fn(g: *mut c_void, cf: *mut c_void) -> u64 {
            let bag = handler_bag(g, cf_this(cf), false);
            if !is_cell(bag) {
                return NULL;
            }
            let v = get(g, bag, $name);
            if blob::is_object(g, v) {
                v
            } else {
                NULL
            }
        }
        // WebIDL `[LegacyTreatNonObjectAsNull]`: anything but an object
        // (a string, a number) is stored as null.
        #[crate::host_fn]
        unsafe fn $set_fn(g: *mut c_void, cf: *mut c_void) -> u64 {
            let bag = handler_bag(g, cf_this(cf), true);
            if is_cell(bag) {
                let v = cf_arg(cf, 0);
                define(g, bag, $name, if blob::is_object(g, v) { v } else { NULL }, 0);
            }
            UNDEF
        }
    };
}

handler_accessor!(get_onloadstart, set_onloadstart, "onloadstart");
handler_accessor!(get_onprogress, set_onprogress, "onprogress");
handler_accessor!(get_onabort, set_onabort, "onabort");
handler_accessor!(get_onerror, set_onerror, "onerror");
handler_accessor!(get_onload, set_onload, "onload");
handler_accessor!(get_ontimeout, set_ontimeout, "ontimeout");
handler_accessor!(get_onloadend, set_onloadend, "onloadend");
handler_accessor!(get_onreadystatechange, set_onreadystatechange, "onreadystatechange");

// ---------------------------------------------------------------------------
// The network side: what the transport reports, queued for the pump
// ---------------------------------------------------------------------------

/// `*mut c_void` is not `Send`; the pump hand-off only uses it to name the
/// queue it posts to, never dereferences it off the JS thread.
#[derive(Clone, Copy)]
struct Ctx(*mut c_void);
unsafe impl Send for Ctx {}
unsafe impl Sync for Ctx {}

impl Ctx {
    fn ptr(self) -> *mut c_void {
        self.0
    }
}

struct HeadMsg {
    status: u16,
    status_text: String,
    headers: Vec<(String, String)>,
    url: String,
}

#[derive(Default)]
struct Inbox {
    /// A delivery is queued and has not yet taken what is here.
    queued: bool,
    upload: Option<(u64, u64)>,
    head: Option<HeadMsg>,
    /// Body pieces are kept only when the body is not content-coded, for a
    /// `responseText` that grows during `LOADING`.
    identity: bool,
    chunk: Vec<u8>,
    received: u64,
    total: Option<u64>,
    body_moved: bool,
    done: Option<Result<http::Response, http::HttpError>>,
}

/// One request's mailbox between the network thread and the pump.
struct Shared {
    flight: u64,
    ctx: Ctx,
    cid: Option<crate::context::ContextId>,
    inbox: Mutex<Inbox>,
}

impl Shared {
    /// Records with `f`, and queues a delivery unless one is already queued.
    fn post(self: &Arc<Self>, f: impl FnOnce(&mut Inbox)) {
        let queue = {
            let mut i = self.inbox.lock().unwrap();
            f(&mut i);
            !std::mem::replace(&mut i.queued, true)
        };
        if queue {
            let me = Arc::clone(self);
            crate::web::schedule_native_from(
                self.ctx.ptr(),
                self.cid,
                0.0,
                Box::new(move |g| unsafe { tick(g, &me) }),
            );
        }
    }
}

struct Obs(Arc<Shared>);

impl http::Observe for Obs {
    fn upload(&self, sent: u64, total: u64) {
        self.0.post(|i| i.upload = Some((sent, total)));
    }

    fn head(&self, h: &http::ProgressHead<'_>) {
        let identity = http::header(h.headers, "content-encoding")
            .map_or(true, |v| v.trim().is_empty() || v.trim().eq_ignore_ascii_case("identity"));
        let msg = HeadMsg {
            status: h.status,
            status_text: h.status_text.to_string(),
            headers: h.headers.to_vec(),
            url: h.url.to_string(),
        };
        self.0.post(|i| {
            i.identity = identity;
            i.head = Some(msg);
        });
    }

    fn body(&self, chunk: &[u8], received: u64, total: Option<u64>) {
        self.0.post(|i| {
            if i.identity {
                i.chunk.extend_from_slice(chunk);
            }
            i.received = received;
            i.total = total;
            i.body_moved = true;
        });
    }
}

// ---------------------------------------------------------------------------
// The JS side: the spec's steps, run by the delivery
// ---------------------------------------------------------------------------

/// One delivery: everything the request's inbox accumulated, as events.
unsafe fn tick(g: *mut c_void, sh: &Arc<Shared>) {
    let Some(obj) = flight_obj(g, sh.flight) else { return };
    let flight = sh.flight;
    let (upload, head, chunk, received, total, moved, done) = {
        let mut i = sh.inbox.lock().unwrap();
        i.queued = false;
        (
            i.upload.take(),
            i.head.take(),
            std::mem::take(&mut i.chunk),
            i.received,
            i.total,
            std::mem::take(&mut i.body_moved),
            i.done.take(),
        )
    };
    if let Some((sent, total)) = upload {
        upload_progress(g, obj, sent, total);
        if !current(g, obj, flight) {
            return;
        }
    }
    if let Some(h) = head {
        process_head(g, obj, h);
        if !current(g, obj, flight) {
            return;
        }
    }
    if moved {
        process_chunk(g, obj, chunk, received, total);
        if !current(g, obj, flight) {
            return;
        }
    }
    if let Some(result) = done {
        finish(g, obj, result);
        // `finish` ends the request on every path it fires events on; one
        // that bailed out early must not leave the object protected for good.
        if current(g, obj, flight) && end_flight(g, obj, false) {
            crate::ffi::redjs_unprotect(g, obj);
        }
    }
}

unsafe fn upload_of(g: *mut c_void, obj: u64) -> u64 {
    get_a(g, obj, classes(g).s_upload)
}

/// "Process request body chunk length", and its end.
unsafe fn upload_progress(g: *mut c_void, obj: u64, sent: u64, total: u64) {
    let Some(d) = xd(g, obj) else { return };
    if d.upload_complete {
        return;
    }
    if d.upload_listened {
        fire_progress(g, upload_of(g, obj), "progress", sent, total);
    }
    if sent >= total {
        end_upload(g, obj);
    }
}

/// "Process request end-of-body": `load` and `loadend` at the upload object.
unsafe fn end_upload(g: *mut c_void, obj: u64) {
    let Some(d) = xd(g, obj) else { return };
    if d.upload_complete {
        return;
    }
    d.upload_complete = true;
    if !d.upload_listened {
        return;
    }
    let len = d.req_len;
    let up = upload_of(g, obj);
    fire_progress(g, up, "load", len, len);
    fire_progress(g, up, "loadend", len, len);
}

/// "Process response": the head is here.
unsafe fn process_head(g: *mut c_void, obj: u64, h: HeadMsg) {
    let Some(d) = xd(g, obj) else { return };
    if d.state != OPENED || !d.send_flag {
        return;
    }
    let flight = d.flight;
    // A response means the request body is behind us, even when the server
    // answered before it read all of it.
    if !d.upload_complete {
        end_upload(g, obj);
        if !current(g, obj, flight) {
            return;
        }
    }
    let Some(d) = xd(g, obj) else { return };
    d.error = false;
    d.status = h.status;
    d.status_text = h.status_text;
    // Fetch's forbidden response-header names: the jar has them, script
    // never sees them.
    d.resp_headers = h
        .headers
        .into_iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case("set-cookie") && !k.eq_ignore_ascii_case("set-cookie2"))
        .collect();
    d.resp_url = match h.url.split_once('#') {
        Some((u, _)) => u.to_string(),
        None => h.url,
    };
    d.has_body = true;
    d.state = HEADERS_RECEIVED;
    fire_readystatechange(g, obj);
}

/// "Process response body chunk": `LOADING`, `readystatechange`, `progress`.
unsafe fn process_chunk(g: *mut c_void, obj: u64, chunk: Vec<u8>, received: u64, total: Option<u64>) {
    let Some(d) = xd(g, obj) else { return };
    if d.state != HEADERS_RECEIVED && d.state != LOADING {
        return;
    }
    let flight = d.flight;
    d.body.extend_from_slice(&chunk);
    d.received = received;
    d.total = total;
    if d.state == HEADERS_RECEIVED {
        d.state = LOADING;
    }
    fire_readystatechange(g, obj);
    if !current(g, obj, flight) {
        return;
    }
    let Some(d) = xd(g, obj) else { return };
    d.reported = received;
    fire_progress(g, obj, "progress", received, total.unwrap_or(0));
}

/// The request is over: the response's end, or a network error.
unsafe fn finish(g: *mut c_void, obj: u64, result: Result<http::Response, http::HttpError>) {
    let Some(d) = xd(g, obj) else { return };
    let flight = d.flight;
    match result {
        Ok(resp) => {
            // The observer reported the head; this is the backstop for a
            // transport that did not.
            if d.state == OPENED && d.send_flag {
                process_head(
                    g,
                    obj,
                    HeadMsg {
                        status: resp.status,
                        status_text: resp.status_text.clone(),
                        headers: resp.headers.clone(),
                        url: resp.url.clone(),
                    },
                );
                if !current(g, obj, flight) {
                    return;
                }
            }
            let Some(d) = xd(g, obj) else { return };
            if d.state != HEADERS_RECEIVED && d.state != LOADING {
                return;
            }
            if d.received == 0 && !resp.body.is_empty() {
                d.received = resp.body.len() as u64;
            }
            d.body = resp.body;
            d.text = None;
            let transmitted = d.received;
            let length = d.total.unwrap_or(0);
            // Chromium: one more `progress` only for bytes not yet reported.
            if transmitted != d.reported {
                d.reported = transmitted;
                fire_progress(g, obj, "progress", transmitted, length);
                if !current(g, obj, flight) {
                    return;
                }
            }
            let owed = end_flight(g, obj, false);
            if let Some(d) = xd(g, obj) {
                d.state = DONE;
                d.send_flag = false;
            }
            fire_readystatechange(g, obj);
            fire_progress(g, obj, "load", transmitted, length);
            fire_progress(g, obj, "loadend", transmitted, length);
            if owed {
                crate::ffi::redjs_unprotect(g, obj);
            }
        }
        Err(e) => {
            let event = if d.timed_out {
                "timeout"
            } else if e.kind == http::ErrorKind::Aborted {
                "abort"
            } else {
                // A browser's console says so too (net::ERR_…); the event
                // itself carries no reason.
                eprintln!("[redjs] XMLHttpRequest: {} {} failed: {}", d.method, d.url, e.message);
                "error"
            };
            let owed = end_flight(g, obj, false);
            request_error_steps(g, obj, event);
            if owed {
                crate::ffi::redjs_unprotect(g, obj);
            }
        }
    }
}

/// "Request error steps" for `event` (`error`, `abort` or `timeout`).
unsafe fn request_error_steps(g: *mut c_void, obj: u64, event: &str) {
    let Some(d) = xd(g, obj) else { return };
    d.state = DONE;
    d.send_flag = false;
    d.network_error();
    fire_readystatechange(g, obj);
    if let Some(d) = xd(g, obj) {
        if !d.upload_complete {
            d.upload_complete = true;
            if d.upload_listened {
                let up = upload_of(g, obj);
                fire_progress(g, up, event, 0, 0);
                fire_progress(g, up, "loadend", 0, 0);
            }
        }
    }
    fire_progress(g, obj, event, 0, 0);
    fire_progress(g, obj, "loadend", 0, 0);
}

/// The timeout, checked when it may have run out. `xhr.timeout` can change
/// during the request (it is measured from `send()`; an app may slide it on
/// every progress event), so there is ONE live check per request: a change
/// bumps `timer` and queues a fresh one, which retires the old, and a check
/// that finds time left re-queues itself for the rest.
unsafe fn arm_timeout(g: *mut c_void, d: &mut XhrData) {
    d.timer = d.timer.wrapping_add(1);
    if d.flight == 0 || d.timeout_ms == 0 {
        return;
    }
    let elapsed = d.started.map_or(0, |t| t.elapsed().as_millis() as u64);
    queue_timeout_check(g, d.flight, d.timer, (d.timeout_ms as u64).saturating_sub(elapsed));
}

unsafe fn queue_timeout_check(g: *mut c_void, flight: u64, timer: u64, delay_ms: u64) {
    crate::web::schedule_native(g, delay_ms as f64, Box::new(move |g| unsafe { check_timeout(g, flight, timer) }));
}

unsafe fn check_timeout(g: *mut c_void, flight: u64, timer: u64) {
    let Some(obj) = flight_obj(g, flight) else { return };
    let Some(d) = xd(g, obj) else { return };
    if d.flight != flight || d.timer != timer || d.timeout_ms == 0 {
        return;
    }
    let elapsed = d.started.map_or(0, |t| t.elapsed().as_millis() as u64);
    if elapsed < d.timeout_ms as u64 {
        queue_timeout_check(g, flight, timer, d.timeout_ms as u64 - elapsed);
        return;
    }
    d.timed_out = true;
    let owed = end_flight(g, obj, true);
    request_error_steps(g, obj, "timeout");
    if owed {
        crate::ffi::redjs_unprotect(g, obj);
    }
}

// ---------------------------------------------------------------------------
// The URLs that never touch the network
// ---------------------------------------------------------------------------

fn refused(message: String) -> http::HttpError {
    http::HttpError { kind: http::ErrorKind::Refused, message }
}

fn failed(message: String) -> http::HttpError {
    http::HttpError { kind: http::ErrorKind::Network, message }
}

/// A complete answer made here rather than by the transport, delivered the
/// way the transport delivers one: the head, the body as one piece, the end.
/// So the events are a network response's: `readystatechange` 2, 3 and 4,
/// one `progress`, then `load`.
fn answer(sh: &Arc<Shared>, status: u16, status_text: &str, headers: Vec<(String, String)>, url: &str, body: Vec<u8>) {
    // `responseURL` excludes the fragment, as for a network response.
    let url = url.split('#').next().unwrap_or_default().to_string();
    let len = body.len() as u64;
    let resp = http::Response {
        status,
        status_text: status_text.to_string(),
        headers: headers.clone(),
        url: url.clone(),
        redirected: false,
        body: body.clone(),
        protocol: "",
        reused: false,
        connection_id: 0,
        remote_addr: None,
        cache: http::cache::Outcome::Off,
    };
    let head = HeadMsg { status, status_text: status_text.to_string(), headers, url };
    sh.post(move |i| {
        i.identity = true;
        i.head = Some(head);
        if len > 0 {
            i.chunk = body;
            i.received = len;
            i.total = Some(len);
            i.body_moved = true;
        }
        i.done = Some(Ok(resp));
    });
}

/// Fetch's blob scheme, as `fetch` serves it (fetch.rs `fetch_blob`): GET
/// only, the entry `open()` resolved, `200` with `Content-Length` and the
/// Blob's type, or `206` for one valid `Range`. A path-backed Blob is read on
/// a read thread.
unsafe fn local_blob(
    g: *mut c_void,
    sh: &Arc<Shared>,
    method: &str,
    url: &str,
    entry: Option<super::url::BlobEntry>,
    headers: &[(String, String)],
) {
    if method != "GET" {
        sh.post(|i| i.done = Some(Err(failed(format!("a blob: URL can only be fetched with GET, not {method}")))));
        return;
    }
    let Some(entry) = entry else {
        sh.post(|i| {
            i.done = Some(Err(failed(
                "this blob: URL is not in the blob URL store (it was revoked before open(), or never created in this context)"
                    .into(),
            )))
        });
        return;
    };
    let (start, len, content_range) = match super::fetch::blob_range(entry.range.len, headers) {
        Ok(r) => r,
        Err(m) => {
            sh.post(|i| i.done = Some(Err(failed(m))));
            return;
        }
    };
    let status: u16 = if content_range.is_some() { 206 } else { 200 };
    let status_text = if status == 206 { "Partial Content" } else { "OK" };
    let mut hs = vec![("content-length".to_string(), len.to_string()), ("content-type".to_string(), entry.mime.clone())];
    if let Some(cr) = content_range {
        hs.push(("content-range".to_string(), cr));
    }
    let window = blob::Range { source: entry.range.source.clone(), offset: entry.range.offset + start, len };
    if let Some(bytes) = window.in_memory() {
        answer(sh, status, status_text, hs, url, bytes.to_vec());
        return;
    }
    let (sh, url) = (Arc::clone(sh), url.to_string());
    blob::read_async(
        g,
        window,
        Box::new(move |_g, r| match r {
            Ok(bytes) => answer(&sh, status, status_text, hs, &url, bytes),
            Err(e) => sh.post(|i| i.done = Some(Err(failed(format!("{}: {}", e.name(), e.message()))))),
        }),
    );
}

/// Every other URL `fetch` would read locally (fetch.rs `fetch_local`): a
/// `data:` URL, or a path the app ships (`xhr.open("GET", "data/x.json")`).
/// GET and HEAD only. The raw URL is what is read, as `fetch` is given it:
/// the resolved one would be under `location.href`'s made-up origin.
unsafe fn local_read(g: *mut c_void, sh: &Arc<Shared>, method: &str, raw_url: &str, url: &str) {
    if method != "GET" && method != "HEAD" {
        sh.post(|i| {
            i.done = Some(Err(failed(format!("{method} is not supported for local and data: URLs; only GET and HEAD are"))))
        });
        return;
    }
    let f = super::fsutil::fetch_url(g, raw_url);
    if f.status == 0 {
        sh.post(|i| i.done = Some(Err(failed("the URL could not be read".into()))));
        return;
    }
    let body = if method == "GET" { f.bytes } else { Vec::new() };
    answer(sh, f.status as u16, f.status_text, vec![("content-type".to_string(), f.mime)], url, body);
}

// ---------------------------------------------------------------------------
// Request headers
// ---------------------------------------------------------------------------

/// Fetch's forbidden request-header names: the user agent owns them.
fn forbidden_request_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(
        n.as_str(),
        "accept-charset"
            | "accept-encoding"
            | "access-control-request-headers"
            | "access-control-request-method"
            | "connection"
            | "content-length"
            | "cookie"
            | "cookie2"
            | "date"
            | "dnt"
            | "expect"
            | "host"
            | "keep-alive"
            | "origin"
            | "referer"
            | "set-cookie"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "via"
    ) || n.starts_with("proxy-")
        || n.starts_with("sec-")
}

fn http_trim(s: &str) -> &str {
    s.trim_matches(|c| c == ' ' || c == '\t' || c == '\r' || c == '\n')
}

/// The `charset` parameter of a MIME type, if any.
pub(super) fn charset_of(mime: &str) -> Option<String> {
    mime.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("charset").then(|| v.trim().trim_matches('"').to_string())
    })
}

/// "Final MIME type": the override, or the response's `Content-Type`.
fn final_mime(d: &XhrData) -> String {
    if let Some(m) = &d.override_mime {
        return m.clone();
    }
    http::header(&d.resp_headers, "content-type").unwrap_or("").to_string()
}

/// "Text response": the body decoded by the final charset (UTF-8 without
/// one), a BOM overriding it.
fn decode_text(d: &mut XhrData) -> &str {
    let len = d.body.len();
    if d.text.as_ref().map(|(n, _)| *n) != Some(len) {
        let enc = charset_of(&final_mime(d))
            .and_then(|c| encoding_rs::Encoding::for_label(c.as_bytes()))
            .unwrap_or(encoding_rs::UTF_8);
        let (text, _, _) = enc.decode(&d.body);
        d.text = Some((len, text.into_owned()));
    }
    &d.text.as_ref().unwrap().1
}

// ---------------------------------------------------------------------------
// XMLHttpRequest
// ---------------------------------------------------------------------------

unsafe fn this_xhr<'a>(g: *mut c_void, cf: *mut c_void, what: &str) -> Result<&'a mut XhrData, u64> {
    xd(g, cf_this(cf)).ok_or_else(|| {
        throw_type_error(g, &format!("Illegal invocation: XMLHttpRequest.{what} called on a non-XMLHttpRequest"))
    })
}

macro_rules! this_or_throw {
    ($g:expr, $cf:expr, $what:literal) => {
        match this_xhr($g, $cf, $what) {
            Ok(d) => d,
            Err(e) => return e,
        }
    };
}

unsafe fn invalid_state(g: *mut c_void, msg: &str) -> u64 {
    domexception::throw(g, "InvalidStateError", msg)
}

#[crate::host_fn]
unsafe fn xhr_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    // OrdinaryCreateFromConstructor: a subclass's prototype, else ours.
    let nt = cf_new_target(cf);
    let mut proto = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
    if !is_cell(proto) {
        proto = c.xhr.proto;
    }
    let obj = host_create(g, c.xhr_class, proto, XhrData::new());
    let upload = new_object(g, c.upload.proto);
    glue::rjs_define(g, obj, c.s_upload, upload, 0);
    obj
}

/// The document's base URL: `location.href` when there is one.
pub(super) unsafe fn base_url(g: *mut c_void) -> Option<url::Url> {
    let loc = get(g, glue::rjs_global(g), "location");
    if !is_cell(loc) {
        return None;
    }
    url::Url::parse(&read_string(g, get(g, loc, "href"))?).ok()
}

#[crate::host_fn]
unsafe fn xhr_open(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if xd(g, this).is_none() {
        return throw_type_error(g, "Illegal invocation: XMLHttpRequest.open called on a non-XMLHttpRequest");
    }
    if cf_argc(cf) < 2 {
        return throw_type_error(
            g,
            &format!(
                "Failed to execute 'open' on 'XMLHttpRequest': 2 arguments required, but only {} present.",
                cf_argc(cf)
            ),
        );
    }
    let Ok(m) = to_js_string(g, cf_arg(cf, 0)) else { return UNDEF };
    let method = read_string(g, m).unwrap_or_default();
    let Ok(u) = to_js_string(g, cf_arg(cf, 1)) else { return UNDEF };
    let raw_url = read_string(g, u).unwrap_or_default();
    if !headers::valid_name(&method) {
        return domexception::throw(g, "SyntaxError", &format!("'{method}' is not a valid HTTP method."));
    }
    let upper = method.to_ascii_uppercase();
    if matches!(upper.as_str(), "CONNECT" | "TRACE" | "TRACK") {
        return domexception::throw(g, "SecurityError", &format!("'{method}' HTTP method is unsupported."));
    }
    // Normalize: the six standard methods are upper-cased, others kept.
    let method = if matches!(upper.as_str(), "DELETE" | "GET" | "HEAD" | "OPTIONS" | "POST" | "PUT") {
        upper
    } else {
        method
    };
    let parsed = match base_url(g) {
        Some(base) => base.join(&raw_url),
        None => url::Url::parse(&raw_url),
    };
    let Ok(mut parsed) = parsed else {
        return domexception::throw(g, "SyntaxError", &format!("Invalid URL: '{raw_url}'"));
    };
    let is_async = cf_argc(cf) < 3 || glue::rjs_to_boolean(g, cf_arg(cf, 2)) != 0;
    if !is_async {
        advise(
            "sync",
            "a synchronous request (open(..., false)) was refused: it would freeze the UI thread for \
             the whole round trip, and this runtime's event loop cannot block. Use an asynchronous one.",
        );
        return domexception::throw(
            g,
            "InvalidAccessError",
            "Synchronous XMLHttpRequest is not supported by this runtime: it would block the UI thread. \
             Pass true (or nothing) as the third argument.",
        );
    }
    let user = cf_arg(cf, 3);
    let pass = cf_arg(cf, 4);
    if parsed.host().is_some() && (is_cell(user) || is_cell(pass)) {
        // The URL keeps them, as the spec says; the transport sends no
        // Authorization from them.
        if is_cell(user) {
            let _ = parsed.set_username(&read_string(g, user).unwrap_or_default());
        }
        if is_cell(pass) {
            let _ = parsed.set_password(Some(&read_string(g, pass).unwrap_or_default()));
        }
        advise(
            "credentials",
            "open()'s user and password are kept in the URL but not sent: this runtime does no HTTP \
             authentication. Set an Authorization header instead.",
        );
    }

    // Terminate what is in flight, silently.
    let owed = end_flight(g, this, true);
    let Some(d) = xd(g, this) else { return UNDEF };
    d.send_flag = false;
    d.upload_listened = false;
    d.method = method;
    d.url = parsed.to_string();
    d.blob = if parsed.scheme() == "blob" { super::url::lookup(g, parsed.as_str()) } else { None };
    d.raw_url = raw_url;
    d.headers.clear();
    d.network_error();
    d.timed_out = false;
    glue::rjs_define(g, this, classes(g).s_response, UNDEF, 0);
    let fire = d.state != OPENED;
    d.state = OPENED;
    if fire {
        fire_readystatechange(g, this);
    }
    if owed {
        crate::ffi::redjs_unprotect(g, this);
    }
    UNDEF
}

#[crate::host_fn]
unsafe fn xhr_set_request_header(g: *mut c_void, cf: *mut c_void) -> u64 {
    if cf_argc(cf) < 2 {
        return throw_type_error(
            g,
            &format!(
                "Failed to execute 'setRequestHeader' on 'XMLHttpRequest': 2 arguments required, but only {} present.",
                cf_argc(cf)
            ),
        );
    }
    let d = this_or_throw!(g, cf, "setRequestHeader");
    if d.state != OPENED {
        return invalid_state(g, "Failed to execute 'setRequestHeader' on 'XMLHttpRequest': The object's state must be OPENED.");
    }
    if d.send_flag {
        return invalid_state(g, "Failed to execute 'setRequestHeader' on 'XMLHttpRequest': send() has already been called.");
    }
    let Ok(n) = to_js_string(g, cf_arg(cf, 0)) else { return UNDEF };
    let Ok(v) = to_js_string(g, cf_arg(cf, 1)) else { return UNDEF };
    let name = read_string(g, n).unwrap_or_default();
    let value = http_trim(&read_string(g, v).unwrap_or_default()).to_string();
    if !headers::valid_name(&name) {
        return domexception::throw(g, "SyntaxError", &format!("'{name}' is not a valid HTTP header field name."));
    }
    if !headers::valid_value(&value) {
        return domexception::throw(g, "SyntaxError", &format!("'{value}' is not a valid HTTP header field value."));
    }
    if forbidden_request_header(&name) {
        // Chromium's console line, word for word.
        advise(&format!("unsafe:{}", name.to_ascii_lowercase()), &format!("Refused to set unsafe header \"{name}\""));
        return UNDEF;
    }
    // `to_js_string` can run script; take the data again.
    let d = this_or_throw!(g, cf, "setRequestHeader");
    match d.headers.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(&name)) {
        Some((_, existing)) => {
            existing.push_str(", ");
            existing.push_str(&value);
        }
        None => d.headers.push((name, value)),
    }
    UNDEF
}

/// Does anything listen on the upload object? (The spec's "upload listener
/// flag", taken at `send()`.)
unsafe fn upload_listened(g: *mut c_void, upload: u64) -> bool {
    is_cell(upload) && PROGRESS_TYPES.iter().any(|t| events::has_listener(g, upload, t))
}

#[crate::host_fn]
unsafe fn xhr_send(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let d = this_or_throw!(g, cf, "send");
    if d.state != OPENED {
        return invalid_state(g, "Failed to execute 'send' on 'XMLHttpRequest': The object's state must be OPENED.");
    }
    if d.send_flag {
        return invalid_state(g, "Failed to execute 'send' on 'XMLHttpRequest': The object's state must be OPENED.");
    }
    let body = if d.method == "GET" || d.method == "HEAD" {
        None
    } else {
        match request::extract_body(g, cf_arg(cf, 0)) {
            Ok(b) => b,
            Err(m) => return throw_type_error(g, &format!("Failed to execute 'send' on 'XMLHttpRequest': {m}")),
        }
    };
    // Extracting a body can run script (a FormData entry's getter, toString).
    let d = this_or_throw!(g, cf, "send");
    if d.state != OPENED || d.send_flag {
        return UNDEF;
    }
    let mut req_headers = d.headers.clone();
    let (bytes, content_type) = match body {
        Some((b, t)) => (Some(b), t),
        None => (None, None),
    };
    if let Some(t) = content_type {
        if !req_headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
            req_headers.push(("Content-Type".into(), t));
        }
    }
    d.req_len = bytes.as_ref().map_or(0, |b| b.len() as u64);
    d.upload_complete = bytes.is_none();
    d.timed_out = false;
    let upload = upload_of(g, this);
    let listened = bytes.is_some() && upload_listened(g, upload);
    let Some(d) = xd(g, this) else { return UNDEF };
    d.upload_listened = listened;
    d.send_flag = true;
    let req_len = d.req_len;

    fire_progress(g, this, "loadstart", 0, 0);
    if let Some(d) = xd(g, this) {
        if !d.upload_complete && d.upload_listened {
            fire_progress(g, upload, "loadstart", 0, req_len);
        }
    }
    let Some(d) = xd(g, this) else { return UNDEF };
    if d.state != OPENED || !d.send_flag {
        return UNDEF;
    }

    let flight = NEXT_FLIGHT.fetch_add(1, Ordering::Relaxed);
    let cancel = http::Cancel::default();
    d.flight = flight;
    d.cancel = Some(cancel.clone());
    d.started = Some(Instant::now());
    let timeout_ms = d.timeout_ms;
    flights().lock().unwrap().insert((g as usize, flight), this);
    crate::ffi::redjs_protect(g, this);

    let sh = Arc::new(Shared {
        flight,
        ctx: Ctx(g),
        cid: crate::context::id_of(g),
        inbox: Mutex::new(Inbox::default()),
    });
    let lower = d.url.to_ascii_lowercase();
    let network = lower.starts_with("http://") || lower.starts_with("https://");
    // Everything but http(s) is answered as `fetch` answers the same URL: the
    // blob URL store, a `data:` URL, or the app's own files.
    let refusal = if network {
        (INFLIGHT.load(Ordering::SeqCst) >= MAX_INFLIGHT)
            .then(|| format!("more than {MAX_INFLIGHT} requests are already in flight"))
    } else if let Some(scheme) = super::fetch::unsupported_scheme(&lower) {
        advise(
            &format!("scheme:{scheme}"),
            &format!("{scheme}: URLs are not supported (as for fetch); the request is a network error"),
        );
        Some(format!("the {scheme}: scheme is not supported"))
    } else {
        None
    };
    if let Some(m) = refusal {
        // The network error is reported asynchronously, as a browser does.
        sh.post(|i| i.done = Some(Err(refused(m))));
    } else if lower.starts_with("blob:") {
        let entry = d.blob.clone();
        let (method, url) = (d.method.clone(), d.url.clone());
        local_blob(g, &sh, &method, &url, entry, &req_headers);
    } else if !network {
        let (method, raw, url) = (d.method.clone(), d.raw_url.clone(), d.url.clone());
        local_read(g, &sh, &method, &raw, &url);
    } else {
        let req = http::Request {
            method: d.method.clone(),
            url: d.url.clone(),
            headers: req_headers,
            body: bytes,
            redirect: http::Redirect::Follow,
            credentials: if d.with_credentials {
                cookies::Credentials::Include
            } else {
                cookies::Credentials::SameOrigin
            },
            connect_timeout: CONNECT_TIMEOUT,
            io_timeout: IO_TIMEOUT,
            observer: Some(http::Observer(Arc::new(Obs(Arc::clone(&sh))))),
            // XMLHttpRequest has no cache mode; it goes through the HTTP cache
            // as a browser's does.
            cache: http::cache::CacheMode::Default,
        };
        INFLIGHT.fetch_add(1, Ordering::SeqCst);
        let worker = Arc::clone(&sh);
        let started = http::start(
            req,
            cancel,
            Box::new(move |result| {
                INFLIGHT.fetch_sub(1, Ordering::SeqCst);
                worker.post(|i| i.done = Some(result));
            }),
        );
        if let Err(e) = started {
            INFLIGHT.fetch_sub(1, Ordering::SeqCst);
            sh.post(|i| i.done = Some(Err(e)));
        }
    }
    if timeout_ms > 0 {
        if let Some(d) = xd(g, this) {
            arm_timeout(g, d);
        }
    }
    UNDEF
}

#[crate::host_fn]
unsafe fn xhr_abort(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let _ = this_or_throw!(g, cf, "abort");
    let owed = end_flight(g, this, true);
    if let Some(d) = xd(g, this) {
        if (d.state == OPENED && d.send_flag) || d.state == HEADERS_RECEIVED || d.state == LOADING {
            request_error_steps(g, this, "abort");
        }
    }
    if let Some(d) = xd(g, this) {
        if d.state == DONE {
            // No `readystatechange` for this one (spec).
            d.state = UNSENT;
            d.network_error();
        }
    }
    if owed {
        crate::ffi::redjs_unprotect(g, this);
    }
    UNDEF
}

#[crate::host_fn]
unsafe fn xhr_get_response_header(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Ok(n) = to_js_string(g, cf_arg(cf, 0)) else { return UNDEF };
    let name = read_string(g, n).unwrap_or_default();
    let d = this_or_throw!(g, cf, "getResponseHeader");
    if d.state < HEADERS_RECEIVED || d.error {
        return NULL;
    }
    let values: Vec<&str> = d
        .resp_headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(&name))
        .map(|(_, v)| v.as_str())
        .collect();
    if values.is_empty() {
        NULL
    } else {
        js_string(g, &values.join(", "))
    }
}

#[crate::host_fn]
unsafe fn xhr_get_all_response_headers(g: *mut c_void, cf: *mut c_void) -> u64 {
    let d = this_or_throw!(g, cf, "getAllResponseHeaders");
    if d.state < HEADERS_RECEIVED || d.error {
        return js_string(g, "");
    }
    let lowered: Vec<(String, String)> =
        d.resp_headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.clone())).collect();
    let mut out = String::new();
    for (k, v) in headers::sorted_combined(&lowered) {
        out.push_str(&k);
        out.push_str(": ");
        out.push_str(&v);
        out.push_str("\r\n");
    }
    js_string(g, &out)
}

#[crate::host_fn]
unsafe fn xhr_override_mime_type(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Ok(m) = to_js_string(g, cf_arg(cf, 0)) else { return UNDEF };
    let mime = read_string(g, m).unwrap_or_default();
    let d = this_or_throw!(g, cf, "overrideMimeType");
    if d.state == LOADING || d.state == DONE {
        return invalid_state(
            g,
            "Failed to execute 'overrideMimeType' on 'XMLHttpRequest': MimeType cannot be overridden when the state is LOADING or DONE.",
        );
    }
    let essence = mime.split(';').next().unwrap_or("").trim();
    let valid = essence.split_once('/').is_some_and(|(t, s)| headers::valid_name(t) && headers::valid_name(s));
    d.override_mime = Some(if valid { mime.trim().to_ascii_lowercase() } else { "application/octet-stream".into() });
    d.text = None;
    UNDEF
}

#[crate::host_fn]
unsafe fn xhr_ready_state(g: *mut c_void, cf: *mut c_void) -> u64 {
    let d = this_or_throw!(g, cf, "readyState");
    enc_i32(d.state as i32)
}

#[crate::host_fn]
unsafe fn xhr_upload(g: *mut c_void, cf: *mut c_void) -> u64 {
    let _ = this_or_throw!(g, cf, "upload");
    upload_of(g, cf_this(cf))
}

#[crate::host_fn]
unsafe fn xhr_status(g: *mut c_void, cf: *mut c_void) -> u64 {
    let d = this_or_throw!(g, cf, "status");
    enc_i32(if d.error { 0 } else { d.status as i32 })
}

#[crate::host_fn]
unsafe fn xhr_status_text(g: *mut c_void, cf: *mut c_void) -> u64 {
    let d = this_or_throw!(g, cf, "statusText");
    let t = if d.error { String::new() } else { d.status_text.clone() };
    js_string(g, &t)
}

#[crate::host_fn]
unsafe fn xhr_response_url(g: *mut c_void, cf: *mut c_void) -> u64 {
    let d = this_or_throw!(g, cf, "responseURL");
    let u = if d.error { String::new() } else { d.resp_url.clone() };
    js_string(g, &u)
}

#[crate::host_fn]
unsafe fn xhr_response_text(g: *mut c_void, cf: *mut c_void) -> u64 {
    let d = this_or_throw!(g, cf, "responseText");
    if !d.rtype.texty() {
        return invalid_state(
            g,
            &format!(
                "Failed to read the 'responseText' property from 'XMLHttpRequest': The value is only accessible \
                 if the object's 'responseType' is '' or 'text' (was '{}').",
                d.rtype.name()
            ),
        );
    }
    if d.state != LOADING && d.state != DONE || !d.has_body {
        return js_string(g, "");
    }
    let t = decode_text(d).to_string();
    js_string(g, &t)
}

#[crate::host_fn]
unsafe fn xhr_response_xml(g: *mut c_void, cf: *mut c_void) -> u64 {
    let d = this_or_throw!(g, cf, "responseXML");
    if !matches!(d.rtype, RType::Empty | RType::Document) {
        return invalid_state(
            g,
            &format!(
                "Failed to read the 'responseXML' property from 'XMLHttpRequest': The value is only accessible \
                 if the object's 'responseType' is '' or 'document' (was '{}').",
                d.rtype.name()
            ),
        );
    }
    if d.state == DONE && d.has_body {
        advise("xml", "responseXML is null: this runtime has no XML/HTML parser (read responseText instead)");
    }
    NULL
}

#[crate::host_fn]
unsafe fn xhr_response(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let d = this_or_throw!(g, cf, "response");
    if d.rtype.texty() {
        if d.state != LOADING && d.state != DONE || !d.has_body {
            return js_string(g, "");
        }
        let t = decode_text(d).to_string();
        return js_string(g, &t);
    }
    if d.state != DONE || d.error {
        return NULL;
    }
    let c = classes(g);
    let cached = get_a(g, this, c.s_response);
    if cached != UNDEF {
        return cached;
    }
    let v = match d.rtype {
        RType::ArrayBuffer => {
            let u8 = make_u8(g, &d.body);
            if is_cell(u8) {
                get(g, u8, "buffer")
            } else {
                NULL
            }
        }
        RType::Blob => {
            let mime = if d.has_body { final_mime(d) } else { String::new() };
            let bytes = d.body.clone();
            blob::create(g, UNDEF, bytes, mime)
        }
        RType::Json => {
            if !d.has_body || d.body.is_empty() {
                NULL
            } else {
                // "JSON response": UTF-8, whatever the charset says.
                let (text, _, _) = encoding_rs::UTF_8.decode(&d.body);
                let sv = js_string(g, &text);
                call(g, state(g).json_parse, UNDEF, &[sv]).unwrap_or(NULL)
            }
        }
        RType::Document => {
            if d.has_body {
                advise("xml", "responseType \"document\" gives null: this runtime has no XML/HTML parser");
            }
            NULL
        }
        RType::Empty | RType::Text => unreachable!(),
    };
    glue::rjs_define(g, this, c.s_response, v, 0);
    v
}

#[crate::host_fn]
unsafe fn xhr_response_type_get(g: *mut c_void, cf: *mut c_void) -> u64 {
    let d = this_or_throw!(g, cf, "responseType");
    js_string(g, d.rtype.name())
}

#[crate::host_fn]
unsafe fn xhr_response_type_set(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Ok(v) = to_js_string(g, cf_arg(cf, 0)) else { return UNDEF };
    let s = read_string(g, v).unwrap_or_default();
    let d = this_or_throw!(g, cf, "responseType");
    // WebIDL: an unknown enum value is ignored on assignment.
    let Some(t) = RType::parse(&s) else { return UNDEF };
    if d.state == LOADING || d.state == DONE {
        return invalid_state(
            g,
            "Failed to set the 'responseType' property on 'XMLHttpRequest': The response type cannot be set if the \
             object's state is LOADING or DONE.",
        );
    }
    d.rtype = t;
    UNDEF
}

#[crate::host_fn]
unsafe fn xhr_timeout_get(g: *mut c_void, cf: *mut c_void) -> u64 {
    let d = this_or_throw!(g, cf, "timeout");
    enc_f64(d.timeout_ms as f64)
}

#[crate::host_fn]
unsafe fn xhr_timeout_set(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Ok(n) = to_number(g, cf_arg(cf, 0)) else { return UNDEF };
    // WebIDL `unsigned long`: modulo 2^32.
    let ms = if n.is_finite() { n.trunc().rem_euclid(4294967296.0) as u32 } else { 0 };
    let d = this_or_throw!(g, cf, "timeout");
    d.timeout_ms = ms;
    arm_timeout(g, d);
    UNDEF
}

#[crate::host_fn]
unsafe fn xhr_with_credentials_get(g: *mut c_void, cf: *mut c_void) -> u64 {
    let d = this_or_throw!(g, cf, "withCredentials");
    enc_bool(d.with_credentials)
}

#[crate::host_fn]
unsafe fn xhr_with_credentials_set(g: *mut c_void, cf: *mut c_void) -> u64 {
    let v = glue::rjs_to_boolean(g, cf_arg(cf, 0)) != 0;
    let d = this_or_throw!(g, cf, "withCredentials");
    if (d.state != UNSENT && d.state != OPENED) || d.send_flag {
        return invalid_state(
            g,
            "Failed to set the 'withCredentials' property on 'XMLHttpRequest': The value may only be set if the \
             object's state is UNSENT or OPENED.",
        );
    }
    d.with_credentials = v;
    UNDEF
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

/// Needs `Event.prototype` (ProgressEvent) and `EventTarget.prototype`.
pub unsafe fn install(g: *mut c_void, event_proto: u64, event_target_proto: u64) {
    let xhr_class = make_host_class::<XhrData>("XMLHttpRequest");

    let progress = define_class(g, &ClassSpec {
        name: "ProgressEvent",
        arity: 1,
        construct: Some(progress_event_construct),
        parent_proto: event_proto,
        methods: &[],
        getters: &[("lengthComputable", pe_length_computable), ("loaded", pe_loaded), ("total", pe_total)],
        accessors: &[],
    });

    let target = define_class(g, &ClassSpec {
        name: "XMLHttpRequestEventTarget",
        arity: 0,
        construct: None,
        parent_proto: event_target_proto,
        methods: &[],
        getters: &[],
        accessors: &[
            ("onloadstart", get_onloadstart, set_onloadstart),
            ("onprogress", get_onprogress, set_onprogress),
            ("onabort", get_onabort, set_onabort),
            ("onerror", get_onerror, set_onerror),
            ("onload", get_onload, set_onload),
            ("ontimeout", get_ontimeout, set_ontimeout),
            ("onloadend", get_onloadend, set_onloadend),
        ],
    });

    let upload = define_class(g, &ClassSpec {
        name: "XMLHttpRequestUpload",
        arity: 0,
        construct: None,
        parent_proto: target.proto,
        methods: &[],
        getters: &[],
        accessors: &[],
    });

    let xhr = define_class(g, &ClassSpec {
        name: "XMLHttpRequest",
        arity: 0,
        construct: Some(xhr_construct),
        parent_proto: target.proto,
        methods: &[
            ("open", 2, xhr_open),
            ("setRequestHeader", 2, xhr_set_request_header),
            ("send", 0, xhr_send),
            ("abort", 0, xhr_abort),
            ("getResponseHeader", 1, xhr_get_response_header),
            ("getAllResponseHeaders", 0, xhr_get_all_response_headers),
            ("overrideMimeType", 1, xhr_override_mime_type),
        ],
        getters: &[
            ("readyState", xhr_ready_state),
            ("upload", xhr_upload),
            ("status", xhr_status),
            ("statusText", xhr_status_text),
            ("responseURL", xhr_response_url),
            ("response", xhr_response),
            ("responseText", xhr_response_text),
            ("responseXML", xhr_response_xml),
        ],
        accessors: &[
            ("timeout", xhr_timeout_get, xhr_timeout_set),
            ("withCredentials", xhr_with_credentials_get, xhr_with_credentials_set),
            ("responseType", xhr_response_type_get, xhr_response_type_set),
            ("onreadystatechange", get_onreadystatechange, set_onreadystatechange),
        ],
    });
    // WebIDL `const` members, on both the constructor and the prototype.
    for (name, v) in [("UNSENT", 0), ("OPENED", 1), ("HEADERS_RECEIVED", 2), ("LOADING", 3), ("DONE", 4)] {
        define(g, xhr.ctor, name, enc_i32(v), 1 | 2 | 4);
        define(g, xhr.proto, name, enc_i32(v), 1 | 2 | 4);
    }

    let c: &'static XhrClasses = Box::leak(Box::new(XhrClasses {
        xhr_class,
        xhr,
        upload,
        progress,
        s_handlers: atom_priv(g, "redjs#xhrHandlers"),
        s_upload: atom_priv(g, "redjs#xhrUpload"),
        s_response: atom_priv(g, "redjs#xhrResponse"),
    }));
    classes_reg().lock().unwrap().insert(g as usize, c);
}

#[cfg(test)]
mod jsc_tests;
