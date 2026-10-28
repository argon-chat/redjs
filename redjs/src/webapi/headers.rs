//! `Headers` (WHATWG Fetch §"Headers class").
//!
//! Split out of `fetch.rs` when `fetch` grew a real network transport, because
//! the old implementation was not a subset — it was a **trap**. `get`, `has`
//! and `forEach` worked; `set`, `append` and `delete` were bound to a `noop`
//! host function. Code that did
//!
//! ```js
//! const h = new Headers();
//! h.set('Authorization', `Bearer ${token}`);
//! await fetch(url, { headers: h });
//! ```
//!
//! got a `Headers` object that reported nothing, threw nothing, and sent no
//! credential. The constructor was no better: it read exactly one key,
//! `content-type`, and dropped every other entry of a record initializer on the
//! floor. Both are the same defect class as the status-0 `Response` this epic
//! exists to remove — a success-shaped answer to a request that did not happen.
//!
//! This is the real thing: a mutable header list with spec name/value
//! validation, combining `get`, sorted iteration, and `getSetCookie`.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::util::*;
use crate::capi;
use crate::glue;

pub struct HeadersData {
    /// Insertion order, names lower-cased. Spec ordering is applied at
    /// iteration time, not here, so `getSetCookie` can stay faithful.
    pub list: Vec<(String, String)>,
}

struct HeadersClass {
    cls: capi::JSClassRef,
    class: Class,
}

unsafe impl Send for HeadersClass {}
unsafe impl Sync for HeadersClass {}

fn registry() -> &'static Mutex<HashMap<usize, &'static HeadersClass>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static HeadersClass>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> &'static HeadersClass {
    registry().lock().unwrap().get(&(g as usize)).copied().expect("webapi::headers not installed")
}

pub(crate) fn forget(gone: crate::context::Gone) {
    registry().lock().unwrap().remove(&gone.key);
}

unsafe fn this_data<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut HeadersData> {
    host_data::<HeadersData>(g, classes(g).cls, cf_this(cf))
}

/// A `Headers` object over `list`.
pub unsafe fn create(g: *mut c_void, list: Vec<(String, String)>) -> u64 {
    let c = classes(g);
    host_create(g, c.cls, c.class.proto, HeadersData { list })
}

/// The header list of a `Headers` instance; `None` for anything else.
pub unsafe fn list_of(g: *mut c_void, v: u64) -> Option<Vec<(String, String)>> {
    host_data::<HeadersData>(g, classes(g).cls, v).map(|d| d.list.clone())
}

// ---------------------------------------------------------------------------
// Validation (Fetch §"header name" / §"header value")
// ---------------------------------------------------------------------------

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(is_token_byte)
}

pub fn valid_value(value: &str) -> bool {
    !value.bytes().any(|b| b == 0 || b == b'\r' || b == b'\n')
}

/// Spec: values are stripped of leading/trailing HTTP whitespace on the way in.
fn normalize_value(value: &str) -> String {
    value.trim_matches(|c| c == ' ' || c == '\t' || c == '\r' || c == '\n').to_string()
}

/// Reads a `HeadersInit`: a `Headers` instance, a sequence of `[name, value]`
/// pairs, or a record. Every entry is validated; a bad one is an error, never
/// a skipped header.
pub unsafe fn from_init(g: *mut c_void, v: u64) -> Result<Vec<(String, String)>, String> {
    if v == UNDEF || v == NULL {
        return Ok(Vec::new());
    }
    if !is_cell(v) {
        return Err("headers init must be a Headers, a sequence of pairs, or a record".into());
    }
    if let Some(list) = list_of(g, v) {
        return Ok(list);
    }

    let mut out: Vec<(String, String)> = Vec::new();

    // Sequence form: anything with a numeric `length` whose entries are pairs.
    let len_v = get(g, v, "length");
    if len_v != UNDEF {
        let n = array_len(g, v);
        for i in 0..n {
            let pair = get_i(g, v, i);
            if !is_cell(pair) || array_len(g, pair) != 2 {
                return Err(format!("headers init entry {i} is not a [name, value] pair"));
            }
            let name = read_string(g, get_i(g, pair, 0)).unwrap_or_default();
            let value = read_string(g, get_i(g, pair, 1)).unwrap_or_default();
            push_validated(&mut out, &name, &value)?;
        }
        return Ok(out);
    }

    // Record form — every own enumerable key, not just the one we used to read.
    let mut keys = UNDEF;
    if glue::rjs_own_keys(g, v, &mut keys) != 0 || !is_cell(keys) {
        return Err("headers init record could not be enumerated".into());
    }
    let n = array_len(g, keys);
    for i in 0..n {
        let k = get_i(g, keys, i);
        let Some(name) = read_string(g, k) else { continue };
        let value = read_string(g, get(g, v, &name)).unwrap_or_default();
        push_validated(&mut out, &name, &value)?;
    }
    Ok(out)
}

fn push_validated(out: &mut Vec<(String, String)>, name: &str, value: &str) -> Result<(), String> {
    let value = normalize_value(value);
    if !valid_name(name) {
        return Err(format!("{name:?} is not a valid header name"));
    }
    if !valid_value(&value) {
        return Err(format!("{value:?} is not a valid value for header {name:?}"));
    }
    out.push((name.to_ascii_lowercase(), value));
    Ok(())
}

/// Spec iteration order: sorted by name, same-name values combined with `", "`,
/// except `set-cookie`, which stays one entry per value.
pub fn sorted_combined(list: &[(String, String)]) -> Vec<(String, String)> {
    let mut names: Vec<String> = Vec::new();
    for (k, _) in list {
        if !names.iter().any(|n| n == k) {
            names.push(k.clone());
        }
    }
    names.sort();
    let mut out = Vec::new();
    for name in names {
        if name == "set-cookie" {
            for (k, v) in list.iter().filter(|(k, _)| *k == name) {
                out.push((k.clone(), v.clone()));
            }
            continue;
        }
        let joined: Vec<&str> =
            list.iter().filter(|(k, _)| *k == name).map(|(_, v)| v.as_str()).collect();
        out.push((name, joined.join(", ")));
    }
    out
}

// ---------------------------------------------------------------------------
// Prototype
// ---------------------------------------------------------------------------

unsafe fn arg_name(g: *mut c_void, cf: *mut c_void, i: u32) -> Option<String> {
    let s = read_string(g, cf_arg(cf, i))?;
    valid_name(&s).then(|| s.to_ascii_lowercase())
}

unsafe extern "sysv64" fn h_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let list = match from_init(g, cf_arg(cf, 0)) {
        Ok(l) => l,
        Err(m) => return throw_type_error(g, &format!("new Headers: {m}")),
    };
    let c = classes(g);
    let proto = {
        let nt = cf_new_target(cf);
        let p = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
        if is_cell(p) { p } else { c.class.proto }
    };
    host_create(g, c.cls, proto, HeadersData { list })
}

unsafe extern "sysv64" fn h_get(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(name) = arg_name(g, cf, 0) else {
        return throw_type_error(g, "Headers.get: invalid header name");
    };
    let Some(d) = this_data(g, cf) else {
        return throw_type_error(g, "Headers.get called on a non-Headers");
    };
    let values: Vec<&str> = d.list.iter().filter(|(k, _)| *k == name).map(|(_, v)| v.as_str()).collect();
    if values.is_empty() {
        return NULL;
    }
    let joined = values.join(", ");
    js_string(g, &joined)
}

unsafe extern "sysv64" fn h_get_set_cookie(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(d) = this_data(g, cf) else {
        return throw_type_error(g, "Headers.getSetCookie called on a non-Headers");
    };
    let values: Vec<String> =
        d.list.iter().filter(|(k, _)| k == "set-cookie").map(|(_, v)| v.clone()).collect();
    let arr = new_array(g);
    for (i, v) in values.iter().enumerate() {
        set_i(g, arr, i as u32, js_string(g, v));
    }
    arr
}

unsafe extern "sysv64" fn h_has(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(name) = arg_name(g, cf, 0) else {
        return throw_type_error(g, "Headers.has: invalid header name");
    };
    match this_data(g, cf) {
        Some(d) => enc_bool(d.list.iter().any(|(k, _)| *k == name)),
        None => throw_type_error(g, "Headers.has called on a non-Headers"),
    }
}

unsafe extern "sysv64" fn h_set(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(name) = arg_name(g, cf, 0) else {
        return throw_type_error(g, "Headers.set: invalid header name");
    };
    let value = normalize_value(&read_string(g, cf_arg(cf, 1)).unwrap_or_default());
    if !valid_value(&value) {
        return throw_type_error(g, &format!("Headers.set: {value:?} is not a valid header value"));
    }
    let Some(d) = this_data(g, cf) else {
        return throw_type_error(g, "Headers.set called on a non-Headers");
    };
    d.list.retain(|(k, _)| *k != name);
    d.list.push((name, value));
    UNDEF
}

unsafe extern "sysv64" fn h_append(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(name) = arg_name(g, cf, 0) else {
        return throw_type_error(g, "Headers.append: invalid header name");
    };
    let value = normalize_value(&read_string(g, cf_arg(cf, 1)).unwrap_or_default());
    if !valid_value(&value) {
        return throw_type_error(g, &format!("Headers.append: {value:?} is not a valid header value"));
    }
    let Some(d) = this_data(g, cf) else {
        return throw_type_error(g, "Headers.append called on a non-Headers");
    };
    d.list.push((name, value));
    UNDEF
}

unsafe extern "sysv64" fn h_delete(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(name) = arg_name(g, cf, 0) else {
        return throw_type_error(g, "Headers.delete: invalid header name");
    };
    let Some(d) = this_data(g, cf) else {
        return throw_type_error(g, "Headers.delete called on a non-Headers");
    };
    d.list.retain(|(k, _)| *k != name);
    UNDEF
}

unsafe extern "sysv64" fn h_for_each(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cb = cf_arg(cf, 0);
    let this_v = cf_this(cf);
    let Some(d) = this_data(g, cf) else {
        return throw_type_error(g, "Headers.forEach called on a non-Headers");
    };
    for (k, v) in sorted_combined(&d.list) {
        let kv = js_string(g, &k);
        let vv = js_string(g, &v);
        if call(g, cb, cf_arg(cf, 1), &[vv, kv, this_v]).is_err() {
            break;
        }
    }
    UNDEF
}

/// `entries` / `keys` / `values` — a snapshot array's own iterator, which is
/// exactly the observable behaviour (Headers iteration is a sorted snapshot).
unsafe fn iterate(g: *mut c_void, cf: *mut c_void, what: u8, who: &str) -> u64 {
    let Some(d) = this_data(g, cf) else {
        return throw_type_error(g, &format!("Headers.{who} called on a non-Headers"));
    };
    let entries = sorted_combined(&d.list);
    let arr = new_array(g);
    for (i, (k, v)) in entries.iter().enumerate() {
        let item = match what {
            0 => {
                let pair = new_array(g);
                set_i(g, pair, 0, js_string(g, k));
                set_i(g, pair, 1, js_string(g, v));
                pair
            }
            1 => js_string(g, k),
            _ => js_string(g, v),
        };
        set_i(g, arr, i as u32, item);
    }
    let a = glue::rjs_atom_iterator(g);
    let f = get_a(g, arr, a);
    match call(g, f, arr, &[]) {
        Ok(it) => it,
        Err(_) => UNDEF,
    }
}

unsafe extern "sysv64" fn h_entries(g: *mut c_void, cf: *mut c_void) -> u64 {
    iterate(g, cf, 0, "entries")
}
unsafe extern "sysv64" fn h_keys(g: *mut c_void, cf: *mut c_void) -> u64 {
    iterate(g, cf, 1, "keys")
}
unsafe extern "sysv64" fn h_values(g: *mut c_void, cf: *mut c_void) -> u64 {
    iterate(g, cf, 2, "values")
}

pub unsafe fn install(g: *mut c_void) {
    let cls = make_host_class::<HeadersData>("Headers");
    let class = define_class(g, &ClassSpec {
        name: "Headers",
        arity: 0,
        construct: Some(h_construct),
        parent_proto: 0,
        methods: &[
            ("get", 1, h_get),
            ("getSetCookie", 0, h_get_set_cookie),
            ("has", 1, h_has),
            ("set", 2, h_set),
            ("append", 2, h_append),
            ("delete", 1, h_delete),
            ("forEach", 1, h_for_each),
            ("entries", 0, h_entries),
            ("keys", 0, h_keys),
            ("values", 0, h_values),
        ],
        getters: &[],
        accessors: &[],
    });

    // `for (const [k, v] of headers)`.
    let a = glue::rjs_atom_iterator(g);
    let f = make_fn(g, "[Symbol.iterator]", 0, h_entries);
    glue::rjs_define(g, class.proto, a, f, 2);

    let boxed: &'static HeadersClass = Box::leak(Box::new(HeadersClass { cls, class }));
    registry().lock().unwrap().insert(g as usize, boxed);
}

// ---------------------------------------------------------------------------
// Tests — the list algebra, no JS context needed
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iteration_is_sorted_and_combines_repeats() {
        let list = vec![
            ("x-b".to_string(), "2".to_string()),
            ("x-a".to_string(), "1".to_string()),
            ("x-b".to_string(), "3".to_string()),
        ];
        assert_eq!(
            sorted_combined(&list),
            vec![("x-a".to_string(), "1".to_string()), ("x-b".to_string(), "2, 3".to_string())]
        );
    }

    #[test]
    fn set_cookie_is_never_combined() {
        // Joining two Set-Cookie values with ", " produces one header that no
        // cookie parser can split back apart.
        let list = vec![
            ("set-cookie".to_string(), "a=1; Path=/".to_string()),
            ("set-cookie".to_string(), "b=2; Path=/".to_string()),
        ];
        let out = sorted_combined(&list);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].1, "a=1; Path=/");
        assert_eq!(out[1].1, "b=2; Path=/");
    }

    #[test]
    fn names_and_values_are_validated() {
        assert!(valid_name("Content-Type"));
        assert!(valid_name("X_custom"));
        assert!(!valid_name(""));
        assert!(!valid_name("bad name"));
        assert!(!valid_name("bad:name"));
        assert!(!valid_name("bad\r\nname"));

        assert!(valid_value("application/json"));
        assert!(!valid_value("a\r\nX-Injected: 1"));
        assert!(!valid_value("a\nb"));
        assert!(!valid_value("a\0b"));
    }

    #[test]
    fn values_are_trimmed_on_the_way_in() {
        let mut out = Vec::new();
        push_validated(&mut out, "X-A", "  spaced  ").unwrap();
        assert_eq!(out, vec![("x-a".to_string(), "spaced".to_string())]);
    }

    #[test]
    fn an_invalid_entry_is_an_error_rather_than_a_dropped_header() {
        let mut out = Vec::new();
        assert!(push_validated(&mut out, "X Bad", "1").is_err());
        assert!(push_validated(&mut out, "X-Ok", "a\r\nInjected: 1").is_err());
        assert!(out.is_empty());
    }
}
