//! IndexedDB 3.0 (m25 indexeddb-design): `indexedDB`, every `IDB*` interface,
//! `DOMStringList`, `IDBVersionChangeEvent`, `navigator.storage`.
//!
//! # Shape (§3)
//!
//! - **Clients** ([`client`]) — one per JS context, on that context's thread:
//!   the IDB objects, key conversion and key paths, the structured clone,
//!   the transaction state machine and event dispatch.
//! - **The server** ([`server`]) — one thread per process that owns every
//!   database: the open/delete queues with versionchange/blocked, the
//!   scheduler, index maintenance, key generators, cursors, over a
//!   [`backend::Backend`]. Only plain Rust data crosses.
//! - **Delivery** — the server pushes into the context's inbox (keyed by
//!   [`ContextId`]) and wakes it; the context's pump drains the inbox inside
//!   ONE shared per-pump budget ([`crate::web::register_pump_source`]), each
//!   message one task followed by a microtask checkpoint. While results are
//!   in flight the pump waits for them (bounded): an awaited `get` costs a
//!   thread hop, not a frame.
//!
//! # Backends
//!
//! P1 ships the memory backend ([`memory`]): nothing survives a restart, and
//! the first use says so once. The SQLite backend (P2) drops in behind the
//! same trait; [`IndexedDbHost`] is the seam its folder and key come through,
//! and [`set_backend_factory`] is how a test (or P2) picks one.
//!
//! # Divergences (§5.6), each named where it lives
//!
//! IDB-D1 (timer/rAF-created transactions deactivate at the pump's
//! checkpoint — advised once), IDB-D2 (one readwrite transaction runs per
//! database), IDB-D3 (a second process: `open()` → UnknownError — advised),
//! IDB-D4 (Blob values materialize in memory — advised over 64 MiB), IDB-D5
//! (`getAllRecords` / the getAll options dictionary absent), IDB-D6 (the
//! first result of a burst started outside an IDB callback waits for the
//! next pump), IDB-D7 (no storage buckets, no OPFS; `persist()` is true).

pub mod backend;
mod client;
mod common;
mod cursor;
mod database;
mod factory;
pub mod key;
pub mod keypath;
pub mod memory;
pub mod server;
mod store;
pub mod svalue;

use core::ffi::c_void;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::Duration;

use crate::context::ContextId;
use crate::glue;
use crate::webapi::util::*;

/// The memory backend's cap, and the host's default quota.
pub const DEFAULT_QUOTA: u64 = 1 << 30;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Connection, transaction and request ids: one process-wide counter, never
/// reused, shared by clients and the server.
pub(crate) fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

pub use server::{set_backend_factory, BackendFactory};

// ---------------------------------------------------------------------------
// The host seam (P2: the SQLite backend reads its folder and key here)
// ---------------------------------------------------------------------------

/// At-rest encryption the host asks for (design §5.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdbEncryption {
    Plaintext,
    Encrypted([u8; 32]),
    /// Encrypted files are rekeyed to plaintext.
    Decrypting { old_key: [u8; 32] },
}

/// Where an app's databases live and how they are protected. Read ONCE, on
/// the server thread, at the first IndexedDB use, and latched for the
/// process: a later change applies at the next start ([`config_changed`]).
pub trait IndexedDbHost: Send + Sync {
    /// The folder holding this app's databases.
    fn folder(&self) -> Result<std::path::PathBuf, String>;
    /// At-rest encryption, read after `folder`.
    fn encryption(&self) -> Result<IdbEncryption, String> {
        Ok(IdbEncryption::Plaintext)
    }
    /// Bytes this app may use, given current usage.
    fn quota_bytes(&self, _usage: u64) -> u64 {
        DEFAULT_QUOTA
    }
    /// For advisories: where the bytes live.
    fn describe(&self) -> String;
    /// Every file is now in the requested format.
    fn migrated(&self) {}
}

static HOST: RwLock<Option<&'static dyn IndexedDbHost>> = RwLock::new(None);

/// Installs the storage host. Effective for the process if called before the
/// first IndexedDB use; after it, [`config_changed`] says what happens.
pub fn set_host(h: &'static dyn IndexedDbHost) {
    *HOST.write().unwrap() = Some(h);
    config_changed();
}

pub fn host() -> Option<&'static dyn IndexedDbHost> {
    *HOST.read().unwrap()
}

/// What the server latched at its start, for the late-change advisory.
fn latched() -> &'static Mutex<Option<String>> {
    static L: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(None))
}

pub(crate) fn latch(desc: String) {
    *latched().lock().unwrap() = Some(desc);
}

/// The host's folder or encryption changed (`setStorageFolder` after the
/// fact). Before the first use: nothing to do, the server reads the host
/// when it starts. After it: the change applies at the next start —
/// moving open databases under live connections is not worth its risk — and
/// that is said once.
pub fn config_changed() {
    if !server::started() {
        return;
    }
    let at = latched().lock().unwrap().clone().unwrap_or_else(|| "this process's memory".into());
    advise(
        "config-changed",
        format!(
            "IndexedDB is already open in {at}; the new storage folder / encryption applies at the next start. \
             Existing databases are not moved."
        ),
    );
}

// ---------------------------------------------------------------------------
// Advisories — one-shot, on stderr, and kept for tests / devtools
// ---------------------------------------------------------------------------

fn advised() -> &'static Mutex<(HashSet<String>, Vec<String>)> {
    static A: OnceLock<Mutex<(HashSet<String>, Vec<String>)>> = OnceLock::new();
    A.get_or_init(|| Mutex::new((HashSet::new(), Vec::new())))
}

/// Says `msg` once per `key`, for the process.
pub(crate) fn advise(key: &str, msg: String) {
    let mut a = advised().lock().unwrap();
    if !a.0.insert(key.to_string()) {
        return;
    }
    eprintln!("[redjs] {msg}");
    a.1.push(msg);
}

/// Every advisory said so far, in order.
pub fn advisories() -> Vec<String> {
    advised().lock().unwrap().1.clone()
}

pub(crate) fn advise_memory_only(desc: &str) {
    advise(
        "memory-only",
        format!(
            "IndexedDB: no persistent backend in this build — databases live in {desc}. This run is IN MEMORY ONLY: \
             nothing survives a restart (persistence lands with the SQLite backend)."
        ),
    );
}

pub(crate) fn advise_locked(name: &[u16], detail: &str) {
    let n = String::from_utf16_lossy(name);
    advise(
        &format!("locked:{n}"),
        format!(
            "IndexedDB: database \"{n}\" is open in another instance of the app ({detail}); open() fails with \
             UnknownError. Give the second instance its own storage folder (VN_APP_NAME=…)."
        ),
    );
}

pub(crate) fn advise_d1() {
    advise(
        "IDB-D1",
        "IndexedDB (IDB-D1): a transaction was created in a timer / requestAnimationFrame callback. This runtime \
         drains microtasks once per frame, so it stays active until the end of the frame's pump, not the end of that \
         callback — more permissive than a browser, never stricter. Code that expects TransactionInactiveError from a \
         later callback in the same frame will not see it."
            .into(),
    );
}

pub(crate) fn advise_big_value(bytes: usize) {
    advise(
        "IDB-D4",
        format!(
            "IndexedDB (IDB-D4): a value of {} MiB was read back; Blob values materialize in memory on read.",
            bytes >> 20
        ),
    );
}

// ---------------------------------------------------------------------------
// Per-context install / teardown
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub(crate) struct Intrinsics {
    pub ident_re: u64,
    pub regexp_exec: u64,
}

pub(crate) fn intrinsics(g: *mut c_void) -> Option<Intrinsics> {
    let c = client::try_cl(g)?;
    let c = c.borrow();
    Some(Intrinsics { ident_re: c.ident_re, regexp_exec: c.regexp_exec })
}

unsafe fn keep(g: *mut c_void, v: u64) -> u64 {
    if is_cell(v) {
        crate::ffi::redjs_protect(g, v);
    }
    v
}

/// Installs IndexedDB into context `g`. Needs `Event.prototype` and
/// `EventTarget.prototype`, `DOMException`, the serializer and `navigator`
/// (it hangs `navigator.storage` there). Nothing connects or spawns here:
/// the server thread starts at the first open / deleteDatabase / databases.
pub unsafe fn install(g: *mut c_void, ev_proto: u64, et_proto: u64) {
    let Some(ctx) = crate::context::id_of(g) else { return };
    if client::try_cl(g).is_some() {
        return;
    }
    let cls = factory::install_classes(g, ev_proto, et_proto);
    let global = glue::rjs_global(g);

    // Captured before any user script: the IdentifierName test for key paths.
    let re_ctor = get(g, global, "RegExp");
    let re_args = [js_string(g, keypath::IDENT_RE), js_string(g, "u")];
    let mut ident_re = UNDEF;
    glue::rjs_construct(g, re_ctor, re_args.as_ptr(), 2, &mut ident_re);
    let regexp_exec = get(g, get(g, re_ctor, "prototype"), "exec");

    let inbox = server::register_inbox(ctx);
    client::put(
        g,
        client::Client {
            ctx,
            inbox,
            cls,
            s_bag: atom_priv(g, "redjs#idb"),
            ident_re: keep(g, ident_re),
            regexp_exec: keep(g, regexp_exec),
            factory: UNDEF,
            storage: UNDEF,
            reqs: Default::default(),
            txs: Default::default(),
            conns: Default::default(),
            promises: Default::default(),
            fresh: Vec::new(),
            inflight: 0,
            blocked: Default::default(),
            opaque: false,
        },
    );
    let factory = keep(g, host_create(g, cls.factory, cls.factory_proto, client::Unit));
    let storage = keep(g, host_create(g, cls.storage_manager, cls.storage_manager_proto, client::Unit));
    {
        let c = client::cl(g);
        let mut c = c.borrow_mut();
        c.factory = factory;
        c.storage = storage;
    }

    // `indexedDB` — a readonly attribute (SameObject), as on a window.
    let getter = make_fn(g, "get indexedDB", 0, factory::get_indexed_db);
    let a = atom(g, "indexedDB");
    glue::rjs_define_accessor(g, global, a, getter, UNDEF, 0);
    glue::rjs_atom_free(g, a);

    // `navigator.storage` — on Navigator.prototype, like the other members.
    let nav_ctor = get(g, global, "Navigator");
    let nav_proto = if is_cell(nav_ctor) { get(g, nav_ctor, "prototype") } else { UNDEF };
    let target = if is_cell(nav_proto) { nav_proto } else { get(g, global, "navigator") };
    if is_cell(target) {
        let getter = make_fn(g, "get storage", 0, factory::get_storage);
        let a = atom(g, "storage");
        glue::rjs_define_accessor(g, target, a, getter, UNDEF, 0);
        glue::rjs_atom_free(g, a);
    }

    crate::web::on_checkpoint(client::on_checkpoint);
    crate::web::register_pump_source(crate::web::PumpSource {
        inflight: client::src_inflight,
        ready: client::src_ready,
        drain: client::src_drain,
    });
}

/// Marks a context's origin opaque (a `data:` URL worker): `open()`,
/// `deleteDatabase()` and `databases()` then fail with SecurityError, as in a
/// browser. The seam a worker installer calls.
pub unsafe fn set_opaque_origin(g: *mut c_void, opaque: bool) {
    if let Some(c) = client::try_cl(g) {
        c.borrow_mut().opaque = opaque;
    }
}

/// `crate::context` fan-out, at teardown: no JavaScript runs. The client's
/// state is dropped (its heap is gone), the server aborts the context's
/// transactions and closes its connections (`context_gone`), and anything it
/// sends later is discarded with the inbox.
pub(crate) fn forget(gone: crate::context::Gone) {
    client::remove(gone.key);
    context_gone(gone.id);
}

/// Tells the server a context is gone: its transactions abort (rolled back),
/// its connections close, its queued opens are dropped. Safe from any thread
/// and during teardown; runs no JavaScript.
pub fn context_gone(id: ContextId) {
    if server::started() {
        server::send(server::ToServer::ContextGone { ctx: id });
    }
    server::drop_inbox(id);
}

/// Ends the server's work before exit: transactions that never committed
/// are lost (as when a browser is killed), every database is closed. `true`
/// if the server answered within `timeout`.
pub fn shutdown(timeout: Duration) -> bool {
    if !server::started() {
        return true;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    server::send(server::ToServer::Shutdown { done: tx });
    rx.recv_timeout(timeout).is_ok()
}


#[cfg(test)]
mod tests;
