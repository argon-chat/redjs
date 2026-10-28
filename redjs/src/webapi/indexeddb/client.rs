//! The client half: per-context state, the transaction state machine (§4.1)
//! and the delivery of what the server sends — each inbox message is one
//! browser "task", run with nothing on the JS stack and followed by a
//! microtask checkpoint.
//!
//! # Where state lives
//!
//! - **Rust data of each object** (ids, flags, the schema snapshot a handle
//!   sees) is the host object's box ([`ReqData`], [`TxData`], …).
//! - **JS-valued attributes** (`result`, `error`, `source`, the `on*`
//!   handlers, the per-transaction store-handle cache, cached `keyPath`
//!   arrays, a cursor's key/value) live in a "bag": a null-prototype object
//!   under a private name on the IDB object, so the GC sees every edge and no
//!   script can reach it.
//! - **What is in flight** is in the [`Client`] maps, each entry PROTECTED
//!   while it is: a request until its event fired, a transaction until
//!   `complete`/`abort` fired, a connection until it closed — an app that
//!   drops every reference to an open database still gets `versionchange`.
//!
//! # Borrow discipline
//!
//! A host object's box is reached through a raw pointer (`host_data`), and
//! the client through a `RefCell`. Neither borrow is ever held across a call
//! that can run JavaScript (a dispatch, a key conversion, a clone): every such
//! call is followed by a fresh lookup.

use core::cell::RefCell;
use core::ffi::c_void;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::backend::*;
use super::key::{self, Range};
use super::server::{self, *};
use crate::capi::JSClassRef;
use crate::context::ContextId;
use crate::glue;
use crate::webapi::util::*;
use crate::webapi::{domexception, events, serialize};

// ---------------------------------------------------------------------------
// Per-object data
// ---------------------------------------------------------------------------

pub(super) struct ReqData {
    pub id: ReqId,
    pub done: bool,
    /// An IDBOpenDBRequest (open / deleteDatabase).
    pub open: bool,
    /// Its transaction aborted before its event fired: a late result is
    /// dropped, the `error` event comes from the abort.
    pub aborted: bool,
}

pub(super) struct DbData {
    pub conn: ConnId,
    /// The schema as this connection sees it (an upgrade's changes included).
    pub meta: DbMeta,
    pub close_pending: bool,
    /// The live upgrade transaction, 0 if none.
    pub upgrade: TxId,
    /// Ids for stores and indexes created in an upgrade: never reused within
    /// the connection, even after a delete.
    pub next_id: u64,
}

impl DbData {
    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id.max(self.meta.next_id());
        self.next_id = id + 1;
        id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TxState {
    Active,
    Inactive,
    Committing,
    Finished,
}

pub(super) struct TxData {
    pub id: TxId,
    pub mode: Mode,
    pub durability: Durability,
    pub state: TxState,
    /// The scope's names, sorted (not used for the upgrade transaction, whose
    /// scope is every store of the connection).
    pub names: Vec<Vec<u16>>,
    /// Requests placed whose event has not fired yet, in order.
    pub pending: VecDeque<ReqId>,
    /// Ops sent whose result has not come back yet.
    pub outstanding: usize,
    pub commit_sent: bool,
    pub upgrade: bool,
    /// For the upgrade transaction: the schema to revert to on abort.
    pub snapshot: Option<DbMeta>,
    /// Store ids of the handles in the bag's `stores` array, in order.
    pub handles: Vec<StoreId>,
    /// Requests the abort will fire `error` at (the queued half).
    pub aborted_reqs: Vec<ReqId>,
}

pub(super) struct StoreData {
    pub meta: StoreMeta,
    pub deleted: bool,
    /// Index ids of the handles in the bag's `indexes` array, in order.
    pub handles: Vec<IndexId>,
}

pub(super) struct IndexData {
    pub store: StoreId,
    pub meta: IndexMeta,
    pub deleted: bool,
}

pub(super) struct CursorData {
    pub source: Source,
    pub dir: Dir,
    pub range: Range,
    pub key_only: bool,
    pub pos: Option<Vec<u8>>,
    pub obj_pos: Option<Vec<u8>>,
    pub got_value: bool,
}

pub(super) struct RangeData {
    pub range: Range,
}

pub(super) struct ListData {
    pub items: Vec<Vec<u16>>,
}

pub(super) struct Unit;

// ---------------------------------------------------------------------------
// The per-context client
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub(super) struct Classes {
    pub factory: JSClassRef,
    pub request: JSClassRef,
    /// Child JSClassRefs (parent: `request` / `cursor`, so brand checks against
    /// the parent accept them): JSC reports an API object's CLASS name in
    /// `Object.prototype.toString`, not the prototype's @@toStringTag.
    pub open_request: JSClassRef,
    pub database: JSClassRef,
    pub transaction: JSClassRef,
    pub store: JSClassRef,
    pub index: JSClassRef,
    pub cursor: JSClassRef,
    pub cursor_value: JSClassRef,
    pub key_range: JSClassRef,
    pub string_list: JSClassRef,
    pub storage_manager: JSClassRef,
    pub factory_proto: u64,
    pub request_proto: u64,
    pub open_request_proto: u64,
    pub database_proto: u64,
    pub transaction_proto: u64,
    pub store_proto: u64,
    pub index_proto: u64,
    pub cursor_proto: u64,
    pub cursor_value_proto: u64,
    pub key_range_proto: u64,
    pub string_list_proto: u64,
    pub vce_proto: u64,
    pub storage_manager_proto: u64,
}

pub(super) enum PromiseKind {
    Databases,
    Estimate,
}

pub(super) struct Client {
    pub ctx: ContextId,
    pub inbox: Arc<Inbox>,
    pub cls: Classes,
    pub s_bag: *mut c_void,
    pub ident_re: u64,
    pub regexp_exec: u64,
    pub factory: u64,
    pub storage: u64,
    pub reqs: HashMap<ReqId, u64>,
    pub txs: HashMap<TxId, u64>,
    pub conns: HashMap<ConnId, u64>,
    pub promises: HashMap<ReqId, (u64, u64, PromiseKind)>,
    /// Transactions created since the last checkpoint that ran hooks.
    pub fresh: Vec<TxId>,
    /// Responses the server will send without further input from us.
    pub inflight: usize,
    /// Open/delete requests that got `blocked` (no longer counted in flight).
    pub blocked: HashSet<ReqId>,
    /// An opaque origin (a `data:` worker): `open` throws SecurityError.
    pub opaque: bool,
}

struct Ptr(*const RefCell<Client>);
unsafe impl Send for Ptr {}

fn registry() -> &'static Mutex<HashMap<usize, Ptr>> {
    static R: OnceLock<Mutex<HashMap<usize, Ptr>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn put(g: *mut c_void, c: Client) {
    let p = Box::into_raw(Box::new(RefCell::new(c)));
    registry().lock().unwrap().insert(g as usize, Ptr(p));
}

pub(super) fn try_cl(g: *mut c_void) -> Option<&'static RefCell<Client>> {
    registry().lock().unwrap().get(&(g as usize)).map(|p| unsafe { &*p.0 })
}

pub(super) fn cl(g: *mut c_void) -> &'static RefCell<Client> {
    try_cl(g).expect("IndexedDB is not installed in this context")
}

/// Drops the client of a context that is gone (no JS: its heap is gone).
pub(super) fn remove(key: usize) {
    let p = registry().lock().unwrap().remove(&key);
    if let Some(p) = p {
        drop(unsafe { Box::from_raw(p.0 as *mut RefCell<Client>) });
    }
}

pub(super) fn cls(g: *mut c_void) -> Classes {
    cl(g).borrow().cls
}

// ---- host data -------------------------------------------------------------

macro_rules! data_of {
    ($name:ident, $field:ident, $t:ty) => {
        pub(super) unsafe fn $name<'a>(g: *mut c_void, v: u64) -> Option<&'a mut $t> {
            let c = try_cl(g)?;
            let class = c.borrow().cls.$field;
            host_data::<$t>(g, class, v)
        }
    };
}

data_of!(req_of, request, ReqData);
data_of!(db_of, database, DbData);
data_of!(tx_of, transaction, TxData);
data_of!(store_of, store, StoreData);
data_of!(index_of, index, IndexData);
data_of!(cursor_of, cursor, CursorData);
data_of!(range_of, key_range, RangeData);
data_of!(list_of, string_list, ListData);

// ---- bags ------------------------------------------------------------------

pub(super) unsafe fn new_bag(g: *mut c_void, obj: u64) -> u64 {
    let b = new_object(g, NULL);
    let s = cl(g).borrow().s_bag;
    glue::rjs_define(g, obj, s, b, 1 | 2 | 4);
    b
}

pub(super) unsafe fn bag(g: *mut c_void, obj: u64) -> u64 {
    if !is_cell(obj) {
        return UNDEF;
    }
    let s = cl(g).borrow().s_bag;
    get_a(g, obj, s)
}

pub(super) unsafe fn bget(g: *mut c_void, obj: u64, name: &str) -> u64 {
    let b = bag(g, obj);
    if !is_cell(b) {
        return UNDEF;
    }
    get(g, b, name)
}

pub(super) unsafe fn bset(g: *mut c_void, obj: u64, name: &str, v: u64) {
    let b = bag(g, obj);
    if is_cell(b) {
        define(g, b, name, v, 0);
    }
}

pub(super) unsafe fn protect(g: *mut c_void, v: u64) {
    if is_cell(v) {
        crate::ffi::redjs_protect(g, v);
    }
}

pub(super) unsafe fn unprotect(g: *mut c_void, v: u64) {
    if is_cell(v) {
        crate::ffi::redjs_unprotect(g, v);
    }
}

// ---- errors ----------------------------------------------------------------

pub(super) unsafe fn dom(g: *mut c_void, f: &Fail) -> u64 {
    match f.quota {
        Some((q, r)) => domexception::create_quota_exceeded(g, &f.message, Some(q as f64), Some(r as f64)),
        None => domexception::create(g, f.name, &f.message),
    }
}

pub(super) unsafe fn abort_error(g: *mut c_void, msg: &str) -> u64 {
    domexception::create(g, "AbortError", msg)
}

// ---------------------------------------------------------------------------
// Objects the client creates
// ---------------------------------------------------------------------------

pub(super) unsafe fn new_database(g: *mut c_void, conn: ConnId, meta: DbMeta) -> u64 {
    let c = cls(g);
    let next_id = meta.next_id();
    let db = host_create(g, c.database, c.database_proto, DbData { conn, meta, close_pending: false, upgrade: 0, next_id });
    new_bag(g, db);
    for h in ["onabort", "onclose", "onerror", "onversionchange"] {
        bset(g, db, h, NULL);
    }
    protect(g, db);
    cl(g).borrow_mut().conns.insert(conn, db);
    db
}

pub(super) unsafe fn new_transaction(g: *mut c_void, data: TxData, db: u64) -> u64 {
    let c = cls(g);
    let id = data.id;
    let tx = host_create(g, c.transaction, c.transaction_proto, data);
    new_bag(g, tx);
    bset(g, tx, "db", db);
    bset(g, tx, "error", NULL);
    bset(g, tx, "stores", new_array(g));
    bset(g, tx, "openRequest", NULL);
    for h in ["onabort", "oncomplete", "onerror"] {
        bset(g, tx, h, NULL);
    }
    protect(g, tx);
    cl(g).borrow_mut().txs.insert(id, tx);
    tx
}

pub(super) unsafe fn new_request(g: *mut c_void, id: ReqId, open: bool, source: u64, tx: u64) -> u64 {
    let c = cls(g);
    let (class, proto) = if open { (c.open_request, c.open_request_proto) } else { (c.request, c.request_proto) };
    let req = host_create(g, class, proto, ReqData { id, done: false, open, aborted: false });
    new_bag(g, req);
    bset(g, req, "result", UNDEF);
    bset(g, req, "error", NULL);
    bset(g, req, "source", source);
    bset(g, req, "transaction", tx);
    bset(g, req, "cursor", UNDEF);
    for h in ["onsuccess", "onerror"] {
        bset(g, req, h, NULL);
    }
    if open {
        for h in ["onblocked", "onupgradeneeded"] {
            bset(g, req, h, NULL);
        }
    }
    req
}

/// The store handle of `tx` for store `id` — the same object every time
/// (SameObject per transaction), created from `meta` on first ask.
pub(super) unsafe fn store_handle(g: *mut c_void, tx: u64, meta: &StoreMeta) -> u64 {
    let arr = bget(g, tx, "stores");
    if let Some(i) = tx_of(g, tx).and_then(|td| td.handles.iter().position(|s| *s == meta.id)) {
        return get_i(g, arr, i as u32);
    }
    let c = cls(g);
    let h = host_create(g, c.store, c.store_proto, StoreData { meta: meta.clone(), deleted: false, handles: Vec::new() });
    new_bag(g, h);
    bset(g, h, "transaction", tx);
    bset(g, h, "indexes", new_array(g));
    bset(g, h, "keyPath", UNDEF);
    let Some(td) = tx_of(g, tx) else { return h };
    let n = td.handles.len() as u32;
    td.handles.push(meta.id);
    glue::rjs_define_index(g, arr, n, h);
    h
}

/// The index handle of store handle `store` for `meta` (SameObject).
pub(super) unsafe fn index_handle(g: *mut c_void, store: u64, meta: &IndexMeta) -> u64 {
    let arr = bget(g, store, "indexes");
    if let Some(i) = store_of(g, store).and_then(|sd| sd.handles.iter().position(|x| *x == meta.id)) {
        return get_i(g, arr, i as u32);
    }
    let c = cls(g);
    let store_id = store_of(g, store).map(|s| s.meta.id).unwrap_or(0);
    let h = host_create(g, c.index, c.index_proto, IndexData { store: store_id, meta: meta.clone(), deleted: false });
    new_bag(g, h);
    bset(g, h, "objectStore", store);
    bset(g, h, "keyPath", UNDEF);
    let Some(sd) = store_of(g, store) else { return h };
    let n = sd.handles.len() as u32;
    sd.handles.push(meta.id);
    glue::rjs_define_index(g, arr, n, h);
    h
}

pub(super) unsafe fn string_list(g: *mut c_void, mut items: Vec<Vec<u16>>, sort: bool) -> u64 {
    if sort {
        items.sort();
    }
    let c = cls(g);
    let strs: Vec<u64> = items.iter().map(|s| js_string16(g, s)).collect();
    let obj = host_create(g, c.string_list, c.string_list_proto, ListData { items });
    for (i, s) in strs.into_iter().enumerate() {
        glue::rjs_define_index(g, obj, i as u32, s);
    }
    obj
}

/// A trusted IDBVersionChangeEvent.
pub(super) unsafe fn vc_event(g: *mut c_void, ty: &str, old: u64, new: Option<u64>) -> u64 {
    let c = cls(g);
    let ev = new_object(g, c.vce_proto);
    events::init_event(g, ev, js_string(g, ty), UNDEF, false);
    if let Some(s) = events::state_of(g, ev) {
        define(g, s, "isTrusted", TRUE, 0);
        define(g, s, "idbOld", enc_f64(old as f64), 0);
        define(g, s, "idbNew", new.map(|n| enc_f64(n as f64)).unwrap_or(NULL), 0);
    }
    ev
}

// ---------------------------------------------------------------------------
// Placing requests
// ---------------------------------------------------------------------------

/// "Asynchronously execute a request": a new IDBRequest on `tx` with
/// `source`, its op sent at once (the server runs it when the transaction
/// starts). The request stays protected until its event fired.
pub(super) unsafe fn place(g: *mut c_void, tx: u64, source: u64, op: Op) -> u64 {
    let id = super::next_id();
    let req = new_request(g, id, false, source, tx);
    rearm(g, tx, req, id, op);
    req
}

/// (Re-)queues request `req` (id `id`) on `tx`: a cursor's request is reused
/// for every step.
pub(super) unsafe fn rearm(g: *mut c_void, tx: u64, req: u64, id: ReqId, op: Op) {
    let Some(td) = tx_of(g, tx) else { return };
    let txid = td.id;
    td.pending.push_back(id);
    td.outstanding += 1;
    if let Some(rd) = req_of(g, req) {
        rd.done = false;
    }
    protect(g, req);
    {
        let mut c = cl(g).borrow_mut();
        c.reqs.insert(id, req);
        c.inflight += 1;
    }
    server::send(ToServer::Op { tx: txid, req: id, op });
}

/// A schema step of the upgrade (no request: a failure aborts the upgrade).
pub(super) unsafe fn schema_op(g: *mut c_void, tx: u64, op: Op) {
    let Some(td) = tx_of(g, tx) else { return };
    server::send(ToServer::Op { tx: td.id, req: 0, op });
}

// ---------------------------------------------------------------------------
// The transaction machine
// ---------------------------------------------------------------------------

/// Auto-commit: inactive, nothing pending, not yet committing → commit.
pub(super) unsafe fn maybe_commit(g: *mut c_void, tx: u64) {
    let Some(td) = tx_of(g, tx) else { return };
    if td.state == TxState::Inactive && td.pending.is_empty() && !td.commit_sent {
        commit(g, tx);
    }
}

/// "Commit a transaction" (auto or explicit `commit()`): the server runs it
/// after every op already sent.
pub(super) unsafe fn commit(g: *mut c_void, tx: u64) {
    let Some(td) = tx_of(g, tx) else { return };
    td.state = TxState::Committing;
    td.commit_sent = true;
    let id = td.id;
    cl(g).borrow_mut().inflight += 1;
    server::send(ToServer::Commit { tx: id });
}

/// "Abort a transaction" — the synchronous half: the state, `error`, the
/// upgrade's schema revert, telling the server. The queued half (the
/// requests' `error` events, then `abort`) goes into our own inbox.
pub(super) unsafe fn abort_tx(g: *mut c_void, tx: u64, err: u64, tell_server: bool) {
    let Some(td) = tx_of(g, tx) else { return };
    if td.state == TxState::Finished {
        return;
    }
    let id = td.id;
    let owed = td.outstanding + td.commit_sent as usize;
    td.outstanding = 0;
    td.commit_sent = false;
    td.state = TxState::Finished;
    let reqs: Vec<ReqId> = td.pending.drain(..).collect();
    td.aborted_reqs = reqs.clone();
    let upgrade = td.upgrade;
    {
        let mut c = cl(g).borrow_mut();
        c.inflight = c.inflight.saturating_sub(owed);
        c.fresh.retain(|t| *t != id);
    }
    for r in &reqs {
        let o = cl(g).borrow().reqs.get(r).copied();
        if let Some(rd) = o.and_then(|o| req_of(g, o)) {
            rd.aborted = true;
        }
    }
    bset(g, tx, "error", err);
    if upgrade {
        revert_upgrade(g, tx);
    }
    let inbox = cl(g).borrow().inbox.clone();
    inbox.push(ToClient::AbortEvents { tx: id });
    if tell_server {
        server::send(ToServer::Abort { tx: id });
    }
}

/// "Abort an upgrade transaction": the connection's version and schema
/// revert, every handle of the transaction gets its name and index set
/// back, and the connection closes.
unsafe fn revert_upgrade(g: *mut c_void, tx: u64) {
    let Some(td) = tx_of(g, tx) else { return };
    let snap = td.snapshot.clone().unwrap_or_default();
    let handles = td.handles.clone();
    let db = bget(g, tx, "db");
    let conn = match db_of(g, db) {
        Some(dd) => {
            dd.meta = snap.clone();
            dd.close_pending = true;
            dd.conn
        }
        None => 0,
    };
    let o = cl(g).borrow_mut().conns.remove(&conn);
    if let Some(o) = o {
        unprotect(g, o);
    }
    // A handle whose store / index exists in the reverted schema is live again
    // (name and index set back); one created by this transaction is deleted
    // (WPT transaction-abort-*-metadata-revert).
    let arr = bget(g, tx, "stores");
    for (i, sid) in handles.iter().enumerate() {
        let h = get_i(g, arr, i as u32);
        let Some(sd) = store_of(g, h) else { continue };
        let orig = snap.store(*sid);
        match orig {
            Some(o) => {
                sd.meta = o.clone();
                sd.deleted = false;
            }
            None => {
                sd.meta.indexes.clear();
                sd.deleted = true;
            }
        }
        let ih = sd.handles.clone();
        let iarr = bget(g, h, "indexes");
        for (j, iid) in ih.iter().enumerate() {
            let x = get_i(g, iarr, j as u32);
            let Some(idd) = index_of(g, x) else { continue };
            match orig.and_then(|s| s.indexes.iter().find(|i| i.id == *iid)) {
                Some(oi) => {
                    idd.meta.name = oi.name.clone();
                    idd.deleted = false;
                }
                None => idd.deleted = true,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Does anything on `path` listen for `ty`? When nothing does, no JS can run:
/// delivery skips the event object, the dispatch and the checkpoint (§3.3's
/// fast path — what makes a bulk put cost microseconds per result).
pub(super) unsafe fn listened(g: *mut c_void, path: &[u64], ty: &str) -> bool {
    path.iter().any(|n| is_cell(*n) && events::has_listener(g, *n, ty))
}

/// A plain dispatch (no transaction state involved).
unsafe fn fire_plain(g: *mut c_void, path: &[u64], ty: &str, mk: impl FnOnce() -> u64) -> events::DispatchOutcome {
    if !listened(g, path, ty) {
        return events::DispatchOutcome::default();
    }
    let ev = mk();
    events::dispatch_path(g, ev, path)
}

/// "Fire a success event" / "fire an error event" (and `upgradeneeded`, the
/// same shape on the upgrade transaction): the transaction is active for the
/// dispatch; then a listener that threw aborts it with AbortError, an `error`
/// nobody canceled aborts it with the request's error, and otherwise it
/// commits once nothing is pending.
unsafe fn fire_request_event(g: *mut c_void, path: &[u64], tx: u64, ty: &str, error: bool, mk: impl FnOnce() -> u64) {
    if let Some(td) = tx_of(g, tx) {
        if td.state == TxState::Inactive {
            td.state = TxState::Active;
        }
    }
    let out = fire_plain(g, path, ty, mk);
    let Some(td) = tx_of(g, tx) else { return };
    if td.state == TxState::Committing {
        // After an explicit commit(): an error nobody canceled still aborts
        // (WPT idb-explicit-commit) — the server holds the commit until we
        // say — but a listener that throws does not stop the commit (WPT
        // idb-explicit-commit-throw).
        if error && !out.canceled {
            let e = bget(g, path[0], "error");
            abort_tx(g, tx, e, true);
        }
        return;
    }
    if td.state != TxState::Active {
        return;
    }
    td.state = TxState::Inactive;
    if out.listener_threw {
        abort_tx(g, tx, abort_error(g, "A listener threw, so the transaction was aborted."), true);
    } else if error && !out.canceled {
        let e = bget(g, path[0], "error");
        abort_tx(g, tx, e, true);
    } else {
        maybe_commit(g, tx);
    }
}

// ---------------------------------------------------------------------------
// The pump source and the checkpoint hook
// ---------------------------------------------------------------------------

pub(super) unsafe fn src_inflight(g: *mut c_void) -> bool {
    try_cl(g).map(|c| c.borrow().inflight > 0).unwrap_or(false)
}

pub(super) unsafe fn src_ready(g: *mut c_void) -> bool {
    try_cl(g).map(|c| c.borrow().inbox.ready()).unwrap_or(false)
}

/// Delivers what is queued, one task each, within the pump budget (0 = what
/// was queued when we started, never waiting for more).
pub(super) unsafe fn src_drain(g: *mut c_void) -> usize {
    let Some(c) = try_cl(g) else { return 0 };
    let inbox = c.borrow().inbox.clone();
    if !inbox.ready() {
        return 0;
    }
    // Microtasks left by earlier tasks (timers, rAF) finish before an IDB
    // transaction becomes active: browser order.
    crate::web::checkpoint(g);
    let budget_us = crate::web::pump_budget_us();
    let cap = if budget_us == 0 { inbox.len() } else { usize::MAX };
    let budget = Duration::from_micros(budget_us as u64);
    let start = Instant::now();
    let mut n = 0usize;
    while n < cap {
        let Some(m) = inbox.pop() else { break };
        deliver(g, m);
        crate::web::checkpoint(g);
        n += 1;
        if budget_us != 0 && start.elapsed() >= budget {
            break;
        }
    }
    n
}

/// HTML "cleanup Indexed Database transactions": every transaction created
/// since the last checkpoint is deactivated, and one with nothing pending
/// commits.
pub(super) unsafe fn on_checkpoint(g: *mut c_void) {
    let Some(c) = try_cl(g) else { return };
    let fresh = {
        let mut cb = c.borrow_mut();
        if cb.fresh.is_empty() {
            return;
        }
        core::mem::take(&mut cb.fresh)
    };
    for t in fresh {
        let o = c.borrow().txs.get(&t).copied();
        let Some(o) = o else { continue };
        if let Some(td) = tx_of(g, o) {
            if td.state == TxState::Active {
                td.state = TxState::Inactive;
            }
        }
        maybe_commit(g, o);
    }
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

unsafe fn deliver(g: *mut c_void, m: ToClient) {
    match m {
        ToClient::Result { req, payload } => on_result(g, req, Ok(payload)),
        ToClient::RequestFailed { req, err } => on_result(g, req, Err(err)),
        ToClient::Completed { tx } => on_completed(g, tx),
        ToClient::Aborted { tx, err } => {
            let o = cl(g).borrow().txs.get(&tx).copied();
            if let Some(o) = o {
                let e = match err {
                    Some(f) => dom(g, &f),
                    None => NULL,
                };
                abort_tx(g, o, e, false);
            }
        }
        ToClient::AbortEvents { tx } => on_abort_events(g, tx),
        ToClient::Upgrade { req, conn, tx, old, meta } => on_upgrade(g, req, conn, tx, old, meta),
        ToClient::Opened { req, conn, meta } => {
            let Some(open) = take_open(g, req, false) else {
                server::send(ToServer::Close { conn });
                return;
            };
            let db = new_database(g, conn, meta);
            bset(g, open, "result", db);
            succeed_open(g, open);
        }
        ToClient::OpenFailed { req, err } => {
            let Some(open) = take_open(g, req, false) else { return };
            let e = dom(g, &err);
            fail_open(g, open, e);
        }
        ToClient::Blocked { req, old, new } => {
            let open = {
                let mut c = cl(g).borrow_mut();
                if c.blocked.insert(req) {
                    c.inflight = c.inflight.saturating_sub(1);
                }
                c.reqs.get(&req).copied()
            };
            if let Some(open) = open {
                fire_plain(g, &[open], "blocked", || vc_event(g, "blocked", old, new));
            }
        }
        ToClient::VersionChange { conn, old, new } => {
            let db = cl(g).borrow().conns.get(&conn).copied();
            if let Some(db) = db {
                if db_of(g, db).map(|d| !d.close_pending).unwrap_or(false) {
                    fire_plain(g, &[db], "versionchange", || vc_event(g, "versionchange", old, new));
                }
            }
            server::send(ToServer::VersionChangeDone { conn });
        }
        ToClient::Deleted { req, old } => {
            let Some(open) = take_open(g, req, false) else { return };
            if let Some(rd) = req_of(g, open) {
                rd.done = true;
            }
            bset(g, open, "result", UNDEF);
            bset(g, open, "error", NULL);
            fire_plain(g, &[open], "success", || vc_event(g, "success", old, None));
            finish_open(g, open);
        }
        ToClient::DatabaseList { req, list } => {
            let p = take_promise(g, req);
            if let Some((res, rej)) = p {
                let arr = new_array(g);
                for (i, (name, version)) in list.iter().enumerate() {
                    let o = new_object(g, UNDEF);
                    define(g, o, "name", js_string16(g, name), 0);
                    define(g, o, "version", enc_f64(*version as f64), 0);
                    glue::rjs_define_index(g, arr, i as u32, o);
                }
                let _ = call(g, res, UNDEF, &[arr]);
                unprotect(g, res);
                unprotect(g, rej);
            }
        }
        ToClient::DatabasesFailed { req, err } => {
            if let Some((res, rej)) = take_promise(g, req) {
                let e = dom(g, &err);
                let _ = call(g, rej, UNDEF, &[e]);
                unprotect(g, res);
                unprotect(g, rej);
            }
        }
        ToClient::Estimate { req, usage, quota } => {
            let p = take_promise(g, req);
            if let Some((res, rej)) = p {
                let o = new_object(g, UNDEF);
                define(g, o, "quota", enc_f64(quota as f64), 0);
                define(g, o, "usage", enc_f64(usage as f64), 0);
                let _ = call(g, res, UNDEF, &[o]);
                unprotect(g, res);
                unprotect(g, rej);
            }
        }
        ToClient::ForcedClose { conn } => {
            let db = cl(g).borrow_mut().conns.remove(&conn);
            if let Some(db) = db {
                if let Some(dd) = db_of(g, db) {
                    dd.close_pending = true;
                }
                fire_plain(g, &[db], "close", || events::new_trusted_event(g, "close"));
                unprotect(g, db);
            }
        }
    }
}

unsafe fn take_promise(g: *mut c_void, req: ReqId) -> Option<(u64, u64)> {
    let mut c = cl(g).borrow_mut();
    let p = c.promises.remove(&req)?;
    c.inflight = c.inflight.saturating_sub(1);
    Some((p.0, p.1))
}

/// The open/delete request `req`, its in-flight count settled. `keep` leaves
/// it registered (the upgrade path finishes it later).
unsafe fn take_open(g: *mut c_void, req: ReqId, _keep: bool) -> Option<u64> {
    let mut c = cl(g).borrow_mut();
    if !c.blocked.remove(&req) {
        c.inflight = c.inflight.saturating_sub(1);
    }
    c.reqs.get(&req).copied()
}

/// The open request is done with: unregistered and unprotected.
unsafe fn finish_open(g: *mut c_void, open: u64) {
    if let Some(rd) = req_of(g, open) {
        let id = rd.id;
        cl(g).borrow_mut().reqs.remove(&id);
    }
    unprotect(g, open);
}

unsafe fn succeed_open(g: *mut c_void, open: u64) {
    if let Some(rd) = req_of(g, open) {
        rd.done = true;
    }
    bset(g, open, "error", NULL);
    fire_plain(g, &[open], "success", || events::new_trusted_event_with(g, "success", false, false));
    finish_open(g, open);
}

unsafe fn fail_open(g: *mut c_void, open: u64, err: u64) {
    if let Some(rd) = req_of(g, open) {
        rd.done = true;
    }
    bset(g, open, "result", UNDEF);
    bset(g, open, "error", err);
    fire_plain(g, &[open], "error", || events::new_trusted_event_with(g, "error", true, true));
    finish_open(g, open);
}

unsafe fn on_upgrade(g: *mut c_void, req: ReqId, conn: ConnId, tx: TxId, old: u64, meta: DbMeta) {
    let Some(open) = take_open(g, req, true) else {
        server::send(ToServer::Abort { tx });
        server::send(ToServer::Close { conn });
        return;
    };
    let new = meta.version;
    let mut snap = meta.clone();
    snap.version = old;
    if old == 0 {
        snap.stores.clear();
    }
    let db = new_database(g, conn, meta);
    let data = TxData {
        id: tx,
        mode: Mode::VersionChange,
        durability: Durability::Default,
        state: TxState::Inactive,
        names: Vec::new(),
        pending: VecDeque::new(),
        outstanding: 0,
        commit_sent: false,
        upgrade: true,
        snapshot: Some(snap),
        handles: Vec::new(),
        aborted_reqs: Vec::new(),
    };
    let txo = new_transaction(g, data, db);
    bset(g, txo, "openRequest", open);
    if let Some(dd) = db_of(g, db) {
        dd.upgrade = tx;
    }
    if let Some(rd) = req_of(g, open) {
        rd.done = true;
    }
    bset(g, open, "result", db);
    bset(g, open, "error", NULL);
    bset(g, open, "transaction", txo);
    fire_request_event(g, &[open], txo, "upgradeneeded", false, || vc_event(g, "upgradeneeded", old, Some(new)));
}

unsafe fn on_completed(g: *mut c_void, tx: TxId) {
    let o = cl(g).borrow().txs.get(&tx).copied();
    let Some(txo) = o else { return };
    let Some(td) = tx_of(g, txo) else { return };
    if td.state == TxState::Finished {
        return;
    }
    if td.commit_sent {
        td.commit_sent = false;
        let mut c = cl(g).borrow_mut();
        c.inflight = c.inflight.saturating_sub(1);
    }
    td.state = TxState::Finished;
    let upgrade = td.upgrade;
    let db = bget(g, txo, "db");
    if upgrade {
        // "Set the database's upgrade transaction to null" comes before
        // `complete` (WPT upgrade-transaction-lifecycle-committed).
        if let Some(dd) = db_of(g, db) {
            dd.upgrade = 0;
        }
    }
    fire_plain(g, &[txo, db], "complete", || events::new_trusted_event_with(g, "complete", false, false));
    if upgrade {
        crate::web::checkpoint(g);
        let open = bget(g, txo, "openRequest");
        bset(g, open, "transaction", NULL);
        let closed = db_of(g, db).map(|dd| dd.close_pending).unwrap_or(true);
        if closed {
            // close() during upgradeneeded: the upgrade committed, the open
            // fails (WPT close-in-upgradeneeded).
            bset(g, open, "result", UNDEF);
            fail_open(g, open, abort_error(g, "The connection was closed before the upgrade finished."));
        } else {
            succeed_open(g, open);
        }
    }
    cl(g).borrow_mut().txs.remove(&tx);
    unprotect(g, txo);
}

unsafe fn on_abort_events(g: *mut c_void, tx: TxId) {
    let o = cl(g).borrow().txs.get(&tx).copied();
    let Some(txo) = o else { return };
    let Some(td) = tx_of(g, txo) else { return };
    let reqs = core::mem::take(&mut td.aborted_reqs);
    let upgrade = td.upgrade;
    let db = bget(g, txo, "db");
    for r in reqs {
        let o = cl(g).borrow_mut().reqs.remove(&r);
        let Some(o) = o else { continue };
        if let Some(rd) = req_of(g, o) {
            rd.done = true;
        }
        bset(g, o, "result", UNDEF);
        bset(g, o, "error", abort_error(g, "The transaction was aborted, so the request cannot be fulfilled."));
        fire_plain(g, &[o, txo, db], "error", || events::new_trusted_event_with(g, "error", true, true));
        unprotect(g, o);
        crate::web::checkpoint(g);
    }
    if upgrade {
        if let Some(dd) = db_of(g, db) {
            dd.upgrade = 0;
        }
    }
    fire_plain(g, &[txo, db], "abort", || events::new_trusted_event_with(g, "abort", true, false));
    if upgrade {
        crate::web::checkpoint(g);
        let open = bget(g, txo, "openRequest");
        if is_cell(open) {
            bset(g, open, "transaction", NULL);
            fail_open(g, open, abort_error(g, "The upgrade transaction was aborted."));
        }
    }
    cl(g).borrow_mut().txs.remove(&tx);
    unprotect(g, txo);
}

unsafe fn on_result(g: *mut c_void, req: ReqId, r: Result<Payload, Fail>) {
    let o = cl(g).borrow().reqs.get(&req).copied();
    let Some(ro) = o else { return };
    let Some(rd) = req_of(g, ro) else { return };
    if rd.aborted || rd.done {
        return;
    }
    let txo = bget(g, ro, "transaction");
    let Some(td) = tx_of(g, txo) else { return };
    if td.state == TxState::Finished {
        return;
    }
    td.outstanding = td.outstanding.saturating_sub(1);
    if let Some(p) = td.pending.iter().position(|x| *x == req) {
        td.pending.remove(p);
    }
    {
        let mut c = cl(g).borrow_mut();
        c.inflight = c.inflight.saturating_sub(1);
        c.reqs.remove(&req);
    }
    let server_failed = r.is_err();
    let (value, error) = match r {
        Ok(p) => match result_value(g, ro, p) {
            Ok(v) => (v, NULL),
            Err(e) => (UNDEF, e),
        },
        Err(f) => (UNDEF, dom(g, &f)),
    };
    if let Some(rd) = req_of(g, ro) {
        rd.done = true;
    }
    bset(g, ro, "result", value);
    bset(g, ro, "error", error);
    let db = bget(g, txo, "db");
    let path = [ro, txo, db];
    if error == NULL {
        fire_request_event(g, &path, txo, "success", false, || events::new_trusted_event_with(g, "success", false, false));
    } else {
        fire_request_event(g, &path, txo, "error", true, || events::new_trusted_event_with(g, "error", true, true));
    }
    if server_failed {
        // The server holds this transaction's commit until the failure is
        // handled; it lives on (canceled), so let it go.
        if let Some(td) = tx_of(g, txo) {
            if td.state != TxState::Finished {
                server::send(ToServer::ErrorHandled { tx: td.id });
            }
        }
    }
    unprotect(g, ro);
}

unsafe fn value_to_js(g: *mut c_void, v: &Value) -> Result<u64, u64> {
    if v.len() > 64 << 20 {
        super::advise_big_value(v.len());
    }
    serialize::deserialize_storage(g, v)
}

unsafe fn result_value(g: *mut c_void, req: u64, p: Payload) -> Result<u64, u64> {
    Ok(match p {
        Payload::Undefined | Payload::Value(None) => UNDEF,
        Payload::Key(k) => key::bytes_to_js(g, &k),
        Payload::Value(Some(v)) => value_to_js(g, &v)?,
        Payload::Keys(ks) => {
            let arr = new_array(g);
            for (i, k) in ks.iter().enumerate() {
                glue::rjs_define_index(g, arr, i as u32, key::bytes_to_js(g, k));
            }
            arr
        }
        Payload::Values(vs) => {
            let arr = new_array(g);
            for (i, v) in vs.iter().enumerate() {
                let x = value_to_js(g, v)?;
                glue::rjs_define_index(g, arr, i as u32, x);
            }
            arr
        }
        Payload::Count(n) => enc_f64(n as f64),
        Payload::Cursor(hit) => {
            let cur = bget(g, req, "cursor");
            let Some(cd) = cursor_of(g, cur) else { return Ok(NULL) };
            let (store_cursor, key_only) = (matches!(cd.source, Source::Store(_)), cd.key_only);
            match hit {
                Some(h) => {
                    let k = key::bytes_to_js(g, &h.key);
                    let pk = if store_cursor { k } else { key::bytes_to_js(g, &h.pkey) };
                    let val = match (&h.value, key_only) {
                        (Some(v), false) => value_to_js(g, v)?,
                        _ => UNDEF,
                    };
                    if let Some(cd) = cursor_of(g, cur) {
                        cd.pos = Some(h.key);
                        cd.obj_pos = Some(h.pkey);
                        cd.got_value = true;
                    }
                    bset(g, cur, "key", k);
                    bset(g, cur, "primaryKey", pk);
                    bset(g, cur, "value", val);
                    cur
                }
                None => {
                    if let Some(cd) = cursor_of(g, cur) {
                        cd.pos = None;
                        cd.obj_pos = None;
                        cd.got_value = false;
                    }
                    bset(g, cur, "key", UNDEF);
                    bset(g, cur, "primaryKey", UNDEF);
                    bset(g, cur, "value", UNDEF);
                    NULL
                }
            }
        }
    })
}
