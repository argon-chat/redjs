//! `fetch`, `Request` and `Response` (WHATWG Fetch).
//!
//! ## The contract this module is built around
//!
//! **A request either happens, or the promise rejects.** There is no third
//! outcome. What used to be here did have a third outcome: an `http(s)` URL
//! printed
//!
//! ```text
//! [js:fetch] http(s) is not supported in the player
//! ```
//!
//! to stderr and then *resolved* with a `Response` carrying `status: 0` and an
//! empty body. Nothing threw, nothing rejected, and the caller — an application
//! resolving its backend instance, loading an account, reading remote config —
//! saw a well-formed answer that described a request no socket had ever carried.
//! A silent wrong answer, in the runtime's own words.
//!
//! Everything below follows from removing that outcome:
//!
//! - `http:`/`https:` go through a real client ([`super::http`]).
//! - Anything this runtime cannot do is **refused by name**: the rejection says
//!   which option it was and why, so the failure is a sentence, not a mystery.
//! - `Response` never carries a synthetic status. The only `status: 0` this
//!   module can produce is `Response.error()`, which is the spec's own network-
//!   error sentinel and is tagged `type: "error"` so it cannot be confused with
//!   a real reply.
//! - A body that arrives short is an error, not a short body (enforced in the
//!   transport, see [`super::http`]).
//!
//! ## Threading
//!
//! `fetch` returns immediately. The round trip runs on a worker thread that
//! never touches JSC; the result comes back through `web::schedule_native`, the
//! same pump hand-off image decoding uses, and the promise is settled from the
//! pump thread with the VM lock held. Blocking the pump on a socket would stall
//! the whole application for the duration of the request, and settling a promise
//! from the worker thread would touch the VM from off-thread; this is the only
//! shape that does neither.
//!
//! ## Streaming
//!
//! Response bodies are **buffered**, not streamed. `Response.body` is a real
//! `ReadableStream` (so `for await (const chunk of res.body)` works), but it is
//! a stream over bytes already in memory: peak cost is the whole body, and the
//! first chunk is not available until the last one has arrived. That is the
//! wrong trade for a multi-hundred-megabyte download and the right one for the
//! JSON and config payloads this exists to unblock — large assets in this engine
//! come off disk through the content provider, not off the network. Wiring the
//! transport to the streams core incrementally is a follow-up, not a shim: it
//! changes only what `body` is fed from.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use super::headers;
use super::http;
use super::request::{self, build_plan, extract_body, Plan};
use super::util::*;
use super::{blob, fsutil};
use crate::capi;
use crate::glue;

/// Simultaneous in-flight network requests. One thread each; past this a
/// request is refused loudly rather than exhausting the thread pool of the
/// process a game is running in.
const MAX_INFLIGHT: usize = 64;
static INFLIGHT: AtomicUsize = AtomicUsize::new(0);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IO_TIMEOUT: Duration = Duration::from_secs(60);

/// `*mut c_void` is not `Send`, but the pump hand-off needs the context key on
/// the worker thread purely to name the queue it posts back to — it is never
/// dereferenced off the JS thread.
#[derive(Clone, Copy)]
struct Ctx(*mut c_void);
unsafe impl Send for Ctx {}

impl Ctx {
    /// A method, not a field read: edition-2021 closures capture individual
    /// fields, and capturing `ctx.0` would capture the bare `*mut c_void`
    /// (not `Send`) instead of the wrapper.
    fn ptr(self) -> *mut c_void {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

pub struct RespData {
    pub bytes: Arc<Vec<u8>>,
    pub headers: Vec<(String, String)>,
    pub url: String,
    pub status: i32,
    pub status_text: String,
    pub redirected: bool,
    /// `"basic"` for a real reply, `"error"` for `Response.error()`.
    pub kind: &'static str,
    /// Spec "null body": `body` is `null` and there is nothing to consume.
    pub has_body: bool,
    /// Set by the first body read; a second one is a TypeError, as in browsers.
    pub body_used: bool,
}

impl RespData {
    fn mime(&self) -> String {
        http::header(&self.headers, "content-type")
            .map(|s| s.to_string())
            .unwrap_or_default()
    }
}

unsafe fn this_resp<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut RespData> {
    host_data::<RespData>(g, state(g).response_class, cf_this(cf))
}

pub unsafe fn create_response(g: *mut c_void, data: RespData) -> u64 {
    let st = state(g);
    host_create(g, st.response_class, st.response.proto, data)
}

// ---------------------------------------------------------------------------
// Errors and promises
// ---------------------------------------------------------------------------

pub const ERR: u32 = 0;
pub const TYPE_ERR: u32 = 1;

pub unsafe fn make_error(g: *mut c_void, kind: u32, name: &str, msg: &str) -> u64 {
    let m: Vec<u16> = msg.encode_utf16().collect();
    let mut err = UNDEF;
    glue::rjs_error_create(g, kind, m.as_ptr(), m.len(), &mut err);
    if !name.is_empty() && is_cell(err) {
        define(g, err, "name", js_string(g, name), 0);
    }
    err
}

/// A promise already rejected with `value`.
pub unsafe fn reject_with(g: *mut c_void, value: u64) -> u64 {
    let (mut res, mut rej, mut p) = (0u64, 0u64, 0u64);
    if crate::ffi::redjs_promise_create(g, &mut res, &mut rej, &mut p) != 0 {
        return UNDEF;
    }
    let _ = call(g, rej, UNDEF, &[value]);
    p
}

/// The rejection every refusal goes through. `TypeError` is what a browser
/// rejects `fetch` with, and the message names the thing we would not do.
pub unsafe fn reject_refusal(g: *mut c_void, msg: &str) -> u64 {
    let e = make_error(g, TYPE_ERR, "", msg);
    reject_with(g, e)
}

// ---------------------------------------------------------------------------
// Body reading, shared by Request and Response
// ---------------------------------------------------------------------------

/// Marks the body consumed, or returns the rejection for a double read.
unsafe fn take_body(g: *mut c_void, cf: *mut c_void, who: &str) -> Result<Arc<Vec<u8>>, u64> {
    let Some(d) = this_resp(g, cf) else {
        return Err(reject_refusal(g, &format!("Response.{who} called on a non-Response")));
    };
    if d.body_used {
        return Err(reject_refusal(g, &format!("Response.{who}: body has already been consumed")));
    }
    d.body_used = true;
    Ok(Arc::clone(&d.bytes))
}

macro_rules! resp_getter {
    ($name:ident, |$d:ident, $g:ident| $body:expr) => {
        unsafe extern "sysv64" fn $name($g: *mut c_void, cf: *mut c_void) -> u64 {
            match this_resp($g, cf) {
                Some($d) => $body,
                None => throw_type_error($g, "Response getter called on a non-Response"),
            }
        }
    };
}

resp_getter!(resp_ok, |d, g| {
    let _ = g;
    enc_bool(d.status >= 200 && d.status < 300)
});
resp_getter!(resp_status, |d, g| {
    let _ = g;
    enc_i32(d.status)
});
resp_getter!(resp_status_text, |d, g| {
    let s = d.status_text.clone();
    js_string(g, &s)
});
resp_getter!(resp_url, |d, g| {
    let s = d.url.clone();
    js_string(g, &s)
});
resp_getter!(resp_redirected, |d, g| {
    let _ = g;
    enc_bool(d.redirected)
});
resp_getter!(resp_body_used, |d, g| {
    let _ = g;
    enc_bool(d.body_used)
});
resp_getter!(resp_type, |d, g| {
    let k = d.kind;
    js_string(g, k)
});

/// Cached per spec identity: `res.headers === res.headers`.
unsafe extern "sysv64" fn resp_headers(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let slot = atom_priv(g, "redjs#respHeaders");
    let cached = get_a(g, this, slot);
    if is_cell(cached) {
        glue::rjs_atom_free(g, slot);
        return cached;
    }
    let Some(d) = this_resp(g, cf) else {
        glue::rjs_atom_free(g, slot);
        return throw_type_error(g, "Response.headers on a non-Response");
    };
    let list = d.headers.clone();
    let h = headers::create(g, list);
    glue::rjs_define(g, this, slot, h, 0);
    glue::rjs_atom_free(g, slot);
    h
}

/// `Response.body` — a `ReadableStream` over the buffered bytes (see the module
/// header for why buffered). `null` for a null-body response, per spec.
unsafe extern "sysv64" fn resp_body(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let slot = atom_priv(g, "redjs#bodyStream");
    let cached = get_a(g, this, slot);
    if is_cell(cached) {
        glue::rjs_atom_free(g, slot);
        return cached;
    }
    let (bytes, has_body) = match this_resp(g, cf) {
        Some(d) => (Arc::clone(&d.bytes), d.has_body),
        None => {
            glue::rjs_atom_free(g, slot);
            return throw_type_error(g, "Response.body on a non-Response");
        }
    };
    if !has_body {
        glue::rjs_atom_free(g, slot);
        return NULL;
    }
    let stream = make_body_stream(g, &bytes);
    if is_cell(stream) {
        glue::rjs_define(g, this, slot, stream, 0);
        // Reading the stream is reading the body.
        if let Some(d) = this_resp(g, cf) {
            d.body_used = true;
        }
    }
    glue::rjs_atom_free(g, slot);
    stream
}

/// Lazily-built `bytes => ReadableStream` factory.
///
/// Lazy because `fetch::install` runs before `streams::install_surface` (the
/// install order in `webapi::install` is load-bearing), so `ReadableStream` does
/// not exist yet at install time. Held as a protected function value rather than
/// a global, so nothing is added to `globalThis`.
fn stream_factories() -> &'static Mutex<HashMap<usize, u64>> {
    static F: OnceLock<Mutex<HashMap<usize, u64>>> = OnceLock::new();
    F.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The factory was protected in the gone context; that heap is gone with it.
pub(crate) fn forget(gone: crate::context::Gone) {
    stream_factories().lock().unwrap().remove(&gone.key);
    super::request::forget(gone);
}

unsafe fn make_body_stream(g: *mut c_void, bytes: &[u8]) -> u64 {
    let factory = {
        let mut map = stream_factories().lock().unwrap();
        match map.get(&(g as usize)) {
            Some(&f) => f,
            None => {
                const SRC: &str = "(bytes => new ReadableStream({ \
                     start(c) { c.enqueue(bytes); c.close(); } \
                   }))";
                let url = "redjs/fetch-body-stream";
                let mut out = 0u64;
                let rc = glue::rjs_eval8(g, SRC.as_ptr(), SRC.len(), url.as_ptr(), url.len(), &mut out);
                if rc != 0 || !is_cell(out) {
                    return UNDEF;
                }
                crate::ffi::redjs_protect(g, out);
                map.insert(g as usize, out);
                out
            }
        }
    };
    let u8v = make_u8(g, bytes);
    match call(g, factory, UNDEF, &[u8v]) {
        Ok(v) => v,
        Err(_) => UNDEF,
    }
}

unsafe extern "sysv64" fn resp_array_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    let bytes = match take_body(g, cf, "arrayBuffer") {
        Ok(b) => b,
        Err(p) => return p,
    };
    let u8v = make_u8(g, &bytes);
    let buffer = get(g, u8v, "buffer");
    resolved_promise(g, buffer)
}

unsafe extern "sysv64" fn resp_bytes(g: *mut c_void, cf: *mut c_void) -> u64 {
    let bytes = match take_body(g, cf, "bytes") {
        Ok(b) => b,
        Err(p) => return p,
    };
    let u8v = make_u8(g, &bytes);
    resolved_promise(g, u8v)
}

unsafe extern "sysv64" fn resp_text(g: *mut c_void, cf: *mut c_void) -> u64 {
    let bytes = match take_body(g, cf, "text") {
        Ok(b) => b,
        Err(p) => return p,
    };
    let s = String::from_utf8_lossy(&bytes).into_owned();
    resolved_promise(g, js_string(g, &s))
}

unsafe extern "sysv64" fn resp_json(g: *mut c_void, cf: *mut c_void) -> u64 {
    let st = state(g);
    let bytes = match take_body(g, cf, "json") {
        Ok(b) => b,
        Err(p) => return p,
    };
    let s = String::from_utf8_lossy(&bytes).into_owned();
    let sv = js_string(g, &s);
    match call(g, st.json_parse, UNDEF, &[sv]) {
        Ok(v) => resolved_promise(g, v),
        // The parse error itself, not a generic one — a truncated or HTML error
        // page is the usual cause and the position tells you which.
        Err(e) => reject_with(g, e),
    }
}

unsafe extern "sysv64" fn resp_blob(g: *mut c_void, cf: *mut c_void) -> u64 {
    let mime = match this_resp(g, cf) {
        Some(d) => d.mime(),
        None => return reject_refusal(g, "Response.blob called on a non-Response"),
    };
    let bytes = match take_body(g, cf, "blob") {
        Ok(b) => b,
        Err(p) => return p,
    };
    let b = blob::create(g, UNDEF, bytes.as_slice().to_vec(), mime);
    resolved_promise(g, b)
}

unsafe extern "sysv64" fn resp_clone(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(d) = this_resp(g, cf) else {
        return throw_type_error(g, "Response.clone called on a non-Response");
    };
    if d.body_used {
        return throw_type_error(g, "Response.clone: body has already been consumed");
    }
    let data = RespData {
        bytes: Arc::clone(&d.bytes),
        headers: d.headers.clone(),
        url: d.url.clone(),
        status: d.status,
        status_text: d.status_text.clone(),
        redirected: d.redirected,
        kind: d.kind,
        has_body: d.has_body,
        body_used: false,
    };
    create_response(g, data)
}

/// Reads a `ResponseInit` / `RequestInit` `headers` + `status` + `statusText`.
unsafe fn response_init(g: *mut c_void, init: u64, default_mime: Option<String>) -> Result<(i32, String, Vec<(String, String)>), String> {
    let mut list = Vec::new();
    let mut status = 200i32;
    let mut status_text = String::new();
    if is_cell(init) {
        let h = get(g, init, "headers");
        list = headers::from_init(g, h)?;
        let s = get(g, init, "status");
        if s != UNDEF {
            let n = read_number(g, s);
            if !(200.0..=599.0).contains(&n) {
                return Err(format!("status {n} is outside the range 200-599"));
            }
            status = n as i32;
        }
        let t = get(g, init, "statusText");
        if t != UNDEF {
            status_text = read_string(g, t).unwrap_or_default();
        }
    }
    if let Some(mime) = default_mime {
        if !list.iter().any(|(k, _)| k == "content-type") && !mime.is_empty() {
            list.push(("content-type".into(), mime));
        }
    }
    Ok((status, status_text, list))
}

unsafe extern "sysv64" fn response_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let body_v = cf_arg(cf, 0);
    let (bytes, implied, has_body) = match extract_body(g, body_v) {
        Ok(Some((b, m))) => (b, m, true),
        Ok(None) => (Vec::new(), None, false),
        Err(m) => return throw_type_error(g, &format!("new Response: {m}")),
    };
    let (status, status_text, list) = match response_init(g, cf_arg(cf, 1), implied) {
        Ok(v) => v,
        Err(m) => return throw_type_error(g, &format!("new Response: {m}")),
    };
    create_response(g, RespData {
        bytes: Arc::new(bytes),
        headers: list,
        url: String::new(),
        status,
        status_text,
        redirected: false,
        kind: "default",
        has_body,
        body_used: false,
    })
}

/// `Response.json(data, init)`.
unsafe extern "sysv64" fn response_static_json(g: *mut c_void, cf: *mut c_void) -> u64 {
    let global = glue::rjs_global(g);
    let stringify = get(g, get(g, global, "JSON"), "stringify");
    let text = match call(g, stringify, UNDEF, &[cf_arg(cf, 0)]) {
        Ok(v) => read_string(g, v).unwrap_or_default(),
        Err(e) => {
            glue::rjs_throw(g, e);
            return UNDEF;
        }
    };
    let (status, status_text, list) =
        match response_init(g, cf_arg(cf, 1), Some("application/json".into())) {
            Ok(v) => v,
            Err(m) => return throw_type_error(g, &format!("Response.json: {m}")),
        };
    create_response(g, RespData {
        bytes: Arc::new(text.into_bytes()),
        headers: list,
        url: String::new(),
        status,
        status_text,
        redirected: false,
        kind: "default",
        has_body: true,
        body_used: false,
    })
}

/// `Response.error()` — the spec's network-error sentinel. This is the ONLY
/// legitimate `status: 0` in this module, and it is tagged `type: "error"` so
/// it can never be mistaken for the fake one that used to come back from
/// `fetch('https://…')`.
unsafe extern "sysv64" fn response_static_error(g: *mut c_void, _cf: *mut c_void) -> u64 {
    create_response(g, RespData {
        bytes: Arc::new(Vec::new()),
        headers: Vec::new(),
        url: String::new(),
        status: 0,
        status_text: String::new(),
        redirected: false,
        kind: "error",
        has_body: false,
        body_used: false,
    })
}

// ---------------------------------------------------------------------------
// AbortSignal bridge
// ---------------------------------------------------------------------------

struct BridgeData {
    cancel: Arc<http::Cancel>,
}

fn bridge_class() -> capi::JSClassRef {
    static CLS: OnceLock<usize> = OnceLock::new();
    *CLS.get_or_init(|| unsafe { make_host_class::<BridgeData>("FetchAbortBridge") as usize })
        as capi::JSClassRef
}

/// The `abort` listener. A listener OBJECT rather than a function, because a
/// native function value cannot carry the `Arc<Cancel>`; DOM listeners may be
/// objects with `handleEvent` (and `events::call_listener` honours that).
unsafe extern "sysv64" fn bridge_handle_event(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Some(d) = host_data::<BridgeData>(g, bridge_class(), cf_this(cf)) {
        d.cancel.abort();
    }
    UNDEF
}

unsafe fn make_bridge(g: *mut c_void, cancel: Arc<http::Cancel>) -> u64 {
    let obj = host_create(g, bridge_class(), UNDEF, BridgeData { cancel });
    define(g, obj, "handleEvent", make_fn(g, "handleEvent", 1, bridge_handle_event), 2);
    obj
}

unsafe fn signal_is_aborted(g: *mut c_void, signal: u64) -> bool {
    is_cell(signal) && truthy_flag(get(g, signal, "aborted"))
}

unsafe fn signal_reason(g: *mut c_void, signal: u64) -> u64 {
    let r = if is_cell(signal) { get(g, signal, "reason") } else { UNDEF };
    if r == UNDEF || r == NULL {
        super::domexception::create(g, "AbortError", "The operation was aborted")
    } else {
        r
    }
}

unsafe fn listen(g: *mut c_void, signal: u64, bridge: u64, add: bool) {
    let name = if add { "addEventListener" } else { "removeEventListener" };
    let f = get(g, signal, name);
    if is_cell(f) {
        let ev = js_string(g, "abort");
        let _ = call(g, f, signal, &[ev, bridge]);
    }
}

// ---------------------------------------------------------------------------
// fetch
// ---------------------------------------------------------------------------

/// Everything the delivery task needs, all of it `Send`.
struct Delivery {
    resolve: u64,
    reject: u64,
    signal: u64,
    bridge: u64,
    cancel: Arc<http::Cancel>,
    result: Result<http::Response, http::HttpError>,
}
unsafe impl Send for Delivery {}

unsafe fn settle(g: *mut c_void, d: Delivery) {
    if is_cell(d.signal) && is_cell(d.bridge) {
        listen(g, d.signal, d.bridge, false);
    }

    // An abort that landed while the result was queued still wins: the caller
    // asked for the request to stop before it observed an answer.
    let aborted = d.cancel.is_aborted() || signal_is_aborted(g, d.signal);

    match d.result {
        _ if aborted => {
            let reason = signal_reason(g, d.signal);
            let _ = call(g, d.reject, UNDEF, &[reason]);
        }
        Ok(resp) => {
            let has_body = !matches!(resp.status, 204 | 205 | 304);
            // Fetch's "forbidden response-header names": the jar has already
            // seen them (http::send, per hop); script never does, so
            // `headers.get("set-cookie")` is null and `getSetCookie()` is [].
            let mut headers = resp.headers;
            headers.retain(|(k, _)| !k.eq_ignore_ascii_case("set-cookie") && !k.eq_ignore_ascii_case("set-cookie2"));
            let obj = create_response(g, RespData {
                bytes: Arc::new(resp.body),
                headers,
                url: resp.url,
                status: resp.status as i32,
                status_text: resp.status_text,
                redirected: resp.redirected,
                kind: "basic",
                has_body,
                body_used: false,
            });
            let _ = call(g, d.resolve, UNDEF, &[obj]);
        }
        Err(e) => {
            // Browsers reject `fetch` with a TypeError for every network
            // failure; the message carries what actually went wrong.
            let err = match e.kind {
                http::ErrorKind::Aborted => super::domexception::create(g, "AbortError", "The operation was aborted"),
                _ => make_error(g, TYPE_ERR, "", &format!("Failed to fetch {}", e.message)),
            };
            let _ = call(g, d.reject, UNDEF, &[err]);
        }
    }

    crate::ffi::redjs_unprotect(g, d.resolve);
    crate::ffi::redjs_unprotect(g, d.reject);
    if is_cell(d.bridge) {
        crate::ffi::redjs_unprotect(g, d.bridge);
    }
    if is_cell(d.signal) {
        crate::ffi::redjs_unprotect(g, d.signal);
    }
}

unsafe fn fetch_network(g: *mut c_void, plan: Plan) -> u64 {
    let (mut resolve, mut reject, mut promise) = (0u64, 0u64, 0u64);
    if crate::ffi::redjs_promise_create(g, &mut resolve, &mut reject, &mut promise) != 0 {
        return reject_refusal(g, "fetch: could not create a promise");
    }

    // A signal that is already aborted never opens a socket (spec, and it is
    // also the cheapest correct answer).
    if signal_is_aborted(g, plan.signal) {
        let reason = signal_reason(g, plan.signal);
        let _ = call(g, reject, UNDEF, &[reason]);
        return promise;
    }

    if INFLIGHT.load(Ordering::SeqCst) >= MAX_INFLIGHT {
        let e = make_error(
            g,
            TYPE_ERR,
            "",
            &format!("Failed to fetch: more than {MAX_INFLIGHT} requests are already in flight"),
        );
        let _ = call(g, reject, UNDEF, &[e]);
        return promise;
    }

    let cancel = http::Cancel::new();
    let bridge = if is_cell(plan.signal) {
        let b = make_bridge(g, Arc::clone(&cancel));
        listen(g, plan.signal, b, true);
        crate::ffi::redjs_protect(g, b);
        crate::ffi::redjs_protect(g, plan.signal);
        b
    } else {
        UNDEF
    };

    crate::ffi::redjs_protect(g, resolve);
    crate::ffi::redjs_protect(g, reject);

    let req = http::Request {
        method: plan.method,
        url: plan.url,
        headers: plan.headers,
        body: plan.body,
        redirect: if plan.redirect == "error" { http::Redirect::Error } else { http::Redirect::Follow },
        // `check_supported` already refused anything else.
        credentials: super::cookies::Credentials::parse(&plan.credentials)
            .unwrap_or(super::cookies::Credentials::SameOrigin),
        connect_timeout: CONNECT_TIMEOUT,
        io_timeout: IO_TIMEOUT,
    };

    let ctx = Ctx(g);
    let id = crate::context::id_of(g);
    let signal = plan.signal;
    let worker_cancel = Arc::clone(&cancel);
    INFLIGHT.fetch_add(1, Ordering::SeqCst);
    let spawned = std::thread::Builder::new().name("redjs-fetch".into()).spawn(move || {
        let result = http::send(&req, &worker_cancel);
        let delivery = Delivery { resolve, reject, signal, bridge, cancel: worker_cancel, result };
        INFLIGHT.fetch_sub(1, Ordering::SeqCst);
        // Back onto the JS thread: the pump runs this with the VM lock held,
        // and only in the context that asked. `resolve`/`reject` are values
        // of THAT heap; a newer context at the same address must never call
        // them, and a torn-down one must not get its queue back.
        crate::web::schedule_native_from(ctx.ptr(), id, 0.0, Box::new(move |g| unsafe { settle(g, delivery) }));
    });

    if let Err(e) = spawned {
        INFLIGHT.fetch_sub(1, Ordering::SeqCst);
        let err = make_error(g, TYPE_ERR, "", &format!("Failed to fetch: could not start a request thread: {e}"));
        let _ = call(g, reject, UNDEF, &[err]);
        crate::ffi::redjs_unprotect(g, resolve);
        crate::ffi::redjs_unprotect(g, reject);
        if is_cell(bridge) {
            listen(g, signal, bridge, false);
            crate::ffi::redjs_unprotect(g, bridge);
            crate::ffi::redjs_unprotect(g, signal);
        }
    }
    promise
}

/// Local files and `data:` URIs — the paths that were already here, with the
/// one hole closed: a `data:` URI that does not parse now REJECTS instead of
/// resolving with a status-0 response.
unsafe fn fetch_local(g: *mut c_void, plan: &Plan) -> u64 {
    if plan.method != "GET" && plan.method != "HEAD" {
        return reject_refusal(g, &format!(
            "fetch: {} is not supported for local and data: URLs — only GET and HEAD are",
            plan.method
        ));
    }
    let fetched = fsutil::fetch_url(g, &plan.url);
    if fetched.status == 0 {
        // fsutil signals "could not even be interpreted" with status 0. That is
        // a failure, and the whole point of this change is that it must not
        // reach JS wearing a Response.
        return reject_refusal(g, &format!("Failed to fetch {}: the URL could not be read", plan.url));
    }
    let mime = fetched.mime.clone();
    let has_body = plan.method == "GET";
    let resp = create_response(g, RespData {
        bytes: Arc::new(if has_body { fetched.bytes } else { Vec::new() }),
        headers: vec![("content-type".into(), mime)],
        url: plan.url.clone(),
        status: fetched.status,
        status_text: fetched.status_text.to_string(),
        redirected: false,
        kind: "basic",
        has_body,
        body_used: false,
    });
    resolved_promise(g, resp)
}

unsafe extern "sysv64" fn fetch_fn(g: *mut c_void, cf: *mut c_void) -> u64 {
    // Spec: `fetch` reports argument problems by REJECTING, never by throwing.
    let plan = match build_plan(g, cf_arg(cf, 0), cf_arg(cf, 1)) {
        Ok(p) => p,
        Err(m) => return reject_refusal(g, &format!("Failed to fetch: {m}")),
    };

    let lower = plan.url.trim_start().to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return fetch_network(g, plan);
    }
    if lower.starts_with("blob:") {
        return fetch_blob(g, &plan);
    }
    if let Some(scheme) = unsupported_scheme(&lower) {
        return reject_refusal(g, &format!(
            "Failed to fetch: the {scheme}: scheme is not supported by this runtime"
        ));
    }
    fetch_local(g, &plan)
}

/// Fetch §"parse a single range header value" with allowWhitespace = true,
/// which is how the blob scheme fetch calls it: `bytes`, optional tabs or
/// spaces, `=`, then `a-b`, `a-` or `-n` with optional tabs or spaces around
/// the numbers (`bytes= 9 - 21`, as Chromium and WPT `fetch/range/blob.any.js`
/// accept), and nothing after. `(start, end)`; `None` for anything else,
/// including a list of ranges. A number too large for `u64` is `u64::MAX`:
/// the spec reads the digits as a mathematical value, so an end past the
/// Blob is clamped by the caller and a start past it is a network error.
fn parse_single_range(v: &str) -> Option<(Option<u64>, Option<u64>)> {
    let b = v.as_bytes();
    let mut i = "bytes".len();
    if !b.starts_with(b"bytes") {
        return None;
    }
    let ws = |i: &mut usize| {
        while *i < b.len() && (b[*i] == b' ' || b[*i] == b'\t') {
            *i += 1;
        }
    };
    let digits = |i: &mut usize| -> Option<u64> {
        let from = *i;
        let mut n = 0u64;
        while *i < b.len() && b[*i].is_ascii_digit() {
            n = n.saturating_mul(10).saturating_add((b[*i] - b'0') as u64);
            *i += 1;
        }
        (*i > from).then_some(n)
    };
    ws(&mut i);
    if b.get(i) != Some(&b'=') {
        return None;
    }
    i += 1;
    ws(&mut i);
    let start = digits(&mut i);
    ws(&mut i);
    if b.get(i) != Some(&b'-') {
        return None;
    }
    i += 1;
    ws(&mut i);
    let end = digits(&mut i);
    if i != b.len() {
        return None;
    }
    match (start, end) {
        (None, None) => None,
        (Some(s), Some(e)) if s > e => None,
        r => Some(r),
    }
}

/// What a blob fetch settles with, all of it `Send`.
struct BlobDelivery {
    resolve: u64,
    reject: u64,
    signal: u64,
    url: String,
    status: i32,
    headers: Vec<(String, String)>,
}
unsafe impl Send for BlobDelivery {}

impl BlobDelivery {
    unsafe fn settle(self, g: *mut c_void, body: Result<Arc<Vec<u8>>, super::blob::ReadError>) {
        // An abort that landed while the read ran still wins, as for http.
        if signal_is_aborted(g, self.signal) {
            let reason = signal_reason(g, self.signal);
            let _ = call(g, self.reject, UNDEF, &[reason]);
        } else {
            match body {
                Ok(bytes) => {
                    let resp = create_response(g, RespData {
                        bytes,
                        headers: self.headers,
                        url: self.url,
                        status: self.status,
                        status_text: if self.status == 206 { "Partial Content".into() } else { "OK".into() },
                        redirected: false,
                        kind: "basic",
                        has_body: true,
                        body_used: false,
                    });
                    let _ = call(g, self.resolve, UNDEF, &[resp]);
                }
                Err(e) => {
                    // A network error, as for any blob whose read fails.
                    let err = make_error(g, TYPE_ERR, "", &format!("Failed to fetch: {}: {}", e.name(), e.message()));
                    let _ = call(g, self.reject, UNDEF, &[err]);
                }
            }
        }
        crate::ffi::redjs_unprotect(g, self.resolve);
        crate::ffi::redjs_unprotect(g, self.reject);
        if is_cell(self.signal) {
            crate::ffi::redjs_unprotect(g, self.signal);
        }
    }
}

/// Fetch §"scheme fetch" for `blob:`: GET only; the entry in this context's
/// store or a network error; `200` with `Content-Length` and `Content-Type`
/// (the Blob's type, even when it is empty), or `206` with `Content-Range`
/// for one valid `Range`. A memory Blob answers at once (its `Arc` shared
/// when the whole of it is asked for); a path-backed one is read on a read
/// thread under the snapshot rule, like `text()`.
unsafe fn fetch_blob(g: *mut c_void, plan: &Plan) -> u64 {
    if plan.method != "GET" {
        return reject_refusal(g, &format!("Failed to fetch: a blob: URL can only be fetched with GET, not {}", plan.method));
    }
    let Some(entry) = super::url::lookup(g, &plan.url) else {
        return reject_refusal(g, "Failed to fetch: this blob: URL is not in the blob URL store (it was revoked, or never created in this context)");
    };
    let full = entry.range.len;
    // "Get `Range`" combines every value with ", ", so two Range headers are
    // a list of ranges, which is a network error.
    let values: Vec<&str> = plan.headers.iter().filter(|(k, _)| k == "range").map(|(_, v)| v.as_str()).collect();
    let range = (!values.is_empty()).then(|| values.join(", "));
    let (start, len, content_range) = match range {
        None => (0, full, None),
        Some(v) => {
            let Some(r) = parse_single_range(&v) else {
                return reject_refusal(g, &format!("Failed to fetch: the Range header {v:?} is not a single byte range"));
            };
            let (s, e) = match r {
                // A suffix: the last n bytes (all of them when n is larger).
                (None, Some(n)) => (full.saturating_sub(n), full.saturating_sub(1)),
                (Some(s), e) => {
                    if s >= full {
                        return reject_refusal(g, &format!("Failed to fetch: the Range {v:?} starts past the end of a {full}-byte Blob"));
                    }
                    (s, e.map_or(full - 1, |e| e.min(full - 1)))
                }
                (None, None) => unreachable!(),
            };
            if full == 0 || e < s {
                return reject_refusal(g, &format!("Failed to fetch: the Range {v:?} selects nothing in an empty Blob"));
            }
            (s, e - s + 1, Some(format!("bytes {s}-{e}/{full}")))
        }
    };
    let mut headers = vec![("content-length".to_string(), len.to_string()), ("content-type".to_string(), entry.mime.clone())];
    if let Some(cr) = &content_range {
        headers.push(("content-range".to_string(), cr.clone()));
    }
    let status = if content_range.is_some() { 206 } else { 200 };
    if signal_is_aborted(g, plan.signal) {
        let reason = signal_reason(g, plan.signal);
        return reject_with(g, reason);
    }
    let window = super::blob::Range { source: entry.range.source.clone(), offset: entry.range.offset + start, len };

    let (mut resolve, mut reject, mut promise) = (0u64, 0u64, 0u64);
    if crate::ffi::redjs_promise_create(g, &mut resolve, &mut reject, &mut promise) != 0 {
        return reject_refusal(g, "fetch: could not create a promise");
    }
    crate::ffi::redjs_protect(g, resolve);
    crate::ffi::redjs_protect(g, reject);
    if is_cell(plan.signal) {
        crate::ffi::redjs_protect(g, plan.signal);
    }
    // `Response.url` is serialized with "exclude fragment" (Fetch).
    let url = plan.url.split('#').next().unwrap_or_default().to_string();
    let d = BlobDelivery { resolve, reject, signal: plan.signal, url, status, headers };
    if let super::blob::Source::Memory(arc) = &window.source {
        let body = if window.offset == 0 && window.len as usize == arc.len() {
            Ok(Arc::clone(arc))
        } else {
            window.in_memory().map(|b| Arc::new(b.to_vec())).ok_or(super::blob::ReadError::NotReadable)
        };
        d.settle(g, body);
        return promise;
    }
    super::blob::read_async(g, window, Box::new(move |g, r| unsafe { d.settle(g, r.map(Arc::new)) }));
    promise
}

/// A scheme we recognise as a scheme but will not serve. Anything without a
/// scheme is a local path and goes to `fetch_local`.
fn unsupported_scheme(lower_url: &str) -> Option<&'static str> {
    for s in ["ws", "wss", "ftp", "ftps", "about", "chrome", "javascript"] {
        if lower_url.starts_with(s) && lower_url[s.len()..].starts_with(':') {
            return Some(s);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

pub unsafe fn install(g: *mut c_void) -> (crate::capi::JSClassRef, Class) {
    // Order matters only in that `Request` and `Response` both want `Headers`.
    headers::install(g);
    request::install(g);

    let resp_cls = make_host_class::<RespData>("Response");
    let response = define_class(g, &ClassSpec {
        name: "Response",
        arity: 0,
        construct: Some(response_construct),
        parent_proto: 0,
        methods: &[
            ("arrayBuffer", 0, resp_array_buffer),
            ("bytes", 0, resp_bytes),
            ("text", 0, resp_text),
            ("json", 0, resp_json),
            ("blob", 0, resp_blob),
            ("clone", 0, resp_clone),
        ],
        getters: &[
            ("ok", resp_ok),
            ("status", resp_status),
            ("statusText", resp_status_text),
            ("url", resp_url),
            ("redirected", resp_redirected),
            ("bodyUsed", resp_body_used),
            ("body", resp_body),
            ("type", resp_type),
            ("headers", resp_headers),
        ],
        accessors: &[],
    });
    define(g, response.ctor, "json", make_fn(g, "json", 2, response_static_json), 2);
    define(g, response.ctor, "error", make_fn(g, "error", 0, response_static_error), 2);

    let n16: Vec<u16> = "fetch".encode_utf16().collect();
    let mut f = UNDEF;
    glue::rjs_function_create(g, n16.as_ptr(), n16.len(), 2, fetch_fn as *const c_void, &mut f);
    define(g, glue::rjs_global(g), "fetch", f, 2);

    (resp_cls, response)
}

// ---------------------------------------------------------------------------
// Tests — through real JS, against a real socket
// ---------------------------------------------------------------------------

#[cfg(test)]
mod jsc_tests {
    use crate::api::Context;
    use crate::webapi::testing::*;
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    /// The shared `testing::ctx` installs the web classes but not the scheduler
    /// (`web::install`), which normally comes from the host. `fetch` needs it:
    /// results are delivered as native tasks on the pump, and the tests below
    /// also use `setTimeout`.
    fn fetch_ctx() -> Context {
        let c = ctx();
        unsafe {
            crate::web::install(c.raw());
        }
        c
    }

    /// Runs an async body to completion, pumping the scheduler (which is where
    /// a fetch result comes back through) and the microtask queue.
    fn run(c: &Context, body: &str) -> String {
        s(
            c,
            &format!(
                "globalThis.__out = '<pending>'; (async () => {{ {body} }})().then(\
                   v => {{ globalThis.__out = String(v); }}, \
                   e => {{ globalThis.__out = 'ERROR: ' + ((e && e.name ? e.name + ': ' : '') + ((e && e.message) || e)); }}); 'started'"
            ),
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            unsafe {
                crate::web::pump(c.raw(), crate::web::PUMP_TIMERS | crate::web::PUMP_MICROTASKS, core::ptr::null_mut());
            }
            c.drain_microtasks();
            let out = s(c, "globalThis.__out");
            if out != "<pending>" {
                return out;
            }
            if Instant::now() > deadline {
                return "<timed out>".into();
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// A loopback listener answering each connection with the next canned reply.
    fn serve(replies: Vec<Vec<u8>>) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for reply in replies {
                let Ok((mut sock, _)) = listener.accept() else { return };
                let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
                let mut got = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    match sock.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            got.extend_from_slice(&buf[..n]);
                            if let Some(i) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                                let len = String::from_utf8_lossy(&got[..i])
                                    .lines()
                                    .find_map(|l| {
                                        l.to_ascii_lowercase()
                                            .strip_prefix("content-length:")
                                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                    })
                                    .unwrap_or(0);
                                if got.len() >= i + 4 + len {
                                    break;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                let _ = sock.write_all(&reply);
                let _ = sock.flush();
                let _ = sock.shutdown(std::net::Shutdown::Write);
            }
        });
        port
    }

    // -- the defect this epic exists for -------------------------------------

    #[test]
    fn an_http_url_no_longer_resolves_with_a_synthetic_status_zero() {
        // THE regression test. The old implementation logged to stderr and
        // resolved a Response with status 0 and an empty body, so a caller saw
        // a success-shaped answer to a request that never happened. Nothing
        // here may resolve with a fabricated status again: either a real
        // response comes back, or the promise rejects.
        let c = fetch_ctx();
        let port = serve(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi".to_vec()]);
        let out = run(
            &c,
            &format!(
                "const r = await fetch('http://127.0.0.1:{port}/x'); \
                 if (r.status === 0) return 'FABRICATED STATUS 0'; \
                 return r.status + ':' + (await r.text());"
            ),
        );
        assert_eq!(out, "200:hi");
    }

    #[test]
    fn a_failed_request_rejects_rather_than_resolving() {
        // Nothing is listening on port 1. A resolved promise here — with any
        // status, any body — is the bug.
        let c = fetch_ctx();
        let out = run(
            &c,
            "try { const r = await fetch('http://127.0.0.1:1/nope'); \
               return 'RESOLVED ' + r.status; } \
             catch (e) { return 'rejected'; }",
        );
        assert_eq!(out, "rejected", "a failed fetch must reject, not resolve");
    }

    #[test]
    fn a_failed_request_rejects_with_a_typeerror_like_a_browser() {
        let c = fetch_ctx();
        let out = run(
            &c,
            "try { await fetch('http://127.0.0.1:1/nope'); return 'RESOLVED'; } \
             catch (e) { return (e instanceof TypeError) + ':' + /Failed to fetch/.test(e.message); }",
        );
        assert_eq!(out, "true:true");
    }

    #[test]
    fn an_unsupported_scheme_rejects_by_name_instead_of_resolving() {
        let c = fetch_ctx();
        for (url, needle) in [("ws://example.com/socket", "ws"), ("ftp://example.com/f", "ftp")] {
            let out = run(
                &c,
                &format!(
                    "try {{ const r = await fetch('{url}'); return 'RESOLVED ' + r.status; }} \
                     catch (e) {{ return e.message; }}"
                ),
            );
            assert!(out.contains(needle), "expected {needle}: to be named in the rejection, got {out}");
            assert!(!out.starts_with("RESOLVED"), "{url} resolved instead of rejecting: {out}");
        }
    }

    #[test]
    fn every_unsupported_option_is_refused_by_its_own_name() {
        let c = fetch_ctx();
        let cases = [
            ("{ redirect: 'manual' }", "manual"),
            ("{ credentials: 'bogus' }", "bogus"),
            ("{ mode: 'no-cors' }", "no-cors"),
            ("{ cache: 'only-if-cached' }", "only-if-cached"),
            ("{ integrity: 'sha256-abc' }", "integrity"),
            ("{ method: 'CONNECT' }", "CONNECT"),
        ];
        for (init, needle) in cases {
            let out = run(
                &c,
                &format!(
                    "try {{ const r = await fetch('http://127.0.0.1:1/x', {init}); \
                       return 'RESOLVED ' + r.status; }} \
                     catch (e) {{ return e.message; }}"
                ),
            );
            assert!(!out.starts_with("RESOLVED"), "{init} resolved instead of rejecting: {out}");
            assert!(out.contains(needle), "{init} did not name {needle:?} in its refusal: {out}");
        }
    }

    // -- the working path ------------------------------------------------------

    #[test]
    fn json_headers_and_status_come_back_from_a_real_server() {
        let c = fetch_ctx();
        let port = serve(vec![
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Trace: abc\r\nContent-Length: 27\r\n\r\n{\"instance\":\"eu\",\"ok\":true}".to_vec(),
        ]);
        let out = run(
            &c,
            &format!(
                "const r = await fetch('http://127.0.0.1:{port}/resolve'); \
                 const j = await r.json(); \
                 return [r.ok, r.status, r.statusText, r.type, r.headers.get('x-trace'), \
                         r.headers.get('content-type'), j.instance, j.ok].join('|');"
            ),
        );
        assert_eq!(out, "true|200|OK|basic|abc|application/json|eu|true");
    }

    // -- credentials and the jar (webapi::cookies has the rules) --------------

    /// A loopback server that answers each of `n` connections with
    /// `reply(request head)` — so a reply can echo the Cookie it received.
    fn serve_fn(n: usize, reply: impl Fn(&str) -> String + Send + 'static) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..n {
                let Ok((mut sock, _)) = listener.accept() else { return };
                let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
                let mut got = Vec::new();
                let mut buf = [0u8; 4096];
                while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(k) => got.extend_from_slice(&buf[..k]),
                    }
                }
                let head = String::from_utf8_lossy(&got).into_owned();
                let _ = sock.write_all(reply(&head).as_bytes());
                let _ = sock.flush();
                let _ = sock.shutdown(std::net::Shutdown::Write);
            }
        });
        port
    }

    /// The reply: echo the request's Cookie header as the body, plus `extra`
    /// response headers.
    fn echo_cookie(head: &str, extra: &str) -> String {
        let cookie = head
            .lines()
            .find_map(|l| l.strip_prefix("cookie: ").or_else(|| l.strip_prefix("Cookie: ")))
            .unwrap_or("-")
            .to_string();
        format!("HTTP/1.1 200 OK\r\n{extra}Content-Length: {}\r\n\r\n{cookie}", cookie.len())
    }

    #[test]
    fn include_stores_set_cookie_and_sends_it_back_and_omit_does_neither() {
        let _g = super::super::cookies::test_lock();
        super::super::cookies::reset_for_tests();
        let c = fetch_ctx();
        let port = serve_fn(4, |head| {
            if head.starts_with("GET /login") {
                echo_cookie(head, "Set-Cookie: sid=abc; Path=/; HttpOnly\r\nSet-Cookie: theme=dark; Path=/\r\n")
            } else {
                echo_cookie(head, "")
            }
        });
        let out = run(
            &c,
            &format!(
                "const base = 'http://127.0.0.1:{port}'; \
                 const a = await fetch(base + '/login', {{ credentials: 'include' }}); \
                 const first = await a.text(); \
                 const hidden = [String(a.headers.get('set-cookie')), a.headers.getSetCookie().length, \
                                 [...a.headers.keys()].includes('set-cookie')].join(','); \
                 const b = await (await fetch(base + '/me', {{ credentials: 'include' }})).text(); \
                 const c = await (await fetch(base + '/me', {{ credentials: 'omit' }})).text(); \
                 const d = await (await fetch(base + '/me')).text(); \
                 return [first, hidden, b, c, d].join('|');"
            ),
        );
        assert_eq!(out, "-|null,0,false|sid=abc; theme=dark|-|-");
        super::super::cookies::reset_for_tests();
    }

    #[test]
    fn document_cookie_is_empty_and_ignores_writes() {
        let _g = super::super::cookies::test_lock();
        super::super::cookies::reset_for_tests();
        super::super::cookies::set_from_header(
            &url::Url::parse("https://example.com/").unwrap(),
            "visible=1",
            super::super::cookies::Api::Http,
        )
        .unwrap();
        let c = fetch_ctx();
        let out = s(&c, "document.cookie = 'x=1'; JSON.stringify([typeof document.cookie, document.cookie])");
        assert_eq!(out, "[\"string\",\"\"]");
        assert!(super::super::cookies::get(None).iter().all(|k| k.name != "x"), "the write went nowhere");
        super::super::cookies::reset_for_tests();
    }

    #[test]
    fn a_post_sends_its_method_headers_and_body() {
        let c = fetch_ctx();
        let port = serve(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec()]);
        let out = run(
            &c,
            &format!(
                "const r = await fetch('http://127.0.0.1:{port}/api/envelope', {{ \
                    method: 'POST', \
                    headers: {{ 'Content-Type': 'application/json', 'X-Token': 'abc' }}, \
                    body: JSON.stringify({{ hello: 'world' }}) }}); \
                 return r.status + ':' + (await r.text());"
            ),
        );
        assert_eq!(out, "200:ok");
    }

    #[test]
    fn an_error_status_is_a_resolved_response_not_a_rejection() {
        let c = fetch_ctx();
        let port = serve(vec![b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\n\r\ndown".to_vec()]);
        let out = run(
            &c,
            &format!(
                "const r = await fetch('http://127.0.0.1:{port}/x'); \
                 return [r.ok, r.status, r.statusText, await r.text()].join('|');"
            ),
        );
        assert_eq!(out, "false|503|Service Unavailable|down");
    }

    #[test]
    fn redirects_are_followed_and_the_final_url_is_reported() {
        let c = fetch_ctx();
        let port = serve(vec![
            b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ndone".to_vec(),
        ]);
        let out = run(
            &c,
            &format!(
                "const r = await fetch('http://127.0.0.1:{port}/start'); \
                 return [r.status, r.redirected, r.url.endsWith('/final'), await r.text()].join('|');"
            ),
        );
        assert_eq!(out, "200|true|true|done");
    }

    #[test]
    fn the_response_body_is_a_readable_stream() {
        let c = fetch_ctx();
        let port = serve(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nhello world".to_vec()]);
        let out = run(
            &c,
            &format!(
                "const r = await fetch('http://127.0.0.1:{port}/x'); \
                 if (!(r.body instanceof ReadableStream)) return 'NOT A STREAM'; \
                 const rd = r.body.getReader(); let text = ''; \
                 for (;;) {{ const {{ value, done }} = await rd.read(); if (done) break; \
                   text += new TextDecoder().decode(value); }} \
                 return text;"
            ),
        );
        assert_eq!(out, "hello world");
    }

    #[test]
    fn a_204_has_a_null_body() {
        let c = fetch_ctx();
        let port = serve(vec![b"HTTP/1.1 204 No Content\r\n\r\n".to_vec()]);
        let out = run(
            &c,
            &format!(
                "const r = await fetch('http://127.0.0.1:{port}/x'); \
                 return [r.status, r.body === null, (await r.text()).length].join('|');"
            ),
        );
        assert_eq!(out, "204|true|0");
    }

    #[test]
    fn a_body_cannot_be_read_twice() {
        let c = fetch_ctx();
        let port = serve(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi".to_vec()]);
        let out = run(
            &c,
            &format!(
                "const r = await fetch('http://127.0.0.1:{port}/x'); \
                 await r.text(); \
                 try {{ await r.text(); return 'READ TWICE'; }} catch (e) {{ return 'rejected:' + r.bodyUsed; }}"
            ),
        );
        assert_eq!(out, "rejected:true");
    }

    // -- AbortSignal -----------------------------------------------------------

    #[test]
    fn a_pre_aborted_signal_rejects_with_its_own_reason() {
        let c = fetch_ctx();
        let out = run(
            &c,
            "const ac = new AbortController(); ac.abort(new Error('nope')); \
             try { await fetch('http://127.0.0.1:1/x', { signal: ac.signal }); return 'RESOLVED'; } \
             catch (e) { return e.message; }",
        );
        assert_eq!(out, "nope");
    }

    #[test]
    fn aborting_mid_flight_rejects_the_pending_fetch() {
        // A server that accepts and then never answers. Without the socket
        // shutdown behind AbortController this sits on the 60s read timeout.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let held = listener.accept();
            std::thread::sleep(Duration::from_secs(30));
            drop(held);
        });

        let c = fetch_ctx();
        let started = Instant::now();
        let out = run(
            &c,
            &format!(
                "const ac = new AbortController(); \
                 const p = fetch('http://127.0.0.1:{port}/silent', {{ signal: ac.signal }}); \
                 setTimeout(() => ac.abort(), 200); \
                 try {{ await p; return 'RESOLVED'; }} catch (e) {{ \
                   return e instanceof DOMException ? e.name + '/' + e.code : 'not a DOMException: ' + e.name; }}"
            ),
        );
        assert_eq!(out, "AbortError/20");
        assert!(started.elapsed() < Duration::from_secs(15), "abort did not cut the connection short");
    }

    // -- Headers, which used to be a trap ---------------------------------------

    #[test]
    fn headers_set_append_and_delete_actually_do_something() {
        // These three were bound to a no-op host function: `h.set(...)` returned
        // undefined and changed nothing, so an Authorization header assembled
        // this way was never sent and never reported missing.
        let c = fetch_ctx();
        let out = run(
            &c,
            "const h = new Headers(); \
             h.set('Authorization', 'Bearer abc'); \
             h.append('X-Multi', '1'); h.append('X-Multi', '2'); \
             h.set('X-Gone', 'x'); h.delete('X-Gone'); \
             return [h.get('authorization'), h.get('x-multi'), h.has('x-gone'), String(h.get('x-gone'))].join('|');",
        );
        assert_eq!(out, "Bearer abc|1, 2|false|null");
    }

    #[test]
    fn a_headers_record_initializer_keeps_every_key() {
        // The old constructor read exactly one key — `content-type` — and
        // dropped the rest without a word.
        let c = fetch_ctx();
        let out = run(
            &c,
            "const h = new Headers({ 'Content-Type': 'application/json', \
                                     'X-Token': 'abc', 'Accept': 'text/plain' }); \
             return [h.get('content-type'), h.get('x-token'), h.get('accept')].join('|');",
        );
        assert_eq!(out, "application/json|abc|text/plain");
    }

    #[test]
    fn headers_iterate_sorted_and_from_pairs() {
        let c = fetch_ctx();
        let out = run(
            &c,
            "const h = new Headers([['x-b', '2'], ['x-a', '1']]); \
             return [...h].map(([k, v]) => k + '=' + v).join(',');",
        );
        assert_eq!(out, "x-a=1,x-b=2");
    }

    #[test]
    fn an_injected_header_value_is_refused() {
        let c = fetch_ctx();
        let out = run(
            &c,
            "const h = new Headers(); \
             try { h.set('X-Evil', 'a\\r\\nX-Injected: 1'); return 'ACCEPTED'; } \
             catch (e) { return 'refused'; }",
        );
        assert_eq!(out, "refused");
    }

    // -- Request, which used to drop its own arguments ---------------------------

    #[test]
    fn a_request_keeps_the_method_headers_and_body_it_was_given() {
        // The bootstrap stub built `{ url, method, headers: new Headers() }` and
        // discarded init.headers and init.body outright, so
        // `fetch(new Request(url, {method:'POST', body}))` sent an empty GET.
        let c = fetch_ctx();
        let out = run(
            &c,
            "const r = new Request('http://example.com/x', { \
               method: 'post', headers: { 'X-Token': 'abc' }, body: 'payload' }); \
             return [r.method, r.url, r.headers.get('x-token'), await r.text()].join('|');",
        );
        assert_eq!(out, "POST|http://example.com/x|abc|payload");
    }

    #[test]
    fn fetch_accepts_a_request_object() {
        let c = fetch_ctx();
        let port = serve(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec()]);
        let out = run(
            &c,
            &format!(
                "const req = new Request('http://127.0.0.1:{port}/x', {{ method: 'POST', body: 'abc' }}); \
                 const r = await fetch(req); return r.status + ':' + (await r.text());"
            ),
        );
        assert_eq!(out, "200:ok");
    }

    #[test]
    fn a_readable_stream_body_is_refused_by_name() {
        let c = fetch_ctx();
        let out = run(
            &c,
            "const body = new ReadableStream({ start(c) { c.enqueue(new Uint8Array([1])); c.close(); } }); \
             try { await fetch('http://127.0.0.1:1/x', { method: 'POST', body }); return 'ACCEPTED'; } \
             catch (e) { return /ReadableStream/.test(e.message) ? 'named' : 'unnamed: ' + e.message; }",
        );
        assert_eq!(out, "named");
    }

    // -- Response ----------------------------------------------------------------

    #[test]
    fn response_error_is_the_only_status_zero_and_says_so() {
        let c = fetch_ctx();
        let out = run(&c, "const r = Response.error(); return r.status + '|' + r.type + '|' + r.ok;");
        assert_eq!(out, "0|error|false");
    }

    #[test]
    fn response_json_builds_a_json_response() {
        let c = fetch_ctx();
        let out = run(
            &c,
            "const r = Response.json({ a: 1 }, { status: 201 }); \
             return [r.status, r.headers.get('content-type'), await r.text()].join('|');",
        );
        assert_eq!(out, "201|application/json|{\"a\":1}");
    }

    #[test]
    fn a_data_uri_still_works_and_a_broken_one_rejects() {
        let c = fetch_ctx();
        let ok = run(&c, "const r = await fetch('data:text/plain,hello'); return await r.text();");
        assert_eq!(ok, "hello");

        let bad = run(
            &c,
            "try { const r = await fetch('data:text/plain;base64'); return 'RESOLVED ' + r.status; } \
             catch (e) { return 'rejected'; }",
        );
        assert_eq!(bad, "rejected", "a malformed data: URI must not resolve with a fake status");
    }
}
