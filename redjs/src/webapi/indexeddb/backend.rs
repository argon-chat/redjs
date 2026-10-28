//! The storage seam: what the IndexedDB server needs from a place that keeps
//! bytes. Everything IndexedDB-specific — index maintenance, uniqueness, key
//! generation, cursor iteration, the scheduler — is the server's and runs on
//! top of these primitives, so a backend is an ordered byte store with a
//! write transaction and nothing more.
//!
//! Two implementations stand behind it: [`super::memory::MemoryBackend`] (P1,
//! always compiled, the no-host case and every conformance run) and the
//! SQLite one (P2, `indexeddb-sqlite`). The contract both honour:
//!
//! - **Keys are the encoded keys of [`super::key`]**, compared with `memcmp`.
//!   A record is `(store, key) → value`. An index entry is ONE byte string,
//!   `index key ‖ primary key` (the encoding is prefix-free, so these sort by
//!   (index key, primary key)); bounds on entries use [`super::key::AFTER`].
//! - **At most one write transaction per database at a time** (the
//!   scheduler's IDB-D2). Reads may happen at any time, inside or outside it;
//!   a read-only IndexedDB transaction never overlaps the writer's scope, so
//!   it never observes uncommitted rows — one connection per database works.
//! - **Schema and the key generator are transactional** like records: a
//!   rollback reverts `createObjectStore`, `deleteIndex`, the version and the
//!   generator's current number (WPT `transaction-abort-generator-revert`).
//! - Values are shared `Arc` bytes: the server never copies a value to hand
//!   it to a client.

use std::ops::Bound;
use std::sync::Arc;

use super::keypath::KeyPath;

pub type StoreId = u64;
pub type IndexId = u64;
pub type Value = Arc<Vec<u8>>;

#[derive(Clone, Debug, PartialEq)]
pub struct IndexMeta {
    pub id: IndexId,
    pub name: Vec<u16>,
    pub key_path: KeyPath,
    pub unique: bool,
    pub multi_entry: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoreMeta {
    pub id: StoreId,
    pub name: Vec<u16>,
    pub key_path: Option<KeyPath>,
    pub auto_increment: bool,
    pub indexes: Vec<IndexMeta>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DbMeta {
    pub name: Vec<u16>,
    pub version: u64,
    pub stores: Vec<StoreMeta>,
}

impl DbMeta {
    pub fn store(&self, id: StoreId) -> Option<&StoreMeta> {
        self.stores.iter().find(|s| s.id == id)
    }
    pub fn store_by_name(&self, name: &[u16]) -> Option<&StoreMeta> {
        self.stores.iter().find(|s| s.name == name)
    }
    /// The next free id for a store or an index (one id space per database).
    pub fn next_id(&self) -> u64 {
        self.stores.iter().flat_map(|s| core::iter::once(s.id).chain(s.indexes.iter().map(|i| i.id))).max().unwrap_or(0) + 1
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    Default,
    Strict,
    Relaxed,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BackendError {
    /// Another process holds the database (IDB-D3): `open()` → UnknownError.
    Locked(String),
    /// A write would exceed the quota: QuotaExceededError.
    Quota { quota: u64, requested: u64 },
    /// Anything else the medium reported: UnknownError.
    Io(String),
}

pub type R<T> = Result<T, BackendError>;

pub trait Backend: Send {
    /// For advisories: where the bytes live.
    fn describe(&self) -> String;
    /// Does anything survive a restart?
    fn persistent(&self) -> bool;
    /// Every database that exists, with its version (version 0 = mid-creation).
    fn databases(&mut self) -> R<Vec<(Vec<u16>, u64)>>;
    /// Opens a database, creating it at version 0 if absent.
    fn open(&mut self, name: &[u16]) -> R<Box<dyn Database>>;
    /// Deletes a database; returns its version (0 if it did not exist). The
    /// server only calls this with no handle of that database open.
    fn delete(&mut self, name: &[u16]) -> R<u64>;
    /// Bytes in use, across every database (`navigator.storage.estimate`).
    fn usage(&mut self) -> u64;
    /// The cap writes are held to.
    fn quota(&mut self) -> u64;
}

pub trait Database: Send {
    /// The metadata as of now — including an upgrade's uncommitted changes.
    fn meta(&self) -> &DbMeta;

    fn begin_write(&mut self, durability: Durability) -> R<()>;
    fn commit_write(&mut self) -> R<()>;
    fn rollback_write(&mut self);
    /// A point inside the write transaction that [`Database::rollback_to`]
    /// returns to: a request that fails half-way undoes only itself (SQLite:
    /// `SAVEPOINT`). Outside a write transaction, a no-op.
    fn savepoint(&mut self) -> usize;
    fn rollback_to(&mut self, sp: usize);

    // Schema — only inside a write transaction of an upgrade.
    fn set_version(&mut self, v: u64) -> R<()>;
    fn create_store(&mut self, meta: StoreMeta) -> R<()>;
    fn delete_store(&mut self, id: StoreId) -> R<()>;
    fn rename_store(&mut self, id: StoreId, name: Vec<u16>) -> R<()>;
    fn create_index(&mut self, store: StoreId, meta: IndexMeta) -> R<()>;
    fn delete_index(&mut self, store: StoreId, id: IndexId) -> R<()>;
    fn rename_index(&mut self, store: StoreId, id: IndexId, name: Vec<u16>) -> R<()>;

    /// The key generator's current number (starts at 1).
    fn key_generator(&mut self, store: StoreId) -> R<f64>;
    fn set_key_generator(&mut self, store: StoreId, v: f64) -> R<()>;

    // Records.
    fn get(&mut self, store: StoreId, key: &[u8]) -> R<Option<Value>>;
    fn put(&mut self, store: StoreId, key: &[u8], value: Value) -> R<()>;
    fn delete(&mut self, store: StoreId, key: &[u8]) -> R<()>;
    /// Keys in `(lo, hi)`, ascending (or descending), at most `limit`.
    fn keys(&mut self, store: StoreId, lo: Bound<&[u8]>, hi: Bound<&[u8]>, reverse: bool, limit: usize) -> R<Vec<Vec<u8>>>;
    fn count(&mut self, store: StoreId, lo: Bound<&[u8]>, hi: Bound<&[u8]>) -> R<u64>;
    fn clear(&mut self, store: StoreId) -> R<()>;

    // Index entries (`index key ‖ primary key`).
    fn index_add(&mut self, index: IndexId, entry: Vec<u8>) -> R<()>;
    fn index_remove(&mut self, index: IndexId, entry: &[u8]) -> R<()>;
    fn index_scan(&mut self, index: IndexId, lo: Bound<&[u8]>, hi: Bound<&[u8]>, reverse: bool, limit: usize) -> R<Vec<Vec<u8>>>;
    fn index_count(&mut self, index: IndexId, lo: Bound<&[u8]>, hi: Bound<&[u8]>) -> R<u64>;
    fn index_clear(&mut self, index: IndexId) -> R<()>;
}
