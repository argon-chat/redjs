//! Native `fetch` (local files + data: URIs — games ship their assets; http
//! resolves to a network-error response per fetch semantics), with native
//! `Response` and a minimal `Headers`.

use core::ffi::c_void;
use std::sync::Arc;

use super::fsutil;
use super::util::*;
use crate::glue;

pub struct RespData {
    pub bytes: Arc<Vec<u8>>,
    pub mime: String,
    pub url: String,
    pub status: i32,
    pub status_text: String,
}

pub struct HeadersData {
    pub list: Vec<(String, String)>,
}

unsafe fn this_resp<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut RespData> {
    host_data::<RespData>(g, state(g).response_class, cf_this(cf))
}

pub unsafe fn create_response(g: *mut c_void, data: RespData) -> u64 {
    let st = state(g);
    host_create(g, st.response_class, st.response.proto, data)
}

// ---------------------------------------------------------------------------
// Headers (subset: case-insensitive get/has/forEach over a fixed list)
// ---------------------------------------------------------------------------

unsafe fn this_headers<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut HeadersData> {
    host_data::<HeadersData>(g, headers_class(g), cf_this(cf))
}

fn headers_class(g: *mut c_void) -> crate::capi::JSClassRef {
    // Stored alongside the response class in WebState via a second slot; to
    // keep WebState lean we piggyback on a per-process class (one definition
    // is fine for all contexts — JSClassRef is context-independent).
    use std::sync::OnceLock;
    static CLS: OnceLock<usize> = OnceLock::new();
    let _ = g;
    *CLS.get_or_init(|| unsafe { make_host_class::<HeadersData>("Headers") as usize }) as crate::capi::JSClassRef
}

unsafe extern "sysv64" fn headers_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let mut list: Vec<(String, String)> = Vec::new();
    let init = cf_arg(cf, 0);
    if is_cell(init) {
        // sequence of [k, v] or record — support the record form.
        let keys = get(g, init, "length");
        if keys == UNDEF {
            // Record: enumerate is not exposed via our ABI; accept the common
            // case of a content-type field.
            let ct = get(g, init, "content-type");
            if let Some(v) = read_string(g, ct) {
                if ct != UNDEF {
                    list.push(("content-type".into(), v));
                }
            }
        }
    }
    let proto = {
        let nt = cf_new_target(cf);
        let p = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
        p
    };
    host_create(g, headers_class(g), proto, HeadersData { list })
}

unsafe extern "sysv64" fn headers_get(g: *mut c_void, cf: *mut c_void) -> u64 {
    let k = read_string(g, cf_arg(cf, 0)).unwrap_or_default().to_ascii_lowercase();
    match this_headers(g, cf) {
        Some(h) => match h.list.iter().find(|(key, _)| key.eq_ignore_ascii_case(&k)) {
            Some((_, v)) => {
                let v = v.clone();
                js_string(g, &v)
            }
            None => NULL,
        },
        None => NULL,
    }
}

unsafe extern "sysv64" fn headers_has(g: *mut c_void, cf: *mut c_void) -> u64 {
    let k = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    match this_headers(g, cf) {
        Some(h) => enc_bool(h.list.iter().any(|(key, _)| key.eq_ignore_ascii_case(&k))),
        None => FALSE,
    }
}

unsafe extern "sysv64" fn headers_for_each(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cb = cf_arg(cf, 0);
    let this_v = cf_this(cf);
    let pairs: Vec<(String, String)> = match this_headers(g, cf) {
        Some(h) => h.list.clone(),
        None => return UNDEF,
    };
    for (k, v) in pairs {
        let kv = js_string(g, &k);
        let vv = js_string(g, &v);
        if call(g, cb, UNDEF, &[vv, kv, this_v]).is_err() {
            break;
        }
    }
    UNDEF
}

unsafe extern "sysv64" fn noop(_g: *mut c_void, _cf: *mut c_void) -> u64 {
    UNDEF
}

unsafe fn make_headers(g: *mut c_void, content_type: &str) -> u64 {
    let proto = get(g, get(g, glue::rjs_global(g), "Headers"), "prototype");
    host_create(
        g,
        headers_class(g),
        proto,
        HeadersData { list: vec![("content-type".into(), content_type.to_string())] },
    )
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

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

resp_getter!(resp_ok, |d, g| enc_bool(d.status >= 200 && d.status < 300));
resp_getter!(resp_status, |d, g| enc_i32(d.status));
resp_getter!(resp_status_text, |d, g| {
    let s = d.status_text.clone();
    js_string(g, &s)
});
resp_getter!(resp_url, |d, g| {
    let s = d.url.clone();
    js_string(g, &s)
});
resp_getter!(resp_redirected, |_d, _g| FALSE);
resp_getter!(resp_body_used, |_d, _g| FALSE);
resp_getter!(resp_type, |_d, g| js_string(g, "basic"));
resp_getter!(resp_headers, |d, g| {
    let ct = d.mime.clone();
    make_headers(g, &ct)
});

unsafe extern "sysv64" fn resp_array_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(d) = this_resp(g, cf) else {
        return rejected_promise(g, 1, "Response.arrayBuffer called on a non-Response");
    };
    let bytes = d.bytes.as_slice().to_vec();
    let u8v = make_u8(g, &bytes);
    let buffer = get(g, u8v, "buffer");
    resolved_promise(g, buffer)
}

unsafe extern "sysv64" fn resp_bytes(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(d) = this_resp(g, cf) else {
        return rejected_promise(g, 1, "Response.bytes called on a non-Response");
    };
    let bytes = d.bytes.as_slice().to_vec();
    let u8v = make_u8(g, &bytes);
    resolved_promise(g, u8v)
}

unsafe extern "sysv64" fn resp_text(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(d) = this_resp(g, cf) else {
        return rejected_promise(g, 1, "Response.text called on a non-Response");
    };
    let s = String::from_utf8_lossy(&d.bytes).into_owned();
    resolved_promise(g, js_string(g, &s))
}

unsafe extern "sysv64" fn resp_json(g: *mut c_void, cf: *mut c_void) -> u64 {
    let st = state(g);
    let Some(d) = this_resp(g, cf) else {
        return rejected_promise(g, 1, "Response.json called on a non-Response");
    };
    let s = String::from_utf8_lossy(&d.bytes).into_owned();
    let sv = js_string(g, &s);
    match call(g, st.json_parse, UNDEF, &[sv]) {
        Ok(v) => resolved_promise(g, v),
        Err(_) => rejected_promise(g, 3, "Response.json: invalid JSON"),
    }
}

unsafe extern "sysv64" fn resp_blob(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(d) = this_resp(g, cf) else {
        return rejected_promise(g, 1, "Response.blob called on a non-Response");
    };
    let bytes = d.bytes.as_slice().to_vec();
    let mime = d.mime.clone();
    let blob = super::blob::create(g, UNDEF, bytes, mime);
    resolved_promise(g, blob)
}

unsafe extern "sysv64" fn resp_clone(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(d) = this_resp(g, cf) else {
        return throw_type_error(g, "Response.clone called on a non-Response");
    };
    let data = RespData {
        bytes: Arc::clone(&d.bytes),
        mime: d.mime.clone(),
        url: d.url.clone(),
        status: d.status,
        status_text: d.status_text.clone(),
    };
    create_response(g, data)
}

unsafe extern "sysv64" fn response_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    // `new Response(body?, init?)` — string / bufferish / Blob / FormData bodies.
    let body = cf_arg(cf, 0);
    let mut bytes: Vec<u8> = Vec::new();
    // Fetch §"body with type": a BodyInit that carries its own type supplies the
    // Content-Type. FormData's is the one that MATTERS — the boundary is
    // generated per body, so a caller cannot write the header itself.
    let mut mime = "text/plain".to_string();
    if body != UNDEF && body != NULL {
        if let Some((encoded, content_type)) = super::formdata::encode_body(g, body) {
            bytes = encoded;
            mime = content_type;
        } else if let Some(b) = super::blob::bytes_of(g, body) {
            let t = read_string(g, get(g, body, "type")).unwrap_or_default();
            bytes = b;
            if !t.is_empty() {
                mime = t;
            }
        } else if let Some(b) = bufferish(g, body) {
            bytes = b.to_vec();
        } else if let Some(s) = read_string(g, body) {
            bytes = s.into_bytes();
        }
    }
    let init = cf_arg(cf, 1);
    let status = if is_cell(init) { dec_f64(get(g, init, "status")).unwrap_or(200.0) as i32 } else { 200 };
    let status_text = if is_cell(init) {
        match get(g, init, "statusText") {
            v if v != UNDEF => read_string(g, v).unwrap_or_default(),
            _ => String::new(),
        }
    } else {
        String::new()
    };
    create_response(g, RespData {
        bytes: Arc::new(bytes),
        mime,
        url: String::new(),
        status,
        status_text,
    })
}

// ---------------------------------------------------------------------------
// fetch
// ---------------------------------------------------------------------------

unsafe extern "sysv64" fn fetch_fn(g: *mut c_void, cf: *mut c_void) -> u64 {
    let input = cf_arg(cf, 0);
    // string | Request-like {url} | URL (toString coercion covers it).
    let url = if is_cell(input) && glue::rjs_kind(g, input) != 5 {
        let u = get(g, input, "url");
        if u != UNDEF {
            read_string(g, u).unwrap_or_default()
        } else {
            read_string(g, input).unwrap_or_default()
        }
    } else {
        read_string(g, input).unwrap_or_default()
    };

    if url.starts_with("http://") || url.starts_with("https://") {
        eprintln!("[js:fetch] http(s) is not supported in the player: {url}");
    }
    let fetched = fsutil::fetch_url(g, &url);
    let resp = create_response(g, RespData {
        bytes: Arc::new(fetched.bytes),
        mime: fetched.mime,
        url,
        status: fetched.status,
        status_text: fetched.status_text.to_string(),
    });
    resolved_promise(g, resp)
}

// ---------------------------------------------------------------------------

pub unsafe fn install(g: *mut c_void) -> (crate::capi::JSClassRef, Class) {
    define_class(g, &ClassSpec {
        name: "Headers",
        arity: 0,
        construct: Some(headers_construct),
        parent_proto: 0,
        methods: &[
            ("get", 1, headers_get),
            ("has", 1, headers_has),
            ("forEach", 1, headers_for_each),
            ("set", 2, noop),
            ("append", 2, noop),
            ("delete", 1, noop),
        ],
        getters: &[],
        accessors: &[],
    });

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
            ("type", resp_type),
            ("headers", resp_headers),
        ],
        accessors: &[],
    });

    let n16: Vec<u16> = "fetch".encode_utf16().collect();
    let mut f = UNDEF;
    glue::rjs_function_create(g, n16.as_ptr(), n16.len(), 2, fetch_fn as *const c_void, &mut f);
    define(g, glue::rjs_global(g), "fetch", f, 2);

    (resp_cls, response)
}
