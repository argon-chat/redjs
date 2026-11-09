//! `IDBObjectStore` and `IDBIndex`: the request-making surface. Every
//! method runs the spec's checks in the spec's order (WebIDL conversions
//! first), then hands a plain-data op to [`place`].

use core::ffi::c_void;

use super::backend::*;
use super::client::*;
use super::common::*;
use super::keypath::{self, KeyPath};
use super::server::{Dir, Mode, Op, Source};
use super::svalue::Parsed;
use crate::glue;
use crate::webapi::util::*;

const S: &str = "IDBObjectStore";
const I: &str = "IDBIndex";

pub(super) const STORE: ClassSpec<'static> = ClassSpec {
    name: "IDBObjectStore",
    arity: 0,
    construct: None,
    parent_proto: 0,
    methods: &[
        ("put", 1, store_put),
        ("add", 1, store_add),
        ("delete", 1, store_delete),
        ("clear", 0, store_clear),
        ("get", 1, store_get),
        ("getKey", 1, store_get_key),
        ("getAll", 0, store_get_all),
        ("getAllKeys", 0, store_get_all_keys),
        ("count", 0, store_count),
        ("openCursor", 0, store_open_cursor),
        ("openKeyCursor", 0, store_open_key_cursor),
        ("index", 1, store_index),
        ("createIndex", 2, store_create_index),
        ("deleteIndex", 1, store_delete_index),
    ],
    getters: &[
        ("keyPath", store_key_path),
        ("indexNames", store_index_names),
        ("transaction", store_transaction),
        ("autoIncrement", store_auto_increment),
    ],
    accessors: &[("name", store_name, store_set_name)],
};

pub(super) const INDEX: ClassSpec<'static> = ClassSpec {
    name: "IDBIndex",
    arity: 0,
    construct: None,
    parent_proto: 0,
    methods: &[
        ("get", 1, index_get),
        ("getKey", 1, index_get_key),
        ("getAll", 0, index_get_all),
        ("getAllKeys", 0, index_get_all_keys),
        ("count", 0, index_count),
        ("openCursor", 0, index_open_cursor),
        ("openKeyCursor", 0, index_open_key_cursor),
    ],
    getters: &[
        ("objectStore", index_object_store),
        ("keyPath", index_key_path),
        ("multiEntry", index_multi_entry),
        ("unique", index_unique),
    ],
    accessors: &[("name", index_name, index_set_name)],
};

// ---------------------------------------------------------------------------
// Shared checks
// ---------------------------------------------------------------------------

/// A store handle's or index handle's transaction.
unsafe fn tx_for(g: *mut c_void, this: u64, index: bool) -> u64 {
    if index {
        bget(g, bget(g, this, "objectStore"), "transaction")
    } else {
        bget(g, this, "transaction")
    }
}

/// "Has been deleted": an index handle counts its store's deletion too.
unsafe fn deleted(g: *mut c_void, this: u64, index: bool) -> bool {
    if index {
        let Some(idd) = index_of(g, this) else { return true };
        if idd.deleted {
            return true;
        }
        store_of(g, bget(g, this, "objectStore")).map(|s| s.deleted).unwrap_or(true)
    } else {
        store_of(g, this).map(|s| s.deleted).unwrap_or(true)
    }
}

/// Deleted → InvalidStateError; not active → TransactionInactiveError;
/// `write` on a read-only transaction → ReadOnlyError.
unsafe fn usable(g: *mut c_void, this: u64, index: bool, write: bool, method: &str) -> Option<u64> {
    let iface = if index { I } else { S };
    let tx = tx_for(g, this, index);
    if deleted(g, this, index) {
        let what = if index { "The index or its object store has been deleted." } else { "The object store has been deleted." };
        fail(g, "InvalidStateError", iface, method, what);
        return None;
    }
    let Some(td) = tx_of(g, tx) else {
        illegal(g);
        return None;
    };
    if td.state != TxState::Active {
        fail(g, "TransactionInactiveError", iface, method, "The transaction is not active.");
        return None;
    }
    if write && td.mode == Mode::ReadOnly {
        fail(g, "ReadOnlyError", iface, method, "The transaction is read-only.");
        return None;
    }
    Some(tx)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Read {
    Get,
    GetKey,
    GetAll,
    GetAllKeys,
    Count,
    Cursor,
    KeyCursor,
}

impl Read {
    fn name(self) -> &'static str {
        match self {
            Read::Get => "get",
            Read::GetKey => "getKey",
            Read::GetAll => "getAll",
            Read::GetAllKeys => "getAllKeys",
            Read::Count => "count",
            Read::Cursor => "openCursor",
            Read::KeyCursor => "openKeyCursor",
        }
    }
}

/// Every read, on a store or an index.
unsafe fn read(g: *mut c_void, cf: *mut c_void, kind: Read, index: bool) -> u64 {
    let this = cf_this(cf);
    let iface = if index { I } else { S };
    let method = kind.name();
    let source = if index {
        let Some(idd) = index_of(g, this) else { return illegal(g) };
        Source::Index(idd.store, idd.meta.id)
    } else {
        let Some(sd) = store_of(g, this) else { return illegal(g) };
        Source::Store(sd.meta.id)
    };
    let single = matches!(kind, Read::Get | Read::GetKey);
    if single && !args(g, cf, 1, iface, method) {
        return UNDEF;
    }
    let mut count = 0u32;
    let mut dir = Dir::Next;
    match kind {
        Read::GetAll | Read::GetAllKeys => {
            let c = cf_arg(cf, 1);
            if c != UNDEF {
                match enforce_u32(g, c, iface, method) {
                    Ok(n) => count = n,
                    Err(()) => return UNDEF,
                }
            }
        }
        Read::Cursor | Read::KeyCursor => match direction(g, cf_arg(cf, 1), iface, method) {
            Ok(d) => dir = d,
            Err(()) => return UNDEF,
        },
        _ => {}
    }
    let Some(tx) = usable(g, this, index, false, method) else { return UNDEF };
    let Ok(range) = to_range(g, cf_arg(cf, 0), single, iface, method) else { return UNDEF };
    let key_only = kind == Read::KeyCursor;
    let cursor_range = range.clone();
    let op = match kind {
        Read::Get => Op::Get { source, range, keys: false },
        Read::GetKey => Op::Get { source, range, keys: true },
        Read::GetAll => Op::GetAll { source, range, count, keys: false },
        Read::GetAllKeys => Op::GetAll { source, range, count, keys: true },
        Read::Count => Op::Count { source, range },
        Read::Cursor | Read::KeyCursor => {
            Op::Cursor { source, range: range.clone(), dir, key_only, pos: None, key: None, pkey: None, count: 1 }
        }
    };
    let req = place(g, tx, this, op);
    if matches!(kind, Read::Cursor | Read::KeyCursor) {
        let c = cls(g);
        let (class, proto) = if key_only { (c.cursor, c.cursor_proto) } else { (c.cursor_value, c.cursor_value_proto) };
        let cur = host_create(
            g,
            class,
            proto,
            CursorData { source, dir, range: cursor_range, key_only, pos: None, obj_pos: None, got_value: false },
        );
        new_bag(g, cur);
        bset(g, cur, "source", this);
        bset(g, cur, "request", req);
        bset(g, cur, "transaction", tx);
        bset(g, cur, "key", UNDEF);
        bset(g, cur, "primaryKey", UNDEF);
        bset(g, cur, "value", UNDEF);
        bset(g, req, "cursor", cur);
    }
    req
}

// ---------------------------------------------------------------------------
// IDBObjectStore
// ---------------------------------------------------------------------------

unsafe fn put_impl(g: *mut c_void, cf: *mut c_void, no_overwrite: bool) -> u64 {
    let this = cf_this(cf);
    let method = if no_overwrite { "add" } else { "put" };
    if store_of(g, this).is_none() {
        return illegal(g);
    }
    if !args(g, cf, 1, S, method) {
        return UNDEF;
    }
    let Some(tx) = usable(g, this, false, true, method) else { return UNDEF };
    let value = cf_arg(cf, 0);
    let kv = cf_arg(cf, 1);
    let key_given = cf_argc(cf) >= 2 && kv != UNDEF;
    let Some(sd) = store_of(g, this) else { return UNDEF };
    let (kp, ai, sid) = (sd.meta.key_path.clone(), sd.meta.auto_increment, sd.meta.id);
    if kp.is_some() && key_given {
        return fail(g, "DataError", S, method, "The object store uses in-line keys and the key parameter was provided.");
    }
    if kp.is_none() && !ai && !key_given {
        return fail(
            g,
            "DataError",
            S,
            method,
            "The object store uses out-of-line keys and has no key generator and the key parameter was not provided.",
        );
    }
    let mut key = None;
    if key_given {
        match conv_key(g, kv, S, method) {
            Ok(k) => key = Some(k.encode()),
            Err(()) => return UNDEF,
        }
    }
    let Ok(bytes) = clone_for(g, tx, value, S, method) else { return UNDEF };
    if let Some(kp) = &kp {
        let p = Parsed::parse(&bytes);
        match p.as_ref().map(|p| p.extract(kp)) {
            Some(Ok(Some(k))) => key = Some(k.encode()),
            Some(Ok(None)) => {
                return fail(g, "DataError", S, method, "Evaluating the object store's key path yielded a value that is not a valid key.");
            }
            _ => {
                if !ai {
                    return fail(g, "DataError", S, method, "Evaluating the object store's key path did not yield a value.");
                }
                let injectable = match (kp, p.as_ref()) {
                    (KeyPath::String(s), Some(p)) => p.can_inject(s),
                    _ => false,
                };
                if !injectable {
                    return fail(g, "DataError", S, method, "A generated key could not be inserted into the value.");
                }
            }
        }
    }
    place(g, tx, this, Op::Put { store: sid, key, value: bytes, no_overwrite })
}

#[crate::host_fn]
unsafe fn store_put(g: *mut c_void, cf: *mut c_void) -> u64 {
    put_impl(g, cf, false)
}

#[crate::host_fn]
unsafe fn store_add(g: *mut c_void, cf: *mut c_void) -> u64 {
    put_impl(g, cf, true)
}

#[crate::host_fn]
unsafe fn store_delete(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let Some(sd) = store_of(g, this) else { return illegal(g) };
    let sid = sd.meta.id;
    if !args(g, cf, 1, S, "delete") {
        return UNDEF;
    }
    let Some(tx) = usable(g, this, false, true, "delete") else { return UNDEF };
    let Ok(range) = to_range(g, cf_arg(cf, 0), true, S, "delete") else { return UNDEF };
    place(g, tx, this, Op::Delete { store: sid, range })
}

#[crate::host_fn]
unsafe fn store_clear(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let Some(sd) = store_of(g, this) else { return illegal(g) };
    let sid = sd.meta.id;
    let Some(tx) = usable(g, this, false, true, "clear") else { return UNDEF };
    place(g, tx, this, Op::Clear { store: sid })
}

macro_rules! reads {
    ($($f:ident => $kind:expr, $index:expr;)*) => {
        $(#[crate::host_fn]
        unsafe fn $f(g: *mut c_void, cf: *mut c_void) -> u64 {
            read(g, cf, $kind, $index)
        })*
    };
}

reads! {
    store_get => Read::Get, false;
    store_get_key => Read::GetKey, false;
    store_get_all => Read::GetAll, false;
    store_get_all_keys => Read::GetAllKeys, false;
    store_count => Read::Count, false;
    store_open_cursor => Read::Cursor, false;
    store_open_key_cursor => Read::KeyCursor, false;
    index_get => Read::Get, true;
    index_get_key => Read::GetKey, true;
    index_get_all => Read::GetAll, true;
    index_get_all_keys => Read::GetAllKeys, true;
    index_count => Read::Count, true;
    index_open_cursor => Read::Cursor, true;
    index_open_key_cursor => Read::KeyCursor, true;
}

#[crate::host_fn]
unsafe fn store_index(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if store_of(g, this).is_none() {
        return illegal(g);
    }
    if !args(g, cf, 1, S, "index") {
        return UNDEF;
    }
    let Ok(name) = str16(g, cf_arg(cf, 0)) else { return UNDEF };
    let Some(sd) = store_of(g, this) else { return UNDEF };
    if sd.deleted {
        return fail(g, "InvalidStateError", S, "index", "The object store has been deleted.");
    }
    let tx = bget(g, this, "transaction");
    if tx_of(g, tx).map(|t| t.state == TxState::Finished).unwrap_or(true) {
        return fail(g, "InvalidStateError", S, "index", "The transaction has finished.");
    }
    let Some(sd) = store_of(g, this) else { return UNDEF };
    let Some(meta) = sd.meta.indexes.iter().find(|i| i.name == name).cloned() else {
        return fail(g, "NotFoundError", S, "index", "The specified index was not found.");
    };
    index_handle(g, this, &meta)
}

/// The upgrade-only schema checks: not an upgrade → InvalidStateError,
/// deleted → InvalidStateError, inactive → TransactionInactiveError.
unsafe fn schema_checks(g: *mut c_void, this: u64, index: bool, method: &str) -> Option<u64> {
    let iface = if index { I } else { S };
    let tx = tx_for(g, this, index);
    let Some(td) = tx_of(g, tx) else {
        illegal(g);
        return None;
    };
    if td.mode != Mode::VersionChange {
        fail(g, "InvalidStateError", iface, method, "The database is not running a version change transaction.");
        return None;
    }
    if deleted(g, this, index) {
        let what = if index { "The index or its object store has been deleted." } else { "The object store has been deleted." };
        fail(g, "InvalidStateError", iface, method, what);
        return None;
    }
    if tx_of(g, tx).map(|t| t.state != TxState::Active).unwrap_or(true) {
        fail(g, "TransactionInactiveError", iface, method, "The transaction is not active.");
        return None;
    }
    Some(tx)
}

/// The connection's own copy of store `sid` (what `objectStoreNames` and
/// new handles read).
unsafe fn conn_store<'a>(g: *mut c_void, tx: u64, sid: StoreId) -> Option<&'a mut StoreMeta> {
    let db = bget(g, tx, "db");
    db_of(g, db)?.meta.stores.iter_mut().find(|s| s.id == sid)
}

#[crate::host_fn]
unsafe fn store_create_index(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if store_of(g, this).is_none() {
        return illegal(g);
    }
    if !args(g, cf, 2, S, "createIndex") {
        return UNDEF;
    }
    let Ok(name) = str16(g, cf_arg(cf, 0)) else { return UNDEF };
    let Ok(kp) = keypath::from_js(g, cf_arg(cf, 1)) else { return UNDEF };
    let ov = cf_arg(cf, 2);
    let (multi, unique) = if ov == UNDEF || ov == NULL {
        (false, false)
    } else if !is_object(g, ov) {
        return throw_type_error(g, "Failed to execute 'createIndex' on 'IDBObjectStore': The provided value is not of type 'IDBIndexParameters'.");
    } else {
        let Ok(m) = try_get(g, ov, "multiEntry") else { return UNDEF };
        let m = glue::rjs_to_boolean(g, m) != 0;
        let Ok(u) = try_get(g, ov, "unique") else { return UNDEF };
        (m, glue::rjs_to_boolean(g, u) != 0)
    };
    let Some(tx) = schema_checks(g, this, false, "createIndex") else { return UNDEF };
    if store_of(g, this).map(|s| s.meta.indexes.iter().any(|i| i.name == name)).unwrap_or(false) {
        return fail(g, "ConstraintError", S, "createIndex", "An index with the specified name already exists.");
    }
    if !keypath::valid(g, &kp) {
        return fail(g, "SyntaxError", S, "createIndex", "The keyPath argument contains an invalid key path.");
    }
    if kp.is_list() && multi {
        return fail(g, "InvalidAccessError", S, "createIndex", "The keyPath argument was an array and the multiEntry option is true.");
    }
    let db = bget(g, tx, "db");
    let Some(id) = db_of(g, db).map(|d| d.alloc_id()) else { return UNDEF };
    let meta = IndexMeta { id, name, key_path: kp, unique, multi_entry: multi };
    let Some(sd) = store_of(g, this) else { return UNDEF };
    let sid = sd.meta.id;
    sd.meta.indexes.push(meta.clone());
    if let Some(s) = conn_store(g, tx, sid) {
        s.indexes.push(meta.clone());
    }
    schema_op(g, tx, Op::CreateIndex(sid, meta.clone()));
    index_handle(g, this, &meta)
}

#[crate::host_fn]
unsafe fn store_delete_index(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if store_of(g, this).is_none() {
        return illegal(g);
    }
    if !args(g, cf, 1, S, "deleteIndex") {
        return UNDEF;
    }
    let Ok(name) = str16(g, cf_arg(cf, 0)) else { return UNDEF };
    let Some(tx) = schema_checks(g, this, false, "deleteIndex") else { return UNDEF };
    let Some(sd) = store_of(g, this) else { return UNDEF };
    let Some(pos) = sd.meta.indexes.iter().position(|i| i.name == name) else {
        return fail(g, "NotFoundError", S, "deleteIndex", "The specified index was not found.");
    };
    let meta = sd.meta.indexes.remove(pos);
    let sid = sd.meta.id;
    if let Some(j) = sd.handles.iter().position(|x| *x == meta.id) {
        let h = get_i(g, bget(g, this, "indexes"), j as u32);
        if let Some(idd) = index_of(g, h) {
            idd.deleted = true;
        }
    }
    if let Some(s) = conn_store(g, tx, sid) {
        s.indexes.retain(|i| i.id != meta.id);
    }
    schema_op(g, tx, Op::DeleteIndex(sid, meta.id));
    UNDEF
}

#[crate::host_fn]
unsafe fn store_name(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(sd) = store_of(g, cf_this(cf)) else { return illegal(g) };
    let n = sd.meta.name.clone();
    js_string16(g, &n)
}

#[crate::host_fn]
unsafe fn store_set_name(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if store_of(g, this).is_none() {
        return illegal(g);
    }
    let Ok(name) = str16(g, cf_arg(cf, 0)) else { return UNDEF };
    let Some(tx) = schema_checks(g, this, false, "name") else { return UNDEF };
    let Some(sd) = store_of(g, this) else { return UNDEF };
    if sd.meta.name == name {
        return UNDEF;
    }
    let sid = sd.meta.id;
    let db = bget(g, tx, "db");
    if db_of(g, db).map(|d| d.meta.store_by_name(&name).is_some()).unwrap_or(false) {
        return crate::webapi::domexception::throw(g, "ConstraintError", "Failed to set the 'name' property on 'IDBObjectStore': An object store with the specified name already exists.");
    }
    if let Some(sd) = store_of(g, this) {
        sd.meta.name = name.clone();
    }
    if let Some(s) = conn_store(g, tx, sid) {
        s.name = name.clone();
    }
    schema_op(g, tx, Op::RenameStore(sid, name));
    UNDEF
}

/// The `keyPath` getters: the same Array object every time.
unsafe fn cached_key_path(g: *mut c_void, this: u64, kp: Option<KeyPath>) -> u64 {
    let Some(kp) = kp else { return NULL };
    if let KeyPath::String(s) = &kp {
        return js_string16(g, s);
    }
    let c = bget(g, this, "keyPath");
    if c != UNDEF {
        return c;
    }
    let v = keypath::to_js(g, &kp);
    bset(g, this, "keyPath", v);
    v
}

#[crate::host_fn]
unsafe fn store_key_path(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let Some(sd) = store_of(g, this) else { return illegal(g) };
    let kp = sd.meta.key_path.clone();
    cached_key_path(g, this, kp)
}

#[crate::host_fn]
unsafe fn store_index_names(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(sd) = store_of(g, cf_this(cf)) else { return illegal(g) };
    let names = sd.meta.indexes.iter().map(|i| i.name.clone()).collect();
    string_list(g, names, true)
}

#[crate::host_fn]
unsafe fn store_transaction(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if store_of(g, this).is_none() {
        return illegal(g);
    }
    bget(g, this, "transaction")
}

#[crate::host_fn]
unsafe fn store_auto_increment(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(sd) = store_of(g, cf_this(cf)) else { return illegal(g) };
    enc_bool(sd.meta.auto_increment)
}

// ---------------------------------------------------------------------------
// IDBIndex
// ---------------------------------------------------------------------------

#[crate::host_fn]
unsafe fn index_name(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(idd) = index_of(g, cf_this(cf)) else { return illegal(g) };
    let n = idd.meta.name.clone();
    js_string16(g, &n)
}

#[crate::host_fn]
unsafe fn index_set_name(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if index_of(g, this).is_none() {
        return illegal(g);
    }
    let Ok(name) = str16(g, cf_arg(cf, 0)) else { return UNDEF };
    let Some(tx) = schema_checks(g, this, true, "name") else { return UNDEF };
    let Some(idd) = index_of(g, this) else { return UNDEF };
    if idd.meta.name == name {
        return UNDEF;
    }
    let (sid, iid) = (idd.store, idd.meta.id);
    let store = bget(g, this, "objectStore");
    if store_of(g, store).map(|s| s.meta.indexes.iter().any(|i| i.name == name)).unwrap_or(false) {
        return crate::webapi::domexception::throw(g, "ConstraintError", "Failed to set the 'name' property on 'IDBIndex': An index with the specified name already exists.");
    }
    if let Some(idd) = index_of(g, this) {
        idd.meta.name = name.clone();
    }
    if let Some(sd) = store_of(g, store) {
        if let Some(i) = sd.meta.indexes.iter_mut().find(|i| i.id == iid) {
            i.name = name.clone();
        }
    }
    if let Some(s) = conn_store(g, tx, sid) {
        if let Some(i) = s.indexes.iter_mut().find(|i| i.id == iid) {
            i.name = name.clone();
        }
    }
    schema_op(g, tx, Op::RenameIndex(sid, iid, name));
    UNDEF
}

#[crate::host_fn]
unsafe fn index_object_store(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if index_of(g, this).is_none() {
        return illegal(g);
    }
    bget(g, this, "objectStore")
}

#[crate::host_fn]
unsafe fn index_key_path(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let Some(idd) = index_of(g, this) else { return illegal(g) };
    let kp = idd.meta.key_path.clone();
    cached_key_path(g, this, Some(kp))
}

#[crate::host_fn]
unsafe fn index_multi_entry(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(idd) = index_of(g, cf_this(cf)) else { return illegal(g) };
    enc_bool(idd.meta.multi_entry)
}

#[crate::host_fn]
unsafe fn index_unique(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(idd) = index_of(g, cf_this(cf)) else { return illegal(g) };
    enc_bool(idd.meta.unique)
}

