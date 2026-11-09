//! `IDBCursor` / `IDBCursorWithValue`. A cursor is stateless on the server:
//! each step sends the position, so writes made while iterating are seen
//! exactly as the spec's iteration defines (§4.10).

use core::ffi::c_void;

use super::client::*;
use super::common::*;
use super::key::Range;
use super::server::{Dir, Mode, Op, Source};
use super::svalue::Parsed;
use crate::webapi::util::*;

const C: &str = "IDBCursor";

pub(super) const CURSOR: ClassSpec<'static> = ClassSpec {
    name: "IDBCursor",
    arity: 0,
    construct: None,
    parent_proto: 0,
    methods: &[
        ("advance", 1, advance),
        ("continue", 0, continue_),
        ("continuePrimaryKey", 2, continue_primary_key),
        ("update", 1, update),
        ("delete", 0, delete),
    ],
    getters: &[
        ("source", source),
        ("direction", direction_get),
        ("key", key_get),
        ("primaryKey", primary_key),
        ("request", request),
    ],
    accessors: &[],
};

unsafe fn is_cursor(g: *mut c_void, v: u64) -> bool {
    cursor_of(g, v).is_some()
}

/// "This's source or effective object store has been deleted".
unsafe fn source_deleted(g: *mut c_void, cur: u64) -> bool {
    let src = bget(g, cur, "source");
    if let Some(sd) = store_of(g, src) {
        return sd.deleted;
    }
    if let Some(idd) = index_of(g, src) {
        if idd.deleted {
            return true;
        }
        return store_of(g, bget(g, src, "objectStore")).map(|s| s.deleted).unwrap_or(true);
    }
    true
}

/// The effective object store's handle.
unsafe fn effective_store(g: *mut c_void, cur: u64) -> u64 {
    let src = bget(g, cur, "source");
    if store_of(g, src).is_some() {
        src
    } else {
        bget(g, src, "objectStore")
    }
}

/// Not active → TransactionInactiveError, then (`write`) read-only →
/// ReadOnlyError, then deleted → InvalidStateError, then iterating →
/// InvalidStateError: the order WPT `idbcursor-*-exception-order` pins.
unsafe fn checks(g: *mut c_void, cur: u64, write: bool, method: &str) -> Option<u64> {
    let tx = bget(g, cur, "transaction");
    let Some(td) = tx_of(g, tx) else {
        illegal(g);
        return None;
    };
    if td.state != TxState::Active {
        fail(g, "TransactionInactiveError", C, method, "The transaction is not active.");
        return None;
    }
    if write && td.mode == Mode::ReadOnly {
        fail(g, "ReadOnlyError", C, method, "The record may not be updated inside a read-only transaction.");
        return None;
    }
    if source_deleted(g, cur) {
        fail(g, "InvalidStateError", C, method, "The cursor's source or effective object store has been deleted.");
        return None;
    }
    if !cursor_of(g, cur).map(|c| c.got_value).unwrap_or(false) {
        fail(g, "InvalidStateError", C, method, "The cursor is being iterated or has iterated past its end.");
        return None;
    }
    Some(tx)
}

/// One iteration request on the cursor's (reused) request.
unsafe fn step(g: *mut c_void, cur: u64, tx: u64, key: Option<Vec<u8>>, pkey: Option<Vec<u8>>, count: u32) {
    let Some(cd) = cursor_of(g, cur) else { return };
    cd.got_value = false;
    let pos = cd.pos.clone().map(|p| (p, cd.obj_pos.clone().unwrap_or_default()));
    let op = Op::Cursor { source: cd.source, range: cd.range.clone(), dir: cd.dir, key_only: cd.key_only, pos, key, pkey, count };
    let req = bget(g, cur, "request");
    let Some(id) = req_of(g, req).map(|r| r.id) else { return };
    rearm(g, tx, req, id, op);
}

#[crate::host_fn]
unsafe fn advance(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if !is_cursor(g, this) {
        return illegal(g);
    }
    if !args(g, cf, 1, C, "advance") {
        return UNDEF;
    }
    let Ok(n) = enforce_u32(g, cf_arg(cf, 0), C, "advance") else { return UNDEF };
    if n == 0 {
        return throw_type_error(g, "Failed to execute 'advance' on 'IDBCursor': A count argument with value 0 (zero) was supplied, must be greater than 0.");
    }
    let Some(tx) = checks(g, this, false, "advance") else { return UNDEF };
    step(g, this, tx, None, None, n);
    UNDEF
}

#[crate::host_fn]
unsafe fn continue_(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if !is_cursor(g, this) {
        return illegal(g);
    }
    let Some(tx) = checks(g, this, false, "continue") else { return UNDEF };
    let kv = cf_arg(cf, 0);
    let mut key = None;
    if kv != UNDEF {
        let Ok(k) = conv_key(g, kv, C, "continue") else { return UNDEF };
        let enc = k.encode();
        let Some(cd) = cursor_of(g, this) else { return UNDEF };
        if let Some(pos) = &cd.pos {
            let forward = matches!(cd.dir, Dir::Next | Dir::NextUnique);
            if forward && enc <= *pos {
                return fail(g, "DataError", C, "continue", "The parameter is less than or equal to this cursor's position.");
            }
            if !forward && enc >= *pos {
                return fail(g, "DataError", C, "continue", "The parameter is greater than or equal to this cursor's position.");
            }
        }
        key = Some(enc);
    }
    step(g, this, tx, key, None, 1);
    UNDEF
}

#[crate::host_fn]
unsafe fn continue_primary_key(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if !is_cursor(g, this) {
        return illegal(g);
    }
    if !args(g, cf, 2, C, "continuePrimaryKey") {
        return UNDEF;
    }
    let m = "continuePrimaryKey";
    let tx = bget(g, this, "transaction");
    let Some(td) = tx_of(g, tx) else { return illegal(g) };
    if td.state != TxState::Active {
        return fail(g, "TransactionInactiveError", C, m, "The transaction is not active.");
    }
    if source_deleted(g, this) {
        return fail(g, "InvalidStateError", C, m, "The cursor's source or effective object store has been deleted.");
    }
    let Some(cd) = cursor_of(g, this) else { return UNDEF };
    if !matches!(cd.source, Source::Index(..)) {
        return fail(g, "InvalidAccessError", C, m, "The cursor's source is not an index.");
    }
    if !matches!(cd.dir, Dir::Next | Dir::Prev) {
        return fail(g, "InvalidAccessError", C, m, "The cursor's direction is not 'next' or 'prev'.");
    }
    if !cd.got_value {
        return fail(g, "InvalidStateError", C, m, "The cursor is being iterated or has iterated past its end.");
    }
    let Ok(k) = conv_key(g, cf_arg(cf, 0), C, m) else { return UNDEF };
    let Ok(p) = conv_key(g, cf_arg(cf, 1), C, m) else { return UNDEF };
    let (k, p) = (k.encode(), p.encode());
    let Some(cd) = cursor_of(g, this) else { return UNDEF };
    let pos = cd.pos.clone().unwrap_or_default();
    let opos = cd.obj_pos.clone().unwrap_or_default();
    if cd.dir == Dir::Next {
        if k < pos {
            return fail(g, "DataError", C, m, "The key parameter is less than this cursor's position.");
        }
        if k == pos && p <= opos {
            return fail(g, "DataError", C, m, "The primary key is less than or equal to this cursor's position.");
        }
    } else {
        if k > pos {
            return fail(g, "DataError", C, m, "The key parameter is greater than this cursor's position.");
        }
        if k == pos && p >= opos {
            return fail(g, "DataError", C, m, "The primary key is greater than or equal to this cursor's position.");
        }
    }
    step(g, this, tx, Some(k), Some(p), 1);
    UNDEF
}

/// The cursor's effective key (the primary key of the current record).
unsafe fn effective_key(g: *mut c_void, cur: u64) -> Option<Vec<u8>> {
    let cd = cursor_of(g, cur)?;
    match cd.source {
        Source::Store(_) => cd.pos.clone(),
        Source::Index(..) => cd.obj_pos.clone(),
    }
}

#[crate::host_fn]
unsafe fn update(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if !is_cursor(g, this) {
        return illegal(g);
    }
    if !args(g, cf, 1, C, "update") {
        return UNDEF;
    }
    let Some(tx) = checks(g, this, true, "update") else { return UNDEF };
    if cursor_of(g, this).map(|c| c.key_only).unwrap_or(true) {
        return fail(g, "InvalidStateError", C, "update", "The cursor is a key cursor.");
    }
    let Ok(bytes) = clone_for(g, tx, cf_arg(cf, 0), C, "update") else { return UNDEF };
    let store = effective_store(g, this);
    let Some(sd) = store_of(g, store) else { return UNDEF };
    let (sid, kp) = (sd.meta.id, sd.meta.key_path.clone());
    let Some(ek) = effective_key(g, this) else { return UNDEF };
    if let Some(kp) = &kp {
        let same = Parsed::parse(&bytes).and_then(|p| p.extract(kp).ok().flatten()).map(|k| k.encode() == ek).unwrap_or(false);
        if !same {
            return fail(
                g,
                "DataError",
                C,
                "update",
                "The effective object store of this cursor uses in-line keys and evaluating the key path of the value parameter results in a different value than the cursor's effective key.",
            );
        }
    }
    place(g, tx, this, Op::Put { store: sid, key: Some(ek), value: bytes, no_overwrite: false })
}

#[crate::host_fn]
unsafe fn delete(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if !is_cursor(g, this) {
        return illegal(g);
    }
    let Some(tx) = checks(g, this, true, "delete") else { return UNDEF };
    if cursor_of(g, this).map(|c| c.key_only).unwrap_or(true) {
        return fail(g, "InvalidStateError", C, "delete", "The cursor is a key cursor.");
    }
    let store = effective_store(g, this);
    let Some(sid) = store_of(g, store).map(|s| s.meta.id) else { return UNDEF };
    let Some(ek) = effective_key(g, this) else { return UNDEF };
    place(g, tx, this, Op::Delete { store: sid, range: Range::only(ek) })
}

#[crate::host_fn]
unsafe fn source(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if !is_cursor(g, this) {
        return illegal(g);
    }
    bget(g, this, "source")
}

#[crate::host_fn]
unsafe fn direction_get(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(cd) = cursor_of(g, cf_this(cf)) else { return illegal(g) };
    js_string(g, dir_str(cd.dir))
}

#[crate::host_fn]
unsafe fn key_get(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if !is_cursor(g, this) {
        return illegal(g);
    }
    bget(g, this, "key")
}

#[crate::host_fn]
unsafe fn primary_key(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if !is_cursor(g, this) {
        return illegal(g);
    }
    bget(g, this, "primaryKey")
}

#[crate::host_fn]
unsafe fn request(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if !is_cursor(g, this) {
        return illegal(g);
    }
    bget(g, this, "request")
}

/// `IDBCursorWithValue.prototype.value`.
#[crate::host_fn]
pub(super) unsafe fn value(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    match cursor_of(g, this) {
        Some(cd) if !cd.key_only => bget(g, this, "value"),
        _ => illegal(g),
    }
}
