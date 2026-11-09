//! `IDBFactory` (`indexedDB`), `IDBRequest` / `IDBOpenDBRequest`,
//! `IDBVersionChangeEvent`, `DOMStringList`, `IDBKeyRange`, `StorageManager`
//! (`navigator.storage`) — and the class install for every IDB interface.

use core::ffi::c_void;

use super::client::*;
use super::common::*;
use super::key::{self, Bound, Key, Range};
use super::server::{self, ToServer};
use super::{cursor, database, store};
use crate::capi;
use crate::glue;
use crate::webapi::util::*;
use crate::webapi::{domexception, events};

const F: &str = "IDBFactory";

unsafe fn is_factory(g: *mut c_void, v: u64) -> bool {
    host_data::<Unit>(g, cls(g).factory, v).is_some()
}

unsafe fn denied(g: *mut c_void) -> bool {
    cl(g).borrow().opaque
}

#[crate::host_fn]
unsafe fn factory_open(g: *mut c_void, cf: *mut c_void) -> u64 {
    if !is_factory(g, cf_this(cf)) {
        return illegal(g);
    }
    if !args(g, cf, 1, F, "open") {
        return UNDEF;
    }
    let Ok(name) = str16(g, cf_arg(cf, 0)) else { return UNDEF };
    let v = cf_arg(cf, 1);
    let version = if cf_argc(cf) >= 2 && v != UNDEF {
        match enforce_range_u64(g, v, "Failed to execute 'open' on 'IDBFactory'") {
            Ok(n) => Some(n),
            Err(()) => return UNDEF,
        }
    } else {
        None
    };
    if version == Some(0) {
        return throw_type_error(g, "Failed to execute 'open' on 'IDBFactory': The version provided must not be 0.");
    }
    if denied(g) {
        return fail(g, "SecurityError", F, "open", "Access to the Indexed Database API is denied in this context.");
    }
    let id = super::next_id();
    let req = new_request(g, id, true, NULL, NULL);
    protect(g, req);
    let ctx = {
        let mut c = cl(g).borrow_mut();
        c.reqs.insert(id, req);
        c.inflight += 1;
        c.ctx
    };
    server::send(ToServer::Open { ctx, req: id, name, version });
    req
}

#[crate::host_fn]
unsafe fn factory_delete(g: *mut c_void, cf: *mut c_void) -> u64 {
    if !is_factory(g, cf_this(cf)) {
        return illegal(g);
    }
    if !args(g, cf, 1, F, "deleteDatabase") {
        return UNDEF;
    }
    let Ok(name) = str16(g, cf_arg(cf, 0)) else { return UNDEF };
    if denied(g) {
        return fail(g, "SecurityError", F, "deleteDatabase", "Access to the Indexed Database API is denied in this context.");
    }
    let id = super::next_id();
    let req = new_request(g, id, true, NULL, NULL);
    protect(g, req);
    let ctx = {
        let mut c = cl(g).borrow_mut();
        c.reqs.insert(id, req);
        c.inflight += 1;
        c.ctx
    };
    server::send(ToServer::Delete { ctx, req: id, name });
    req
}

/// A promise settled by a server answer (`databases()`, `estimate()`).
unsafe fn server_promise(g: *mut c_void, kind: PromiseKind, msg: impl FnOnce(crate::context::ContextId, u64) -> ToServer) -> u64 {
    let (mut res, mut rej, mut p) = (0u64, 0u64, 0u64);
    if crate::ffi::redjs_promise_create(g, &mut res, &mut rej, &mut p) != 0 {
        return UNDEF;
    }
    protect(g, res);
    protect(g, rej);
    let id = super::next_id();
    let ctx = {
        let mut c = cl(g).borrow_mut();
        c.promises.insert(id, (res, rej, kind));
        c.inflight += 1;
        c.ctx
    };
    server::send(msg(ctx, id));
    p
}

#[crate::host_fn]
unsafe fn factory_databases(g: *mut c_void, cf: *mut c_void) -> u64 {
    if !is_factory(g, cf_this(cf)) {
        return domexception::rejected_with(g, {
            let m: Vec<u16> = "Illegal invocation".encode_utf16().collect();
            let mut e = UNDEF;
            glue::rjs_error_create(g, 1, m.as_ptr(), m.len(), &mut e);
            e
        });
    }
    if denied(g) {
        return domexception::rejected(g, "SecurityError", "Access to the Indexed Database API is denied in this context.");
    }
    server_promise(g, PromiseKind::Databases, |ctx, req| ToServer::Databases { ctx, req })
}

#[crate::host_fn]
unsafe fn factory_cmp(g: *mut c_void, cf: *mut c_void) -> u64 {
    if !is_factory(g, cf_this(cf)) {
        return illegal(g);
    }
    if !args(g, cf, 2, F, "cmp") {
        return UNDEF;
    }
    let Ok(a) = conv_key(g, cf_arg(cf, 0), F, "cmp") else { return UNDEF };
    let Ok(b) = conv_key(g, cf_arg(cf, 1), F, "cmp") else { return UNDEF };
    enc_i32(match a.encode().cmp(&b.encode()) {
        core::cmp::Ordering::Less => -1,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    })
}

// ---------------------------------------------------------------------------
// IDBRequest / IDBOpenDBRequest
// ---------------------------------------------------------------------------


unsafe fn is_request(g: *mut c_void, v: u64) -> bool {
    req_of(g, v).is_some()
}

unsafe fn is_open_request(g: *mut c_void, v: u64) -> bool {
    req_of(g, v).map(|r| r.open).unwrap_or(false)
}

#[crate::host_fn]
unsafe fn req_result(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let Some(rd) = req_of(g, this) else { return illegal(g) };
    if !rd.done {
        return domexception::throw(g, "InvalidStateError", "Failed to read the 'result' property from 'IDBRequest': The request has not finished.");
    }
    bget(g, this, "result")
}

#[crate::host_fn]
unsafe fn req_error(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let Some(rd) = req_of(g, this) else { return illegal(g) };
    if !rd.done {
        return domexception::throw(g, "InvalidStateError", "Failed to read the 'error' property from 'IDBRequest': The request has not finished.");
    }
    bget(g, this, "error")
}

#[crate::host_fn]
unsafe fn req_source(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if !is_request(g, this) {
        return illegal(g);
    }
    bget(g, this, "source")
}

#[crate::host_fn]
unsafe fn req_transaction(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if !is_request(g, this) {
        return illegal(g);
    }
    bget(g, this, "transaction")
}

#[crate::host_fn]
unsafe fn req_ready_state(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(rd) = req_of(g, cf_this(cf)) else { return illegal(g) };
    js_string(g, if rd.done { "done" } else { "pending" })
}

handler!(req_get_onsuccess, req_set_onsuccess, "onsuccess", is_request);
handler!(req_get_onerror, req_set_onerror, "onerror", is_request);
handler!(req_get_onblocked, req_set_onblocked, "onblocked", is_open_request);
handler!(req_get_onupgradeneeded, req_set_onupgradeneeded, "onupgradeneeded", is_open_request);

// ---------------------------------------------------------------------------
// IDBVersionChangeEvent
// ---------------------------------------------------------------------------

#[crate::host_fn]
unsafe fn vce_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "Failed to construct 'IDBVersionChangeEvent': 1 argument required, but only 0 present.");
    }
    let c = cls(g);
    let obj = ordinary_create(g, cf_new_target(cf), c.vce_proto);
    let init = cf_arg(cf, 1);
    if init != UNDEF && init != NULL && !is_object(g, init) {
        return throw_type_error(g, "Failed to construct 'IDBVersionChangeEvent': The provided value is not of type 'IDBVersionChangeEventInit'.");
    }
    events::init_event(g, obj, cf_arg(cf, 0), init, false);
    let (mut old, mut new) = (enc_i32(0), NULL);
    if is_object(g, init) {
        let Ok(nv) = try_get(g, init, "newVersion") else { return UNDEF };
        if nv != UNDEF && nv != NULL {
            let Ok(d) = to_number(g, nv) else { return UNDEF };
            new = enc_f64(to_u64_modulo(d));
        }
        let Ok(ov) = try_get(g, init, "oldVersion") else { return UNDEF };
        if ov != UNDEF {
            let Ok(d) = to_number(g, ov) else { return UNDEF };
            old = enc_f64(to_u64_modulo(d));
        }
    }
    if let Some(s) = events::state_of(g, obj) {
        define(g, s, "idbOld", old, 0);
        define(g, s, "idbNew", new, 0);
    }
    obj
}

unsafe fn vce_field(g: *mut c_void, cf: *mut c_void, f: &str) -> u64 {
    let Some(s) = events::state_of(g, cf_this(cf)) else { return illegal(g) };
    let v = get(g, s, f);
    if v == UNDEF {
        return illegal(g);
    }
    v
}

#[crate::host_fn]
unsafe fn vce_old(g: *mut c_void, cf: *mut c_void) -> u64 {
    vce_field(g, cf, "idbOld")
}

#[crate::host_fn]
unsafe fn vce_new(g: *mut c_void, cf: *mut c_void) -> u64 {
    vce_field(g, cf, "idbNew")
}

// ---------------------------------------------------------------------------
// DOMStringList
// ---------------------------------------------------------------------------

#[crate::host_fn]
unsafe fn list_length(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(l) = list_of(g, cf_this(cf)) else { return illegal(g) };
    enc_f64(l.items.len() as f64)
}

#[crate::host_fn]
unsafe fn list_item(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if list_of(g, this).is_none() {
        return illegal(g);
    }
    if !args(g, cf, 1, "DOMStringList", "item") {
        return UNDEF;
    }
    let Ok(d) = to_number(g, cf_arg(cf, 0)) else { return UNDEF };
    let i = if d.is_finite() { (d.trunc() % 4_294_967_296.0 + 4_294_967_296.0) % 4_294_967_296.0 } else { 0.0 } as usize;
    let Some(l) = list_of(g, this) else { return NULL };
    match l.items.get(i) {
        Some(s) => {
            let s = s.clone();
            js_string16(g, &s)
        }
        None => NULL,
    }
}

#[crate::host_fn]
unsafe fn list_contains(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if list_of(g, this).is_none() {
        return illegal(g);
    }
    if !args(g, cf, 1, "DOMStringList", "contains") {
        return UNDEF;
    }
    let Ok(s) = str16(g, cf_arg(cf, 0)) else { return UNDEF };
    enc_bool(list_of(g, this).map(|l| l.items.contains(&s)).unwrap_or(false))
}

// ---------------------------------------------------------------------------
// IDBKeyRange
// ---------------------------------------------------------------------------

const K: &str = "IDBKeyRange";

unsafe fn make_range(g: *mut c_void, lower: Option<(Key, bool)>, upper: Option<(Key, bool)>) -> u64 {
    let b = |e: &Option<(Key, bool)>| match e {
        None => Bound::Unbounded,
        Some((k, true)) => Bound::Excl(k.encode()),
        Some((k, false)) => Bound::Incl(k.encode()),
    };
    let range = Range { lower: b(&lower), upper: b(&upper) };
    let lo = lower.as_ref().map(|(k, _)| key::to_js(g, k)).unwrap_or(UNDEF);
    let hi = upper.as_ref().map(|(k, _)| key::to_js(g, k)).unwrap_or(UNDEF);
    let c = cls(g);
    let obj = host_create(g, c.key_range, c.key_range_proto, RangeData { range });
    new_bag(g, obj);
    bset(g, obj, "lower", lo);
    bset(g, obj, "upper", hi);
    obj
}

#[crate::host_fn]
unsafe fn kr_only(g: *mut c_void, cf: *mut c_void) -> u64 {
    if !args(g, cf, 1, K, "only") {
        return UNDEF;
    }
    let Ok(k) = conv_key(g, cf_arg(cf, 0), K, "only") else { return UNDEF };
    make_range(g, Some((k.clone(), false)), Some((k, false)))
}

#[crate::host_fn]
unsafe fn kr_lower_bound(g: *mut c_void, cf: *mut c_void) -> u64 {
    if !args(g, cf, 1, K, "lowerBound") {
        return UNDEF;
    }
    let open = glue::rjs_to_boolean(g, cf_arg(cf, 1)) != 0;
    let Ok(k) = conv_key(g, cf_arg(cf, 0), K, "lowerBound") else { return UNDEF };
    make_range(g, Some((k, open)), None)
}

#[crate::host_fn]
unsafe fn kr_upper_bound(g: *mut c_void, cf: *mut c_void) -> u64 {
    if !args(g, cf, 1, K, "upperBound") {
        return UNDEF;
    }
    let open = glue::rjs_to_boolean(g, cf_arg(cf, 1)) != 0;
    let Ok(k) = conv_key(g, cf_arg(cf, 0), K, "upperBound") else { return UNDEF };
    make_range(g, None, Some((k, open)))
}

#[crate::host_fn]
unsafe fn kr_bound(g: *mut c_void, cf: *mut c_void) -> u64 {
    if !args(g, cf, 2, K, "bound") {
        return UNDEF;
    }
    let lo_open = glue::rjs_to_boolean(g, cf_arg(cf, 2)) != 0;
    let hi_open = glue::rjs_to_boolean(g, cf_arg(cf, 3)) != 0;
    let Ok(lo) = conv_key(g, cf_arg(cf, 0), K, "bound") else { return UNDEF };
    let Ok(hi) = conv_key(g, cf_arg(cf, 1), K, "bound") else { return UNDEF };
    let (a, b) = (lo.encode(), hi.encode());
    if a > b {
        return fail(g, "DataError", K, "bound", "The lower key is greater than the upper key.");
    }
    if a == b && (lo_open || hi_open) {
        return fail(g, "DataError", K, "bound", "The lower key and upper key are equal and one of the bounds is open.");
    }
    make_range(g, Some((lo, lo_open)), Some((hi, hi_open)))
}

#[crate::host_fn]
unsafe fn kr_includes(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if range_of(g, this).is_none() {
        return illegal(g);
    }
    if !args(g, cf, 1, K, "includes") {
        return UNDEF;
    }
    let Ok(k) = conv_key(g, cf_arg(cf, 0), K, "includes") else { return UNDEF };
    enc_bool(range_of(g, this).map(|r| r.range.contains(&k.encode())).unwrap_or(false))
}

#[crate::host_fn]
unsafe fn kr_lower(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if range_of(g, this).is_none() {
        return illegal(g);
    }
    bget(g, this, "lower")
}

#[crate::host_fn]
unsafe fn kr_upper(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if range_of(g, this).is_none() {
        return illegal(g);
    }
    bget(g, this, "upper")
}

#[crate::host_fn]
unsafe fn kr_lower_open(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(r) = range_of(g, cf_this(cf)) else { return illegal(g) };
    enc_bool(bound_open(&r.range.lower))
}

#[crate::host_fn]
unsafe fn kr_upper_open(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(r) = range_of(g, cf_this(cf)) else { return illegal(g) };
    enc_bool(bound_open(&r.range.upper))
}

// ---------------------------------------------------------------------------
// StorageManager (navigator.storage)
// ---------------------------------------------------------------------------

unsafe fn is_storage(g: *mut c_void, v: u64) -> bool {
    host_data::<Unit>(g, cls(g).storage_manager, v).is_some()
}

unsafe fn illegal_promise(g: *mut c_void) -> u64 {
    let m: Vec<u16> = "Illegal invocation".encode_utf16().collect();
    let mut e = UNDEF;
    glue::rjs_error_create(g, 1, m.as_ptr(), m.len(), &mut e);
    domexception::rejected_with(g, e)
}

#[crate::host_fn]
unsafe fn sm_estimate(g: *mut c_void, cf: *mut c_void) -> u64 {
    if !is_storage(g, cf_this(cf)) {
        return illegal_promise(g);
    }
    server_promise(g, PromiseKind::Estimate, |ctx, req| ToServer::Estimate { ctx, req })
}

/// A desktop app's data is never evicted by this runtime: `true` is the
/// honest answer to both (IDB-D7).
#[crate::host_fn]
unsafe fn sm_persist(g: *mut c_void, cf: *mut c_void) -> u64 {
    if !is_storage(g, cf_this(cf)) {
        return illegal_promise(g);
    }
    resolved_promise(g, TRUE)
}

#[crate::host_fn]
pub(super) unsafe fn get_indexed_db(g: *mut c_void, _cf: *mut c_void) -> u64 {
    try_cl(g).map(|c| c.borrow().factory).unwrap_or(UNDEF)
}

#[crate::host_fn]
pub(super) unsafe fn get_storage(g: *mut c_void, _cf: *mut c_void) -> u64 {
    try_cl(g).map(|c| c.borrow().storage).unwrap_or(UNDEF)
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

/// A child JSClassRef: same box type as `parent` (whose finalizer frees it —
/// JSC runs every finalizer up the chain, so the child must have none), its
/// own class name for `Object.prototype.toString`.
unsafe fn sub_class(name: &str, parent: capi::JSClassRef) -> capi::JSClassRef {
    let cname = std::ffi::CString::new(name).unwrap();
    let mut def = capi::JSClassDefinition::empty();
    def.class_name = cname.as_ptr();
    def.parent_class = parent;
    capi::JSClassCreate(&def)
}

unsafe fn inherit(g: *mut c_void, child: &Class, parent: &Class) {
    capi::JSObjectSetPrototype(g, child.ctor as usize as capi::JSObjectRef, parent.ctor as usize as capi::JSValueRef);
}

/// `define_class` gives every prototype its `@@toStringTag`.
unsafe fn class(g: *mut c_void, spec: &ClassSpec) -> Class {
    define_class(g, spec)
}

/// Every IDB interface object, prototype chains as in a browser (prototype
/// AND interface-object inheritance), `@@toStringTag` on each prototype.
pub(super) unsafe fn install_classes(g: *mut c_void, ev_proto: u64, et_proto: u64) -> Classes {
    let global = glue::rjs_global(g);
    let event_target = get(g, global, "EventTarget");
    let event_ctor = get(g, global, "Event");
    let et = Class { ctor: event_target, proto: et_proto };
    let ev = Class { ctor: event_ctor, proto: ev_proto };

    let factory = class(g, &ClassSpec {
        name: "IDBFactory",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[("open", 1, factory_open), ("deleteDatabase", 1, factory_delete), ("databases", 0, factory_databases), ("cmp", 2, factory_cmp)],
        getters: &[],
        accessors: &[],
    });

    let request = class(g, &ClassSpec {
        name: "IDBRequest",
        arity: 0,
        construct: None,
        parent_proto: et_proto,
        methods: &[],
        getters: &[
            ("result", req_result),
            ("error", req_error),
            ("source", req_source),
            ("transaction", req_transaction),
            ("readyState", req_ready_state),
        ],
        accessors: &[("onsuccess", req_get_onsuccess, req_set_onsuccess), ("onerror", req_get_onerror, req_set_onerror)],
    });
    inherit(g, &request, &et);

    let open_request = class(g, &ClassSpec {
        name: "IDBOpenDBRequest",
        arity: 0,
        construct: None,
        parent_proto: request.proto,
        methods: &[],
        getters: &[],
        accessors: &[("onblocked", req_get_onblocked, req_set_onblocked), ("onupgradeneeded", req_get_onupgradeneeded, req_set_onupgradeneeded)],
    });
    inherit(g, &open_request, &request);

    let database = class(g, &database::DATABASE);
    capi::JSObjectSetPrototype(g, database.proto as usize as capi::JSObjectRef, et_proto as usize as capi::JSValueRef);
    inherit(g, &database, &et);

    let transaction = class(g, &database::TRANSACTION);
    capi::JSObjectSetPrototype(g, transaction.proto as usize as capi::JSObjectRef, et_proto as usize as capi::JSValueRef);
    inherit(g, &transaction, &et);

    let store = class(g, &store::STORE);
    let index = class(g, &store::INDEX);
    let cursor = class(g, &cursor::CURSOR);
    let cursor_value = class(g, &ClassSpec {
        name: "IDBCursorWithValue",
        arity: 0,
        construct: None,
        parent_proto: cursor.proto,
        methods: &[],
        getters: &[("value", cursor::value)],
        accessors: &[],
    });
    inherit(g, &cursor_value, &cursor);

    let key_range = class(g, &ClassSpec {
        name: "IDBKeyRange",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[("includes", 1, kr_includes)],
        getters: &[("lower", kr_lower), ("upper", kr_upper), ("lowerOpen", kr_lower_open), ("upperOpen", kr_upper_open)],
        accessors: &[],
    });
    for (name, arity, f) in [
        ("only", 1u32, kr_only as HostFn),
        ("lowerBound", 1, kr_lower_bound),
        ("upperBound", 1, kr_upper_bound),
        ("bound", 2, kr_bound),
    ] {
        define(g, key_range.ctor, name, make_fn(g, name, arity, f), 2);
    }

    let vce = class(g, &ClassSpec {
        name: "IDBVersionChangeEvent",
        arity: 1,
        construct: Some(vce_construct),
        parent_proto: ev_proto,
        methods: &[],
        getters: &[("oldVersion", vce_old), ("newVersion", vce_new)],
        accessors: &[],
    });
    inherit(g, &vce, &ev);

    let string_list = class(g, &ClassSpec {
        name: "DOMStringList",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[("item", 1, list_item), ("contains", 1, list_contains)],
        getters: &[("length", list_length)],
        accessors: &[],
    });
    // An indexed getter + an integer `length`: WebIDL gives the interface
    // `@@iterator` = %Array.prototype.values% (Array.from / for…of work).
    let array_proto = get(g, get(g, global, "Array"), "prototype");
    let values = get(g, array_proto, "values");
    glue::rjs_define(g, string_list.proto, glue::rjs_atom_wellknown(g, 0), values, 2);

    let storage_manager = class(g, &ClassSpec {
        name: "StorageManager",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[("estimate", 0, sm_estimate), ("persist", 0, sm_persist), ("persisted", 0, sm_persist)],
        getters: &[],
        accessors: &[],
    });

    let request_class = make_host_class::<ReqData>("IDBRequest");
    let cursor_class = make_host_class::<CursorData>("IDBCursor");
    Classes {
        factory: make_host_class::<Unit>("IDBFactory"),
        request: request_class,
        open_request: sub_class("IDBOpenDBRequest", request_class),
        database: make_host_class::<DbData>("IDBDatabase"),
        transaction: make_host_class::<TxData>("IDBTransaction"),
        store: make_host_class::<StoreData>("IDBObjectStore"),
        index: make_host_class::<IndexData>("IDBIndex"),
        cursor: cursor_class,
        cursor_value: sub_class("IDBCursorWithValue", cursor_class),
        key_range: make_host_class::<RangeData>("IDBKeyRange"),
        string_list: make_host_class::<ListData>("DOMStringList"),
        storage_manager: make_host_class::<Unit>("StorageManager"),
        factory_proto: factory.proto,
        request_proto: request.proto,
        open_request_proto: open_request.proto,
        database_proto: database.proto,
        transaction_proto: transaction.proto,
        store_proto: store.proto,
        index_proto: index.proto,
        cursor_proto: cursor.proto,
        cursor_value_proto: cursor_value.proto,
        key_range_proto: key_range.proto,
        string_list_proto: string_list.proto,
        vce_proto: vce.proto,
        storage_manager_proto: storage_manager.proto,
    }
}
