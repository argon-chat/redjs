//! `Request` (WHATWG Fetch) and the parsing of `(input, init)` that both
//! `new Request(...)` and `fetch(...)` share.
//!
//! Split out of `fetch.rs` on size (CLAUDE.md rule 3) along the natural seam:
//! this file decides **what request was asked for and whether this runtime will
//! perform it**; `fetch.rs` performs it and shapes the answer.
//!
//! ## What was here before
//!
//! `Request` was six lines of `bootstrap.js`:
//!
//! ```js
//! g.Request = class Request {
//!   constructor(input, init) {
//!     init = init || {};
//!     this.url = String((input && input.url) || input || '');
//!     this.method = init.method || 'GET';
//!     this.headers = new g.Headers();
//!   }
//! };
//! ```
//!
//! It dropped `init.headers` and `init.body` on the floor, so
//! `fetch(new Request(url, { method: 'POST', body, headers }))` sent a bodiless
//! GET with no headers and reported nothing wrong — the same silent-wrong-answer
//! shape as the status-0 `Response`. The stub is guarded by
//! `if (typeof g.Request !== 'function')`, and the native class installed here
//! runs first, so `bootstrap.js` no longer defines anything.
//!
//! ## Refusals
//!
//! [`check_supported`] is the single place where an unsupported request shape is
//! turned away, and every arm names the option and says why. The rule it
//! encodes: an option this runtime cannot honour must never be *ignored*,
//! because a request that quietly omits the caller's cookies, integrity hash or
//! redirect policy succeeds while doing the wrong thing.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use super::fetch::{reject_refusal, reject_with};
use super::headers;
use super::util::*;
use super::{blob, formdata};
use crate::capi;
use crate::glue;

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

pub struct RequestData {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Arc<Vec<u8>>>,
    pub redirect: String,
    pub credentials: String,
    pub mode: String,
    pub cache: String,
    pub integrity: String,
    pub keepalive: bool,
    /// The `AbortSignal`, or UNDEF. Held as a plain property on the instance
    /// too (see `request_construct`) so the GC keeps it alive.
    pub signal: u64,
    pub body_used: bool,
}

struct RequestClass {
    cls: capi::JSClassRef,
    class: Class,
}
unsafe impl Send for RequestClass {}
unsafe impl Sync for RequestClass {}

fn request_registry() -> &'static Mutex<HashMap<usize, &'static RequestClass>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static RequestClass>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn forget(gone: crate::context::Gone) {
    request_registry().lock().unwrap().remove(&gone.key);
}

fn request_class(g: *mut c_void) -> &'static RequestClass {
    request_registry().lock().unwrap().get(&(g as usize)).copied().expect("webapi::fetch not installed")
}

pub unsafe fn request_data<'a>(g: *mut c_void, v: u64) -> Option<&'a mut RequestData> {
    host_data::<RequestData>(g, request_class(g).cls, v)
}

macro_rules! req_getter {
    ($name:ident, |$d:ident, $g:ident| $body:expr) => {
        unsafe extern "sysv64" fn $name($g: *mut c_void, cf: *mut c_void) -> u64 {
            match request_data($g, cf_this(cf)) {
                Some($d) => $body,
                None => throw_type_error($g, "Request getter called on a non-Request"),
            }
        }
    };
}

req_getter!(req_method, |d, g| {
    let s = d.method.clone();
    js_string(g, &s)
});
req_getter!(req_url, |d, g| {
    let s = d.url.clone();
    js_string(g, &s)
});
req_getter!(req_redirect, |d, g| {
    let s = d.redirect.clone();
    js_string(g, &s)
});
req_getter!(req_credentials, |d, g| {
    let s = d.credentials.clone();
    js_string(g, &s)
});
req_getter!(req_mode, |d, g| {
    let s = d.mode.clone();
    js_string(g, &s)
});
req_getter!(req_cache, |d, g| {
    let s = d.cache.clone();
    js_string(g, &s)
});
req_getter!(req_integrity, |d, g| {
    let s = d.integrity.clone();
    js_string(g, &s)
});
req_getter!(req_keepalive, |d, g| {
    let _ = g;
    enc_bool(d.keepalive)
});
req_getter!(req_signal, |d, g| {
    let _ = g;
    d.signal
});
req_getter!(req_body_used, |d, g| {
    let _ = g;
    enc_bool(d.body_used)
});

unsafe extern "sysv64" fn req_headers(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let slot = atom_priv(g, "redjs#reqHeaders");
    let cached = get_a(g, this, slot);
    if is_cell(cached) {
        glue::rjs_atom_free(g, slot);
        return cached;
    }
    let Some(d) = request_data(g, this) else {
        glue::rjs_atom_free(g, slot);
        return throw_type_error(g, "Request.headers on a non-Request");
    };
    let h = headers::create(g, d.headers.clone());
    glue::rjs_define(g, this, slot, h, 0);
    glue::rjs_atom_free(g, slot);
    h
}

unsafe fn req_take_body(g: *mut c_void, cf: *mut c_void, who: &str) -> Result<Vec<u8>, u64> {
    let Some(d) = request_data(g, cf_this(cf)) else {
        return Err(reject_refusal(g, &format!("Request.{who} called on a non-Request")));
    };
    if d.body_used {
        return Err(reject_refusal(g, &format!("Request.{who}: body has already been consumed")));
    }
    d.body_used = true;
    Ok(d.body.as_ref().map(|b| b.as_slice().to_vec()).unwrap_or_default())
}

unsafe extern "sysv64" fn req_text(g: *mut c_void, cf: *mut c_void) -> u64 {
    match req_take_body(g, cf, "text") {
        Ok(b) => {
            let s = String::from_utf8_lossy(&b).into_owned();
            resolved_promise(g, js_string(g, &s))
        }
        Err(p) => p,
    }
}

unsafe extern "sysv64" fn req_json(g: *mut c_void, cf: *mut c_void) -> u64 {
    let st = state(g);
    match req_take_body(g, cf, "json") {
        Ok(b) => {
            let s = String::from_utf8_lossy(&b).into_owned();
            let sv = js_string(g, &s);
            match call(g, st.json_parse, UNDEF, &[sv]) {
                Ok(v) => resolved_promise(g, v),
                Err(e) => reject_with(g, e),
            }
        }
        Err(p) => p,
    }
}

unsafe extern "sysv64" fn req_array_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    match req_take_body(g, cf, "arrayBuffer") {
        Ok(b) => {
            let u8v = make_u8(g, &b);
            resolved_promise(g, get(g, u8v, "buffer"))
        }
        Err(p) => p,
    }
}

unsafe extern "sysv64" fn req_bytes(g: *mut c_void, cf: *mut c_void) -> u64 {
    match req_take_body(g, cf, "bytes") {
        Ok(b) => {
            let u8v = make_u8(g, &b);
            resolved_promise(g, u8v)
        }
        Err(p) => p,
    }
}

unsafe extern "sysv64" fn req_clone(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(d) = request_data(g, cf_this(cf)) else {
        return throw_type_error(g, "Request.clone called on a non-Request");
    };
    if d.body_used {
        return throw_type_error(g, "Request.clone: body has already been consumed");
    }
    let copy = RequestData {
        method: d.method.clone(),
        url: d.url.clone(),
        headers: d.headers.clone(),
        body: d.body.clone(),
        redirect: d.redirect.clone(),
        credentials: d.credentials.clone(),
        mode: d.mode.clone(),
        cache: d.cache.clone(),
        integrity: d.integrity.clone(),
        keepalive: d.keepalive,
        signal: d.signal,
        body_used: false,
    };
    let signal = copy.signal;
    let c = request_class(g);
    let obj = host_create(g, c.cls, c.class.proto, copy);
    if is_cell(signal) {
        let slot = atom_priv(g, "redjs#reqSignal");
        glue::rjs_define(g, obj, slot, signal, 0);
        glue::rjs_atom_free(g, slot);
    }
    obj
}

/// The parsed, validated result of `(input, init)`.
pub struct Plan {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub redirect: String,
    pub credentials: String,
    pub mode: String,
    pub cache: String,
    pub integrity: String,
    pub keepalive: bool,
    pub signal: u64,
}

/// Fetch §"Request constructor", minus what this runtime refuses. Errors are
/// full sentences: they name the option and say why.
pub unsafe fn build_plan(g: *mut c_void, input: u64, init: u64) -> Result<Plan, String> {
    let mut plan = Plan {
        method: "GET".into(),
        url: String::new(),
        headers: Vec::new(),
        body: None,
        redirect: "follow".into(),
        credentials: "same-origin".into(),
        mode: "cors".into(),
        cache: "default".into(),
        integrity: String::new(),
        keepalive: false,
        signal: UNDEF,
    };

    // -- input: a Request, a URL, or a string ------------------------------
    if let Some(src) = request_data(g, input) {
        plan.method = src.method.clone();
        plan.url = src.url.clone();
        plan.headers = src.headers.clone();
        plan.body = src.body.as_ref().map(|b| b.as_slice().to_vec());
        plan.redirect = src.redirect.clone();
        plan.credentials = src.credentials.clone();
        plan.mode = src.mode.clone();
        plan.cache = src.cache.clone();
        plan.integrity = src.integrity.clone();
        plan.keepalive = src.keepalive;
        plan.signal = src.signal;
    } else if is_cell(input) && glue::rjs_kind(g, input) != 5 {
        // A URL object, or any object exposing `url` (the shape the old
        // bootstrap `Request` had).
        let u = get(g, input, "url");
        plan.url = if u != UNDEF {
            read_string(g, u).unwrap_or_default()
        } else {
            read_string(g, input).unwrap_or_default()
        };
        let m = get(g, input, "method");
        if m != UNDEF {
            plan.method = read_string(g, m).unwrap_or_default();
        }
    } else {
        plan.url = read_string(g, input).ok_or_else(|| "the URL could not be read as a string".to_string())?;
    }

    // -- init --------------------------------------------------------------
    if is_cell(init) {
        let m = get(g, init, "method");
        if m != UNDEF {
            plan.method = read_string(g, m).unwrap_or_default();
        }
        let h = get(g, init, "headers");
        if h != UNDEF && h != NULL {
            plan.headers = headers::from_init(g, h)?;
        }
        for (key, slot) in [
            ("redirect", &mut plan.redirect),
            ("credentials", &mut plan.credentials),
            ("mode", &mut plan.mode),
            ("cache", &mut plan.cache),
            ("integrity", &mut plan.integrity),
        ] {
            let v = get(g, init, key);
            if v != UNDEF && v != NULL {
                *slot = read_string(g, v).unwrap_or_default();
            }
        }
        let k = get(g, init, "keepalive");
        if k != UNDEF {
            plan.keepalive = truthy_flag(k);
        }
        let s = get(g, init, "signal");
        if s != UNDEF && s != NULL {
            plan.signal = s;
        }
        let b = get(g, init, "body");
        if b != UNDEF && b != NULL {
            if let Some((bytes, implied)) = extract_body(g, b)? {
                plan.body = Some(bytes);
                if let Some(mime) = implied {
                    if !plan.headers.iter().any(|(k, _)| k == "content-type") && !mime.is_empty() {
                        plan.headers.push(("content-type".into(), mime));
                    }
                }
            }
        }
    }

    plan.method = plan.method.to_ascii_uppercase();
    check_supported(&plan)?;
    Ok(plan)
}

/// Everything this runtime will not do, refused **by name**. A request that
/// silently drops the thing the caller asked for is the defect this module was
/// rewritten to remove; each arm below is one shape of it.
pub fn check_supported(plan: &Plan) -> Result<(), String> {
    // Fetch §"forbidden method": a browser refuses these outright.
    if matches!(plan.method.as_str(), "CONNECT" | "TRACE" | "TRACK") {
        return Err(format!("method {:?} is forbidden by the Fetch specification", plan.method));
    }
    if !plan.method.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)) {
        return Err(format!("{:?} is not a valid HTTP method", plan.method));
    }

    match plan.redirect.as_str() {
        "follow" | "error" => {}
        // Would need an opaque-redirect response, which this Response cannot be.
        "manual" => {
            return Err("redirect: \"manual\" is not supported by this runtime — \
                        use \"follow\" (the default) or \"error\""
                .into())
        }
        other => return Err(format!("redirect: {other:?} is not a valid redirect mode")),
    }

    match plan.credentials.as_str() {
        "omit" | "same-origin" => {}
        // There is no cookie jar in the player. Accepting this would send the
        // request WITHOUT the cookies the caller believes it attached.
        "include" => {
            return Err("credentials: \"include\" is not supported by this runtime — \
                        it has no cookie store, so cookies would be silently omitted. \
                        Set the Cookie header explicitly if you need one"
                .into())
        }
        other => return Err(format!("credentials: {other:?} is not a valid credentials mode")),
    }

    match plan.mode.as_str() {
        "cors" | "same-origin" | "navigate" => {}
        // An opaque response is a Response whose body and headers read as empty
        // — indistinguishable, from JS, from the status-0 bug being fixed here.
        "no-cors" => {
            return Err("mode: \"no-cors\" is not supported by this runtime — \
                        it would require an opaque response"
                .into())
        }
        other => return Err(format!("mode: {other:?} is not a valid request mode")),
    }

    // No HTTP cache exists, so every mode but these would be a lie about
    // freshness; "only-if-cached" in particular could never succeed.
    match plan.cache.as_str() {
        "default" | "no-store" | "no-cache" | "reload" => {}
        "only-if-cached" | "force-cache" => {
            return Err(format!(
                "cache: {:?} is not supported by this runtime — it has no HTTP cache",
                plan.cache
            ))
        }
        other => return Err(format!("cache: {other:?} is not a valid cache mode")),
    }

    if !plan.integrity.is_empty() {
        return Err(format!(
            "integrity: {:?} is not supported by this runtime — the digest would go unchecked, \
             which is worse than not asking for it",
            plan.integrity
        ));
    }
    Ok(())
}

unsafe extern "sysv64" fn request_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let plan = match build_plan(g, cf_arg(cf, 0), cf_arg(cf, 1)) {
        Ok(p) => p,
        Err(m) => return throw_type_error(g, &format!("new Request: {m}")),
    };
    let c = request_class(g);
    // Subclassing: the instance prototype comes from new.target.
    let nt = cf_new_target(cf);
    let p = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
    let proto = if is_cell(p) { p } else { c.class.proto };
    let signal = plan.signal;
    let obj = host_create(g, c.cls, proto, RequestData {
        method: plan.method,
        url: plan.url,
        headers: plan.headers,
        body: plan.body.map(Arc::new),
        redirect: plan.redirect,
        credentials: plan.credentials,
        mode: plan.mode,
        cache: plan.cache,
        integrity: plan.integrity,
        keepalive: plan.keepalive,
        signal,
        body_used: false,
    });
    // Keeps the signal reachable for the GC for as long as the Request is.
    if is_cell(signal) {
        let slot = atom_priv(g, "redjs#reqSignal");
        glue::rjs_define(g, obj, slot, signal, 0);
        glue::rjs_atom_free(g, slot);
    }
    obj
}

// ---------------------------------------------------------------------------
// BodyInit
// ---------------------------------------------------------------------------

/// `Ok(None)` = no body. `Ok(Some((bytes, implied content-type)))` otherwise.
pub unsafe fn extract_body(g: *mut c_void, v: u64) -> Result<Option<(Vec<u8>, Option<String>)>, String> {
    if v == UNDEF || v == NULL {
        return Ok(None);
    }
    // A ReadableStream body would have to be pumped into the request, which
    // this buffered transport cannot do. Named, not ignored: silently sending
    // "[object ReadableStream]" as the body is the failure mode being avoided.
    if is_cell(v) {
        let reader = get(g, v, "getReader");
        if is_cell(reader) && glue::rjs_kind(g, reader) == 9 {
            return Err("a ReadableStream body is not supported by this runtime — \
                        buffer it (await new Response(stream).arrayBuffer()) and pass the bytes"
                .into());
        }
    }
    // FormData first: it is the one BodyInit whose Content-Type the caller
    // cannot write itself (the multipart boundary is generated per body).
    if let Some(r) = formdata::encode_body(g, v) {
        let (bytes, ct) = r?;
        return Ok(Some((bytes, Some(ct))));
    }
    if let Some(r) = blob::bytes_of(g, v, "a fetch/Request/Response body") {
        let bytes = r.map_err(|e| blob::sync_read_failed("a fetch/Request/Response body", e))?;
        let t = blob::data_of(g, v).map(|b| b.mime.clone()).unwrap_or_default();
        return Ok(Some((bytes, (!t.is_empty()).then_some(t))));
    }
    let st = state(g);
    if is_cell(v) && capi::JSValueIsObjectOfClass(g, v as usize as capi::JSValueRef, st.usp_class) {
        let s = read_string(g, v).unwrap_or_default();
        return Ok(Some((s.into_bytes(), Some("application/x-www-form-urlencoded;charset=UTF-8".into()))));
    }
    if let Some(b) = bufferish(g, v) {
        return Ok(Some((b.to_vec(), None)));
    }
    let s = read_string(g, v).ok_or_else(|| "the body could not be read".to_string())?;
    Ok(Some((s.into_bytes(), Some("text/plain;charset=UTF-8".into()))))
}


// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

/// Called from `fetch::install`, before `Response` — both need `Headers`, and
/// defining `Request` natively is what stops the `bootstrap.js` stub (guarded by
/// `typeof g.Request !== 'function'`) from ever being installed.
pub unsafe fn install(g: *mut c_void) {
    let cls = make_host_class::<RequestData>("Request");
    let class = define_class(g, &ClassSpec {
        name: "Request",
        arity: 1,
        construct: Some(request_construct),
        parent_proto: 0,
        methods: &[
            ("clone", 0, req_clone),
            ("text", 0, req_text),
            ("json", 0, req_json),
            ("arrayBuffer", 0, req_array_buffer),
            ("bytes", 0, req_bytes),
        ],
        getters: &[
            ("method", req_method),
            ("url", req_url),
            ("headers", req_headers),
            ("redirect", req_redirect),
            ("credentials", req_credentials),
            ("mode", req_mode),
            ("cache", req_cache),
            ("integrity", req_integrity),
            ("keepalive", req_keepalive),
            ("signal", req_signal),
            ("bodyUsed", req_body_used),
        ],
        accessors: &[],
    });
    let boxed: &'static RequestClass = Box::leak(Box::new(RequestClass { cls, class }));
    request_registry().lock().unwrap().insert(g as usize, boxed);
}
