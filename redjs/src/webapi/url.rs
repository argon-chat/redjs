//! Native `URL` + `URLSearchParams` backed by the `url` crate (rust-url is
//! the reference WHATWG URL implementation — servo/deno use it), so parsing,
//! serialization and component setters follow the standard for free.
//!
//! URL instances are host objects carrying `Box<::url::Url>`; URLSearchParams
//! carry their pair list plus an optional protected link to the parent URL
//! (spec: the two stay in sync).
//!
//! `URL.createObjectURL` / `revokeObjectURL` keep a real blob URL store per
//! context. Everything in redjs that takes a URL resolves `blob:` through it:
//! `fetch`, an `Image`'s `src` and a `createImageBitmap` string (image.rs).
//! A host resolves them with [`resolve_blob_url`] (vue-native: `<img>`,
//! `<video>`, `<lottie>`). The store is per context: once Workers exist, a
//! URL made in a worker will not resolve in the window. A browser has one
//! store per user agent, where it does — a note for the Workers design.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

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

// ---------------------------------------------------------------------------
// Blob URLs (File API §8): the per-context store
// ---------------------------------------------------------------------------
//
// An entry holds the Blob's SOURCE — the `Arc` of its bytes or its path, and
// its window — never the Blob object: no JS value is protected, so the store
// cannot keep a heap alive, and a host consumer on another thread (vn-tree's
// `<img>`) can read it. The bytes stay alive until `revokeObjectURL`, or until
// the context goes, as in a browser.

/// What a blob URL resolves to.
#[derive(Clone, Debug)]
pub(crate) struct BlobEntry {
    pub range: super::blob::Range,
    pub mime: String,
}

#[derive(Default)]
struct BlobUrls {
    /// The origin part of this context's blob URLs, made on first use.
    origin: Option<String>,
    urls: HashMap<String, BlobEntry>,
}

fn blob_urls() -> &'static Mutex<HashMap<usize, BlobUrls>> {
    static S: OnceLock<Mutex<HashMap<usize, BlobUrls>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The store belonged to a context that is gone; its bytes go with it.
pub(crate) fn forget(gone: crate::context::Gone) {
    blob_urls().lock().unwrap().remove(&gone.key);
}

/// A random (version 4) UUID, lowercase — the shape every browser uses.
fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    if getrandom::fill(&mut b).is_err() {
        // Never a constant: two URLs must not collide. The clock and a counter
        // are unique per call, if not unpredictable.
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        b[..8].copy_from_slice(&crate::web::now_ms().to_bits().to_le_bytes());
        b[8..].copy_from_slice(&N.fetch_add(1, std::sync::atomic::Ordering::Relaxed).to_le_bytes());
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// The key a lookup uses: the parsed URL without its fragment (Chromium and
/// the URL Standard's "resolve a blob URL" both ignore it). `None` = not a
/// `blob:` URL at all.
fn store_key(url: &str) -> Option<String> {
    let mut u = ::url::Url::parse(url.trim()).ok()?;
    if u.scheme() != "blob" {
        return None;
    }
    u.set_fragment(None);
    Some(u.as_str().to_string())
}

/// File API "generate a new blob URL": `blob:` + the serialization of the
/// environment's origin + `/` + a UUID. The origin is this context's
/// `location.origin`, read once; where there is none, or it is opaque
/// (`"null"`), the spec lets the value be implementation-defined, and it is
/// `vn`.
unsafe fn origin_part(g: *mut c_void) -> String {
    if let Some(o) = blob_urls().lock().unwrap().get(&(g as usize)).and_then(|s| s.origin.clone()) {
        return o;
    }
    let loc = get(g, glue::rjs_global(g), "location");
    let o = if is_cell(loc) { read_string(g, get(g, loc, "origin")).unwrap_or_default() } else { String::new() };
    let o = if o.is_empty() || o == "null" { "vn".to_string() } else { o };
    blob_urls().lock().unwrap().entry(g as usize).or_default().origin = Some(o.clone());
    o
}

unsafe extern "sysv64" fn create_object_url(g: *mut c_void, cf: *mut c_void) -> u64 {
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "Failed to execute 'createObjectURL' on 'URL': 1 argument required, but only 0 present.");
    }
    let origin = origin_part(g);
    // `(Blob or MediaSource)`: there is no MediaSource here.
    let Some(b) = super::blob::data_of(g, cf_arg(cf, 0)) else {
        return throw_type_error(g, "Failed to execute 'createObjectURL' on 'URL': Overload resolution failed.");
    };
    let entry = BlobEntry { range: b.range(), mime: b.mime.clone() };
    let url = format!("blob:{origin}/{}", uuid_v4());
    let key = store_key(&url).unwrap_or_else(|| url.clone());
    blob_urls().lock().unwrap().entry(g as usize).or_default().urls.insert(key, entry);
    js_string(g, &url)
}

/// `revokeObjectURL(url)`: anything that is not a `blob:` URL of this
/// context's store is ignored, as the spec says — never an error.
unsafe extern "sysv64" fn revoke_object_url(g: *mut c_void, cf: *mut c_void) -> u64 {
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "Failed to execute 'revokeObjectURL' on 'URL': 1 argument required, but only 0 present.");
    }
    let Ok(s) = to_js_string(g, cf_arg(cf, 0)) else { return UNDEF };
    let url = read_string(g, s).unwrap_or_default();
    if let Some(key) = store_key(&url) {
        if let Some(st) = blob_urls().lock().unwrap().get_mut(&(g as usize)) {
            st.urls.remove(&key);
        }
    }
    UNDEF
}

/// This context's entry for `url` (fetch's blob scheme).
pub(crate) fn lookup(g: *mut c_void, url: &str) -> Option<BlobEntry> {
    let key = store_key(url)?;
    blob_urls().lock().unwrap().get(&(g as usize))?.urls.get(&key).cloned()
}

/// A blob URL, resolved for a HOST consumer (vn-tree's `<img>` / `<video>`).
#[derive(Clone, Debug, PartialEq)]
pub enum BlobTarget {
    /// A path-backed Blob or File: load it by path, without a copy. `range`
    /// is `Some((offset, len))` for a slice and `None` for the whole file.
    /// `size` / `mtime` are the snapshot its reads are checked against; see
    /// [`BlobTarget::snapshot_holds`].
    Path { path: std::path::PathBuf, range: Option<(u64, u64)>, size: u64, mtime: Option<std::time::SystemTime>, mime: String },
    /// Bytes in memory: the shared `Arc` and the window onto it.
    Bytes { bytes: std::sync::Arc<Vec<u8>>, offset: usize, len: usize, mime: String },
}

impl BlobTarget {
    pub fn mime(&self) -> &str {
        match self {
            BlobTarget::Path { mime, .. } | BlobTarget::Bytes { mime, .. } => mime,
        }
    }

    /// Is a path-backed target's file still the one its `File` was made from
    /// (same size and mtime)? A browser fails a load whose file changed —
    /// the same rule `text()` applies. Always true for bytes.
    pub fn snapshot_holds(&self) -> bool {
        match self {
            BlobTarget::Path { path, size, mtime, .. } => {
                std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() == *size && m.modified().ok() == *mtime)
            }
            BlobTarget::Bytes { .. } => true,
        }
    }
}

/// Resolves a `blob:` URL against context `g`'s store. `None` for a URL
/// that was never created here, was revoked, or names a Blob that could no
/// longer be read when it was made. Takes no JS lock and runs no JS: the
/// store is plain Rust data.
pub fn resolve_blob_url(g: *mut c_void, url: &str) -> Option<BlobTarget> {
    use super::blob::Source;
    let e = lookup(g, url)?;
    match e.range.source {
        Source::Disk(f) => {
            let whole = e.range.offset == 0 && e.range.len == f.size;
            Some(BlobTarget::Path {
                path: f.path.clone(),
                range: (!whole).then_some((e.range.offset, e.range.len)),
                size: f.size,
                mtime: f.mtime,
                mime: e.mime,
            })
        }
        Source::Memory(bytes) => {
            Some(BlobTarget::Bytes { bytes, offset: e.range.offset as usize, len: e.range.len as usize, mime: e.mime })
        }
        Source::Unreadable(_) => None,
    }
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

/// `GetMethod(v, @@iterator)`: `Ok(None)` for undefined / null, a TypeError
/// for anything else that cannot be called. An exception is left pending.
unsafe fn iterator_method(g: *mut c_void, v: u64) -> Result<Option<u64>, ()> {
    let a = glue::rjs_atom_iterator(g);
    let mut method = UNDEF;
    let rc = glue::rjs_get(g, v, a, &mut method);
    glue::rjs_atom_free(g, a);
    if rc != 0 {
        glue::rjs_throw(g, method);
        return Err(());
    }
    if method == UNDEF || method == NULL {
        return Ok(None);
    }
    if glue::rjs_kind(g, method) != 9 {
        throw_type_error(g, "Failed to construct 'URLSearchParams': @@iterator is not a function.");
        return Err(());
    }
    Ok(Some(method))
}

/// WebIDL `sequence<T>` through the iteration protocol, each element handed
/// to `each` as it comes (so nothing is held where the collector cannot see).
unsafe fn for_each_of(g: *mut c_void, v: u64, method: u64, mut each: impl FnMut(u64) -> Result<(), ()>) -> Result<(), ()> {
    let iter = call(g, method, v, &[]).map_err(|e| {
        glue::rjs_throw(g, e);
    })?;
    if !super::blob::is_object(g, iter) {
        throw_type_error(g, "Result of the Symbol.iterator method is not an object");
        return Err(());
    }
    let next = try_get(g, iter, "next")?;
    loop {
        let r = call(g, next, iter, &[]).map_err(|e| {
            glue::rjs_throw(g, e);
        })?;
        if !super::blob::is_object(g, r) {
            throw_type_error(g, "Iterator result is not an object");
            return Err(());
        }
        if glue::rjs_to_boolean(g, try_get(g, r, "done")?) != 0 {
            return Ok(());
        }
        each(try_get(g, r, "value")?)?;
    }
}

/// ToUSVString, rethrowing (a Symbol, a throwing `toString`).
unsafe fn usv(g: *mut c_void, v: u64) -> Result<String, ()> {
    let s = to_js_string(g, v)?;
    Ok(read_string(g, s).unwrap_or_default())
}

/// URL §6.2 "URLSearchParams(init)": `init` is a
/// `(sequence<sequence<USVString>> or record<USVString, USVString> or
/// USVString)`. An object with `@@iterator` (an array of pairs, a Map, another
/// URLSearchParams) is the sequence, any other object the record, anything
/// else a query string whose leading `?` is dropped.
unsafe fn usp_init(g: *mut c_void, init: u64) -> Result<Vec<(String, String)>, ()> {
    if init == UNDEF {
        return Ok(Vec::new());
    }
    if !super::blob::is_object(g, init) {
        let s = usv(g, init)?;
        return Ok(parse_query(s.strip_prefix('?').unwrap_or(&s)));
    }
    let mut list = Vec::new();
    if let Some(method) = iterator_method(g, init)? {
        for_each_of(g, init, method, |pair| {
            let not_a_sequence = || {
                throw_type_error(
                    g,
                    "Failed to construct 'URLSearchParams': The provided value cannot be converted to a sequence.",
                );
            };
            if !super::blob::is_object(g, pair) {
                not_a_sequence();
                return Err(());
            }
            let Some(inner) = iterator_method(g, pair)? else {
                not_a_sequence();
                return Err(());
            };
            let mut items: Vec<String> = Vec::new();
            for_each_of(g, pair, inner, |item| {
                items.push(usv(g, item)?);
                Ok(())
            })?;
            if items.len() != 2 {
                throw_type_error(
                    g,
                    "Failed to construct 'URLSearchParams': Sequence initializer must only contain pair elements",
                );
                return Err(());
            }
            let value = items.pop().unwrap_or_default();
            let name = items.pop().unwrap_or_default();
            list.push((name, value));
            Ok(())
        })?;
        return Ok(list);
    }
    // The record: own enumerable string keys, in [[OwnPropertyKeys]] order.
    let mut keys = UNDEF;
    if glue::rjs_own_keys(g, init, &mut keys) != 0 || !is_cell(keys) {
        // A Proxy's `ownKeys` trap threw: that exception is the answer.
        if keys != UNDEF {
            glue::rjs_throw(g, keys);
        } else {
            throw_type_error(g, "Failed to construct 'URLSearchParams': the record could not be enumerated.");
        }
        return Err(());
    }
    for i in 0..array_len(g, keys) {
        let k = get_i(g, keys, i);
        let name = usv(g, k)?;
        let value = usv(g, try_get(g, init, &name)?)?;
        list.push((name, value));
    }
    Ok(list)
}

unsafe extern "sysv64" fn usp_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let st = state(g);
    let Ok(list) = usp_init(g, cf_arg(cf, 0)) else { return UNDEF };
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
                _ => revoke_object_url as *const c_void,
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

#[cfg(test)]
mod jsc_tests {
    use super::{resolve_blob_url, BlobTarget};
    use crate::webapi::file::testing::*;
    use crate::webapi::testing::*;

    #[test]
    fn object_urls_have_the_file_api_shape_and_revoke() {
        let c = file_ctx();
        ok(&c, "(() => { const u = URL.createObjectURL(new Blob(['x'])); \
                 return /^blob:app:\\/\\/redpew\\/[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(u) \
                   && u !== URL.createObjectURL(new Blob(['x'])); })()");
        assert_eq!(throws(&c, "URL.createObjectURL({})"), "Failed to execute 'createObjectURL' on 'URL': Overload resolution failed.");
        assert_eq!(throws(&c, "URL.createObjectURL()"), "Failed to execute 'createObjectURL' on 'URL': 1 argument required, but only 0 present.");
        assert_eq!(throws(&c, "URL.revokeObjectURL()"), "Failed to execute 'revokeObjectURL' on 'URL': 1 argument required, but only 0 present.");
        ok(&c, "URL.revokeObjectURL('https://example.com/') === undefined && URL.revokeObjectURL('not a url') === undefined");
        assert_eq!(
            run(&c, "const u = URL.createObjectURL(new Blob(['hello'], { type: 'text/x' })); \
                     const r = await fetch(u + '#frag'); \
                     const t = [r.status, r.statusText, r.ok, r.type, r.url === u, r.headers.get('content-type'), r.headers.get('content-length'), await r.text()].join(); \
                     URL.revokeObjectURL(u + '#other'); \
                     try { await fetch(u); return 'fetched after revoke'; } catch (e) { return t + '|' + e.name; }"),
            "200,OK,true,basic,true,text/x,5,hello|TypeError"
        );
        // An empty type is still a header (Fetch: `Content-Type` is always set).
        assert_eq!(run(&c, "const r = await fetch(URL.createObjectURL(new Blob([]))); return JSON.stringify(r.headers.get('content-type'));"), "\"\"");
        assert!(run(&c, "await fetch(URL.createObjectURL(new Blob(['x'])), { method: 'POST', body: 'y' });").contains("only be fetched with GET"));
    }

    #[test]
    fn a_range_request_gets_206_and_bad_ranges_are_network_errors() {
        let c = file_ctx();
        s(&c, "globalThis.u = URL.createObjectURL(new Blob(['0123456789'], { type: 'a/b' })); 0");
        let r = |range: &str| {
            run(&c, &format!(
                "const r = await fetch(u, {{ headers: {{ Range: '{range}' }} }}); \
                 return [r.status, r.headers.get('content-range'), r.headers.get('content-length'), await r.text()].join();"
            ))
        };
        assert_eq!(r("bytes=2-4"), "206,bytes 2-4/10,3,234");
        assert_eq!(r("bytes=7-"), "206,bytes 7-9/10,3,789");
        assert_eq!(r("bytes=-3"), "206,bytes 7-9/10,3,789");
        assert_eq!(r("bytes=-30"), "206,bytes 0-9/10,10,0123456789");
        assert_eq!(r("bytes=8-100"), "206,bytes 8-9/10,2,89");
        // allowWhitespace: tabs and spaces around `=` and the numbers
        // (WPT fetch/range/blob.any.js "with whitespace": `bytes= 9 - 21`).
        assert_eq!(r("bytes= 1-2"), "206,bytes 1-2/10,2,12");
        assert_eq!(r("bytes \\t= 2 \\t- 4"), "206,bytes 2-4/10,3,234");
        // Trailing whitespace never reaches the parser: a header value is normalized.
        assert_eq!(r("bytes=1-2 "), "206,bytes 1-2/10,2,12");
        // An end too large for any integer is clamped like any end past the Blob;
        // a start that large starts past it.
        assert_eq!(r("bytes=3-99999999999999999999999"), "206,bytes 3-9/10,7,3456789");
        assert_eq!(r("bytes=-99999999999999999999999"), "206,bytes 0-9/10,10,0123456789");
        for bad in [
            "bytes=10-",
            "bytes=5-2",
            "bytes=1-2,4-5",
            "items=1-2",
            "bytes=-",
            "bytes=1 2-3",
            "Bytes=1-2",
            "bytes=99999999999999999999999-",
        ] {
            assert!(r(bad).starts_with("ERROR: TypeError"), "{bad}");
        }
        // Two Range headers combine into a list, which is a network error.
        assert!(run(&c, "const h = new Headers(); h.append('Range', 'bytes=0-1'); h.append('Range', 'bytes=2-3'); \
                         await fetch(u, { headers: h });")
            .starts_with("ERROR: TypeError"));
    }

    #[test]
    fn a_path_backed_blob_url_resolves_by_path_and_fetches_off_thread() {
        let c = file_ctx();
        let t = TempFile::new("pic.png", b"\x89PNG not really");
        t.expose(&c, "picked");
        let whole = s(&c, "globalThis.whole = URL.createObjectURL(picked)");
        let part = s(&c, "globalThis.part = URL.createObjectURL(picked.slice(1, 4))");
        let mem = s(&c, "URL.createObjectURL(new Blob(['mem'], { type: 'text/plain' }))");
        let g = c.raw();
        match resolve_blob_url(g, &whole) {
            Some(BlobTarget::Path { path, range: None, size: 15, mime, .. }) => {
                assert_eq!(path, t.0);
                assert_eq!(mime, "image/png");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(resolve_blob_url(g, &part), Some(BlobTarget::Path { range: Some((1, 3)), .. })));
        match resolve_blob_url(g, &(mem.clone() + "#x")) {
            Some(BlobTarget::Bytes { bytes, offset: 0, len: 3, mime }) => {
                assert_eq!(&bytes[..], b"mem");
                assert_eq!(mime, "text/plain");
            }
            other => panic!("{other:?}"),
        }
        assert!(resolve_blob_url(g, "blob:app://redpew/00000000-0000-4000-8000-000000000000").is_none());
        assert!(resolve_blob_url(g, &whole).unwrap().snapshot_holds());
        assert_eq!(run(&c, "return await (await fetch(part)).text();"), "PNG");
        std::fs::write(&t.0, b"changed").unwrap();
        assert!(!resolve_blob_url(g, &whole).unwrap().snapshot_holds());
        assert!(run(&c, "await fetch(whole);").starts_with("ERROR: TypeError: Failed to fetch: NotReadableError"));
        // An abort that lands while the read runs still wins.
        assert_eq!(
            run(&c, "const ac = new AbortController(); const p = fetch(URL.createObjectURL(new File([picked], 'x')), { signal: ac.signal }); \
                     ac.abort(); try { await p; return 'resolved'; } catch (e) { return e.name; }"),
            "AbortError"
        );
        s(&c, "URL.revokeObjectURL(whole); 0");
        assert!(resolve_blob_url(g, &whole).is_none());
    }

    /// URL §6.2: `init` is a record, a sequence of pairs (an array, a Map,
    /// another URLSearchParams: anything with `@@iterator`) or a string. Until
    /// m26 every object was read as a string, so `new URLSearchParams({ a: 1 })`
    /// was `"%5Bobject+Object%5D="` and a copy of another one was empty.
    #[test]
    fn the_init_is_a_record_a_sequence_of_pairs_or_a_string() {
        let c = ctx();
        let q = |init: &str| s(&c, &format!("new URLSearchParams({init}).toString()"));
        // The record: own enumerable string keys in property order, values
        // stringified.
        assert_eq!(q("{ b: 1, a: 'x y', c: null, d: undefined }"), "b=1&a=x+y&c=null&d=undefined");
        assert_eq!(
            q("Object.defineProperty({ shown: 1 }, 'hidden', { value: 2, enumerable: false })"),
            "shown=1"
        );
        assert_eq!(q("Object.create({ inherited: 1 })"), "");
        // Sequences: an array of pairs, a Map, another URLSearchParams (a copy,
        // not a view), a generator of pairs.
        assert_eq!(q("[['a', '1'], ['a', '2'], ['b', 3]]"), "a=1&a=2&b=3");
        assert_eq!(q("new Map([['k', 'v'], ['x', 'y']])"), "k=v&x=y");
        ok(&c, "(() => { const a = new URLSearchParams('p=1&q=2'); const b = new URLSearchParams(a); \
                 b.append('r', '3'); return a.toString() === 'p=1&q=2' && b.toString() === 'p=1&q=2&r=3'; })()");
        assert_eq!(q("(function* () { yield ['g', '1']; yield new Set(['h', '2']); })()"), "g=1&h=2");
        // Strings: a leading `?` is dropped (once), and anything that is not an
        // object is stringified, `null` included (WebIDL picks USVString).
        assert_eq!(q("'?a=1&b=2'"), "a=1&b=2");
        assert_eq!(q("'??a=1'"), "%3Fa=1");
        assert_eq!(q("null"), "null=");
        assert_eq!(q("undefined"), "");
        assert_eq!(q("42"), "42=");
        // What a browser refuses, refused the same way.
        for bad in ["[['a']]", "[['a', 'b', 'c']]", "[1]", "['ab']"] {
            let m = throws(&c, &format!("new URLSearchParams({bad})"));
            assert!(m.contains("URLSearchParams"), "{bad}: {m}");
        }
        assert_eq!(
            throws(&c, "new URLSearchParams({ [Symbol.iterator]: 5 })"),
            "Failed to construct 'URLSearchParams': @@iterator is not a function."
        );
        // A throwing getter or iterator is the constructor's exception.
        assert_eq!(throws(&c, "new URLSearchParams({ get a() { throw new RangeError('getter'); } })"), "getter");
        assert_eq!(
            throws(&c, "new URLSearchParams(new Proxy({}, { ownKeys() { throw new RangeError('trap'); } }))"),
            "trap"
        );
        assert_eq!(
            throws(&c, "new URLSearchParams({ *[Symbol.iterator]() { throw new RangeError('iter'); } })"),
            "iter"
        );
        ok(&c, "new URLSearchParams({ a: 1 }) instanceof URLSearchParams");
        // The URL a URLSearchParams body makes (fetch / XHR send) is unchanged.
        ok(&c, "new URLSearchParams([['x', '1 2']]).toString() === 'x=1+2'");
    }
}
