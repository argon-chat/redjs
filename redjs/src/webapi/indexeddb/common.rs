//! Small pieces every IDB class uses: brand failures, argument counts,
//! WebIDL conversions (enums, `[EnforceRange] unsigned long`), "convert a
//! value to a key range", the clone-during-transaction step, and the `on*`
//! event-handler accessor generator.

use core::ffi::c_void;

use super::client::*;
use super::key::{self, Bound, Conv, Key, Range};
use super::server::Dir;
use crate::glue;
use crate::webapi::util::*;
use crate::webapi::{domexception, serialize};

pub(super) unsafe fn illegal(g: *mut c_void) -> u64 {
    throw_type_error(g, "Illegal invocation")
}

/// WebIDL's argument-count check; `false` after throwing.
pub(super) unsafe fn args(g: *mut c_void, cf: *mut c_void, n: u32, iface: &str, method: &str) -> bool {
    let got = cf_argc(cf);
    if got >= n {
        return true;
    }
    let plural = if n == 1 { "" } else { "s" };
    throw_type_error(g, &format!("Failed to execute '{method}' on '{iface}': {n} argument{plural} required, but only {got} present."));
    false
}

/// Throws a DOMException `name` with a browser-shaped message.
pub(super) unsafe fn fail(g: *mut c_void, name: &str, iface: &str, method: &str, msg: &str) -> u64 {
    domexception::throw(g, name, &format!("Failed to execute '{method}' on '{iface}': {msg}"))
}

pub(super) unsafe fn is_object(g: *mut c_void, v: u64) -> bool {
    is_cell(v) && glue::rjs_kind(g, v) >= 8
}

/// `DOMString` conversion: ToString (rethrowing), as code units.
pub(super) unsafe fn str16(g: *mut c_void, v: u64) -> Result<Vec<u16>, ()> {
    let s = to_js_string(g, v)?;
    Ok(read_string16(g, s).unwrap_or_default())
}

/// "Convert a value to a key", throwing DataError for an invalid key.
pub(super) unsafe fn conv_key(g: *mut c_void, v: u64, iface: &str, method: &str) -> Result<Key, ()> {
    match key::from_js(g, v)? {
        Conv::Key(k) => Ok(k),
        Conv::Invalid => {
            fail(g, "DataError", iface, method, "The parameter is not a valid key.");
            Err(())
        }
    }
}

/// "Convert a value to a key range" (§7.3): a range as is; undefined/null →
/// everything (or DataError with `null_disallowed`); a key → only it.
pub(super) unsafe fn to_range(g: *mut c_void, v: u64, null_disallowed: bool, iface: &str, method: &str) -> Result<Range, ()> {
    if let Some(r) = range_of(g, v) {
        return Ok(r.range.clone());
    }
    if v == UNDEF || v == NULL {
        if null_disallowed {
            fail(g, "DataError", iface, method, "No key or key range specified.");
            return Err(());
        }
        return Ok(Range::all());
    }
    let k = conv_key(g, v, iface, method)?;
    Ok(Range::only(k.encode()))
}

/// `[EnforceRange] unsigned long`.
pub(super) unsafe fn enforce_u32(g: *mut c_void, v: u64, iface: &str, method: &str) -> Result<u32, ()> {
    let d = to_number(g, v)?;
    let t = d.trunc();
    if !d.is_finite() || !(0.0..=4_294_967_295.0).contains(&t) {
        throw_type_error(g, &format!("Failed to execute '{method}' on '{iface}': Value is outside the 'unsigned long' value range."));
        return Err(());
    }
    Ok(t as u32)
}

/// WebIDL `unsigned long long` (no EnforceRange): modulo 2^64.
pub(super) fn to_u64_modulo(d: f64) -> f64 {
    if !d.is_finite() {
        return 0.0;
    }
    let m = 18_446_744_073_709_551_616.0f64;
    let t = d.trunc() % m;
    if t < 0.0 { t + m } else { t }
}

/// The `IDBCursorDirection` enum; `undefined` = "next".
pub(super) unsafe fn direction(g: *mut c_void, v: u64, iface: &str, method: &str) -> Result<Dir, ()> {
    if v == UNDEF {
        return Ok(Dir::Next);
    }
    let s = str16(g, v)?;
    let s = String::from_utf16_lossy(&s);
    Ok(match s.as_str() {
        "next" => Dir::Next,
        "nextunique" => Dir::NextUnique,
        "prev" => Dir::Prev,
        "prevunique" => Dir::PrevUnique,
        _ => {
            throw_type_error(
                g,
                &format!("Failed to execute '{method}' on '{iface}': The provided value '{s}' is not a valid enum value of type IDBCursorDirection."),
            );
            return Err(());
        }
    })
}

pub(super) fn dir_str(d: Dir) -> &'static str {
    match d {
        Dir::Next => "next",
        Dir::NextUnique => "nextunique",
        Dir::Prev => "prev",
        Dir::PrevUnique => "prevunique",
    }
}

/// "A clone of value … during transaction": the transaction is inactive
/// while the serializer runs (a getter that issues a request gets
/// TransactionInactiveError — WPT structured-clone-transaction-state).
pub(super) unsafe fn clone_for(g: *mut c_void, tx: u64, value: u64, iface: &str, method: &str) -> Result<Vec<u8>, ()> {
    let prev = match tx_of(g, tx) {
        Some(td) => {
            let p = td.state;
            td.state = TxState::Inactive;
            p
        }
        None => return Err(()),
    };
    let r = serialize::serialize(g, value, &serialize::Options { for_storage: true, transfer: &[] });
    let still = match tx_of(g, tx) {
        Some(td) if td.state == TxState::Inactive => {
            td.state = prev;
            true
        }
        _ => false,
    };
    match r {
        Ok(s) if still => Ok(s.bytes),
        Ok(_) => {
            fail(g, "TransactionInactiveError", iface, method, "The transaction finished while the value was being cloned.");
            Err(())
        }
        Err(e) => {
            glue::rjs_throw(g, e);
            Err(())
        }
    }
}

pub(super) fn bound_open(b: &Bound) -> bool {
    matches!(b, Bound::Excl(_))
}

/// Defines an `on<type>` IDL attribute pair over the object's bag, brand
/// checked with `$check` (WebIDL EventHandler: a non-object stores null).
macro_rules! handler {
    ($get:ident, $set:ident, $name:literal, $check:expr) => {
        pub(super) unsafe extern "sysv64" fn $get(g: *mut core::ffi::c_void, cf: *mut core::ffi::c_void) -> u64 {
            let this = crate::webapi::util::cf_this(cf);
            if !$check(g, this) {
                return super::common::illegal(g);
            }
            let v = super::client::bget(g, this, $name);
            if v == crate::webapi::util::UNDEF { crate::webapi::util::NULL } else { v }
        }
        pub(super) unsafe extern "sysv64" fn $set(g: *mut core::ffi::c_void, cf: *mut core::ffi::c_void) -> u64 {
            let this = crate::webapi::util::cf_this(cf);
            if !$check(g, this) {
                return super::common::illegal(g);
            }
            let v = crate::webapi::util::cf_arg(cf, 0);
            let v = if super::common::is_object(g, v) { v } else { crate::webapi::util::NULL };
            super::client::bset(g, this, $name, v);
            crate::webapi::util::UNDEF
        }
    };
}
pub(super) use handler;
