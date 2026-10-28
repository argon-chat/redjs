//! The in-memory backend: ordered maps, an undo log for the one write
//! transaction a database can have at a time, and byte accounting against the
//! quota. Nothing survives the process; `persistent()` says so, and the module
//! above advises once.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Bound;
use std::sync::{Arc, Mutex, OnceLock};

use super::backend::*;

#[derive(Default, Clone)]
struct MemStore {
    records: BTreeMap<Vec<u8>, Value>,
    key_gen: f64,
}

enum Undo {
    Put { store: StoreId, key: Vec<u8>, old: Option<Value> },
    IndexAdd { index: IndexId, entry: Vec<u8> },
    IndexRemove { index: IndexId, entry: Vec<u8> },
    KeyGen { store: StoreId, old: f64 },
    Meta(DbMeta),
    StoreCreated { id: StoreId },
    StoreDeleted { id: StoreId, data: MemStore, indexes: Vec<(IndexId, BTreeSet<Vec<u8>>)> },
    IndexCreated { id: IndexId },
    IndexDeleted { id: IndexId, data: BTreeSet<Vec<u8>> },
    Cleared { store: StoreId, records: BTreeMap<Vec<u8>, Value> },
    IndexCleared { index: IndexId, data: BTreeSet<Vec<u8>> },
}

struct MemDb {
    meta: DbMeta,
    stores: HashMap<StoreId, MemStore>,
    indexes: HashMap<IndexId, BTreeSet<Vec<u8>>>,
    undo: Option<Vec<Undo>>,
    usage: Arc<Mutex<u64>>,
    quota: u64,
}

fn rec_size(k: &[u8], v: &[u8]) -> u64 {
    (k.len() + v.len()) as u64
}

impl MemDb {
    fn log(&mut self, u: Undo) {
        if let Some(l) = &mut self.undo {
            l.push(u);
        }
    }

    fn add_usage(&self, delta: i64) {
        let mut u = self.usage.lock().unwrap();
        *u = (*u as i64 + delta).max(0) as u64;
    }

    fn store_mut(&mut self, id: StoreId) -> R<&mut MemStore> {
        self.stores.get_mut(&id).ok_or_else(|| BackendError::Io(format!("no object store #{id}")))
    }

    fn index_mut(&mut self, id: IndexId) -> R<&mut BTreeSet<Vec<u8>>> {
        self.indexes.get_mut(&id).ok_or_else(|| BackendError::Io(format!("no index #{id}")))
    }

    fn undo_one(&mut self, u: Undo) {
        match u {
            Undo::Put { store, key, old } => {
                if let Some(s) = self.stores.get_mut(&store) {
                    let cur = match &old {
                        Some(v) => s.records.insert(key.clone(), v.clone()),
                        None => s.records.remove(&key),
                    };
                    let delta = old.as_ref().map(|v| rec_size(&key, v) as i64).unwrap_or(0)
                        - cur.as_ref().map(|v| rec_size(&key, v) as i64).unwrap_or(0);
                    self.add_usage(delta);
                }
            }
            Undo::IndexAdd { index, entry } => {
                if let Some(i) = self.indexes.get_mut(&index) {
                    if i.remove(&entry) {
                        self.add_usage(-(entry.len() as i64));
                    }
                }
            }
            Undo::IndexRemove { index, entry } => {
                if let Some(i) = self.indexes.get_mut(&index) {
                    let n = entry.len() as i64;
                    if i.insert(entry) {
                        self.add_usage(n);
                    }
                }
            }
            Undo::KeyGen { store, old } => {
                if let Some(s) = self.stores.get_mut(&store) {
                    s.key_gen = old;
                }
            }
            Undo::Meta(m) => self.meta = m,
            Undo::StoreCreated { id } => {
                if let Some(s) = self.stores.remove(&id) {
                    self.add_usage(-(s.records.iter().map(|(k, v)| rec_size(k, v)).sum::<u64>() as i64));
                }
            }
            Undo::StoreDeleted { id, data, indexes } => {
                self.add_usage(data.records.iter().map(|(k, v)| rec_size(k, v)).sum::<u64>() as i64);
                self.stores.insert(id, data);
                for (iid, set) in indexes {
                    self.add_usage(set.iter().map(|e| e.len() as u64).sum::<u64>() as i64);
                    self.indexes.insert(iid, set);
                }
            }
            Undo::IndexCreated { id } => {
                if let Some(set) = self.indexes.remove(&id) {
                    self.add_usage(-(set.iter().map(|e| e.len() as u64).sum::<u64>() as i64));
                }
            }
            Undo::IndexDeleted { id, data } => {
                self.add_usage(data.iter().map(|e| e.len() as u64).sum::<u64>() as i64);
                self.indexes.insert(id, data);
            }
            Undo::Cleared { store, records } => {
                if let Some(s) = self.stores.get_mut(&store) {
                    let n = records.iter().map(|(k, v)| rec_size(k, v)).sum::<u64>() as i64;
                    let m = s.records.iter().map(|(k, v)| rec_size(k, v)).sum::<u64>() as i64;
                    s.records = records;
                    self.add_usage(n - m);
                }
            }
            Undo::IndexCleared { index, data } => {
                let n = data.iter().map(|e| e.len() as u64).sum::<u64>() as i64;
                if let Some(i) = self.indexes.get_mut(&index) {
                    let m = i.iter().map(|e| e.len() as u64).sum::<u64>() as i64;
                    *i = data;
                    self.add_usage(n - m);
                }
            }
        }
    }
}

fn to_owned_bound(b: Bound<&[u8]>) -> Bound<Vec<u8>> {
    match b {
        Bound::Included(x) => Bound::Included(x.to_vec()),
        Bound::Excluded(x) => Bound::Excluded(x.to_vec()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// An empty or inverted range would make `BTreeMap::range` panic.
fn sane(lo: &Bound<Vec<u8>>, hi: &Bound<Vec<u8>>) -> bool {
    match (lo, hi) {
        (Bound::Included(a), Bound::Included(b)) => a <= b,
        (Bound::Included(a), Bound::Excluded(b)) | (Bound::Excluded(a), Bound::Included(b)) => a < b,
        (Bound::Excluded(a), Bound::Excluded(b)) => a < b,
        _ => true,
    }
}

/// A handle on one in-memory database.
struct MemHandle(Arc<Mutex<MemDb>>);

/// `meta()` returns a reference, and the database lives behind a mutex: the
/// handle keeps a copy that every schema change refreshes. The server is one
/// thread, so the copy and the original never disagree when read.
struct Handle {
    db: MemHandle,
    meta: DbMeta,
}

impl Handle {
    fn with<T>(&mut self, f: impl FnOnce(&mut MemDb) -> T) -> T {
        let mut g = self.db.0.lock().unwrap();
        f(&mut g)
    }
    /// A schema change (or a rollback): the cached metadata follows.
    fn schema<T>(&mut self, f: impl FnOnce(&mut MemDb) -> T) -> T {
        let mut g = self.db.0.lock().unwrap();
        let out = f(&mut g);
        self.meta = g.meta.clone();
        out
    }
    fn read<T>(&self, f: impl FnOnce(&MemDb) -> T) -> T {
        let g = self.db.0.lock().unwrap();
        f(&g)
    }
}

impl Database for Handle {
    fn meta(&self) -> &DbMeta {
        &self.meta
    }

    fn begin_write(&mut self, _durability: Durability) -> R<()> {
        self.with(|d| {
            d.undo = Some(Vec::new());
        });
        Ok(())
    }

    fn commit_write(&mut self) -> R<()> {
        self.with(|d| d.undo = None);
        Ok(())
    }

    fn rollback_write(&mut self) {
        self.schema(|d| {
            if let Some(mut log) = d.undo.take() {
                while let Some(u) = log.pop() {
                    d.undo_one(u);
                }
            }
        });
    }

    fn savepoint(&mut self) -> usize {
        self.read(|d| d.undo.as_ref().map(|l| l.len()).unwrap_or(0))
    }

    fn rollback_to(&mut self, sp: usize) {
        self.schema(|d| {
            let Some(mut log) = d.undo.take() else { return };
            while log.len() > sp {
                let u = log.pop().unwrap();
                d.undo_one(u);
            }
            d.undo = Some(log);
        });
    }

    fn set_version(&mut self, v: u64) -> R<()> {
        self.schema(|d| {
            let m = d.meta.clone();
            d.log(Undo::Meta(m));
            d.meta.version = v;
        });
        Ok(())
    }

    fn create_store(&mut self, meta: StoreMeta) -> R<()> {
        self.schema(|d| {
            let m = d.meta.clone();
            d.log(Undo::Meta(m));
            d.log(Undo::StoreCreated { id: meta.id });
            d.stores.insert(meta.id, MemStore { records: BTreeMap::new(), key_gen: 1.0 });
            for ix in &meta.indexes {
                d.indexes.insert(ix.id, BTreeSet::new());
            }
            d.meta.stores.push(meta);
        });
        Ok(())
    }

    fn delete_store(&mut self, id: StoreId) -> R<()> {
        self.schema(|d| {
            let m = d.meta.clone();
            let index_ids: Vec<IndexId> = m.store(id).map(|s| s.indexes.iter().map(|i| i.id).collect()).unwrap_or_default();
            d.log(Undo::Meta(m));
            let data = d.stores.remove(&id).unwrap_or_default();
            d.add_usage(-(data.records.iter().map(|(k, v)| rec_size(k, v)).sum::<u64>() as i64));
            let mut indexes = Vec::new();
            for iid in index_ids {
                if let Some(set) = d.indexes.remove(&iid) {
                    d.add_usage(-(set.iter().map(|e| e.len() as u64).sum::<u64>() as i64));
                    indexes.push((iid, set));
                }
            }
            d.log(Undo::StoreDeleted { id, data, indexes });
            d.meta.stores.retain(|s| s.id != id);
        });
        Ok(())
    }

    fn rename_store(&mut self, id: StoreId, name: Vec<u16>) -> R<()> {
        self.schema(|d| {
            let m = d.meta.clone();
            d.log(Undo::Meta(m));
            if let Some(s) = d.meta.stores.iter_mut().find(|s| s.id == id) {
                s.name = name;
            }
        });
        Ok(())
    }

    fn create_index(&mut self, store: StoreId, meta: IndexMeta) -> R<()> {
        self.schema(|d| {
            let m = d.meta.clone();
            d.log(Undo::Meta(m));
            d.log(Undo::IndexCreated { id: meta.id });
            d.indexes.insert(meta.id, BTreeSet::new());
            if let Some(s) = d.meta.stores.iter_mut().find(|s| s.id == store) {
                s.indexes.push(meta);
            }
        });
        Ok(())
    }

    fn delete_index(&mut self, store: StoreId, id: IndexId) -> R<()> {
        self.schema(|d| {
            let m = d.meta.clone();
            d.log(Undo::Meta(m));
            let data = d.indexes.remove(&id).unwrap_or_default();
            d.add_usage(-(data.iter().map(|e| e.len() as u64).sum::<u64>() as i64));
            d.log(Undo::IndexDeleted { id, data });
            if let Some(s) = d.meta.stores.iter_mut().find(|s| s.id == store) {
                s.indexes.retain(|i| i.id != id);
            }
        });
        Ok(())
    }

    fn rename_index(&mut self, store: StoreId, id: IndexId, name: Vec<u16>) -> R<()> {
        self.schema(|d| {
            let m = d.meta.clone();
            d.log(Undo::Meta(m));
            if let Some(s) = d.meta.stores.iter_mut().find(|s| s.id == store) {
                if let Some(i) = s.indexes.iter_mut().find(|i| i.id == id) {
                    i.name = name;
                }
            }
        });
        Ok(())
    }

    fn key_generator(&mut self, store: StoreId) -> R<f64> {
        self.with(|d| d.store_mut(store).map(|s| s.key_gen))
    }

    fn set_key_generator(&mut self, store: StoreId, v: f64) -> R<()> {
        self.with(|d| {
            let s = d.store_mut(store)?;
            let old = s.key_gen;
            s.key_gen = v;
            d.log(Undo::KeyGen { store, old });
            Ok(())
        })
    }

    fn get(&mut self, store: StoreId, key: &[u8]) -> R<Option<Value>> {
        self.read(|d| Ok(d.stores.get(&store).and_then(|s| s.records.get(key).cloned())))
    }

    fn put(&mut self, store: StoreId, key: &[u8], value: Value) -> R<()> {
        self.with(|d| {
            let quota = d.quota;
            let used = *d.usage.lock().unwrap();
            let s = d.store_mut(store)?;
            let new = rec_size(key, &value);
            let old_size = s.records.get(key).map(|v| rec_size(key, v)).unwrap_or(0);
            if used.saturating_sub(old_size).saturating_add(new) > quota {
                return Err(BackendError::Quota { quota, requested: new });
            }
            let old = s.records.insert(key.to_vec(), value);
            d.add_usage(new as i64 - old_size as i64);
            d.log(Undo::Put { store, key: key.to_vec(), old });
            Ok(())
        })
    }

    fn delete(&mut self, store: StoreId, key: &[u8]) -> R<()> {
        self.with(|d| {
            let s = d.store_mut(store)?;
            if let Some(old) = s.records.remove(key) {
                d.add_usage(-(rec_size(key, &old) as i64));
                d.log(Undo::Put { store, key: key.to_vec(), old: Some(old) });
            }
            Ok(())
        })
    }

    fn keys(&mut self, store: StoreId, lo: Bound<&[u8]>, hi: Bound<&[u8]>, reverse: bool, limit: usize) -> R<Vec<Vec<u8>>> {
        self.read(|d| {
            let Some(s) = d.stores.get(&store) else { return Ok(Vec::new()) };
            let (lo, hi) = (to_owned_bound(lo), to_owned_bound(hi));
            if !sane(&lo, &hi) {
                return Ok(Vec::new());
            }
            let r = s.records.range((lo, hi));
            Ok(if reverse { r.rev().take(limit).map(|(k, _)| k.clone()).collect() } else { r.take(limit).map(|(k, _)| k.clone()).collect() })
        })
    }

    fn count(&mut self, store: StoreId, lo: Bound<&[u8]>, hi: Bound<&[u8]>) -> R<u64> {
        self.read(|d| {
            let Some(s) = d.stores.get(&store) else { return Ok(0) };
            let (lo, hi) = (to_owned_bound(lo), to_owned_bound(hi));
            if !sane(&lo, &hi) {
                return Ok(0);
            }
            if matches!((&lo, &hi), (Bound::Unbounded, Bound::Unbounded)) {
                return Ok(s.records.len() as u64);
            }
            Ok(s.records.range((lo, hi)).count() as u64)
        })
    }

    fn clear(&mut self, store: StoreId) -> R<()> {
        self.with(|d| {
            let s = d.store_mut(store)?;
            let records = core::mem::take(&mut s.records);
            d.add_usage(-(records.iter().map(|(k, v)| rec_size(k, v)).sum::<u64>() as i64));
            d.log(Undo::Cleared { store, records });
            Ok(())
        })
    }

    fn index_add(&mut self, index: IndexId, entry: Vec<u8>) -> R<()> {
        self.with(|d| {
            let n = entry.len() as i64;
            let set = d.index_mut(index)?;
            if set.insert(entry.clone()) {
                d.add_usage(n);
                d.log(Undo::IndexAdd { index, entry });
            }
            Ok(())
        })
    }

    fn index_remove(&mut self, index: IndexId, entry: &[u8]) -> R<()> {
        self.with(|d| {
            let set = d.index_mut(index)?;
            if set.remove(entry) {
                d.add_usage(-(entry.len() as i64));
                d.log(Undo::IndexRemove { index, entry: entry.to_vec() });
            }
            Ok(())
        })
    }

    fn index_scan(&mut self, index: IndexId, lo: Bound<&[u8]>, hi: Bound<&[u8]>, reverse: bool, limit: usize) -> R<Vec<Vec<u8>>> {
        self.read(|d| {
            let Some(set) = d.indexes.get(&index) else { return Ok(Vec::new()) };
            let (lo, hi) = (to_owned_bound(lo), to_owned_bound(hi));
            if !sane(&lo, &hi) {
                return Ok(Vec::new());
            }
            let r = set.range((lo, hi));
            Ok(if reverse { r.rev().take(limit).cloned().collect() } else { r.take(limit).cloned().collect() })
        })
    }

    fn index_count(&mut self, index: IndexId, lo: Bound<&[u8]>, hi: Bound<&[u8]>) -> R<u64> {
        self.read(|d| {
            let Some(set) = d.indexes.get(&index) else { return Ok(0) };
            let (lo, hi) = (to_owned_bound(lo), to_owned_bound(hi));
            if !sane(&lo, &hi) {
                return Ok(0);
            }
            Ok(set.range((lo, hi)).count() as u64)
        })
    }

    fn index_clear(&mut self, index: IndexId) -> R<()> {
        self.with(|d| {
            let set = d.index_mut(index)?;
            let data = core::mem::take(set);
            d.add_usage(-(data.iter().map(|e| e.len() as u64).sum::<u64>() as i64));
            d.log(Undo::IndexCleared { index, data });
            Ok(())
        })
    }
}

/// Every database, in this process's memory.
pub struct MemoryBackend {
    dbs: HashMap<Vec<u16>, Arc<Mutex<MemDb>>>,
    usage: Arc<Mutex<u64>>,
    quota: u64,
}

impl MemoryBackend {
    pub fn new(quota: u64) -> MemoryBackend {
        MemoryBackend { dbs: HashMap::new(), usage: Arc::new(Mutex::new(0)), quota }
    }
}

/// Test seam: make the next `open` of a name fail as a backend would (another
/// process holding the file, an I/O error) — the paths the memory backend can
/// never take by itself.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn fail_next_open(name: &[u16], err: BackendError) {
    failures().lock().unwrap().insert(name.to_vec(), err);
}

fn failures() -> &'static Mutex<HashMap<Vec<u16>, BackendError>> {
    static F: OnceLock<Mutex<HashMap<Vec<u16>, BackendError>>> = OnceLock::new();
    F.get_or_init(|| Mutex::new(HashMap::new()))
}

impl Backend for MemoryBackend {
    fn describe(&self) -> String {
        "process memory (nothing survives a restart)".into()
    }

    fn persistent(&self) -> bool {
        false
    }

    fn databases(&mut self) -> R<Vec<(Vec<u16>, u64)>> {
        let mut v: Vec<(Vec<u16>, u64)> = self.dbs.iter().map(|(n, d)| (n.clone(), d.lock().unwrap().meta.version)).collect();
        v.sort();
        Ok(v)
    }

    fn open(&mut self, name: &[u16]) -> R<Box<dyn Database>> {
        if let Some(e) = failures().lock().unwrap().remove(name) {
            return Err(e);
        }
        let usage = self.usage.clone();
        let quota = self.quota;
        let db = self
            .dbs
            .entry(name.to_vec())
            .or_insert_with(|| {
                Arc::new(Mutex::new(MemDb {
                    meta: DbMeta { name: name.to_vec(), version: 0, stores: Vec::new() },
                    stores: HashMap::new(),
                    indexes: HashMap::new(),
                    undo: None,
                    usage,
                    quota,
                }))
            })
            .clone();
        let meta = db.lock().unwrap().meta.clone();
        Ok(Box::new(Handle { db: MemHandle(db), meta }))
    }

    fn delete(&mut self, name: &[u16]) -> R<u64> {
        match self.dbs.remove(name) {
            Some(db) => {
                let d = db.lock().unwrap();
                let bytes: u64 = d.stores.values().flat_map(|s| s.records.iter().map(|(k, v)| rec_size(k, v))).sum::<u64>()
                    + d.indexes.values().flat_map(|s| s.iter().map(|e| e.len() as u64)).sum::<u64>();
                d.add_usage(-(bytes as i64));
                Ok(d.meta.version)
            }
            None => Ok(0),
        }
    }

    fn usage(&mut self) -> u64 {
        *self.usage.lock().unwrap()
    }

    fn quota(&mut self) -> u64 {
        self.quota
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(id: u64) -> StoreMeta {
        StoreMeta { id, name: vec![b's' as u16], key_path: None, auto_increment: false, indexes: vec![] }
    }

    #[test]
    fn rollback_reverts_records_schema_and_generator() {
        let mut b = MemoryBackend::new(1 << 30);
        let mut db = b.open(&[b'x' as u16]).unwrap();
        db.begin_write(Durability::Default).unwrap();
        db.set_version(1).unwrap();
        db.create_store(st(1)).unwrap();
        db.put(1, b"k1", Arc::new(vec![1, 2, 3])).unwrap();
        db.commit_write().unwrap();
        let used = b.usage();
        assert_eq!(used, 5);

        db.begin_write(Durability::Default).unwrap();
        db.put(1, b"k1", Arc::new(vec![9])).unwrap();
        db.put(1, b"k2", Arc::new(vec![9])).unwrap();
        db.set_key_generator(1, 7.0).unwrap();
        db.clear(1).unwrap();
        db.create_index(1, IndexMeta { id: 2, name: vec![], key_path: super::super::keypath::KeyPath::String(vec![]), unique: false, multi_entry: false }).unwrap();
        db.index_add(2, b"ab".to_vec()).unwrap();
        db.set_version(2).unwrap();
        db.rollback_write();

        assert_eq!(db.meta().version, 1);
        assert_eq!(db.meta().stores[0].indexes.len(), 0);
        assert_eq!(db.get(1, b"k1").unwrap().as_deref(), Some(&vec![1, 2, 3]));
        assert_eq!(db.get(1, b"k2").unwrap(), None);
        assert_eq!(db.key_generator(1).unwrap(), 1.0);
        assert_eq!(b.usage(), used, "byte accounting rolls back too");
    }

    #[test]
    fn ranges_counts_and_quota() {
        let mut b = MemoryBackend::new(20);
        let mut db = b.open(&[b'q' as u16]).unwrap();
        db.begin_write(Durability::Default).unwrap();
        db.create_store(st(1)).unwrap();
        for k in [b"a", b"b", b"c", b"d"] {
            db.put(1, k, Arc::new(vec![0])).unwrap();
        }
        assert_eq!(db.keys(1, Bound::Excluded(b"a"), Bound::Included(b"c"), false, 10).unwrap(), vec![b"b".to_vec(), b"c".to_vec()]);
        assert_eq!(db.keys(1, Bound::Unbounded, Bound::Unbounded, true, 1).unwrap(), vec![b"d".to_vec()]);
        assert_eq!(db.count(1, Bound::Included(b"b"), Bound::Unbounded).unwrap(), 3);
        assert!(db.keys(1, Bound::Included(b"z"), Bound::Included(b"a"), false, 10).unwrap().is_empty(), "inverted is empty, not a panic");
        match db.put(1, b"big", Arc::new(vec![0; 64])) {
            Err(BackendError::Quota { quota: 20, .. }) => {}
            other => panic!("{other:?}"),
        }
        db.commit_write().unwrap();
    }
}
