//! `EventSource` (HTML server-sent events). The stream is parsed on the network thread; the JS
//! thread gets whole events.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use super::util::*;
use super::{blob, cookies, events, http};
use crate::capi;
use crate::glue;

const CONNECTING: u8 = 0;
const OPEN: u8 = 1;
const CLOSED: u8 = 2;

const DEFAULT_RETRY_MS: u64 = 3000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IO_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, PartialEq)]
pub(crate) struct SseEvent {
    pub ty: String,
    pub data: String,
    pub id: String,
}

#[derive(Default)]
pub(crate) struct Parser {
    line: Vec<u8>,
    started: bool,
    after_cr: bool,
    data: String,
    event: String,
    id_buf: String,
    pub last_id: String,
    pub retry: Option<u64>,
}

const BOM: &[u8] = b"\xEF\xBB\xBF";

impl Parser {
    pub fn new(last_id: &str) -> Self {
        Parser { id_buf: last_id.into(), last_id: last_id.into(), ..Default::default() }
    }

    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<SseEvent>) {
        if !self.started {
            self.line.extend_from_slice(bytes);
            if self.line.len() < BOM.len() && BOM.starts_with(&self.line) {
                return;
            }
            self.started = true;
            let mut pending = std::mem::take(&mut self.line);
            if pending.starts_with(BOM) {
                pending.drain(..BOM.len());
            }
            return self.feed(&pending, out);
        }
        let mut rest = bytes;
        if self.after_cr {
            self.after_cr = false;
            if let Some(stripped) = rest.strip_prefix(b"\n") {
                rest = stripped;
            }
        }
        while let Some(i) = rest.iter().position(|&b| b == b'\r' || b == b'\n') {
            self.line.extend_from_slice(&rest[..i]);
            self.end_line(out);
            let cr = rest[i] == b'\r';
            rest = &rest[i + 1..];
            if cr {
                match rest.first() {
                    Some(b'\n') => rest = &rest[1..],
                    None => self.after_cr = true,
                    _ => {}
                }
            }
        }
        self.line.extend_from_slice(rest);
    }

    fn end_line(&mut self, out: &mut Vec<SseEvent>) {
        let raw = std::mem::take(&mut self.line);
        let line = String::from_utf8_lossy(&raw);
        if line.is_empty() {
            return self.dispatch(out);
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.find(':') {
            Some(i) => (&line[..i], line[i + 1..].strip_prefix(' ').unwrap_or(&line[i + 1..])),
            None => (&line[..], ""),
        };
        match field {
            "event" => self.event = value.to_string(),
            "data" => {
                self.data.push_str(value);
                self.data.push('\n');
            }
            "id" if !value.contains('\0') => self.id_buf = value.to_string(),
            "retry" if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
                if let Ok(n) = value.parse() {
                    self.retry = Some(n);
                }
            }
            _ => {}
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        self.last_id.clone_from(&self.id_buf);
        let ty = std::mem::take(&mut self.event);
        if self.data.is_empty() {
            return;
        }
        let mut data = std::mem::take(&mut self.data);
        data.pop();
        let ty = if ty.is_empty() { "message".to_string() } else { ty };
        out.push(SseEvent { ty, data, id: self.last_id.clone() });
    }
}

pub struct EsData {
    id: u64,
    state: u8,
    url: String,
    with_credentials: bool,
    attempt: u64,
    cancel: Option<http::Cancel>,
    origin: String,
    last_id: String,
    retry_ms: u64,
}

struct Classes {
    host: capi::JSClassRef,
    source: Class,
    s_handlers: *mut c_void,
}

unsafe impl Send for Classes {}
unsafe impl Sync for Classes {}

fn registry() -> &'static Mutex<HashMap<usize, &'static Classes>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static Classes>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> &'static Classes {
    registry().lock().unwrap().get(&(g as usize)).copied().expect("webapi::eventsource not installed")
}

/// `(context key, source id)` → the protected `EventSource`, while it is not CLOSED.
fn live() -> &'static Mutex<HashMap<(usize, u64), u64>> {
    static L: OnceLock<Mutex<HashMap<(usize, u64), u64>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn forget(gone: crate::context::Gone) {
    registry().lock().unwrap().remove(&gone.key);
    live().lock().unwrap().retain(|(k, _), _| *k != gone.key);
}

unsafe fn es<'a>(g: *mut c_void, obj: u64) -> Option<&'a mut EsData> {
    host_data::<EsData>(g, classes(g).host, obj)
}

#[derive(Clone, Copy)]
struct Ctx(*mut c_void);
unsafe impl Send for Ctx {}
unsafe impl Sync for Ctx {}

#[derive(Default)]
struct Inbox {
    queued: bool,
    head: Option<Result<String, String>>,
    events: Vec<SseEvent>,
    last_id: Option<String>,
    retry: Option<u64>,
    ended: bool,
}

struct Shared {
    id: u64,
    attempt: u64,
    ctx: Ctx,
    cid: Option<crate::context::ContextId>,
    inbox: Mutex<Inbox>,
}

impl Shared {
    fn post(self: &Arc<Self>, f: impl FnOnce(&mut Inbox)) {
        let queue = {
            let mut i = self.inbox.lock().unwrap();
            f(&mut i);
            !std::mem::replace(&mut i.queued, true)
        };
        if queue {
            let me = Arc::clone(self);
            crate::web::schedule_native_from(self.ctx.0, self.cid, 0.0, Box::new(move |g| unsafe { tick(g, &me) }));
        }
    }
}

struct Obs {
    sh: Arc<Shared>,
    cancel: http::Cancel,
    accepted: AtomicBool,
    parser: Mutex<Parser>,
}

fn verdict(h: &http::ProgressHead<'_>) -> Result<String, String> {
    if h.status != 200 {
        return Err(format!("the server answered {} {}", h.status, h.status_text));
    }
    let ct = http::header(h.headers, "content-type").unwrap_or("");
    if !ct.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("text/event-stream") {
        return Err(format!("the response's MIME type ({ct:?}) is not \"text/event-stream\""));
    }
    let enc = http::header(h.headers, "content-encoding").unwrap_or("").trim();
    if !enc.is_empty() && !enc.eq_ignore_ascii_case("identity") {
        return Err(format!("the response is {enc}-encoded; an event stream must not be"));
    }
    Ok(url::Url::parse(h.url).map(|u| u.origin().ascii_serialization()).unwrap_or_default())
}

impl http::Observe for Obs {
    fn head(&self, h: &http::ProgressHead<'_>) {
        let v = verdict(h);
        if v.is_ok() {
            self.accepted.store(true, Ordering::SeqCst);
        } else {
            self.cancel.abort();
        }
        self.sh.post(|i| i.head = Some(v));
    }

    fn body(&self, chunk: &[u8], _received: u64, _total: Option<u64>) {
        if !self.accepted.load(Ordering::SeqCst) {
            return;
        }
        let mut evs = Vec::new();
        let (last_id, retry) = {
            let mut p = self.parser.lock().unwrap();
            let before = p.last_id.clone();
            p.feed(chunk, &mut evs);
            ((p.last_id != before).then(|| p.last_id.clone()), p.retry.take())
        };
        if evs.is_empty() && last_id.is_none() && retry.is_none() {
            return;
        }
        self.sh.post(|i| {
            i.events.extend(evs);
            if last_id.is_some() {
                i.last_id = last_id;
            }
            if retry.is_some() {
                i.retry = retry;
            }
        });
    }

    fn streams(&self) -> bool {
        true
    }
}

unsafe fn fire(g: *mut c_void, obj: u64, ty: &str) {
    events::dispatch(g, obj, events::new_trusted_event(g, ty));
}

unsafe fn current(g: *mut c_void, id: u64, attempt: u64) -> Option<u64> {
    let obj = live().lock().unwrap().get(&(g as usize, id)).copied()?;
    es(g, obj).is_some_and(|d| d.attempt == attempt && d.state != CLOSED).then_some(obj)
}

unsafe fn close_down(g: *mut c_void, obj: u64) {
    let Some(d) = es(g, obj) else { return };
    d.state = CLOSED;
    d.attempt += 1;
    if let Some(c) = d.cancel.take() {
        c.abort();
    }
    if live().lock().unwrap().remove(&(g as usize, d.id)).is_some() {
        crate::ffi::redjs_unprotect(g, obj);
    }
}

unsafe fn fail(g: *mut c_void, obj: u64, why: &str) {
    if let Some(d) = es(g, obj) {
        eprintln!("[redjs] EventSource {}: {why}; the connection is closed", d.url);
    }
    close_down(g, obj);
    fire(g, obj, "error");
}

unsafe fn reestablish(g: *mut c_void, obj: u64) {
    let Some(d) = es(g, obj) else { return };
    if d.state == CLOSED {
        return;
    }
    d.state = CONNECTING;
    d.attempt += 1;
    d.cancel = None;
    let (id, attempt, delay) = (d.id, d.attempt, d.retry_ms);
    fire(g, obj, "error");
    crate::web::schedule_native(
        g,
        delay as f64,
        Box::new(move |g| unsafe {
            if let Some(obj) = current(g, id, attempt) {
                connect(g, obj);
            }
        }),
    );
}

unsafe fn tick(g: *mut c_void, sh: &Arc<Shared>) {
    let (head, evs, last_id, retry, ended) = {
        let mut i = sh.inbox.lock().unwrap();
        i.queued = false;
        (i.head.take(), std::mem::take(&mut i.events), i.last_id.take(), i.retry.take(), std::mem::take(&mut i.ended))
    };
    let Some(obj) = current(g, sh.id, sh.attempt) else { return };
    if let Some(d) = es(g, obj) {
        if let Some(id) = last_id {
            d.last_id = id;
        }
        if let Some(ms) = retry {
            d.retry_ms = ms;
        }
    }
    match head {
        Some(Ok(origin)) => {
            if let Some(d) = es(g, obj) {
                d.state = OPEN;
                d.origin = origin;
            }
            fire(g, obj, "open");
        }
        Some(Err(why)) => return fail(g, obj, &why),
        None => {}
    }
    for ev in evs {
        let Some(obj) = current(g, sh.id, sh.attempt) else { return };
        let Some(d) = es(g, obj) else { return };
        let origin = d.origin.clone();
        let msg = super::websocket::new_message_event_with_id(g, &ev.ty, js_string(g, &ev.data), &origin, &ev.id);
        events::dispatch(g, obj, msg);
    }
    if ended {
        if let Some(obj) = current(g, sh.id, sh.attempt) {
            reestablish(g, obj);
        }
    }
}

unsafe fn connect(g: *mut c_void, obj: u64) {
    let Some(d) = es(g, obj) else { return };
    let cancel = http::Cancel::default();
    d.cancel = Some(cancel.clone());
    let sh = Arc::new(Shared {
        id: d.id,
        attempt: d.attempt,
        ctx: Ctx(g),
        cid: crate::context::id_of(g),
        inbox: Mutex::new(Inbox::default()),
    });
    let mut headers = vec![
        ("Accept".to_string(), "text/event-stream".to_string()),
        ("Accept-Encoding".to_string(), "identity".to_string()),
        ("Cache-Control".to_string(), "no-cache".to_string()),
    ];
    if !d.last_id.is_empty() {
        headers.push(("Last-Event-ID".to_string(), d.last_id.clone()));
    }
    let obs = Obs { sh: Arc::clone(&sh), cancel: cancel.clone(), accepted: AtomicBool::new(false), parser: Mutex::new(Parser::new(&d.last_id)) };
    let req = http::Request {
        method: "GET".into(),
        url: d.url.clone(),
        headers,
        body: None,
        redirect: http::Redirect::Follow,
        credentials: if d.with_credentials { cookies::Credentials::Include } else { cookies::Credentials::SameOrigin },
        connect_timeout: CONNECT_TIMEOUT,
        io_timeout: IO_TIMEOUT,
        observer: Some(http::Observer(Arc::new(obs))),
        cache: http::cache::CacheMode::NoStore,
    };
    let worker = Arc::clone(&sh);
    if http::start(req, cancel, Box::new(move |_| worker.post(|i| i.ended = true))).is_err() {
        sh.post(|i| i.ended = true);
    }
}

#[crate::host_fn]
unsafe fn construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "Failed to construct 'EventSource': 1 argument required, but only 0 present.");
    }
    let Ok(s) = to_js_string(g, cf_arg(cf, 0)) else { return UNDEF };
    let raw = read_string(g, s).unwrap_or_default();
    let init = cf_arg(cf, 1);
    let with_credentials = if blob::is_object(g, init) {
        match try_get(g, init, "withCredentials") {
            Ok(v) => v != UNDEF && glue::rjs_to_boolean(g, v) != 0,
            Err(()) => return UNDEF,
        }
    } else {
        false
    };
    let base = super::xhr::base_url(g);
    let Ok(url) = url::Url::options().base_url(base.as_ref()).parse(&raw) else {
        return super::domexception::throw(g, "SyntaxError", &format!("Failed to construct 'EventSource': The URL '{raw}' is invalid."));
    };
    let c = classes(g);
    let nt = cf_new_target(cf);
    let mut proto = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
    if !is_cell(proto) {
        proto = c.source.proto;
    }
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let network = matches!(url.scheme(), "http" | "https");
    let obj = host_create(
        g,
        c.host,
        proto,
        EsData {
            id,
            state: CONNECTING,
            url: url.to_string(),
            with_credentials,
            attempt: 1,
            cancel: None,
            origin: String::new(),
            last_id: String::new(),
            retry_ms: DEFAULT_RETRY_MS,
        },
    );
    live().lock().unwrap().insert((g as usize, id), obj);
    crate::ffi::redjs_protect(g, obj);
    if network {
        connect(g, obj);
    } else {
        let scheme = url.scheme().to_string();
        crate::web::schedule_native(
            g,
            0.0,
            Box::new(move |g| unsafe {
                if let Some(obj) = current(g, id, 1) {
                    fail(g, obj, &format!("the {scheme}: scheme is not supported"));
                }
            }),
        );
    }
    obj
}

unsafe fn this_es<'a>(g: *mut c_void, cf: *mut c_void, what: &str) -> Result<&'a mut EsData, u64> {
    es(g, cf_this(cf)).ok_or_else(|| throw_type_error(g, &format!("Illegal invocation: EventSource.{what} on a non-EventSource")))
}

#[crate::host_fn]
unsafe fn close(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Err(e) = this_es(g, cf, "close") {
        return e;
    }
    close_down(g, cf_this(cf));
    UNDEF
}

#[crate::host_fn]
unsafe fn get_url(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_es(g, cf, "url") {
        Ok(d) => js_string(g, &d.url),
        Err(e) => e,
    }
}

#[crate::host_fn]
unsafe fn get_with_credentials(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_es(g, cf, "withCredentials") {
        Ok(d) => enc_bool(d.with_credentials),
        Err(e) => e,
    }
}

#[crate::host_fn]
unsafe fn get_ready_state(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_es(g, cf, "readyState") {
        Ok(d) => enc_i32(d.state as i32),
        Err(e) => e,
    }
}

unsafe fn handler_bag(g: *mut c_void, this: u64, create: bool) -> u64 {
    let slot = classes(g).s_handlers;
    let bag = get_a(g, this, slot);
    if is_cell(bag) || !create || !is_cell(this) {
        return bag;
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
            let v = if is_cell(bag) { get(g, bag, $name) } else { NULL };
            if blob::is_object(g, v) {
                v
            } else {
                NULL
            }
        }
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

handler_accessor!(get_onopen, set_onopen, "onopen");
handler_accessor!(get_onmessage, set_onmessage, "onmessage");
handler_accessor!(get_onerror, set_onerror, "onerror");

pub unsafe fn install(g: *mut c_void, event_target_proto: u64) {
    let host = make_host_class::<EsData>("EventSource");
    let source = define_class(g, &ClassSpec {
        name: "EventSource",
        arity: 1,
        construct: Some(construct),
        parent_proto: event_target_proto,
        methods: &[("close", 0, close)],
        getters: &[("url", get_url), ("withCredentials", get_with_credentials), ("readyState", get_ready_state)],
        accessors: &[
            ("onopen", get_onopen, set_onopen),
            ("onmessage", get_onmessage, set_onmessage),
            ("onerror", get_onerror, set_onerror),
        ],
    });
    for (name, v) in [("CONNECTING", CONNECTING), ("OPEN", OPEN), ("CLOSED", CLOSED)] {
        define(g, source.ctor, name, enc_i32(v as i32), 1 | 2 | 4);
        define(g, source.proto, name, enc_i32(v as i32), 1 | 2 | 4);
    }
    let c = Classes { host, source, s_handlers: atom_priv(g, "redjs#esHandlers") };
    registry().lock().unwrap().insert(g as usize, Box::leak(Box::new(c)));
}

#[cfg(test)]
mod tests;
