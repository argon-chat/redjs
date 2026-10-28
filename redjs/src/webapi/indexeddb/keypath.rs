//! Key paths (§2.6): the WebIDL conversion, validity, and the way back to JS.
//! Evaluation lives in [`super::svalue`], over the serialized clone.

use core::ffi::c_void;

use crate::glue;
use crate::webapi::util::*;

/// A valid key path: a (possibly empty, possibly dotted) string, or a
/// non-empty list of such strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyPath {
    String(Vec<u16>),
    List(Vec<Vec<u16>>),
}

impl KeyPath {
    pub fn is_list(&self) -> bool {
        matches!(self, KeyPath::List(_))
    }

    pub fn is_empty_string(&self) -> bool {
        matches!(self, KeyPath::String(s) if s.is_empty())
    }
}

/// `(DOMString or sequence<DOMString>)`: an object with `@@iterator` is a
/// sequence (each item `ToString`'d), anything else is `ToString`'d.
/// `Err(())` = an exception is pending.
pub unsafe fn from_js(g: *mut c_void, v: u64) -> Result<KeyPath, ()> {
    if is_cell(v) && glue::rjs_kind(g, v) >= 8 {
        let it_atom = glue::rjs_atom_wellknown(g, 0);
        let mut method = UNDEF;
        if glue::rjs_get(g, v, it_atom, &mut method) != 0 {
            glue::rjs_throw(g, method);
            return Err(());
        }
        if method != UNDEF && method != NULL {
            let items = iterate(g, v, method)?;
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let s = to_js_string(g, item)?;
                out.push(read_string16(g, s).unwrap_or_default());
            }
            return Ok(KeyPath::List(out));
        }
    }
    let s = to_js_string(g, v)?;
    Ok(KeyPath::String(read_string16(g, s).unwrap_or_default()))
}

/// The iterator protocol over `obj` with `method` (= `obj[@@iterator]`).
pub unsafe fn iterate(g: *mut c_void, obj: u64, method: u64) -> Result<Vec<u64>, ()> {
    if !is_cell(method) || glue::rjs_kind(g, method) != 9 {
        throw_type_error(g, "The provided value cannot be converted to a sequence.");
        return Err(());
    }
    let iter = call(g, method, obj, &[]).map_err(|e| glue::rjs_throw(g, e))?;
    if !is_cell(iter) || glue::rjs_kind(g, iter) < 8 {
        throw_type_error(g, "Result of the Symbol.iterator method is not an object");
        return Err(());
    }
    let next = try_get(g, iter, "next")?;
    let mut out = Vec::new();
    loop {
        let r = call(g, next, iter, &[]).map_err(|e| glue::rjs_throw(g, e))?;
        if !is_cell(r) || glue::rjs_kind(g, r) < 8 {
            throw_type_error(g, "Iterator result is not an object");
            return Err(());
        }
        if glue::rjs_to_boolean(g, try_get(g, r, "done")?) != 0 {
            break;
        }
        let item = try_get(g, r, "value")?;
        if is_cell(item) {
            crate::ffi::redjs_protect(g, item);
        }
        out.push(item);
    }
    for &i in &out {
        if is_cell(i) {
            crate::ffi::redjs_unprotect(g, i);
        }
    }
    Ok(out)
}

/// Is every identifier an ECMAScript IdentifierName? Checked by the engine's
/// own Unicode tables: a `u`-flag RegExp over ID_Start / ID_Continue, run
/// through the `RegExp.prototype.exec` captured at install (so a later
/// override cannot reach in).
pub unsafe fn valid(g: *mut c_void, kp: &KeyPath) -> bool {
    match kp {
        KeyPath::String(s) => valid_string(g, s),
        KeyPath::List(items) => !items.is_empty() && items.iter().all(|s| valid_string(g, s)),
    }
}

unsafe fn valid_string(g: *mut c_void, s: &[u16]) -> bool {
    if s.is_empty() {
        return true;
    }
    s.split(|&c| c == b'.' as u16).all(|ident| valid_identifier(g, ident))
}

unsafe fn valid_identifier(g: *mut c_void, ident: &[u16]) -> bool {
    if ident.is_empty() {
        return false;
    }
    // ASCII fast path: the overwhelmingly common case.
    if ident.iter().all(|&c| c < 0x80) {
        let ok_start = |c: u16| (c as u8).is_ascii_alphabetic() || c == b'$' as u16 || c == b'_' as u16;
        let ok_part = |c: u16| ok_start(c) || (c as u8).is_ascii_digit();
        return ok_start(ident[0]) && ident[1..].iter().all(|&c| ok_part(c));
    }
    let Some((re, exec)) = super::intrinsics(g).map(|i| (i.ident_re, i.regexp_exec)) else { return false };
    let s = js_string16(g, ident);
    match call(g, exec, re, &[s]) {
        Ok(m) => m != NULL && m != UNDEF,
        Err(_) => false,
    }
}

/// The value the `keyPath` getters return.
pub unsafe fn to_js(g: *mut c_void, kp: &KeyPath) -> u64 {
    match kp {
        KeyPath::String(s) => js_string16(g, s),
        KeyPath::List(items) => {
            let arr = new_array(g);
            for (i, s) in items.iter().enumerate() {
                glue::rjs_define_index(g, arr, i as u32, js_string16(g, s));
            }
            arr
        }
    }
}

/// The source of the identifier RegExp the install step compiles.
pub const IDENT_RE: &str = r"^[$_\p{ID_Start}][$‌‍\p{ID_Continue}]*$";
