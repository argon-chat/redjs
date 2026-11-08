//! The IndexedDB server: ONE thread per process that owns every database.
//!
//! Every JS context is a client. It sends [`ToServer`] messages (plain Rust
//! data — encoded keys, serialized values, small enums) and receives
//! [`ToClient`] messages through its own [`Inbox`], which the context's pump
//! drains on its own thread. JS values never cross; the server never touches
//! JSC. That is the whole of "two contexts on one database are safe": each
//! context touches only its own objects, and every database has exactly one
//! owner, this thread (the `websocket.rs` invariant).
//!
//! What lives here:
//!
//! - **The connection queue per database name** — opens and deletes run
//!   strictly in order, with the versionchange → (acks) → blocked → wait for
//!   closes dance, then the upgrade transaction or the delete (§4.4).
//! - **The scheduler** — per database, transactions in creation order; one may
//!   start when no earlier unfinished transaction overlaps its scope with
//!   either side readwrite. At most one readwrite transaction RUNS per
//!   database (IDB-D2), which is what lets one backend write transaction host
//!   them all. Read-only ones run concurrently with each other.
//! - **Execution** — a running transaction executes its requests in arrival
//!   order as they arrive, so the server works while the client dispatches
//!   (pipelining). Results go back in order; the inbox is FIFO, so events fire
//!   in request order (WPT `request-event-ordering-*`).
//! - **Store-a-record, indexes, the key generator, cursors** — on top of the
//!   ordered primitives of [`super::backend`]. Cursors are stateless here:
//!   every `continue` carries the cursor's position, which is the spec's own
//!   definition of iteration and makes "modify the store while iterating it"
//!   correct with nothing to invalidate.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Bound;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};

use super::backend::*;
use super::key::{self, Key, Range, AFTER};
use super::keypath::KeyPath;
use super::svalue::{self, Parsed};
use crate::context::ContextId;

pub type ConnId = u64;
pub type TxId = u64;
pub type ReqId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    ReadOnly,
    ReadWrite,
    VersionChange,
}

/// A DOMException to be: its name and message (+ the quota numbers).
#[derive(Clone, Debug, PartialEq)]
pub struct Fail {
    pub name: &'static str,
    pub message: String,
    pub quota: Option<(u64, u64)>,
}

impl Fail {
    pub fn new(name: &'static str, message: impl Into<String>) -> Fail {
        Fail { name, message: message.into(), quota: None }
    }
}

fn from_backend(e: BackendError) -> Fail {
    match e {
        BackendError::Quota { quota, requested } => Fail {
            name: "QuotaExceededError",
            message: "The quota has been exceeded.".into(),
            quota: Some((quota, requested)),
        },
        BackendError::Locked(d) => Fail::new("UnknownError", d),
        BackendError::Io(d) => Fail::new("UnknownError", d),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Next,
    NextUnique,
    Prev,
    PrevUnique,
}

impl Dir {
    fn forward(self) -> bool {
        matches!(self, Dir::Next | Dir::NextUnique)
    }
    fn unique(self) -> bool {
        matches!(self, Dir::NextUnique | Dir::PrevUnique)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Store(StoreId),
    Index(StoreId, IndexId),
}

impl Source {
    fn store(self) -> StoreId {
        match self {
            Source::Store(s) | Source::Index(s, _) => s,
        }
    }
}

#[derive(Debug)]
pub enum Op {
    CreateStore(StoreMeta),
    DeleteStore(StoreId),
    RenameStore(StoreId, Vec<u16>),
    CreateIndex(StoreId, IndexMeta),
    DeleteIndex(StoreId, IndexId),
    RenameIndex(StoreId, IndexId, Vec<u16>),
    /// `key: None` = generate one (and inject it, for an in-line key path).
    Put { store: StoreId, key: Option<Vec<u8>>, value: Vec<u8>, no_overwrite: bool },
    /// `keys`: the primary key instead of the value (`getKey`).
    Get { source: Source, range: Range, keys: bool },
    GetAll { source: Source, range: Range, count: u32, keys: bool },
    Count { source: Source, range: Range },
    Delete { store: StoreId, range: Range },
    Clear { store: StoreId },
    /// Open (`pos: None`) or move a cursor. `pos` = (position, object store
    /// position); `key`/`pkey` = `continue(key)` / `continuePrimaryKey`.
    Cursor {
        source: Source,
        range: Range,
        dir: Dir,
        key_only: bool,
        pos: Option<(Vec<u8>, Vec<u8>)>,
        key: Option<Vec<u8>>,
        pkey: Option<Vec<u8>>,
        count: u32,
    },
}

#[derive(Debug)]
pub struct CursorHit {
    pub key: Vec<u8>,
    pub pkey: Vec<u8>,
    pub value: Option<Value>,
}

#[derive(Debug)]
pub enum Payload {
    Undefined,
    Key(Vec<u8>),
    Value(Option<Value>),
    Keys(Vec<Vec<u8>>),
    Values(Vec<Value>),
    Count(u64),
    Cursor(Option<CursorHit>),
}

pub enum ToServer {
    Open { ctx: ContextId, req: ReqId, name: Vec<u16>, version: Option<u64> },
    Delete { ctx: ContextId, req: ReqId, name: Vec<u16> },
    Databases { ctx: ContextId, req: ReqId },
    Estimate { ctx: ContextId, req: ReqId },
    Begin { ctx: ContextId, conn: ConnId, tx: TxId, scope: Vec<StoreId>, mode: Mode, durability: Durability },
    Op { tx: TxId, req: ReqId, op: Op },
    Commit { tx: TxId },
    Abort { tx: TxId },
    /// The `versionchange` event was fired at this connection.
    VersionChangeDone { conn: ConnId },
    /// The client handled a failed request's `error` event and the transaction
    /// lives on (canceled): a held commit may proceed.
    ErrorHandled { tx: TxId },
    Close { conn: ConnId },
    ContextGone { ctx: ContextId },
    Inspect { query: super::inspection::Query, reply: Sender<Result<super::inspection::Response, String>> },
    Shutdown { done: Sender<()> },
}

#[derive(Debug)]
pub enum ToClient {
    Upgrade { req: ReqId, conn: ConnId, tx: TxId, old: u64, meta: DbMeta },
    Opened { req: ReqId, conn: ConnId, meta: DbMeta },
    OpenFailed { req: ReqId, err: Fail },
    Blocked { req: ReqId, old: u64, new: Option<u64> },
    VersionChange { conn: ConnId, old: u64, new: Option<u64> },
    Result { req: ReqId, payload: Payload },
    RequestFailed { req: ReqId, err: Fail },
    Completed { tx: TxId },
    Aborted { tx: TxId, err: Option<Fail> },
    Deleted { req: ReqId, old: u64 },
    DatabaseList { req: ReqId, list: Vec<(Vec<u16>, u64)> },
    /// `databases()` could not be answered (another instance holds the folder).
    DatabasesFailed { req: ReqId, err: Fail },
    Estimate { req: ReqId, usage: u64, quota: u64 },
    /// The backend lost the database under a live connection (I/O failure,
    /// corruption, the file gone): `close` fires at the IDBDatabase. Never
    /// sent by the memory backend; the SQLite one (P2) needs it.
    ForcedClose { conn: ConnId },
    /// Client-internal, pushed by a context into its OWN inbox: the queued
    /// half of "abort a transaction" (the `error` events at its pending
    /// requests, then `abort`), after the synchronous half already ran.
    AbortEvents { tx: TxId },
}

// ---------------------------------------------------------------------------
// Inboxes — one per context, keyed by ContextId
// ---------------------------------------------------------------------------

pub struct Inbox {
    ctx: ContextId,
    q: Mutex<VecDeque<ToClient>>,
    len: AtomicUsize,
}

impl Inbox {
    pub fn new(ctx: ContextId) -> Inbox {
        Inbox { ctx, q: Mutex::new(VecDeque::new()), len: AtomicUsize::new(0) }
    }

    pub(super) fn push(&self, m: ToClient) {
        self.q.lock().unwrap().push_back(m);
        self.len.fetch_add(1, Ordering::Release);
        crate::web::wake(self.ctx);
    }

    pub fn pop(&self) -> Option<ToClient> {
        let m = self.q.lock().unwrap().pop_front();
        if m.is_some() {
            self.len.fetch_sub(1, Ordering::AcqRel);
        }
        m
    }

    pub fn ready(&self) -> bool {
        self.len.load(Ordering::Acquire) > 0
    }

    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }
}

fn inboxes() -> &'static Mutex<HashMap<u64, Arc<Inbox>>> {
    static I: OnceLock<Mutex<HashMap<u64, Arc<Inbox>>>> = OnceLock::new();
    I.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn register_inbox(ctx: ContextId) -> Arc<Inbox> {
    inboxes().lock().unwrap().entry(ctx.0).or_insert_with(|| Arc::new(Inbox::new(ctx))).clone()
}

/// Drops a context's inbox at once: whatever the server sends it later is
/// discarded (and can never reach a newcomer — ids are never reused).
pub fn drop_inbox(ctx: ContextId) {
    let i = inboxes().lock().unwrap().remove(&ctx.0);
    drop(i);
}

fn deliver(ctx: ContextId, m: ToClient) {
    let i = inboxes().lock().unwrap().get(&ctx.0).cloned();
    if let Some(i) = i {
        i.push(m);
    }
}

// ---------------------------------------------------------------------------
// The thread
// ---------------------------------------------------------------------------

/// The backend factory, called once on the server thread at its start.
pub type BackendFactory = Box<dyn FnOnce() -> Box<dyn Backend> + Send>;

fn factory() -> &'static Mutex<Option<BackendFactory>> {
    static F: OnceLock<Mutex<Option<BackendFactory>>> = OnceLock::new();
    F.get_or_init(|| Mutex::new(None))
}

/// Sets what backend the server will start with. Effective only before the
/// first IndexedDB use in the process (the server latches it).
pub fn set_backend_factory(f: BackendFactory) {
    *factory().lock().unwrap() = Some(f);
}

fn sender() -> &'static Mutex<Option<Sender<ToServer>>> {
    static S: OnceLock<Mutex<Option<Sender<ToServer>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

/// Is the server running (has anything ever touched IndexedDB)?
pub fn started() -> bool {
    sender().lock().unwrap().is_some()
}

pub fn send(m: ToServer) {
    let mut s = sender().lock().unwrap();
    if s.is_none() {
        let (tx, rx) = channel();
        let make = factory().lock().unwrap().take();
        std::thread::Builder::new()
            .name("redjs-indexeddb".into())
            // Values nest as deep as the JS stack allows; the parser is
            // iterative, but the key conversion of a deep array key is not.
            .stack_size(16 << 20)
            .spawn(move || run(rx, make))
            .expect("spawn the IndexedDB server thread");
        *s = Some(tx);
    }
    let _ = s.as_ref().unwrap().send(m);
}

/// Inspector messages never initialize storage or create a database.
pub fn send_inspection(query: super::inspection::Query, reply: Sender<Result<super::inspection::Response, String>>) -> Result<bool, String> {
    let sender = sender().lock().unwrap();
    let Some(sender) = sender.as_ref() else { return Ok(false); };
    sender.send(ToServer::Inspect { query, reply }).map_err(|_| "IndexedDB server is unavailable".to_string())?;
    Ok(true)
}

fn run(rx: Receiver<ToServer>, make: Option<BackendFactory>) {
    let backend: Box<dyn Backend> = match make {
        Some(f) => f(),
        None => match super::host() {
            // A host says where the bytes live: SQLite in its folder, with
            // its key — or, if it cannot say, a backend that refuses every
            // open with the reason. Never a silent memory fallback.
            #[cfg(feature = "indexeddb-sqlite")]
            Some(h) => super::sqlite::from_host(h),
            // A build without the SQLite backend: memory, at the host's quota.
            #[cfg(not(feature = "indexeddb-sqlite"))]
            Some(h) => Box::new(super::memory::MemoryBackend::new(h.quota_bytes(0))),
            // No host: memory only, and the advisory below says so.
            None => Box::new(super::memory::MemoryBackend::new(super::DEFAULT_QUOTA)),
        },
    };
    super::latch(backend.describe());
    if !backend.persistent() {
        super::advise_memory_only(&backend.describe());
    }
    let mut s = Server { backend, dbs: HashMap::new(), conns: HashMap::new(), txs: HashMap::new(), dirty: Vec::new() };
    while let Ok(m) = rx.recv() {
        if let ToServer::Shutdown { done } = m {
            s.shutdown();
            let _ = done.send(());
            continue;
        }
        s.handle(m);
        s.settle();
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

enum Pending {
    Open { ctx: ContextId, req: ReqId, version: Option<u64> },
    Delete { ctx: ContextId, req: ReqId },
}

impl Pending {
    fn ctx(&self) -> ContextId {
        match self {
            Pending::Open { ctx, .. } | Pending::Delete { ctx, .. } => *ctx,
        }
    }
}

enum Active {
    /// versionchange sent to these connections; waiting for them to have
    /// fired it, then for every other connection to close.
    Waiting { pending: Pending, conn: Option<ConnId>, old: u64, new: Option<u64>, acks: HashSet<ConnId>, blocked_sent: bool },
    /// The upgrade transaction is running.
    Upgrading { conn: ConnId, tx: TxId, old: u64 },
}

#[derive(Default)]
struct DbEntry {
    handle: Option<Box<dyn Database>>,
    /// Connections not yet fully closed (close-pending ones included).
    conns: Vec<ConnId>,
    queue: VecDeque<Pending>,
    active: Option<Active>,
    /// Unfinished transactions in creation order.
    txs: Vec<TxId>,
}

struct Conn {
    ctx: ContextId,
    db: Vec<u16>,
    close_pending: bool,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum TxState {
    Waiting,
    Running,
}

struct Tx {
    ctx: ContextId,
    conn: ConnId,
    db: Vec<u16>,
    mode: Mode,
    durability: Durability,
    scope: Vec<StoreId>,
    state: TxState,
    ops: VecDeque<(ReqId, Op)>,
    commit: bool,
    /// Failed requests whose `error` event the client has not handled yet:
    /// the commit waits for them (an uncanceled error aborts, even after an
    /// explicit `commit()` — WPT idb-explicit-commit).
    held: usize,
}

struct Server {
    backend: Box<dyn Backend>,
    dbs: HashMap<Vec<u16>, DbEntry>,
    conns: HashMap<ConnId, Conn>,
    txs: HashMap<TxId, Tx>,
    /// Databases whose queue / scheduler need another look.
    dirty: Vec<Vec<u16>>,
}

impl Server {
    fn inspect(&mut self, query: super::inspection::Query) -> Result<super::inspection::Response, String> {
        use super::inspection::{Query, Response, DatabaseInfo};
        if matches!(query, Query::List) {
            let list = self.backend.inspect_databases().map_err(|error| format!("IndexedDB inspection failed: {error:?}"))?;
            let truncated = list.len() > 256;
            let databases = list.into_iter().take(256).filter_map(|(name, version)| {
                let version = match self.dbs.get(&name).and_then(|entry| entry.active.as_ref()) { Some(Active::Upgrading { old, .. }) => *old, _ => version };
                (version != 0).then(|| DatabaseInfo { name: super::inspection::text(&name), version })
            }).collect();
            return Ok(Response::List { available: true, backend: self.backend.describe(), databases, truncated });
        }
        let name = query.database().ok_or("Missing database name")?.to_vec();
        let busy = self.txs.values().any(|transaction| transaction.db == name && transaction.mode != Mode::ReadOnly)
            || self.dbs.get(&name).is_some_and(|entry| entry.active.is_some() || !entry.queue.is_empty());
        if busy { return Err("IndexedDB database is busy with a write or version change; retry after the transaction completes".into()); }
        if let Some(database) = self.dbs.get_mut(&name).and_then(|entry| entry.handle.as_deref_mut()) {
            return super::inspection::read(database, query);
        }
        let mut database = self.backend.inspect_existing(&name).map_err(|error| format!("IndexedDB inspection failed: {error:?}"))?;
        super::inspection::read(&mut *database, query)
    }

    fn mark(&mut self, db: &[u16]) {
        if !self.dirty.iter().any(|d| d == db) {
            self.dirty.push(db.to_vec());
        }
    }

    fn handle(&mut self, m: ToServer) {
        match m {
            ToServer::Inspect { query, reply } => {
                let result = self.inspect(query);
                let _ = reply.send(result);
            }
            ToServer::Open { ctx, req, name, version } => {
                self.dbs.entry(name.clone()).or_default().queue.push_back(Pending::Open { ctx, req, version });
                self.mark(&name);
            }
            ToServer::Delete { ctx, req, name } => {
                self.dbs.entry(name.clone()).or_default().queue.push_back(Pending::Delete { ctx, req });
                self.mark(&name);
            }
            ToServer::Databases { ctx, req } => {
                // The COMMITTED version: a database mid-upgrade reports the
                // version it had, and one whose first upgrade is still running
                // has not been created yet (WPT get-databases).
                match self.backend.databases() {
                    Ok(l) => {
                        let list = l
                            .into_iter()
                            .map(|(n, v)| match self.dbs.get(&n).and_then(|e| e.active.as_ref()) {
                                Some(Active::Upgrading { old, .. }) => (n, *old),
                                _ => (n, v),
                            })
                            .filter(|(_, v)| *v > 0)
                            .collect();
                        deliver(ctx, ToClient::DatabaseList { req, list });
                    }
                    // Another instance holds the folder (IDB-D3), or the
                    // host's folder is unusable: the promise rejects, as an
                    // open() would — never an empty list that looks true.
                    Err(e) => {
                        let err = match e {
                            BackendError::Locked(d) => {
                                super::advise_folder_locked(&d);
                                Fail::new("UnknownError", format!("The databases are open in another instance of the app ({d})."))
                            }
                            other => from_backend(other),
                        };
                        deliver(ctx, ToClient::DatabasesFailed { req, err });
                    }
                }
            }
            ToServer::Estimate { ctx, req } => {
                let (usage, quota) = (self.backend.usage(), self.backend.quota());
                deliver(ctx, ToClient::Estimate { req, usage, quota });
            }
            ToServer::Begin { ctx, conn, tx, scope, mode, durability } => {
                let Some(c) = self.conns.get(&conn) else {
                    // The connection is gone (its context went away): nothing
                    // will ever ask about this transaction.
                    return;
                };
                let db = c.db.clone();
                self.txs.insert(tx, Tx { ctx, conn, db: db.clone(), mode, durability, scope, state: TxState::Waiting, ops: VecDeque::new(), commit: false, held: 0 });
                self.dbs.entry(db.clone()).or_default().txs.push(tx);
                self.mark(&db);
            }
            ToServer::Op { tx, req, op } => {
                if let Some(t) = self.txs.get_mut(&tx) {
                    t.ops.push_back((req, op));
                    if t.state == TxState::Running {
                        self.run_ops(tx);
                    }
                }
            }
            ToServer::Commit { tx } => {
                if let Some(t) = self.txs.get_mut(&tx) {
                    t.commit = true;
                    if t.state == TxState::Running {
                        self.run_ops(tx);
                    }
                }
            }
            // Client-initiated: the client already ran its half and queued its
            // own events, so nothing goes back.
            ToServer::Abort { tx } => self.abort(tx, None, false),
            ToServer::ErrorHandled { tx } => {
                if let Some(t) = self.txs.get_mut(&tx) {
                    t.held = t.held.saturating_sub(1);
                    if t.state == TxState::Running {
                        self.run_ops(tx);
                    }
                }
            }
            ToServer::VersionChangeDone { conn } => {
                if let Some(c) = self.conns.get(&conn) {
                    let db = c.db.clone();
                    if let Some(Active::Waiting { acks, .. }) = &mut self.dbs.get_mut(&db).unwrap().active {
                        acks.remove(&conn);
                    }
                    self.mark(&db);
                }
            }
            ToServer::Close { conn } => {
                if let Some(c) = self.conns.get_mut(&conn) {
                    c.close_pending = true;
                    let db = c.db.clone();
                    self.maybe_closed(conn);
                    self.mark(&db);
                }
            }
            ToServer::ContextGone { ctx } => self.context_gone(ctx),
            ToServer::Shutdown { .. } => {}
        }
    }

    /// Runs every dirty database's queue and scheduler until nothing moves.
    fn settle(&mut self) {
        let mut rounds = 0;
        while let Some(db) = self.dirty.pop() {
            self.step_queue(&db);
            self.schedule(&db);
            rounds += 1;
            if rounds > 100_000 {
                break;
            }
        }
    }

    fn shutdown(&mut self) {
        // Every transaction that never committed is lost, as in a crashed
        // browser; the memory backend has nothing to flush. (SQLite: close
        // every handle, which checkpoints the WAL.)
        let txs: Vec<TxId> = self.txs.keys().copied().collect();
        for t in txs {
            self.abort(t, Some(Fail::new("AbortError", "The runtime is shutting down.")), true);
        }
        for e in self.dbs.values_mut() {
            e.handle = None;
        }
    }

    // ---- the connection queue ----------------------------------------------

    fn ensure_handle(&mut self, name: &[u16]) -> Result<(), Fail> {
        let e = self.dbs.entry(name.to_vec()).or_default();
        if e.handle.is_none() {
            match self.backend.open(name) {
                Ok(h) => e.handle = Some(h),
                Err(BackendError::Locked(d)) => {
                    super::advise_locked(name, &d);
                    return Err(Fail::new(
                        "UnknownError",
                        format!("This database is open in another instance of the app ({d}); give the second instance its own storage folder."),
                    ));
                }
                Err(err) => return Err(from_backend(err)),
            }
        }
        Ok(())
    }

    fn step_queue(&mut self, name: &[u16]) {
        loop {
            let Some(e) = self.dbs.get_mut(name) else { return };
            if let Some(a) = &e.active {
                if matches!(a, Active::Waiting { .. }) {
                    self.check_waiting(name);
                    let e = self.dbs.get_mut(name).unwrap();
                    if e.active.is_some() {
                        return;
                    }
                    continue;
                }
                return;
            }
            let Some(p) = e.queue.pop_front() else { return };
            match p {
                Pending::Open { ctx, req, version } => {
                    if let Err(err) = self.ensure_handle(name) {
                        deliver(ctx, ToClient::OpenFailed { req, err });
                        continue;
                    }
                    let e = self.dbs.get_mut(name).unwrap();
                    let cur = e.handle.as_ref().unwrap().meta().version;
                    let v = version.unwrap_or(if cur == 0 { 1 } else { cur });
                    if v < cur {
                        deliver(
                            ctx,
                            ToClient::OpenFailed {
                                req,
                                err: Fail::new("VersionError", format!("The requested version ({v}) is less than the existing version ({cur}).")),
                            },
                        );
                        continue;
                    }
                    let conn = super::next_id();
                    self.conns.insert(conn, Conn { ctx, db: name.to_vec(), close_pending: false });
                    let e = self.dbs.get_mut(name).unwrap();
                    if v == cur {
                        e.conns.push(conn);
                        let meta = e.handle.as_ref().unwrap().meta().clone();
                        deliver(ctx, ToClient::Opened { req, conn, meta });
                        continue;
                    }
                    let others: HashSet<ConnId> = e.conns.iter().copied().filter(|c| !self.conns[c].close_pending).collect();
                    for o in &others {
                        let oc = self.conns[o].ctx;
                        deliver(oc, ToClient::VersionChange { conn: *o, old: cur, new: Some(v) });
                    }
                    e.active = Some(Active::Waiting { pending: Pending::Open { ctx, req, version: Some(v) }, conn: Some(conn), old: cur, new: Some(v), acks: others, blocked_sent: false });
                }
                Pending::Delete { ctx, req } => {
                    let cur = match e.handle.as_ref() {
                        Some(h) => h.meta().version,
                        None => self.backend.databases().ok().and_then(|l| l.into_iter().find(|(n, _)| n == name).map(|(_, v)| v)).unwrap_or(0),
                    };
                    let e = self.dbs.get_mut(name).unwrap();
                    let others: HashSet<ConnId> = e.conns.iter().copied().filter(|c| !self.conns[c].close_pending).collect();
                    for o in &others {
                        let oc = self.conns[o].ctx;
                        deliver(oc, ToClient::VersionChange { conn: *o, old: cur, new: None });
                    }
                    e.active = Some(Active::Waiting { pending: Pending::Delete { ctx, req }, conn: None, old: cur, new: None, acks: others, blocked_sent: false });
                }
            }
        }
    }

    /// The Waiting state: acks first, then `blocked` once, then the closes.
    fn check_waiting(&mut self, name: &[u16]) {
        let e = self.dbs.get_mut(name).unwrap();
        let Some(Active::Waiting { acks, blocked_sent, old, new, pending, .. }) = &mut e.active else { return };
        // A connection that closed (or whose context went) needs no ack.
        acks.retain(|c| e.conns.contains(c));
        if !acks.is_empty() {
            return;
        }
        if !e.conns.is_empty() {
            if !*blocked_sent {
                *blocked_sent = true;
                let (ctx, req) = match pending {
                    Pending::Open { ctx, req, .. } | Pending::Delete { ctx, req } => (*ctx, *req),
                };
                deliver(ctx, ToClient::Blocked { req, old: *old, new: *new });
            }
            return;
        }
        let Some(Active::Waiting { pending, conn, old, new, .. }) = e.active.take() else { unreachable!() };
        match pending {
            Pending::Open { ctx, req, .. } => {
                let conn = conn.unwrap();
                let v = new.unwrap();
                let tx = super::next_id();
                e.conns.push(conn);
                let h = e.handle.as_mut().unwrap();
                let begun = h.begin_write(Durability::Default).and_then(|_| h.set_version(v));
                if let Err(err) = begun {
                    h.rollback_write();
                    e.conns.retain(|c| *c != conn);
                    self.conns.remove(&conn);
                    deliver(ctx, ToClient::OpenFailed { req, err: from_backend(err) });
                    self.mark(name);
                    return;
                }
                let meta = h.meta().clone();
                let scope = meta.stores.iter().map(|s| s.id).collect();
                e.txs.push(tx);
                e.active = Some(Active::Upgrading { conn, tx, old });
                self.txs.insert(
                    tx,
                    Tx {
                        ctx,
                        conn,
                        db: name.to_vec(),
                        mode: Mode::VersionChange,
                        durability: Durability::Default,
                        scope,
                        state: TxState::Running,
                        ops: VecDeque::new(),
                        commit: false,
                        held: 0,
                    },
                );
                deliver(ctx, ToClient::Upgrade { req, conn, tx, old, meta });
            }
            Pending::Delete { ctx, req } => {
                e.handle = None;
                let r = self.backend.delete(name);
                match r {
                    Ok(old) => deliver(ctx, ToClient::Deleted { req, old }),
                    Err(err) => deliver(ctx, ToClient::OpenFailed { req, err: from_backend(err) }),
                }
                self.mark(name);
            }
        }
    }

    fn maybe_closed(&mut self, conn: ConnId) {
        let Some(c) = self.conns.get(&conn) else { return };
        if !c.close_pending {
            return;
        }
        if self.txs.values().any(|t| t.conn == conn) {
            return;
        }
        let db = c.db.clone();
        self.conns.remove(&conn);
        if let Some(e) = self.dbs.get_mut(&db) {
            e.conns.retain(|c| *c != conn);
        }
        self.mark(&db);
    }

    // ---- the scheduler ---------------------------------------------------------

    fn schedule(&mut self, name: &[u16]) {
        loop {
            let Some(e) = self.dbs.get(name) else { return };
            let mut start = None;
            for (i, &t) in e.txs.iter().enumerate() {
                let tx = &self.txs[&t];
                if tx.state != TxState::Waiting {
                    continue;
                }
                let rw = tx.mode != Mode::ReadOnly;
                let blocked = e.txs[..i].iter().any(|o| {
                    let o = &self.txs[o];
                    (o.mode != Mode::ReadOnly || rw) && o.scope.iter().any(|s| tx.scope.contains(s))
                });
                let writer_running = rw && e.txs.iter().any(|o| {
                    let o = &self.txs[o];
                    o.state == TxState::Running && o.mode != Mode::ReadOnly
                });
                if !blocked && !writer_running {
                    start = Some(t);
                    break;
                }
            }
            let Some(t) = start else { return };
            let (rw, durability) = {
                let tx = &self.txs[&t];
                (tx.mode != Mode::ReadOnly, tx.durability)
            };
            if rw {
                let e = self.dbs.get_mut(name).unwrap();
                if e.handle.is_none() {
                    // Cannot happen: a connection exists, so the handle does.
                    return;
                }
                if let Err(err) = e.handle.as_mut().unwrap().begin_write(durability) {
                    self.txs.get_mut(&t).unwrap().state = TxState::Running;
                    self.abort(t, Some(from_backend(err)), true);
                    continue;
                }
            }
            self.txs.get_mut(&t).unwrap().state = TxState::Running;
            self.run_ops(t);
        }
    }

    // ---- execution -------------------------------------------------------------

    fn run_ops(&mut self, t: TxId) {
        loop {
            let Some(tx) = self.txs.get_mut(&t) else { return };
            let Some((req, op)) = tx.ops.pop_front() else { break };
            let (ctx, db) = (tx.ctx, tx.db.clone());
            let schema = matches!(op, Op::CreateIndex(..));
            let Some(h) = self.dbs.get_mut(&db).and_then(|e| e.handle.as_mut()) else { return };
            // Each request is atomic: a failure half-way (a unique index, the
            // quota) undoes its own writes, the key generator included.
            let sp = h.savepoint();
            let r = execute(h.as_mut(), op);
            if r.is_err() {
                h.rollback_to(sp);
            }
            match r {
                Ok(p) => {
                    if req != 0 {
                        deliver(ctx, ToClient::Result { req, payload: p });
                    }
                }
                Err(err) if schema || req == 0 => {
                    // A schema step failing (createIndex over rows that break
                    // its uniqueness) aborts the upgrade, asynchronously.
                    self.abort(t, Some(err), true);
                    return;
                }
                Err(err) => {
                    if let Some(t) = self.txs.get_mut(&t) {
                        t.held += 1;
                    }
                    deliver(ctx, ToClient::RequestFailed { req, err });
                }
            }
        }
        let tx = &self.txs[&t];
        if !tx.commit || tx.held > 0 {
            return;
        }
        let (ctx, db, rw) = (tx.ctx, tx.db.clone(), tx.mode != Mode::ReadOnly);
        if rw {
            let h = self.dbs.get_mut(&db).unwrap().handle.as_mut().unwrap();
            if let Err(err) = h.commit_write() {
                self.abort(t, Some(from_backend(err)), true);
                return;
            }
        }
        deliver(ctx, ToClient::Completed { tx: t });
        self.finish(t);
    }

    /// Aborts `t`: its writes roll back; `notify` sends `Aborted` (not for a
    /// context that is gone).
    fn abort(&mut self, t: TxId, err: Option<Fail>, notify: bool) {
        let Some(tx) = self.txs.get(&t) else { return };
        let (ctx, conn, db, running, rw) = (tx.ctx, tx.conn, tx.db.clone(), tx.state == TxState::Running, tx.mode != Mode::ReadOnly);
        if running && rw {
            if let Some(h) = self.dbs.get_mut(&db).and_then(|e| e.handle.as_mut()) {
                h.rollback_write();
            }
        }
        if tx.mode == Mode::VersionChange {
            // An aborted upgrade closes its connection (the open fails).
            if let Some(c) = self.conns.get_mut(&conn) {
                c.close_pending = true;
            }
        }
        if notify {
            deliver(ctx, ToClient::Aborted { tx: t, err });
        }
        self.finish(t);
    }

    fn finish(&mut self, t: TxId) {
        let Some(tx) = self.txs.remove(&t) else { return };
        if let Some(e) = self.dbs.get_mut(&tx.db) {
            e.txs.retain(|x| *x != t);
            if let Some(Active::Upgrading { tx: ut, conn, .. }) = e.active {
                if ut == t {
                    e.active = None;
                    // An aborted (or a closed-during-upgrade) connection is
                    // closed now; the client fails its open request.
                    let _ = conn;
                }
            }
        }
        self.maybe_closed(tx.conn);
        self.mark(&tx.db);
    }

    fn context_gone(&mut self, ctx: ContextId) {
        let mine: Vec<TxId> = self.txs.iter().filter(|(_, t)| t.ctx == ctx).map(|(id, _)| *id).collect();
        for t in mine {
            self.abort(t, None, false);
        }
        let conns: Vec<ConnId> = self.conns.iter().filter(|(_, c)| c.ctx == ctx).map(|(id, _)| *id).collect();
        for c in conns {
            if let Some(conn) = self.conns.remove(&c) {
                if let Some(e) = self.dbs.get_mut(&conn.db) {
                    e.conns.retain(|x| *x != c);
                }
                self.mark(&conn.db);
            }
        }
        let names: Vec<Vec<u16>> = self.dbs.keys().cloned().collect();
        for n in names {
            let e = self.dbs.get_mut(&n).unwrap();
            e.queue.retain(|p| p.ctx() != ctx);
            if let Some(Active::Waiting { pending, .. }) = &e.active {
                if pending.ctx() == ctx {
                    e.active = None;
                }
            }
            self.mark(&n);
        }
        drop_inbox(ctx);
    }
}

// ---------------------------------------------------------------------------
// Operations against a database
// ---------------------------------------------------------------------------

fn bound(b: &key::Bound) -> Bound<&[u8]> {
    match b {
        key::Bound::Unbounded => Bound::Unbounded,
        key::Bound::Incl(x) => Bound::Included(x.as_slice()),
        key::Bound::Excl(x) => Bound::Excluded(x.as_slice()),
    }
}

fn with_after(k: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(k.len() + 1);
    v.extend_from_slice(k);
    v.push(AFTER);
    v
}

/// A range over index keys, as bounds over `index key ‖ primary key` entries.
fn entry_bounds(r: &Range) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
    let lo = match &r.lower {
        key::Bound::Unbounded => Bound::Unbounded,
        key::Bound::Incl(l) => Bound::Included(l.clone()),
        key::Bound::Excl(l) => Bound::Included(with_after(l)),
    };
    let hi = match &r.upper {
        key::Bound::Unbounded => Bound::Unbounded,
        key::Bound::Incl(u) => Bound::Excluded(with_after(u)),
        key::Bound::Excl(u) => Bound::Excluded(u.clone()),
    };
    (lo, hi)
}

fn as_ref(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match b {
        Bound::Included(x) => Bound::Included(x.as_slice()),
        Bound::Excluded(x) => Bound::Excluded(x.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// The tighter of two lower bounds.
fn max_lo(a: Bound<Vec<u8>>, b: Bound<Vec<u8>>) -> Bound<Vec<u8>> {
    use Bound::*;
    match (&a, &b) {
        (Unbounded, _) => b,
        (_, Unbounded) => a,
        (Included(x) | Excluded(x), Included(y) | Excluded(y)) => {
            if x > y {
                a
            } else if y > x {
                b
            } else if matches!(a, Excluded(_)) {
                a
            } else {
                b
            }
        }
    }
}

/// The tighter of two upper bounds.
fn min_hi(a: Bound<Vec<u8>>, b: Bound<Vec<u8>>) -> Bound<Vec<u8>> {
    use Bound::*;
    match (&a, &b) {
        (Unbounded, _) => b,
        (_, Unbounded) => a,
        (Included(x) | Excluded(x), Included(y) | Excluded(y)) => {
            if x < y {
                a
            } else if y < x {
                b
            } else if matches!(a, Excluded(_)) {
                a
            } else {
                b
            }
        }
    }
}

fn split_entry(e: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let n = key::key_len(e).unwrap_or(e.len());
    (e[..n].to_vec(), e[n..].to_vec())
}

fn store_meta(h: &dyn Database, id: StoreId) -> Result<StoreMeta, Fail> {
    h.meta().store(id).cloned().ok_or_else(|| Fail::new("InvalidStateError", "The object store has been deleted."))
}

/// The index keys a value contributes to one index: none (the record is
/// simply absent from the index), one, or a multiEntry array's members.
fn index_keys(p: Option<&Parsed>, ix: &IndexMeta) -> Vec<Vec<u8>> {
    let Some(p) = p else { return Vec::new() };
    let r = if ix.multi_entry { p.extract_multi(&ix.key_path) } else { p.extract(&ix.key_path) };
    match r {
        Ok(Some(Key::Array(items))) if ix.multi_entry => items.iter().map(|k| k.encode()).collect(),
        Ok(Some(k)) => vec![k.encode()],
        _ => Vec::new(),
    }
}

fn entry(ikey: &[u8], pkey: &[u8]) -> Vec<u8> {
    let mut e = Vec::with_capacity(ikey.len() + pkey.len());
    e.extend_from_slice(ikey);
    e.extend_from_slice(pkey);
    e
}

fn remove_record(h: &mut dyn Database, store: &StoreMeta, key: &[u8]) -> Result<(), Fail> {
    if !store.indexes.is_empty() {
        if let Some(old) = h.get(store.id, key).map_err(from_backend)? {
            let parsed = Parsed::parse(&old);
            for ix in &store.indexes {
                for ik in index_keys(parsed.as_ref(), ix) {
                    h.index_remove(ix.id, &entry(&ik, key)).map_err(from_backend)?;
                }
            }
        }
    }
    h.delete(store.id, key).map_err(from_backend)
}

/// The key generator's next current number after `v`. The spec's numbers
/// are mathematical: past 2^53 an f64 `v + 1` would round back to `v`, so the
/// exhausted state is kept as +∞ ("greater than 2^53": the next generation
/// fails with ConstraintError — WPT keygenerator).
fn gen_next(v: f64) -> f64 {
    if v >= key::KEYGEN_MAX {
        f64::INFINITY
    } else {
        v + 1.0
    }
}

fn put(h: &mut dyn Database, store_id: StoreId, key: Option<Vec<u8>>, value: Vec<u8>, no_overwrite: bool) -> Result<Payload, Fail> {
    let store = store_meta(h, store_id)?;
    let mut value = value;
    let key = match key {
        Some(k) => {
            if store.auto_increment {
                // "Possibly update the key generator".
                if let Some(Key::Number(n)) = key::decode(&k) {
                    let cur = h.key_generator(store.id).map_err(from_backend)?;
                    let v = n.min(key::KEYGEN_MAX).floor();
                    if v >= cur {
                        h.set_key_generator(store.id, gen_next(v)).map_err(from_backend)?;
                    }
                }
            }
            k
        }
        None => {
            let cur = h.key_generator(store.id).map_err(from_backend)?;
            if cur > key::KEYGEN_MAX {
                return Err(Fail::new("ConstraintError", "The key generator has reached its maximum."));
            }
            h.set_key_generator(store.id, gen_next(cur)).map_err(from_backend)?;
            if let Some(KeyPath::String(p)) = &store.key_path {
                value = svalue::inject(&value, p, cur).ok_or_else(|| Fail::new("DataError", "The key could not be injected into the value."))?;
            }
            Key::Number(cur).encode()
        }
    };
    let existing = h.get(store.id, &key).map_err(from_backend)?;
    if no_overwrite && existing.is_some() {
        return Err(Fail::new("ConstraintError", "A record with the given key already exists in the object store."));
    }
    // Index keys of the new value, checked for uniqueness BEFORE anything is
    // written: the put is atomic (WPT idbobjectstore-put-unique-index-
    // constraint-is-atomic).
    let parsed = if store.indexes.is_empty() { None } else { Parsed::parse(&value) };
    let mut new_entries: Vec<(IndexId, Vec<u8>)> = Vec::new();
    for ix in &store.indexes {
        for ik in index_keys(parsed.as_ref(), ix) {
            if ix.unique {
                let hi = with_after(&ik);
                let hits = h.index_scan(ix.id, Bound::Included(&ik), Bound::Excluded(&hi), false, 2).map_err(from_backend)?;
                if hits.iter().any(|e| split_entry(e).1 != key) {
                    return Err(Fail::new("ConstraintError", "Unable to add key to index: at least one key does not satisfy the uniqueness requirements."));
                }
            }
            new_entries.push((ix.id, entry(&ik, &key)));
        }
    }
    if existing.is_some() {
        remove_record(h, &store, &key)?;
    }
    h.put(store.id, &key, Arc::new(value)).map_err(from_backend)?;
    for (ix, e) in new_entries {
        h.index_add(ix, e).map_err(from_backend)?;
    }
    Ok(Payload::Key(key))
}

fn create_index(h: &mut dyn Database, store_id: StoreId, meta: IndexMeta) -> Result<Payload, Fail> {
    h.create_index(store_id, meta.clone()).map_err(from_backend)?;
    // Populate from the records already there; a uniqueness violation aborts
    // the upgrade (the caller turns this Err into an abort).
    let keys = h.keys(store_id, Bound::Unbounded, Bound::Unbounded, false, usize::MAX).map_err(from_backend)?;
    for k in keys {
        let Some(v) = h.get(store_id, &k).map_err(from_backend)? else { continue };
        let parsed = Parsed::parse(&v);
        for ik in index_keys(parsed.as_ref(), &meta) {
            if meta.unique {
                let hi = with_after(&ik);
                let hits = h.index_scan(meta.id, Bound::Included(&ik), Bound::Excluded(&hi), false, 1).map_err(from_backend)?;
                if !hits.is_empty() {
                    return Err(Fail::new("ConstraintError", "Unable to create the index: existing records do not satisfy its uniqueness requirement."));
                }
            }
            h.index_add(meta.id, entry(&ik, &k)).map_err(from_backend)?;
        }
    }
    Ok(Payload::Undefined)
}

/// Primary keys a source yields over a range, in order (index: by index key
/// then primary key).
fn primary_keys(h: &mut dyn Database, source: Source, range: &Range, limit: usize) -> Result<Vec<Vec<u8>>, Fail> {
    match source {
        Source::Store(s) => h.keys(s, bound(&range.lower), bound(&range.upper), false, limit).map_err(from_backend),
        Source::Index(_, ix) => {
            let (lo, hi) = entry_bounds(range);
            let es = h.index_scan(ix, as_ref(&lo), as_ref(&hi), false, limit).map_err(from_backend)?;
            Ok(es.iter().map(|e| split_entry(e).1).collect())
        }
    }
}

fn check_source(h: &dyn Database, source: Source) -> Result<(), Fail> {
    let s = store_meta(h, source.store())?;
    if let Source::Index(_, ix) = source {
        if !s.indexes.iter().any(|i| i.id == ix) {
            return Err(Fail::new("InvalidStateError", "The index has been deleted."));
        }
    }
    Ok(())
}

fn execute(h: &mut dyn Database, op: Op) -> Result<Payload, Fail> {
    match op {
        Op::CreateStore(meta) => {
            h.create_store(meta).map_err(from_backend)?;
            Ok(Payload::Undefined)
        }
        Op::DeleteStore(id) => {
            h.delete_store(id).map_err(from_backend)?;
            Ok(Payload::Undefined)
        }
        Op::RenameStore(id, name) => {
            h.rename_store(id, name).map_err(from_backend)?;
            Ok(Payload::Undefined)
        }
        Op::CreateIndex(store, meta) => create_index(h, store, meta),
        Op::DeleteIndex(store, id) => {
            h.delete_index(store, id).map_err(from_backend)?;
            Ok(Payload::Undefined)
        }
        Op::RenameIndex(store, id, name) => {
            h.rename_index(store, id, name).map_err(from_backend)?;
            Ok(Payload::Undefined)
        }
        Op::Put { store, key, value, no_overwrite } => put(h, store, key, value, no_overwrite),
        Op::Get { source, range, keys } => {
            check_source(h, source)?;
            let pk = match (source, range.single()) {
                (Source::Store(_), Some(k)) => Some(k.to_vec()),
                _ => primary_keys(h, source, &range, 1)?.into_iter().next(),
            };
            let Some(pk) = pk else { return Ok(if keys { Payload::Undefined } else { Payload::Value(None) }) };
            if keys {
                if let Source::Store(s) = source {
                    if range.single().is_some() && h.get(s, &pk).map_err(from_backend)?.is_none() {
                        return Ok(Payload::Undefined);
                    }
                }
                return Ok(Payload::Key(pk));
            }
            Ok(Payload::Value(h.get(source.store(), &pk).map_err(from_backend)?))
        }
        Op::GetAll { source, range, count, keys } => {
            check_source(h, source)?;
            let limit = if count == 0 { usize::MAX } else { count as usize };
            let pks = primary_keys(h, source, &range, limit)?;
            if keys {
                return Ok(Payload::Keys(pks));
            }
            let mut vals = Vec::with_capacity(pks.len());
            for pk in pks {
                if let Some(v) = h.get(source.store(), &pk).map_err(from_backend)? {
                    vals.push(v);
                }
            }
            Ok(Payload::Values(vals))
        }
        Op::Count { source, range } => {
            check_source(h, source)?;
            let n = match source {
                Source::Store(s) => h.count(s, bound(&range.lower), bound(&range.upper)).map_err(from_backend)?,
                Source::Index(_, ix) => {
                    let (lo, hi) = entry_bounds(&range);
                    h.index_count(ix, as_ref(&lo), as_ref(&hi)).map_err(from_backend)?
                }
            };
            Ok(Payload::Count(n))
        }
        Op::Delete { store, range } => {
            let meta = store_meta(h, store)?;
            let keys = match range.single() {
                Some(k) => vec![k.to_vec()],
                None => h.keys(store, bound(&range.lower), bound(&range.upper), false, usize::MAX).map_err(from_backend)?,
            };
            for k in keys {
                remove_record(h, &meta, &k)?;
            }
            Ok(Payload::Undefined)
        }
        Op::Clear { store } => {
            let meta = store_meta(h, store)?;
            h.clear(store).map_err(from_backend)?;
            for ix in &meta.indexes {
                h.index_clear(ix.id).map_err(from_backend)?;
            }
            Ok(Payload::Undefined)
        }
        Op::Cursor { source, range, dir, key_only, pos, key, pkey, count } => {
            check_source(h, source)?;
            let mut pos = pos;
            let mut hit = None;
            for i in 0..count.max(1) {
                let k = if i == 0 { key.as_deref() } else { None };
                let pk = if i == 0 { pkey.as_deref() } else { None };
                match step(h, source, &range, dir, pos.as_ref(), k, pk)? {
                    Some(found) => {
                        pos = Some(found.clone());
                        hit = Some(found);
                    }
                    None => {
                        hit = None;
                        break;
                    }
                }
            }
            let Some((k, pk)) = hit else { return Ok(Payload::Cursor(None)) };
            let value = if key_only { None } else { h.get(source.store(), &pk).map_err(from_backend)? };
            Ok(Payload::Cursor(Some(CursorHit { key: k, pkey: pk, value })))
        }
    }
}

/// One step of "iterate a cursor" (§7.4): the next (key, primary key) after
/// `pos` in direction `dir`, honouring `continue(key)` / `continuePrimaryKey`.
fn step(
    h: &mut dyn Database,
    source: Source,
    range: &Range,
    dir: Dir,
    pos: Option<&(Vec<u8>, Vec<u8>)>,
    key: Option<&[u8]>,
    pkey: Option<&[u8]>,
) -> Result<Option<(Vec<u8>, Vec<u8>)>, Fail> {
    match source {
        Source::Store(s) => {
            let mut lo = match &range.lower {
                key::Bound::Unbounded => Bound::Unbounded,
                key::Bound::Incl(x) => Bound::Included(x.clone()),
                key::Bound::Excl(x) => Bound::Excluded(x.clone()),
            };
            let mut hi = match &range.upper {
                key::Bound::Unbounded => Bound::Unbounded,
                key::Bound::Incl(x) => Bound::Included(x.clone()),
                key::Bound::Excl(x) => Bound::Excluded(x.clone()),
            };
            if dir.forward() {
                if let Some(k) = key {
                    lo = max_lo(lo, Bound::Included(k.to_vec()));
                }
                if let Some((p, _)) = pos {
                    lo = max_lo(lo, Bound::Excluded(p.clone()));
                }
            } else {
                if let Some(k) = key {
                    hi = min_hi(hi, Bound::Included(k.to_vec()));
                }
                if let Some((p, _)) = pos {
                    hi = min_hi(hi, Bound::Excluded(p.clone()));
                }
            }
            let ks = h.keys(s, as_ref(&lo), as_ref(&hi), !dir.forward(), 1).map_err(from_backend)?;
            Ok(ks.into_iter().next().map(|k| (k.clone(), k)))
        }
        Source::Index(_, ix) => {
            let (mut lo, mut hi) = entry_bounds(range);
            match dir {
                Dir::Next => {
                    if let Some(k) = key {
                        lo = max_lo(lo, Bound::Included(match pkey {
                            Some(pk) => entry(k, pk),
                            None => k.to_vec(),
                        }));
                    }
                    if let Some((p, op)) = pos {
                        lo = max_lo(lo, Bound::Excluded(entry(p, op)));
                    }
                }
                Dir::NextUnique => {
                    if let Some(k) = key {
                        lo = max_lo(lo, Bound::Included(k.to_vec()));
                    }
                    if let Some((p, _)) = pos {
                        lo = max_lo(lo, Bound::Included(with_after(p)));
                    }
                }
                Dir::Prev => {
                    if let Some(k) = key {
                        hi = min_hi(hi, match pkey {
                            Some(pk) => Bound::Included(entry(k, pk)),
                            None => Bound::Excluded(with_after(k)),
                        });
                    }
                    if let Some((p, op)) = pos {
                        hi = min_hi(hi, Bound::Excluded(entry(p, op)));
                    }
                }
                Dir::PrevUnique => {
                    if let Some(k) = key {
                        hi = min_hi(hi, Bound::Excluded(with_after(k)));
                    }
                    if let Some((p, _)) = pos {
                        hi = min_hi(hi, Bound::Excluded(p.clone()));
                    }
                }
            }
            let es = h.index_scan(ix, as_ref(&lo), as_ref(&hi), !dir.forward(), 1).map_err(from_backend)?;
            let Some(e) = es.into_iter().next() else { return Ok(None) };
            let (ik, pk) = split_entry(&e);
            if dir == Dir::PrevUnique {
                // The LOWEST primary key of that index key.
                let first = h.index_scan(ix, Bound::Included(&ik), Bound::Excluded(&with_after(&ik)), false, 1).map_err(from_backend)?;
                if let Some(f) = first.into_iter().next() {
                    let (_, fpk) = split_entry(&f);
                    return Ok(Some((ik, fpk)));
                }
            }
            let _ = dir.unique();
            Ok(Some((ik, pk)))
        }
    }
}

#[cfg(test)]
mod inspection_tests {
    use super::*;
    #[test]
    fn inspection_refuses_active_writer_without_aborting_it() {
        let name: Vec<u16> = "busy-inspection".encode_utf16().collect();
        let mut backend = super::super::memory::MemoryBackend::new(1 << 20);
        let mut database = backend.open(&name).unwrap();
        database.begin_write(Durability::Default).unwrap();
        database.set_version(1).unwrap();
        database.create_store(StoreMeta { id: 1, name: vec![115], key_path: None, auto_increment: false, indexes: vec![] }).unwrap();
        database.commit_write().unwrap();
        database.begin_write(Durability::Default).unwrap();
        database.put(1, b"pending", Arc::new(vec![99])).unwrap();
        let mut server = Server { backend: Box::new(backend), dbs: HashMap::new(), conns: HashMap::new(), txs: HashMap::new(), dirty: vec![] };
        server.dbs.insert(name.clone(), DbEntry { handle: Some(database), ..Default::default() });
        server.txs.insert(1, Tx { ctx: ContextId(42), conn: 1, db: name.clone(), mode: Mode::ReadWrite, durability: Durability::Default, scope: vec![1], state: TxState::Running, ops: VecDeque::new(), commit: false, held: 0 });
        let error = server.inspect(super::super::inspection::Query::Schema { database: name.clone() }).unwrap_err();
        assert!(error.contains("busy"));
        assert!(server.txs.contains_key(&1), "inspector aborted the application writer");
        let database = server.dbs.get_mut(&name).unwrap().handle.as_deref_mut().unwrap();
        assert_eq!(database.get(1, b"pending").unwrap().unwrap().as_slice(), &[99]);
        database.rollback_write();
    }
}
