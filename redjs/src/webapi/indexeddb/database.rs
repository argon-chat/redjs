//! `IDBDatabase` (a connection) and `IDBTransaction`.

use core::ffi::c_void;
use std::collections::VecDeque;

use super::backend::*;
use super::client::*;
use super::common::*;
use super::keypath::{self, KeyPath};
use super::server::{self, Mode, Op, ToServer};
use crate::webapi::util::*;

const D: &str = "IDBDatabase";
const T: &str = "IDBTransaction";

unsafe fn is_db(g: *mut c_void, v: u64) -> bool {
    db_of(g, v).is_some()
}

unsafe fn is_tx(g: *mut c_void, v: u64) -> bool {
    tx_of(g, v).is_some()
}

handler!(db_get_onabort, db_set_onabort, "onabort", is_db);
handler!(db_get_onclose, db_set_onclose, "onclose", is_db);
handler!(db_get_onerror, db_set_onerror, "onerror", is_db);
handler!(db_get_onversionchange, db_set_onversionchange, "onversionchange", is_db);
handler!(tx_get_onabort, tx_set_onabort, "onabort", is_tx);
handler!(tx_get_oncomplete, tx_set_oncomplete, "oncomplete", is_tx);
handler!(tx_get_onerror, tx_set_onerror, "onerror", is_tx);

pub(super) const DATABASE: ClassSpec<'static> = ClassSpec {
    name: "IDBDatabase",
    arity: 0,
    construct: None,
    parent_proto: 0,
    methods: &[
        ("transaction", 1, db_transaction),
        ("createObjectStore", 1, db_create_store),
        ("deleteObjectStore", 1, db_delete_store),
        ("close", 0, db_close),
    ],
    getters: &[("name", db_name), ("version", db_version), ("objectStoreNames", db_store_names)],
    accessors: &[
        ("onabort", db_get_onabort, db_set_onabort),
        ("onclose", db_get_onclose, db_set_onclose),
        ("onerror", db_get_onerror, db_set_onerror),
        ("onversionchange", db_get_onversionchange, db_set_onversionchange),
    ],
};

pub(super) const TRANSACTION: ClassSpec<'static> = ClassSpec {
    name: "IDBTransaction",
    arity: 0,
    construct: None,
    parent_proto: 0,
    methods: &[("objectStore", 1, tx_object_store), ("commit", 0, tx_commit), ("abort", 0, tx_abort)],
    getters: &[
        ("objectStoreNames", tx_store_names),
        ("mode", tx_mode),
        ("durability", tx_durability),
        ("db", tx_db),
        ("error", tx_error),
    ],
    accessors: &[
        ("onabort", tx_get_onabort, tx_set_onabort),
        ("oncomplete", tx_get_oncomplete, tx_set_oncomplete),
        ("onerror", tx_get_onerror, tx_set_onerror),
    ],
};

// ---------------------------------------------------------------------------
// IDBDatabase
// ---------------------------------------------------------------------------

unsafe extern "sysv64" fn db_name(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(dd) = db_of(g, cf_this(cf)) else { return illegal(g) };
    let n = dd.meta.name.clone();
    js_string16(g, &n)
}

unsafe extern "sysv64" fn db_version(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(dd) = db_of(g, cf_this(cf)) else { return illegal(g) };
    enc_f64(dd.meta.version as f64)
}

unsafe extern "sysv64" fn db_store_names(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(dd) = db_of(g, cf_this(cf)) else { return illegal(g) };
    let names = dd.meta.stores.iter().map(|s| s.name.clone()).collect();
    string_list(g, names, true)
}

/// The live (unfinished) upgrade transaction of a connection, if any.
unsafe fn live_upgrade(g: *mut c_void, db: u64) -> Option<u64> {
    let up = db_of(g, db)?.upgrade;
    if up == 0 {
        return None;
    }
    let o = cl(g).borrow().txs.get(&up).copied()?;
    Some(o)
}

unsafe extern "sysv64" fn db_transaction(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if !is_db(g, this) {
        return illegal(g);
    }
    if !args(g, cf, 1, D, "transaction") {
        return UNDEF;
    }
    let names = match keypath::from_js(g, cf_arg(cf, 0)) {
        Ok(KeyPath::String(s)) => vec![s],
        Ok(KeyPath::List(l)) => l,
        Err(()) => return UNDEF,
    };
    let mv = cf_arg(cf, 1);
    let mode = if mv == UNDEF {
        Mode::ReadOnly
    } else {
        let Ok(s) = str16(g, mv) else { return UNDEF };
        match String::from_utf16_lossy(&s).as_str() {
            "readonly" => Mode::ReadOnly,
            "readwrite" => Mode::ReadWrite,
            "versionchange" => Mode::VersionChange,
            other => {
                return throw_type_error(
                    g,
                    &format!("Failed to execute 'transaction' on 'IDBDatabase': The provided value '{other}' is not a valid enum value of type IDBTransactionMode."),
                )
            }
        }
    };
    let ov = cf_arg(cf, 2);
    let durability = if ov == UNDEF || ov == NULL {
        Durability::Default
    } else if !is_object(g, ov) {
        return throw_type_error(g, "Failed to execute 'transaction' on 'IDBDatabase': The provided value is not of type 'IDBTransactionOptions'.");
    } else {
        let Ok(d) = try_get(g, ov, "durability") else { return UNDEF };
        if d == UNDEF {
            Durability::Default
        } else {
            let Ok(s) = str16(g, d) else { return UNDEF };
            match String::from_utf16_lossy(&s).as_str() {
                "default" => Durability::Default,
                "strict" => Durability::Strict,
                "relaxed" => Durability::Relaxed,
                other => {
                    return throw_type_error(
                        g,
                        &format!("Failed to read the 'durability' property from 'IDBTransactionOptions': The provided value '{other}' is not a valid enum value of type IDBTransactionDurability."),
                    )
                }
            }
        }
    };
    if let Some(up) = live_upgrade(g, this) {
        if tx_of(g, up).map(|t| t.state != TxState::Finished).unwrap_or(false) {
            return fail(g, "InvalidStateError", D, "transaction", "A version change transaction is running.");
        }
    }
    let Some(dd) = db_of(g, this) else { return UNDEF };
    if dd.close_pending {
        return fail(g, "InvalidStateError", D, "transaction", "The database connection is closing.");
    }
    let mut scope = names;
    scope.sort();
    scope.dedup();
    let mut ids = Vec::with_capacity(scope.len());
    for n in &scope {
        match dd.meta.store_by_name(n) {
            Some(s) => ids.push(s.id),
            None => return fail(g, "NotFoundError", D, "transaction", "One of the specified object stores was not found."),
        }
    }
    if scope.is_empty() {
        return fail(g, "InvalidAccessError", D, "transaction", "The storeNames parameter was empty.");
    }
    if mode == Mode::VersionChange {
        return throw_type_error(g, "Failed to execute 'transaction' on 'IDBDatabase': The mode provided ('versionchange') is not one of 'readonly' or 'readwrite'.");
    }
    let conn = dd.conn;
    let id = super::next_id();
    let data = TxData {
        id,
        mode,
        durability,
        state: TxState::Active,
        names: scope,
        pending: VecDeque::new(),
        outstanding: 0,
        commit_sent: false,
        upgrade: false,
        snapshot: None,
        handles: Vec::new(),
        aborted_reqs: Vec::new(),
    };
    let tx = new_transaction(g, data, this);
    let ctx = {
        let mut c = cl(g).borrow_mut();
        c.fresh.push(id);
        c.ctx
    };
    server::send(ToServer::Begin { ctx, conn, tx: id, scope: ids, mode, durability });
    if crate::web::in_coalesced_callback() {
        super::advise_d1();
    }
    tx
}

/// The upgrade transaction, active, or the exception thrown.
unsafe fn upgrade_tx(g: *mut c_void, db: u64, method: &str) -> Option<u64> {
    let Some(tx) = live_upgrade(g, db) else {
        fail(g, "InvalidStateError", D, method, "The database is not running a version change transaction.");
        return None;
    };
    if tx_of(g, tx).map(|t| t.state != TxState::Active).unwrap_or(true) {
        fail(g, "TransactionInactiveError", D, method, "The transaction is not active.");
        return None;
    }
    Some(tx)
}

unsafe extern "sysv64" fn db_create_store(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if !is_db(g, this) {
        return illegal(g);
    }
    if !args(g, cf, 1, D, "createObjectStore") {
        return UNDEF;
    }
    let Ok(name) = str16(g, cf_arg(cf, 0)) else { return UNDEF };
    let ov = cf_arg(cf, 1);
    let (auto_increment, key_path) = if ov == UNDEF || ov == NULL {
        (false, None)
    } else if !is_object(g, ov) {
        return throw_type_error(g, "Failed to execute 'createObjectStore' on 'IDBDatabase': The provided value is not of type 'IDBObjectStoreParameters'.");
    } else {
        // Dictionary members in lexicographic order.
        let Ok(ai) = try_get(g, ov, "autoIncrement") else { return UNDEF };
        let ai = crate::glue::rjs_to_boolean(g, ai) != 0;
        let Ok(kv) = try_get(g, ov, "keyPath") else { return UNDEF };
        let kp = if kv == UNDEF || kv == NULL {
            None
        } else {
            match keypath::from_js(g, kv) {
                Ok(k) => Some(k),
                Err(()) => return UNDEF,
            }
        };
        (ai, kp)
    };
    let Some(tx) = upgrade_tx(g, this, "createObjectStore") else { return UNDEF };
    if let Some(kp) = &key_path {
        if !keypath::valid(g, kp) {
            return fail(g, "SyntaxError", D, "createObjectStore", "The keyPath option is not a valid key path.");
        }
    }
    let Some(dd) = db_of(g, this) else { return UNDEF };
    if dd.meta.store_by_name(&name).is_some() {
        return fail(g, "ConstraintError", D, "createObjectStore", "An object store with the specified name already exists.");
    }
    if auto_increment && matches!(&key_path, Some(k) if k.is_list() || k.is_empty_string()) {
        return fail(
            g,
            "InvalidAccessError",
            D,
            "createObjectStore",
            "The autoIncrement option was set but the keyPath option was empty or an array.",
        );
    }
    let id = dd.alloc_id();
    let meta = StoreMeta { id, name, key_path, auto_increment, indexes: Vec::new() };
    dd.meta.stores.push(meta.clone());
    schema_op(g, tx, Op::CreateStore(meta.clone()));
    store_handle(g, tx, &meta)
}

unsafe extern "sysv64" fn db_delete_store(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if !is_db(g, this) {
        return illegal(g);
    }
    if !args(g, cf, 1, D, "deleteObjectStore") {
        return UNDEF;
    }
    let Ok(name) = str16(g, cf_arg(cf, 0)) else { return UNDEF };
    let Some(tx) = upgrade_tx(g, this, "deleteObjectStore") else { return UNDEF };
    let Some(dd) = db_of(g, this) else { return UNDEF };
    let Some(pos) = dd.meta.stores.iter().position(|s| s.name == name) else {
        return fail(g, "NotFoundError", D, "deleteObjectStore", "The specified object store was not found.");
    };
    let meta = dd.meta.stores.remove(pos);
    mark_store_deleted(g, tx, meta.id);
    schema_op(g, tx, Op::DeleteStore(meta.id));
    UNDEF
}

/// The handle of a deleted store (and its index handles) reports deleted.
unsafe fn mark_store_deleted(g: *mut c_void, tx: u64, id: StoreId) {
    let Some(i) = tx_of(g, tx).and_then(|t| t.handles.iter().position(|s| *s == id)) else { return };
    let h = get_i(g, bget(g, tx, "stores"), i as u32);
    let Some(sd) = store_of(g, h) else { return };
    sd.deleted = true;
    // "Remove all entries from its index set" (indexNames is empty at once).
    sd.meta.indexes.clear();
    let n = sd.handles.len();
    let arr = bget(g, h, "indexes");
    for j in 0..n {
        if let Some(idd) = index_of(g, get_i(g, arr, j as u32)) {
            idd.deleted = true;
        }
    }
}

unsafe extern "sysv64" fn db_close(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let Some(dd) = db_of(g, this) else { return illegal(g) };
    if !dd.close_pending {
        dd.close_pending = true;
        let conn = dd.conn;
        server::send(ToServer::Close { conn });
        let o = cl(g).borrow_mut().conns.remove(&conn);
        if let Some(o) = o {
            unprotect(g, o);
        }
    }
    UNDEF
}

// ---------------------------------------------------------------------------
// IDBTransaction
// ---------------------------------------------------------------------------

unsafe extern "sysv64" fn tx_object_store(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if !is_tx(g, this) {
        return illegal(g);
    }
    if !args(g, cf, 1, T, "objectStore") {
        return UNDEF;
    }
    let Ok(name) = str16(g, cf_arg(cf, 0)) else { return UNDEF };
    let Some(td) = tx_of(g, this) else { return UNDEF };
    if td.state == TxState::Finished {
        return fail(g, "InvalidStateError", T, "objectStore", "The transaction has finished.");
    }
    let in_scope = td.upgrade || td.names.contains(&name);
    let db = bget(g, this, "db");
    let meta = if in_scope { db_of(g, db).and_then(|d| d.meta.store_by_name(&name).cloned()) } else { None };
    let Some(meta) = meta else {
        return fail(g, "NotFoundError", T, "objectStore", "The specified object store was not found.");
    };
    store_handle(g, this, &meta)
}

unsafe extern "sysv64" fn tx_commit(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let Some(td) = tx_of(g, this) else { return illegal(g) };
    if td.state != TxState::Active {
        return fail(g, "InvalidStateError", T, "commit", "The transaction is not active.");
    }
    commit(g, this);
    UNDEF
}

unsafe extern "sysv64" fn tx_abort(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let Some(td) = tx_of(g, this) else { return illegal(g) };
    if matches!(td.state, TxState::Committing | TxState::Finished) {
        return fail(g, "InvalidStateError", T, "abort", "The transaction has already been committed or aborted.");
    }
    td.state = TxState::Inactive;
    abort_tx(g, this, NULL, true);
    UNDEF
}

unsafe extern "sysv64" fn tx_store_names(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let Some(td) = tx_of(g, this) else { return illegal(g) };
    if td.upgrade {
        let db = bget(g, this, "db");
        let names = db_of(g, db).map(|d| d.meta.stores.iter().map(|s| s.name.clone()).collect()).unwrap_or_default();
        return string_list(g, names, true);
    }
    let names = td.names.clone();
    string_list(g, names, false)
}

unsafe extern "sysv64" fn tx_mode(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(td) = tx_of(g, cf_this(cf)) else { return illegal(g) };
    js_string(
        g,
        match td.mode {
            Mode::ReadOnly => "readonly",
            Mode::ReadWrite => "readwrite",
            Mode::VersionChange => "versionchange",
        },
    )
}

unsafe extern "sysv64" fn tx_durability(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(td) = tx_of(g, cf_this(cf)) else { return illegal(g) };
    js_string(
        g,
        match td.durability {
            Durability::Default => "default",
            Durability::Strict => "strict",
            Durability::Relaxed => "relaxed",
        },
    )
}

unsafe extern "sysv64" fn tx_db(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if !is_tx(g, this) {
        return illegal(g);
    }
    bget(g, this, "db")
}

unsafe extern "sysv64" fn tx_error(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if !is_tx(g, this) {
        return illegal(g);
    }
    bget(g, this, "error")
}
