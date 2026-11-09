//! `DOMException` and its one derived interface, `QuotaExceededError`
//! (WebIDL §2.8 / §3.14; whatwg/webidl#1465 for the derived class).
//!
//! # Shape, exactly as a browser has it
//!
//! - `new DOMException(message = "", name = "Error")`; `name`, `message` and
//!   `code` are getters on `DOMException.prototype` (enumerable, configurable,
//!   brand-checked — a non-DOMException `this` is a TypeError).
//! - `code` is the legacy code from the names table for `name`, else 0.
//! - The 25 legacy constants (`INDEX_SIZE_ERR` … `DATA_CLONE_ERR`) sit on both
//!   the interface object and the prototype, read-only, enumerable, permanent.
//! - `DOMException.prototype.__proto__ === Error.prototype`, and
//!   `@@toStringTag` is `"DOMException"`.
//! - Instances are REAL error objects (JSC `ErrorInstance`s, re-prototyped), so
//!   they carry a `stack` from where they were made and `Error.isError` holds —
//!   HTML gives DOMException objects an [[ErrorData]] slot for the same reason.
//! - `QuotaExceededError(message = "", { quota, requested })`: `name` is
//!   `"QuotaExceededError"`, `code` 22, `quota`/`requested` are `number | null`
//!   getters on its own prototype. Negative values, or `requested < quota`, are
//!   a RangeError; a non-finite one is a TypeError (`double`, not
//!   `unrestricted double`). Prototype chain → `DOMException.prototype`,
//!   constructor chain → `DOMException`.
//!
//! # Native callers
//!
//! [`create`] / [`throw`] / [`rejected`] are what every redjs throw site that
//! the spec spells "throw a `FooError` DOMException" goes through; [`parts`] is
//! the serializer's read of the internal slots. Before this module existed the
//! tree's convention was a plain `Error` with a `name` — the fallback these
//! still use for a context that never installed the web API.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::util::*;
use crate::capi;
use crate::glue;

/// WebIDL's DOMException names table — the entries that carry a legacy code.
/// Every other name (DataError, NotAllowedError, ConstraintError, …) is code 0.
const CODES: &[(&str, u16)] = &[
    ("IndexSizeError", 1),
    ("HierarchyRequestError", 3),
    ("WrongDocumentError", 4),
    ("InvalidCharacterError", 5),
    ("NoModificationAllowedError", 7),
    ("NotFoundError", 8),
    ("NotSupportedError", 9),
    ("InUseAttributeError", 10),
    ("InvalidStateError", 11),
    ("SyntaxError", 12),
    ("InvalidModificationError", 13),
    ("NamespaceError", 14),
    ("InvalidAccessError", 15),
    ("TypeMismatchError", 17),
    ("SecurityError", 18),
    ("NetworkError", 19),
    ("AbortError", 20),
    ("URLMismatchError", 21),
    ("QuotaExceededError", 22),
    ("TimeoutError", 23),
    ("InvalidNodeTypeError", 24),
    ("DataCloneError", 25),
];

/// The legacy constants, on the interface object AND its prototype.
const CONSTANTS: &[(&str, u16)] = &[
    ("INDEX_SIZE_ERR", 1),
    ("DOMSTRING_SIZE_ERR", 2),
    ("HIERARCHY_REQUEST_ERR", 3),
    ("WRONG_DOCUMENT_ERR", 4),
    ("INVALID_CHARACTER_ERR", 5),
    ("NO_DATA_ALLOWED_ERR", 6),
    ("NO_MODIFICATION_ALLOWED_ERR", 7),
    ("NOT_FOUND_ERR", 8),
    ("NOT_SUPPORTED_ERR", 9),
    ("INUSE_ATTRIBUTE_ERR", 10),
    ("INVALID_STATE_ERR", 11),
    ("SYNTAX_ERR", 12),
    ("INVALID_MODIFICATION_ERR", 13),
    ("NAMESPACE_ERR", 14),
    ("INVALID_ACCESS_ERR", 15),
    ("VALIDATION_ERR", 16),
    ("TYPE_MISMATCH_ERR", 17),
    ("SECURITY_ERR", 18),
    ("NETWORK_ERR", 19),
    ("ABORT_ERR", 20),
    ("URL_MISMATCH_ERR", 21),
    ("QUOTA_EXCEEDED_ERR", 22),
    ("TIMEOUT_ERR", 23),
    ("INVALID_NODE_TYPE_ERR", 24),
    ("DATA_CLONE_ERR", 25),
];

/// The legacy code for `name`, or 0 (the `code` getter's steps).
pub fn code_for(name: &str) -> u16 {
    CODES.iter().find(|(n, _)| *n == name).map(|(_, c)| *c).unwrap_or(0)
}

struct DomClasses {
    dom: Class,
    quota: Class,
    /// Private slot bag: { name, message, code[, quota, requested] }.
    s_dom: *mut c_void,
}

unsafe impl Send for DomClasses {}
unsafe impl Sync for DomClasses {}

fn registry() -> &'static Mutex<HashMap<usize, &'static DomClasses>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static DomClasses>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> Option<&'static DomClasses> {
    registry().lock().unwrap().get(&(g as usize)).copied()
}

pub(crate) fn forget(gone: crate::context::Gone) {
    registry().lock().unwrap().remove(&gone.key);
}

/// What a DOMException carries — its internal slots, as plain Rust data.
#[derive(Clone, Debug, PartialEq)]
pub struct Parts {
    pub name: String,
    pub message: String,
    /// `Some` exactly for a `QuotaExceededError` instance: `(quota, requested)`.
    pub quota: Option<(Option<f64>, Option<f64>)>,
}

/// The instance itself: a fresh `ErrorInstance` (so it has a `stack` and
/// [[ErrorData]]) moved onto `proto`, with the slot bag attached. No own
/// `message` — that is the prototype getter's job.
unsafe fn make(g: *mut c_void, c: &DomClasses, proto: u64, name: u64, message: u64, quota: Option<(u64, u64)>) -> u64 {
    let mut err = UNDEF;
    glue::rjs_error_create(g, 0, core::ptr::null(), 0, &mut err);
    capi::JSObjectSetPrototype(g, err as usize as capi::JSObjectRef, proto as usize as capi::JSValueRef);
    let slots = new_object(g, NULL);
    define(g, slots, "name", name, 0);
    define(g, slots, "message", message, 0);
    let code = read_string(g, name).map(|n| code_for(&n)).unwrap_or(0);
    define(g, slots, "code", enc_i32(code as i32), 0);
    if let Some((q, r)) = quota {
        define(g, slots, "quota", q, 0);
        define(g, slots, "requested", r, 0);
    }
    glue::rjs_define(g, err, c.s_dom, slots, 1 | 2 | 4);
    err
}

/// A plain `Error` named `name` — what a context without the web API gets.
unsafe fn fallback(g: *mut c_void, name: &str, msg: &str) -> u64 {
    let m: Vec<u16> = msg.encode_utf16().collect();
    let mut err = UNDEF;
    glue::rjs_error_create(g, 0, m.as_ptr(), m.len(), &mut err);
    if is_cell(err) {
        define(g, err, "name", js_string(g, name), 2);
    }
    err
}

/// A new `DOMException` with this name and message — the native half of
/// "throw an `X` DOMException". `name` is taken as given: the names table only
/// decides `code`.
pub unsafe fn create(g: *mut c_void, name: &str, msg: &str) -> u64 {
    let Some(c) = classes(g) else { return fallback(g, name, msg) };
    make(g, c, c.dom.proto, js_string(g, name), js_string(g, msg), None)
}

/// A new `QuotaExceededError` instance (the derived interface, not a
/// DOMException merely *named* that).
pub unsafe fn create_quota_exceeded(g: *mut c_void, msg: &str, quota: Option<f64>, requested: Option<f64>) -> u64 {
    let Some(c) = classes(g) else {
        let e = fallback(g, "QuotaExceededError", msg);
        define(g, e, "code", enc_i32(22), 2);
        return e;
    };
    let num = |v: Option<f64>| v.map(enc_f64).unwrap_or(NULL);
    let name = js_string(g, "QuotaExceededError");
    make(g, c, c.quota.proto, name, js_string(g, msg), Some((num(quota), num(requested))))
}

/// Throws a new DOMException; returns `undefined` for a host function's return.
pub unsafe fn throw(g: *mut c_void, name: &str, msg: &str) -> u64 {
    glue::rjs_throw(g, create(g, name, msg));
    UNDEF
}

/// A promise already rejected with a new DOMException.
pub unsafe fn rejected(g: *mut c_void, name: &str, msg: &str) -> u64 {
    rejected_with(g, create(g, name, msg))
}

/// A promise already rejected with `reason`.
pub unsafe fn rejected_with(g: *mut c_void, reason: u64) -> u64 {
    let (mut res, mut rej, mut p) = (0u64, 0u64, 0u64);
    if crate::ffi::redjs_promise_create(g, &mut res, &mut rej, &mut p) != 0 {
        return UNDEF;
    }
    let _ = call(g, rej, UNDEF, &[reason]);
    p
}

/// The internal slots of a DOMException (or QuotaExceededError) instance;
/// `None` for anything else — including an object that merely inherits from
/// `DOMException.prototype`.
pub unsafe fn parts(g: *mut c_void, v: u64) -> Option<Parts> {
    let c = classes(g)?;
    let slots = slot_of(g, c, v)?;
    let name = read_string(g, get(g, slots, "name")).unwrap_or_default();
    let message = read_string(g, get(g, slots, "message")).unwrap_or_default();
    let quota = if is_quota(g, c, v) {
        let n = |k: &str| dec_f64(get(g, slots, k));
        Some((n("quota"), n("requested")))
    } else {
        None
    };
    Some(Parts { name, message, quota })
}

/// Rebuilds an instance from [`Parts`].
pub unsafe fn from_parts(g: *mut c_void, p: &Parts) -> u64 {
    match p.quota {
        Some((q, r)) => create_quota_exceeded(g, &p.message, q, r),
        None => create(g, &p.name, &p.message),
    }
}

/// The internal slots as the JS values they hold — `(name, message,
/// Some((quota, requested)))` for a QuotaExceededError, each of those a
/// number or `null`. The serializer's read: strings stay exact (a lone
/// surrogate in a message survives, which [`parts`]' `String` would not).
pub(crate) unsafe fn raw_parts(g: *mut c_void, v: u64) -> Option<(u64, u64, Option<(u64, u64)>)> {
    let c = classes(g)?;
    let slots = slot_of(g, c, v)?;
    let quota = if is_quota(g, c, v) { Some((get(g, slots, "quota"), get(g, slots, "requested"))) } else { None };
    Some((get(g, slots, "name"), get(g, slots, "message"), quota))
}

/// The serializer's write: a DOMException (or, with `quota`, a
/// QuotaExceededError) from JS strings and numbers-or-null. `None` before
/// install.
pub(crate) unsafe fn from_raw(g: *mut c_void, name: u64, message: u64, quota: Option<(u64, u64)>) -> Option<u64> {
    let c = classes(g)?;
    let proto = if quota.is_some() { c.quota.proto } else { c.dom.proto };
    Some(make(g, c, proto, name, message, quota))
}

unsafe fn slot_of(g: *mut c_void, c: &DomClasses, v: u64) -> Option<u64> {
    if !is_cell(v) {
        return None;
    }
    let s = get_a(g, v, c.s_dom);
    is_cell(s).then_some(s)
}

/// A QuotaExceededError's bag has `quota` (a number or null); a plain
/// DOMException's has no such key at all.
unsafe fn is_quota(g: *mut c_void, c: &DomClasses, v: u64) -> bool {
    slot_of(g, c, v).is_some_and(|s| get(g, s, "quota") != UNDEF)
}

// ---- JS surface -------------------------------------------------------------

/// WebIDL DOMString conversion of an optional argument: `undefined` → default,
/// anything else ToString (whose exception propagates). `Err` = thrown.
unsafe fn arg_string(g: *mut c_void, v: u64, default: &str) -> Result<u64, ()> {
    if v == UNDEF {
        return Ok(js_string(g, default));
    }
    to_js_string(g, v)
}

/// OrdinaryCreateFromConstructor's prototype: new.target's, else the default.
unsafe fn proto_from(g: *mut c_void, new_target: u64, fallback: u64) -> Result<u64, ()> {
    if !is_cell(new_target) {
        return Ok(fallback);
    }
    let p = try_get(g, new_target, "prototype")?;
    Ok(if is_cell(p) { p } else { fallback })
}

#[crate::host_fn]
unsafe fn dom_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(c) = classes(g) else { return illegal_constructor(g, cf) };
    let Ok(message) = arg_string(g, cf_arg(cf, 0), "") else { return UNDEF };
    let Ok(name) = arg_string(g, cf_arg(cf, 1), "Error") else { return UNDEF };
    let Ok(proto) = proto_from(g, cf_new_target(cf), c.dom.proto) else { return UNDEF };
    make(g, c, proto, name, message, None)
}

/// One `double` member of `QuotaExceededErrorOptions`: absent → None,
/// non-finite → TypeError (the IDL conversion), else the value.
unsafe fn options_member(g: *mut c_void, options: u64, key: &str) -> Result<Option<f64>, ()> {
    if !is_cell(options) {
        return Ok(None);
    }
    let v = try_get(g, options, key)?;
    if v == UNDEF {
        return Ok(None);
    }
    let d = to_number(g, v)?;
    if !d.is_finite() {
        throw_type_error(
            g,
            &format!("Failed to read the '{key}' property from 'QuotaExceededErrorOptions': The provided double value is non-finite."),
        );
        return Err(());
    }
    Ok(Some(d))
}

#[crate::host_fn]
unsafe fn quota_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(c) = classes(g) else { return illegal_constructor(g, cf) };
    let Ok(message) = arg_string(g, cf_arg(cf, 0), "") else { return UNDEF };
    let options = cf_arg(cf, 1);
    if options != UNDEF && options != NULL && !is_cell(options) {
        return throw_type_error(
            g,
            "Failed to construct 'QuotaExceededError': The provided value is not of type 'QuotaExceededErrorOptions'.",
        );
    }
    // Dictionary members convert in lexicographic order: quota, requested.
    let Ok(quota) = options_member(g, options, "quota") else { return UNDEF };
    let Ok(requested) = options_member(g, options, "requested") else { return UNDEF };
    if quota.is_some_and(|q| q < 0.0) {
        return throw_error(g, 2, "Failed to construct 'QuotaExceededError': quota must be at least 0.");
    }
    if requested.is_some_and(|r| r < 0.0) {
        return throw_error(g, 2, "Failed to construct 'QuotaExceededError': requested must be at least 0.");
    }
    if let (Some(q), Some(r)) = (quota, requested) {
        if r < q {
            return throw_error(
                g,
                2,
                "Failed to construct 'QuotaExceededError': requested must be greater than or equal to quota.",
            );
        }
    }
    let Ok(proto) = proto_from(g, cf_new_target(cf), c.quota.proto) else { return UNDEF };
    let num = |v: Option<f64>| v.map(enc_f64).unwrap_or(NULL);
    let name = js_string(g, "QuotaExceededError");
    make(g, c, proto, name, message, Some((num(quota), num(requested))))
}

macro_rules! slot_getter {
    ($fn_name:ident, $iface:literal, $slot:literal) => {
        #[crate::host_fn]
        unsafe fn $fn_name(g: *mut c_void, cf: *mut c_void) -> u64 {
            let this = cf_this(cf);
            let slots = classes(g).and_then(|c| slot_of(g, c, this));
            match slots {
                Some(s) if $iface != "QuotaExceededError" || classes(g).is_some_and(|c| is_quota(g, c, this)) => {
                    get(g, s, $slot)
                }
                _ => throw_type_error(
                    g,
                    concat!("'get ", $slot, "' called on an object that is not a valid instance of ", $iface, "."),
                ),
            }
        }
    };
}

slot_getter!(get_name, "DOMException", "name");
slot_getter!(get_message, "DOMException", "message");
slot_getter!(get_code, "DOMException", "code");
slot_getter!(get_quota, "QuotaExceededError", "quota");
slot_getter!(get_requested, "QuotaExceededError", "requested");

/// A WebIDL attribute: an accessor on the prototype, enumerable and
/// configurable (unlike `define_class`'s DontEnum getters, which are for
/// operations' sake).
unsafe fn attribute(g: *mut c_void, proto: u64, name: &str, f: HostFn) {
    let getter = make_fn(g, &format!("get {name}"), 0, f);
    let a = atom(g, name);
    glue::rjs_define_accessor(g, proto, a, getter, UNDEF, 0);
    glue::rjs_atom_free(g, a);
}


pub unsafe fn install(g: *mut c_void) {
    let s_dom = atom_priv(g, "redjs#domexception");
    let global = glue::rjs_global(g);
    let error_proto = get(g, get(g, global, "Error"), "prototype");

    let dom = define_class(g, &ClassSpec {
        name: "DOMException",
        arity: 0,
        construct: Some(dom_construct),
        parent_proto: error_proto,
        methods: &[],
        getters: &[],
        accessors: &[],
    });
    attribute(g, dom.proto, "name", get_name);
    attribute(g, dom.proto, "message", get_message);
    attribute(g, dom.proto, "code", get_code);
    for (name, value) in CONSTANTS {
        // { writable: false, enumerable: true, configurable: false }
        define(g, dom.ctor, name, enc_i32(*value as i32), 1 | 4);
        define(g, dom.proto, name, enc_i32(*value as i32), 1 | 4);
    }

    let quota = define_class(g, &ClassSpec {
        name: "QuotaExceededError",
        arity: 0,
        construct: Some(quota_construct),
        parent_proto: dom.proto,
        methods: &[],
        getters: &[],
        accessors: &[],
    });
    attribute(g, quota.proto, "quota", get_quota);
    attribute(g, quota.proto, "requested", get_requested);
    // The interface object inherits from its parent's (WebIDL §3.7.1).
    capi::JSObjectSetPrototype(g, quota.ctor as usize as capi::JSObjectRef, dom.ctor as usize as capi::JSValueRef);

    let boxed: &'static DomClasses = Box::leak(Box::new(DomClasses { dom, quota, s_dom }));
    registry().lock().unwrap().insert(g as usize, boxed);
}

#[cfg(test)]
mod tests {
    use crate::webapi::testing::*;

    #[test]
    fn the_code_table_is_webidls() {
        assert_eq!(super::code_for("IndexSizeError"), 1);
        assert_eq!(super::code_for("NotFoundError"), 8);
        assert_eq!(super::code_for("InvalidStateError"), 11);
        assert_eq!(super::code_for("AbortError"), 20);
        assert_eq!(super::code_for("QuotaExceededError"), 22);
        assert_eq!(super::code_for("DataCloneError"), 25);
        // Names without a legacy code.
        for n in ["Error", "NotAllowedError", "DataError", "ConstraintError", "EncodingError", "VersionError"] {
            assert_eq!(super::code_for(n), 0, "{n}");
        }
    }

    #[test]
    fn construction_defaults_and_conversions() {
        let c = ctx();
        ok(&c, "new DOMException().name === 'Error' && new DOMException().message === ''");
        ok(&c, "new DOMException().code === 0");
        ok(&c, "new DOMException('m', 'NotFoundError').code === 8");
        ok(&c, "new DOMException(undefined, undefined).name === 'Error'");
        ok(&c, "new DOMException(1, 2).message === '1' && new DOMException(1, 2).name === '2'");
        ok(&c, "new DOMException(null).message === 'null'");
        // ToString throws propagate, message first.
        let m = throws(&c, "new DOMException(Symbol())");
        assert!(m.to_lowercase().contains("symbol"), "{m}");
        ok(&c, "(() => { try { new DOMException({ toString() { throw 7; } }); } catch (e) { return e === 7; } })()");
        // Constructor requires new.
        assert!(throws(&c, "DOMException()").contains("new"));
        ok(&c, "DOMException.length === 0 && DOMException.name === 'DOMException'");
    }

    #[test]
    fn it_is_a_real_error_with_the_webidl_shape() {
        let c = ctx();
        ok(&c, "Object.getPrototypeOf(DOMException.prototype) === Error.prototype");
        ok(&c, "new DOMException('x', 'AbortError') instanceof Error");
        ok(&c, "new DOMException('x', 'AbortError') instanceof DOMException");
        ok(&c, "Object.prototype.toString.call(new DOMException()) === '[object DOMException]'");
        ok(&c, "String(new DOMException('boom', 'AbortError')) === 'AbortError: boom'");
        ok(&c, "typeof new DOMException('x').stack === 'string'");
        // No own name/message/code: they are the prototype's getters.
        ok(&c, "!Object.prototype.hasOwnProperty.call(new DOMException('x', 'A'), 'message')");
        ok(&c, "Object.keys(new DOMException('x', 'A')).length === 0");
        ok(&c, "(() => { const d = Object.getOwnPropertyDescriptor(DOMException.prototype, 'name'); \
                return typeof d.get === 'function' && d.set === undefined && d.enumerable && d.configurable; })()");
        ok(&c, "(() => { const d = Object.getOwnPropertyDescriptor(DOMException.prototype, 'code'); \
                return d.enumerable && d.get.name === 'get code'; })()");
        // Brand check.
        let m = throws(&c, "DOMException.prototype.name");
        assert!(m.contains("not a valid instance of DOMException"), "{m}");
        let m = throws(&c, "Object.getOwnPropertyDescriptor(DOMException.prototype, 'code').get.call(new Error())");
        assert!(m.contains("DOMException"), "{m}");
        // Subclassing keeps the slots.
        ok(&c, "(() => { class E extends DOMException {}; const e = new E('m', 'NotFoundError'); \
                return e instanceof E && e.code === 8 && e.message === 'm'; })()");
        // A DOMException can be thrown and caught like any error.
        ok(&c, "(() => { try { throw new DOMException('t', 'TimeoutError'); } catch (e) { return e.code === 23; } })()");
    }

    #[test]
    fn the_legacy_constants_are_on_both_objects() {
        let c = ctx();
        ok(&c, "DOMException.INDEX_SIZE_ERR === 1 && DOMException.prototype.INDEX_SIZE_ERR === 1");
        ok(&c, "DOMException.DATA_CLONE_ERR === 25 && new DOMException().DATA_CLONE_ERR === 25");
        ok(&c, "DOMException.VALIDATION_ERR === 16 && DOMException.NO_DATA_ALLOWED_ERR === 6");
        ok(&c, "(() => { const d = Object.getOwnPropertyDescriptor(DOMException, 'ABORT_ERR'); \
                return d.value === 20 && !d.writable && d.enumerable && !d.configurable; })()");
        ok(&c, "Object.keys(DOMException).length === 25");
    }

    #[test]
    fn quota_exceeded_error_is_the_derived_interface() {
        let c = ctx();
        ok(&c, "Object.getPrototypeOf(QuotaExceededError.prototype) === DOMException.prototype");
        ok(&c, "Object.getPrototypeOf(QuotaExceededError) === DOMException");
        ok(&c, "(() => { const e = new QuotaExceededError('full', { quota: 10, requested: 12 }); \
                return e instanceof DOMException && e.name === 'QuotaExceededError' && e.code === 22 \
                  && e.quota === 10 && e.requested === 12 && e.message === 'full'; })()");
        ok(&c, "(() => { const e = new QuotaExceededError(); return e.quota === null && e.requested === null && e.message === ''; })()");
        ok(&c, "new QuotaExceededError('x', { quota: 5 }).requested === null");
        ok(&c, "Object.prototype.toString.call(new QuotaExceededError()) === '[object QuotaExceededError]'");
        // RangeError / TypeError per the IDL.
        ok(&c, "(() => { try { new QuotaExceededError('', { quota: -1 }); } catch (e) { return e instanceof RangeError; } })()");
        ok(&c, "(() => { try { new QuotaExceededError('', { requested: -1 }); } catch (e) { return e instanceof RangeError; } })()");
        ok(&c, "(() => { try { new QuotaExceededError('', { quota: 10, requested: 9 }); } catch (e) { return e instanceof RangeError; } })()");
        ok(&c, "(() => { try { new QuotaExceededError('', { quota: Infinity }); } catch (e) { return e instanceof TypeError; } })()");
        ok(&c, "(() => { try { new QuotaExceededError('', 5); } catch (e) { return e instanceof TypeError; } })()");
        // The quota getter refuses a plain DOMException named QuotaExceededError.
        let m = throws(&c, "Object.getOwnPropertyDescriptor(QuotaExceededError.prototype, 'quota').get.call(new DOMException('', 'QuotaExceededError'))");
        assert!(m.contains("QuotaExceededError"), "{m}");
        // ...which is still code 22 (the names table).
        ok(&c, "new DOMException('', 'QuotaExceededError').code === 22");
    }

    #[test]
    fn native_creation_matches_js_construction() {
        let c = ctx();
        let g = c.raw();
        unsafe {
            let e = super::create(g, "NotSupportedError", "nope");
            let global = crate::glue::rjs_global(g);
            crate::webapi::util::define(g, global, "__e", e, 0);
            let q = super::create_quota_exceeded(g, "full", Some(1.0), None);
            crate::webapi::util::define(g, global, "__q", q, 0);
            let p = super::parts(g, q).unwrap();
            assert_eq!(p.name, "QuotaExceededError");
            assert_eq!(p.quota, Some((Some(1.0), None)));
            assert_eq!(super::parts(g, e).unwrap().quota, None);
            assert!(super::parts(g, crate::webapi::util::new_object(g, 0xa)).is_none());
        }
        ok(&c, "__e instanceof DOMException && __e.name === 'NotSupportedError' && __e.code === 9 && __e.message === 'nope'");
        ok(&c, "__q instanceof QuotaExceededError && __q.quota === 1 && __q.requested === null");
    }
}
