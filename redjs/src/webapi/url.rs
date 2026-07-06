//! Native `URL` + `URLSearchParams` backed by the `url` crate (rust-url is
//! the reference WHATWG URL implementation — servo/deno use it), so parsing,
//! serialization and component setters follow the standard for free.
//!
//! URL instances are host objects carrying `Box<::url::Url>`; URLSearchParams
//! carry their pair list plus an optional protected link to the parent URL
//! (spec: the two stay in sync).

use core::ffi::c_void;

use super::util::*;
use crate::glue;

pub struct UrlData {
    pub url: ::url::Url,
    /// Cached searchParams instance (protected), 0 until first access.
    pub usp: u64,
}

pub struct UspData {
    pub list: Vec<(String, String)>,
    /// Parent URL host object (protected) — mutations serialize back into it.
    pub parent: u64,
}

unsafe fn this_url<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut UrlData> {
    host_data::<UrlData>(g, state(g).url_class, cf_this(cf))
}

unsafe fn this_usp<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut UspData> {
    host_data::<UspData>(g, state(g).usp_class, cf_this(cf))
}

// ---------------------------------------------------------------------------
// URL
// ---------------------------------------------------------------------------

unsafe extern "sysv64" fn url_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let st = state(g);
    let input = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let base_v = cf_arg(cf, 1);

    let parsed = if base_v != UNDEF && base_v != NULL {
        let base_s = read_string(g, base_v).unwrap_or_default();
        match ::url::Url::parse(&base_s) {
            Ok(b) => b.join(&input),
            Err(e) => {
                return throw_type_error(g, &format!("URL constructor: invalid base URL \"{base_s}\": {e}"));
            }
        }
    } else {
        ::url::Url::parse(&input)
    };

    match parsed {
        Ok(u) => {
            let nt = cf_new_target(cf);
            let mut proto = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
            if !is_cell(proto) {
                proto = st.url.proto;
            }
            host_create(g, st.url_class, proto, UrlData { url: u, usp: 0 })
        }
        Err(e) => throw_type_error(g, &format!("URL constructor: \"{input}\" is not a valid URL: {e}")),
    }
}

macro_rules! url_getter {
    ($name:ident, |$u:ident| $body:expr) => {
        unsafe extern "sysv64" fn $name(g: *mut c_void, cf: *mut c_void) -> u64 {
            match this_url(g, cf) {
                Some(d) => {
                    let $u = &d.url;
                    let s: String = $body;
                    js_string(g, &s)
                }
                None => throw_type_error(g, "URL getter called on a non-URL"),
            }
        }
    };
}

url_getter!(get_href, |u| u.as_str().to_string());
url_getter!(get_protocol, |u| format!("{}:", u.scheme()));
url_getter!(get_hostname, |u| u.host_str().unwrap_or("").to_string());
url_getter!(get_port, |u| u.port().map(|p| p.to_string()).unwrap_or_default());
url_getter!(get_pathname, |u| u.path().to_string());
url_getter!(get_username, |u| u.username().to_string());
url_getter!(get_password, |u| u.password().unwrap_or("").to_string());
url_getter!(get_search, |u| match u.query() {
    Some(q) if !q.is_empty() => format!("?{q}"),
    _ => String::new(),
});
url_getter!(get_hash, |u| match u.fragment() {
    Some(f) if !f.is_empty() => format!("#{f}"),
    _ => String::new(),
});
url_getter!(get_host, |u| {
    let host = u.host_str().unwrap_or("");
    match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    }
});
url_getter!(get_origin, |u| u.origin().ascii_serialization());

unsafe fn arg_str(g: *mut c_void, cf: *mut c_void) -> String {
    read_string(g, cf_arg(cf, 0)).unwrap_or_default()
}

/// Keeps the cached searchParams list in sync after URL-side mutations (spec).
unsafe fn resync_usp(g: *mut c_void, d: &mut UrlData) {
    if d.usp != 0 {
        if let Some(usp) = host_data::<UspData>(g, state(g).usp_class, d.usp) {
            usp.list = d.url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        }
    }
}

unsafe extern "sysv64" fn set_href(g: *mut c_void, cf: *mut c_void) -> u64 {
    let s = arg_str(g, cf);
    let Some(d) = this_url(g, cf) else {
        return throw_type_error(g, "URL setter called on a non-URL");
    };
    match ::url::Url::parse(&s) {
        Ok(u) => {
            d.url = u;
            resync_usp(g, d);
            UNDEF
        }
        Err(e) => throw_type_error(g, &format!("URL.href: invalid URL: {e}")),
    }
}

macro_rules! url_setter {
    ($name:ident, |$u:ident, $s:ident| $body:expr) => {
        unsafe extern "sysv64" fn $name(g: *mut c_void, cf: *mut c_void) -> u64 {
            let $s = arg_str(g, cf);
            let Some(d) = this_url(g, cf) else {
                return throw_type_error(g, "URL setter called on a non-URL");
            };
            {
                let $u = &mut d.url;
                let _ = $body; // spec: component setters ignore failures
            }
            resync_usp(g, d);
            UNDEF
        }
    };
}

url_setter!(set_protocol, |u, s| u.set_scheme(s.trim_end_matches(':')));
url_setter!(set_hostname, |u, s| u.set_host(if s.is_empty() { None } else { Some(&s) }));
url_setter!(set_port, |u, s| u.set_port(if s.is_empty() { None } else { s.parse().ok() }));
url_setter!(set_pathname, |u, s| {
    u.set_path(&s);
    Ok::<(), ()>(())
});
url_setter!(set_username, |u, s| u.set_username(&s));
url_setter!(set_password, |u, s| u.set_password(if s.is_empty() { None } else { Some(&s) }));
url_setter!(set_search, |u, s| {
    let q = s.strip_prefix('?').unwrap_or(&s);
    u.set_query(if q.is_empty() { None } else { Some(q) });
    Ok::<(), ()>(())
});
url_setter!(set_hash, |u, s| {
    let f = s.strip_prefix('#').unwrap_or(&s);
    u.set_fragment(if f.is_empty() { None } else { Some(f) });
    Ok::<(), ()>(())
});
url_setter!(set_host, |u, s| {
    // host = hostname[:port]
    if let Some((h, p)) = s.rsplit_once(':') {
        if let Ok(port) = p.parse::<u16>() {
            let _ = u.set_host(Some(h));
            let _ = u.set_port(Some(port));
            Ok(())
        } else {
            u.set_host(if s.is_empty() { None } else { Some(&s) })
        }
    } else {
        u.set_host(if s.is_empty() { None } else { Some(&s) })
    }
});

unsafe extern "sysv64" fn url_to_string(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_url(g, cf) {
        Some(d) => {
            let s = d.url.as_str().to_string();
            js_string(g, &s)
        }
        None => throw_type_error(g, "URL.prototype.toString called on a non-URL"),
    }
}

unsafe extern "sysv64" fn get_search_params(g: *mut c_void, cf: *mut c_void) -> u64 {
    let st = state(g);
    let Some(d) = this_url(g, cf) else {
        return throw_type_error(g, "URL.searchParams called on a non-URL");
    };
    if d.usp == 0 {
        let list = d.url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        let parent = cf_this(cf);
        let usp = host_create(g, st.usp_class, st.usp.proto, UspData { list, parent });
        crate::ffi::redjs_protect(g, usp);
        crate::ffi::redjs_protect(g, parent); // keep the pair alive together
        d.usp = usp;
    }
    d.usp
}

unsafe extern "sysv64" fn can_parse(g: *mut c_void, cf: *mut c_void) -> u64 {
    let input = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let base_v = cf_arg(cf, 1);
    let ok = if base_v != UNDEF && base_v != NULL {
        let base_s = read_string(g, base_v).unwrap_or_default();
        ::url::Url::parse(&base_s).map(|b| b.join(&input).is_ok()).unwrap_or(false)
    } else {
        ::url::Url::parse(&input).is_ok()
    };
    enc_bool(ok)
}

unsafe extern "sysv64" fn create_object_url(g: *mut c_void, _cf: *mut c_void) -> u64 {
    // No blob URL store in the player; unique opaque token.
    static NEXT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);
    let n = NEXT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    js_string(g, &format!("blob:redpew/{n:x}"))
}

unsafe extern "sysv64" fn noop_undef(_g: *mut c_void, _cf: *mut c_void) -> u64 {
    UNDEF
}

// ---------------------------------------------------------------------------
// URLSearchParams
// ---------------------------------------------------------------------------

fn parse_query(q: &str) -> Vec<(String, String)> {
    ::url::form_urlencoded::parse(q.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
}

fn serialize_query(list: &[(String, String)]) -> String {
    let mut s = ::url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in list {
        s.append_pair(k, v);
    }
    s.finish()
}

unsafe extern "sysv64" fn usp_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let st = state(g);
    let init = cf_arg(cf, 0);
    let mut list: Vec<(String, String)> = Vec::new();
    if init != UNDEF && init != NULL {
        if let Some(s) = read_string(g, init) {
            let q = s.strip_prefix('?').unwrap_or(&s);
            list = parse_query(q);
        }
    }
    let nt = cf_new_target(cf);
    let mut proto = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
    if !is_cell(proto) {
        proto = st.usp.proto;
    }
    host_create(g, st.usp_class, proto, UspData { list, parent: 0 })
}

/// Spec: USP mutations write back into the linked URL's query.
unsafe fn sync_parent(g: *mut c_void, d: &UspData) {
    if d.parent == 0 {
        return;
    }
    if let Some(pu) = host_data::<UrlData>(g, state(g).url_class, d.parent) {
        let q = serialize_query(&d.list);
        pu.url.set_query(if q.is_empty() { None } else { Some(&q) });
    }
}

unsafe fn usp_arg(g: *mut c_void, cf: *mut c_void, i: u32) -> String {
    read_string(g, cf_arg(cf, i)).unwrap_or_default()
}

unsafe extern "sysv64" fn usp_get(g: *mut c_void, cf: *mut c_void) -> u64 {
    let k = usp_arg(g, cf, 0);
    match this_usp(g, cf) {
        Some(d) => match d.list.iter().find(|(key, _)| *key == k) {
            Some((_, v)) => {
                let v = v.clone();
                js_string(g, &v)
            }
            None => NULL,
        },
        None => throw_type_error(g, "URLSearchParams method called on wrong this"),
    }
}

unsafe extern "sysv64" fn usp_get_all(g: *mut c_void, cf: *mut c_void) -> u64 {
    let k = usp_arg(g, cf, 0);
    let arr = new_array(g);
    if let Some(d) = this_usp(g, cf) {
        let values: Vec<String> = d.list.iter().filter(|(key, _)| *key == k).map(|(_, v)| v.clone()).collect();
        for (i, v) in values.iter().enumerate() {
            let sv = js_string(g, v);
            set_i(g, arr, i as u32, sv);
        }
    }
    arr
}

unsafe extern "sysv64" fn usp_has(g: *mut c_void, cf: *mut c_void) -> u64 {
    let k = usp_arg(g, cf, 0);
    match this_usp(g, cf) {
        Some(d) => enc_bool(d.list.iter().any(|(key, _)| *key == k)),
        None => throw_type_error(g, "URLSearchParams method called on wrong this"),
    }
}

unsafe extern "sysv64" fn usp_set(g: *mut c_void, cf: *mut c_void) -> u64 {
    let k = usp_arg(g, cf, 0);
    let v = usp_arg(g, cf, 1);
    if let Some(d) = this_usp(g, cf) {
        let mut replaced = false;
        d.list.retain_mut(|(key, val)| {
            if *key == k {
                if replaced {
                    return false;
                }
                *val = v.clone();
                replaced = true;
            }
            true
        });
        if !replaced {
            d.list.push((k, v));
        }
        sync_parent(g, d);
    }
    UNDEF
}

unsafe extern "sysv64" fn usp_append(g: *mut c_void, cf: *mut c_void) -> u64 {
    let k = usp_arg(g, cf, 0);
    let v = usp_arg(g, cf, 1);
    if let Some(d) = this_usp(g, cf) {
        d.list.push((k, v));
        sync_parent(g, d);
    }
    UNDEF
}

unsafe extern "sysv64" fn usp_delete(g: *mut c_void, cf: *mut c_void) -> u64 {
    let k = usp_arg(g, cf, 0);
    if let Some(d) = this_usp(g, cf) {
        d.list.retain(|(key, _)| *key != k);
        sync_parent(g, d);
    }
    UNDEF
}

unsafe extern "sysv64" fn usp_sort(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Some(d) = this_usp(g, cf) {
        d.list.sort_by(|a, b| a.0.cmp(&b.0));
        sync_parent(g, d);
    }
    UNDEF
}

unsafe extern "sysv64" fn usp_for_each(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cb = cf_arg(cf, 0);
    let this_arg = cf_arg(cf, 1);
    let this_v = cf_this(cf);
    let pairs: Vec<(String, String)> = match this_usp(g, cf) {
        Some(d) => d.list.clone(),
        None => return throw_type_error(g, "URLSearchParams.forEach called on wrong this"),
    };
    for (k, v) in pairs {
        let kv = js_string(g, &k);
        let vv = js_string(g, &v);
        if call(g, cb, this_arg, &[vv, kv, this_v]).is_err() {
            break;
        }
    }
    UNDEF
}

unsafe extern "sysv64" fn usp_to_string(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_usp(g, cf) {
        Some(d) => {
            let s = serialize_query(&d.list);
            js_string(g, &s)
        }
        None => throw_type_error(g, "URLSearchParams.toString called on wrong this"),
    }
}

unsafe extern "sysv64" fn usp_get_size(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_usp(g, cf) {
        Some(d) => enc_i32(d.list.len() as i32),
        None => throw_type_error(g, "URLSearchParams.size called on wrong this"),
    }
}

// ---- iteration protocol (entries/keys/values/[Symbol.iterator]) -------------
//
// Spec-shaped: the iterator walks the LIVE pair list by index (mutations during
// iteration are observed, like in browsers), and keeps its URLSearchParams
// alive through a private-symbol slot — GC-visible, no protect bookkeeping.

pub struct UspIterData {
    idx: usize,
    kind: u8, // 0 = entries, 1 = keys, 2 = values
}

unsafe fn make_iter(g: *mut c_void, cf: *mut c_void, kind: u8) -> u64 {
    let st = state(g);
    let this_v = cf_this(cf);
    if host_data::<UspData>(g, st.usp_class, this_v).is_none() {
        return throw_type_error(g, "URLSearchParams iterator called on wrong this");
    }
    let it = host_create(g, st.usp_iter_class, st.usp_iter_proto, UspIterData { idx: 0, kind });
    glue::rjs_define(g, it, st.s_iter_target, this_v, 0);
    it
}

unsafe extern "sysv64" fn usp_entries(g: *mut c_void, cf: *mut c_void) -> u64 {
    make_iter(g, cf, 0)
}

unsafe extern "sysv64" fn usp_keys(g: *mut c_void, cf: *mut c_void) -> u64 {
    make_iter(g, cf, 1)
}

unsafe extern "sysv64" fn usp_values(g: *mut c_void, cf: *mut c_void) -> u64 {
    make_iter(g, cf, 2)
}

unsafe extern "sysv64" fn usp_iter_next(g: *mut c_void, cf: *mut c_void) -> u64 {
    let st = state(g);
    let this_v = cf_this(cf);
    let Some(it) = host_data::<UspIterData>(g, st.usp_iter_class, this_v) else {
        return throw_type_error(g, "next called on a non-URLSearchParams-iterator");
    };
    let target = get_a(g, this_v, st.s_iter_target);
    let result = new_object(g, UNDEF);

    let entry = host_data::<UspData>(g, st.usp_class, target).and_then(|d| {
        let e = d.list.get(it.idx).cloned();
        if e.is_some() {
            it.idx += 1;
        }
        e
    });
    match entry {
        Some((k, v)) => {
            let value = match it.kind {
                1 => js_string(g, &k),
                2 => js_string(g, &v),
                _ => {
                    let pair = new_array(g);
                    set_i(g, pair, 0, js_string(g, &k));
                    set_i(g, pair, 1, js_string(g, &v));
                    pair
                }
            };
            define(g, result, "value", value, 0);
            define(g, result, "done", enc_bool(false), 0);
        }
        None => {
            define(g, result, "value", UNDEF, 0);
            define(g, result, "done", enc_bool(true), 0);
        }
    }
    result
}

unsafe extern "sysv64" fn iter_self(_g: *mut c_void, cf: *mut c_void) -> u64 {
    cf_this(cf)
}

// ---------------------------------------------------------------------------

pub unsafe fn install(
    g: *mut c_void,
) -> (crate::capi::JSClassRef, Class, crate::capi::JSClassRef, Class, crate::capi::JSClassRef, u64) {
    let usp_cls = make_host_class::<UspData>("URLSearchParams");
    let usp = define_class(g, &ClassSpec {
        name: "URLSearchParams",
        arity: 0,
        construct: Some(usp_construct),
        parent_proto: 0,
        methods: &[
            ("get", 1, usp_get),
            ("getAll", 1, usp_get_all),
            ("has", 1, usp_has),
            ("set", 2, usp_set),
            ("append", 2, usp_append),
            ("delete", 1, usp_delete),
            ("sort", 0, usp_sort),
            ("forEach", 1, usp_for_each),
            ("entries", 0, usp_entries),
            ("keys", 0, usp_keys),
            ("values", 0, usp_values),
            ("toString", 0, usp_to_string),
        ],
        getters: &[("size", usp_get_size)],
        accessors: &[],
    });

    // %URLSearchParamsIteratorPrototype%: next() + self-returning
    // [Symbol.iterator]; USP's own [Symbol.iterator] is `entries` (spec).
    let usp_iter_cls = make_host_class::<UspIterData>("URLSearchParams Iterator");
    let usp_iter_proto = new_object(g, UNDEF);
    crate::ffi::redjs_protect(g, usp_iter_proto);
    define(g, usp_iter_proto, "next", make_fn(g, "next", 0, usp_iter_next), 2);
    let it_atom = glue::rjs_atom_iterator(g);
    glue::rjs_define(g, usp_iter_proto, it_atom, make_fn(g, "[Symbol.iterator]", 0, iter_self), 2);
    glue::rjs_define(g, usp.proto, it_atom, get(g, usp.proto, "entries"), 2);
    glue::rjs_atom_free(g, it_atom);

    let url_cls = make_host_class::<UrlData>("URL");
    let url = define_class(g, &ClassSpec {
        name: "URL",
        arity: 1,
        construct: Some(url_construct),
        parent_proto: 0,
        methods: &[("toString", 0, url_to_string), ("toJSON", 0, url_to_string)],
        getters: &[("origin", get_origin), ("searchParams", get_search_params)],
        accessors: &[
            ("href", get_href, set_href),
            ("protocol", get_protocol, set_protocol),
            ("host", get_host, set_host),
            ("hostname", get_hostname, set_hostname),
            ("port", get_port, set_port),
            ("pathname", get_pathname, set_pathname),
            ("search", get_search, set_search),
            ("hash", get_hash, set_hash),
            ("username", get_username, set_username),
            ("password", get_password, set_password),
        ],
    });

    // Statics on the URL constructor.
    let n16 = |s: &str| -> u64 {
        let mut f = 0u64;
        let name16: Vec<u16> = s.encode_utf16().collect();
        glue::rjs_function_create(
            g,
            name16.as_ptr(),
            name16.len(),
            1,
            match s {
                "canParse" => can_parse as *const c_void,
                "createObjectURL" => create_object_url as *const c_void,
                _ => noop_undef as *const c_void,
            },
            &mut f,
        );
        f
    };
    define(g, url.ctor, "canParse", n16("canParse"), 2);
    define(g, url.ctor, "createObjectURL", n16("createObjectURL"), 2);
    define(g, url.ctor, "revokeObjectURL", n16("revokeObjectURL"), 2);

    (url_cls, url, usp_cls, usp, usp_iter_cls, usp_iter_proto)
}
