//! The persistent backend (m25 P2): one SQLite file per IndexedDB database,
//! in SQLite3 Multiple Ciphers (`cpp/idb_sqlite.c`, the `indexeddb-sqlite`
//! feature), behind the same [`Backend`] / [`Database`] trait the memory
//! backend implements. Everything IndexedDB-specific (indexes, uniqueness,
//! key generation, cursors, the scheduler) stays in the server; this is an
//! ordered byte store with one write transaction and savepoints.
//!
//! # On disk (format 1 — `FORMAT`)
//!
//! ```text
//! <folder>\                    the host's folder (vue-native: <storage folder>\indexeddb)
//!   lock                       held open, write-locked, for the life of the process
//!   <h>.sqlite3                one database; <h>.sqlite3-wal while it is open
//!   <h>.unreadable-<utc>.sqlite3   a file we could not open, moved aside — never deleted
//! ```
//!
//! `h` = the first 32 hex digits of SHA-256(`"redjs-idb-v1\0"` ‖ UTF-16LE(name)):
//! a database name is any UTF-16 string, and a hash is the one file name that
//! is always legal and that leaks nothing when the file is encrypted. The real
//! name is inside, in `meta`.
//!
//! ```sql
//! meta   (k TEXT PRIMARY KEY, v) WITHOUT ROWID   -- format=1, name (UTF-16LE), version
//! store  (id INTEGER PRIMARY KEY, name BLOB, key_path BLOB, auto_increment INTEGER, key_gen REAL)
//! idx    (id INTEGER PRIMARY KEY, store_id INTEGER, name BLOB, key_path BLOB, is_unique INTEGER, multi_entry INTEGER)
//! record (store_id INTEGER, key BLOB, value BLOB, PRIMARY KEY (store_id, key))
//! entry  (index_id INTEGER, entry BLOB, PRIMARY KEY (index_id, entry)) WITHOUT ROWID
//! ```
//!
//! - **Keys** are [`super::key`]'s memcmp-ordered encoding and index entries are
//!   `index key ‖ primary key` — SQLite's BLOB order IS memcmp, so the ordered
//!   indexes need no collation. `record` is a rowid table because values run
//!   from bytes to megabytes (SQLite's WITHOUT ROWID guidance is rows under
//!   ~1/20 of a page); `entry` holds only keys, so it is WITHOUT ROWID.
//! - **Values** are the serializer's storage bytes (`serialize.rs`, header
//!   `RJS\x01` with the storage flag; P0 froze that format at version 1).
//! - **Key paths** are `0x00 ‖ UTF-16LE` (a string) or `0x01 ‖ (u32 LE units,
//!   UTF-16LE)*` (a list). `key_gen` lives in the database, so a rollback
//!   reverts the generator for free.
//! - `PRAGMA user_version` = [`FORMAT`]. A file from a newer format is refused
//!   (UnknownError), never rewritten.
//!
//! # Encryption (design §5.2, §5.5)
//!
//! Whole-page ChaCha20-Poly1305, WAL included, under a raw 256-bit key the host
//! supplies (vue-native: random, DPAPI-sealed). Each file's own header decides
//! what it is (`SQLite format 3\0` = plaintext), and the server's start brings
//! every file to the requested format before the first open: plaintext files
//! are encrypted (`Encrypted`), encrypted ones decrypted (`Decrypting`), and an
//! encrypted file this run cannot read — the key is gone or another one — is
//! MOVED ASIDE, with an advisory, never deleted and never overwritten.
//!
//! # Locking (IDB-D3)
//!
//! The server takes `<folder>\lock` with a write lock nobody else may share,
//! and holds it until the process ends — the OS releases it on any exit, a kill
//! included, so it never goes stale. A second process on the same folder gets
//! `Locked` from every open (`open()` → UnknownError + an advisory naming the
//! holder's pid), and tries again on its next open. Every connection is also
//! `locking_mode=EXCLUSIVE`, which keeps the WAL index in heap memory (no
//! `-shm`) and is a second wall.
//!
//! # Durability (§3.4) — never on the frame thread
//!
//! All of this runs on the IndexedDB server thread. WAL with
//! `synchronous=NORMAL` for `relaxed`/`default` (a committed transaction
//! survives a crash or a kill of the app; the browsers' default since Chrome
//! 121 / Firefox 40), `synchronous=FULL` for a `strict` transaction (the WAL is
//! fsynced before `complete`). Closing a database (shutdown, delete)
//! checkpoints the WAL into the main file and removes it.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};

use super::backend::*;
use super::keypath::KeyPath;
use super::IdbEncryption;

/// The on-disk format of this backend (`PRAGMA user_version`).
pub const FORMAT: i64 = 1;

// ---------------------------------------------------------------------------
// FFI — cpp/idb_sqlite.c
// ---------------------------------------------------------------------------

extern "C" {
    fn rjs_sql_open(path: *const c_char, out: *mut *mut c_void) -> c_int;
    fn rjs_sql_close(db: *mut c_void) -> c_int;
    fn rjs_sql_exec(db: *mut c_void, sql: *const c_char) -> c_int;
    fn rjs_sql_errmsg(db: *mut c_void) -> *const c_char;
    fn rjs_sql_errcode(db: *mut c_void) -> c_int;
    fn rjs_sql_errstr(rc: c_int) -> *const c_char;
    fn rjs_sql_autocommit(db: *mut c_void) -> c_int;
    fn rjs_sql_version() -> *const c_char;
    fn rjs_sql_mc_version() -> *const c_char;
    fn rjs_sql_prepare(db: *mut c_void, sql: *const c_char, n: c_int, out: *mut *mut c_void) -> c_int;
    fn rjs_sql_step(s: *mut c_void) -> c_int;
    fn rjs_sql_reset(s: *mut c_void) -> c_int;
    fn rjs_sql_clear_bindings(s: *mut c_void) -> c_int;
    fn rjs_sql_finalize(s: *mut c_void) -> c_int;
    fn rjs_sql_bind_blob(s: *mut c_void, i: c_int, p: *const c_void, n: c_int) -> c_int;
    fn rjs_sql_bind_i64(s: *mut c_void, i: c_int, v: i64) -> c_int;
    fn rjs_sql_bind_f64(s: *mut c_void, i: c_int, v: f64) -> c_int;
    fn rjs_sql_column_type(s: *mut c_void, i: c_int) -> c_int;
    fn rjs_sql_column_blob(s: *mut c_void, i: c_int) -> *const c_void;
    fn rjs_sql_column_bytes(s: *mut c_void, i: c_int) -> c_int;
    fn rjs_sql_column_i64(s: *mut c_void, i: c_int) -> i64;
    fn rjs_sql_column_f64(s: *mut c_void, i: c_int) -> f64;
    fn rjs_sql_key(db: *mut c_void, key: *const u8) -> c_int;
    fn rjs_sql_rekey(db: *mut c_void, key: *const u8) -> c_int;
}

const OK: c_int = 0;
const BUSY: c_int = 5;
const LOCKED: c_int = 6;
const CORRUPT: c_int = 11;
const FULL: c_int = 13;
const NOTADB: c_int = 26;
const ROW: c_int = 100;
const DONE: c_int = 101;
const T_NULL: c_int = 5;

/// `"SQLite 3.54.0, SQLite3 Multiple Ciphers 2.6.1"` — for diagnostics.
pub fn version() -> String {
    unsafe {
        format!(
            "SQLite {}, {}",
            CStr::from_ptr(rjs_sql_version()).to_string_lossy(),
            CStr::from_ptr(rjs_sql_mc_version()).to_string_lossy()
        )
    }
}

#[derive(Debug, Clone)]
struct SqlErr {
    code: c_int,
    msg: String,
}

impl SqlErr {
    fn primary(&self) -> c_int {
        self.code & 0xff
    }
}

impl std::fmt::Display for SqlErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (SQLite {})", self.msg, self.code)
    }
}

type SR<T> = Result<T, SqlErr>;

struct Conn {
    db: *mut c_void,
}

// A connection is only ever touched by one thread at a time (the server
// thread owns every one; SQLITE_THREADSAFE=2).
unsafe impl Send for Conn {}

impl Conn {
    fn open(path: &Path) -> SR<Conn> {
        let p = CString::new(path.to_string_lossy().as_bytes()).map_err(|_| SqlErr { code: 1, msg: "a NUL in the path".into() })?;
        let mut db = core::ptr::null_mut();
        let rc = unsafe { rjs_sql_open(p.as_ptr(), &mut db) };
        let c = Conn { db };
        if rc != OK {
            let e = if db.is_null() { SqlErr { code: rc, msg: errstr(rc) } } else { c.err(rc) };
            return Err(e);
        }
        Ok(c)
    }

    fn err(&self, rc: c_int) -> SqlErr {
        let msg = unsafe { CStr::from_ptr(rjs_sql_errmsg(self.db)).to_string_lossy().into_owned() };
        let code = unsafe { rjs_sql_errcode(self.db) };
        SqlErr { code: if code != 0 { code } else { rc }, msg }
    }

    fn exec(&self, sql: &str) -> SR<()> {
        let s = CString::new(sql).unwrap();
        let rc = unsafe { rjs_sql_exec(self.db, s.as_ptr()) };
        if rc != OK {
            return Err(self.err(rc));
        }
        Ok(())
    }

    fn prepare(&self, sql: &str) -> SR<Stmt> {
        let mut s = core::ptr::null_mut();
        let rc = unsafe { rjs_sql_prepare(self.db, sql.as_ptr() as *const c_char, sql.len() as c_int, &mut s) };
        if rc != OK {
            return Err(self.err(rc));
        }
        Ok(Stmt(s))
    }

    fn key(&self, key: &[u8; 32]) -> SR<()> {
        let rc = unsafe { rjs_sql_key(self.db, key.as_ptr()) };
        if rc != OK {
            return Err(self.err(rc));
        }
        Ok(())
    }

    fn rekey(&self, key: Option<&[u8; 32]>) -> SR<()> {
        let rc = unsafe { rjs_sql_rekey(self.db, key.map(|k| k.as_ptr()).unwrap_or(core::ptr::null())) };
        if rc != OK {
            return Err(self.err(rc));
        }
        Ok(())
    }

    fn in_transaction(&self) -> bool {
        unsafe { rjs_sql_autocommit(self.db) == 0 }
    }

    /// One statement, prepared for this call only (schema, pragmas).
    fn once(&self, sql: &str, params: &[P]) -> SR<()> {
        let s = self.prepare(sql)?;
        self.run(&s, params, |_| true)
    }

    fn query_i64(&self, sql: &str) -> SR<Option<i64>> {
        let s = self.prepare(sql)?;
        let mut out = None;
        self.run(&s, &[], |r| {
            out = Some(r.i64(0));
            false
        })?;
        Ok(out)
    }

    /// Binds, steps (calling `row` per row until it returns false), resets.
    fn run(&self, s: &Stmt, params: &[P], mut row: impl FnMut(&Row) -> bool) -> SR<()> {
        let r = (|| {
            for (i, p) in params.iter().enumerate() {
                let i = (i + 1) as c_int;
                let rc = unsafe {
                    match p {
                        P::B(b) => rjs_sql_bind_blob(s.0, i, b.as_ptr() as *const c_void, b.len() as c_int),
                        P::I(v) => rjs_sql_bind_i64(s.0, i, *v),
                        P::F(v) => rjs_sql_bind_f64(s.0, i, *v),
                    }
                };
                if rc != OK {
                    return Err(self.err(rc));
                }
            }
            loop {
                let rc = unsafe { rjs_sql_step(s.0) };
                match rc {
                    ROW => {
                        if !row(&Row(s.0)) {
                            return Ok(());
                        }
                    }
                    DONE => return Ok(()),
                    _ => return Err(self.err(rc)),
                }
            }
        })();
        unsafe {
            rjs_sql_reset(s.0);
            rjs_sql_clear_bindings(s.0);
        }
        r
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        if !self.db.is_null() {
            unsafe { rjs_sql_close(self.db) };
        }
    }
}

fn errstr(rc: c_int) -> String {
    unsafe { CStr::from_ptr(rjs_sql_errstr(rc)).to_string_lossy().into_owned() }
}

struct Stmt(*mut c_void);

unsafe impl Send for Stmt {}

impl Drop for Stmt {
    fn drop(&mut self) {
        unsafe { rjs_sql_finalize(self.0) };
    }
}

enum P<'a> {
    B(&'a [u8]),
    I(i64),
    F(f64),
}

struct Row(*mut c_void);

impl Row {
    fn blob(&self, i: c_int) -> &[u8] {
        unsafe {
            let p = rjs_sql_column_blob(self.0, i);
            let n = rjs_sql_column_bytes(self.0, i);
            if p.is_null() || n <= 0 {
                &[]
            } else {
                std::slice::from_raw_parts(p as *const u8, n as usize)
            }
        }
    }
    fn i64(&self, i: c_int) -> i64 {
        unsafe { rjs_sql_column_i64(self.0, i) }
    }
    fn f64(&self, i: c_int) -> f64 {
        unsafe { rjs_sql_column_f64(self.0, i) }
    }
    fn is_null(&self, i: c_int) -> bool {
        unsafe { rjs_sql_column_type(self.0, i) == T_NULL }
    }
}

// ---------------------------------------------------------------------------
// Encodings of the metadata
// ---------------------------------------------------------------------------

fn u16le(s: &[u16]) -> Vec<u8> {
    s.iter().flat_map(|u| u.to_le_bytes()).collect()
}

fn from_u16le(b: &[u8]) -> Vec<u16> {
    b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
}

fn enc_key_path(p: &KeyPath) -> Vec<u8> {
    match p {
        KeyPath::String(s) => {
            let mut v = vec![0u8];
            v.extend(u16le(s));
            v
        }
        KeyPath::List(items) => {
            let mut v = vec![1u8];
            for s in items {
                v.extend((s.len() as u32).to_le_bytes());
                v.extend(u16le(s));
            }
            v
        }
    }
}

fn dec_key_path(b: &[u8]) -> Option<KeyPath> {
    match b.first()? {
        0 => Some(KeyPath::String(from_u16le(&b[1..]))),
        1 => {
            let mut items = Vec::new();
            let mut i = 1;
            while i < b.len() {
                let n = u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?) as usize;
                i += 4;
                items.push(from_u16le(b.get(i..i + 2 * n)?));
                i += 2 * n;
            }
            Some(KeyPath::List(items))
        }
        _ => None,
    }
}

/// `<h>` of `<h>.sqlite3` (module header).
pub fn file_stem(name: &[u16]) -> String {
    let mut h = Sha256::new();
    h.update(b"redjs-idb-v1\0");
    h.update(u16le(name));
    h.finalize().iter().take(16).map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Kind {
    Absent,
    /// Created, never written (SQLite writes page 1 lazily).
    Empty,
    Plain,
    Encrypted,
}

fn classify(path: &Path) -> Kind {
    let Ok(mut f) = File::open(path) else { return Kind::Absent };
    let mut head = [0u8; 16];
    let mut n = 0;
    while n < 16 {
        match f.read(&mut head[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(_) => break,
        }
    }
    if n == 0 {
        Kind::Empty
    } else if n == 16 && &head == b"SQLite format 3\0" {
        Kind::Plain
    } else {
        Kind::Encrypted
    }
}

fn sidecars(path: &Path) -> [PathBuf; 3] {
    let s = path.as_os_str().to_string_lossy();
    [PathBuf::from(format!("{s}-wal")), PathBuf::from(format!("{s}-journal")), PathBuf::from(format!("{s}-shm"))]
}

fn file_bytes(path: &Path) -> u64 {
    let main = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    main + sidecars(path).iter().map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum::<u64>()
}

/// `20261028T191502Z` — for moved-aside file names.
fn utc_stamp() -> String {
    let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Moves `<h>.sqlite3` (and its WAL) to `<h>.unreadable-<utc>.sqlite3`.
fn move_aside(path: &Path, why: &str) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut to = dir.join(format!("{stem}.unreadable-{}.sqlite3", utc_stamp()));
    let mut n = 1;
    while to.exists() {
        to = dir.join(format!("{stem}.unreadable-{}-{n}.sqlite3", utc_stamp()));
        n += 1;
    }
    let _ = std::fs::rename(path, &to);
    for (side, suffix) in sidecars(path).iter().zip(["-wal", "-journal", "-shm"]) {
        if side.exists() {
            let _ = std::fs::rename(side, PathBuf::from(format!("{}{suffix}", to.display())));
        }
    }
    super::advise(
        &format!("moved-aside:{}", path.display()),
        format!(
            "IndexedDB: {} could not be opened ({why}). It was MOVED ASIDE to {} — not deleted — and that \
             database starts empty. If its key comes back (the same Windows user on the machine that wrote \
             it), the file can be restored by renaming it back.",
            path.display(),
            to.display()
        ),
    );
    to
}

// ---------------------------------------------------------------------------
// The folder lock
// ---------------------------------------------------------------------------

/// Takes `<folder>\lock` for this process, or says who holds it.
fn take_lock(folder: &Path) -> Result<File, String> {
    let path = folder.join("lock");
    let mut o = OpenOptions::new();
    o.read(true).write(true).create(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        o.share_mode(1 /* FILE_SHARE_READ: others may read who we are, nobody may write */);
    }
    match o.open(&path) {
        Ok(mut f) => {
            let _ = f.set_len(0);
            let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
            let _ = write!(f, "pid {}\nexe {exe}\n", std::process::id());
            let _ = f.flush();
            Ok(f)
        }
        Err(e) if matches!(e.raw_os_error(), Some(32) | Some(33)) => {
            let mut holder = String::new();
            let mut r = OpenOptions::new();
            r.read(true);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                r.share_mode(7);
            }
            if let Ok(mut f) = r.open(&path) {
                let _ = f.read_to_string(&mut holder);
            }
            let pid = holder.lines().find_map(|l| l.strip_prefix("pid ")).unwrap_or("?").trim().to_string();
            Err(format!("held by pid {pid}, lock file {}", path.display()))
        }
        Err(e) => Err(format!("cannot take {}: {e}", path.display())),
    }
}

// ---------------------------------------------------------------------------
// Shared state between the backend and its open databases
// ---------------------------------------------------------------------------

type QuotaFn = Arc<dyn Fn(u64) -> u64 + Send + Sync>;

struct Shared {
    /// stem → (name, version) of every open database.
    open: HashMap<String, (Vec<u16>, u64)>,
    /// stem → bytes on disk (main file + WAL).
    sizes: HashMap<String, u64>,
    quota: u64,
    quota_at: Instant,
    quota_fn: QuotaFn,
}

impl Shared {
    fn usage(&self) -> u64 {
        self.sizes.values().sum()
    }
    fn quota(&mut self, force: bool) -> u64 {
        if force || self.quota_at.elapsed() > Duration::from_secs(2) {
            self.quota = (self.quota_fn)(self.usage());
            self.quota_at = Instant::now();
        }
        self.quota
    }
}

// ---------------------------------------------------------------------------
// One database
// ---------------------------------------------------------------------------

/// The statements every request runs, prepared once per connection.
struct Stmts {
    get: Stmt,
    put: Stmt,
    del: Stmt,
    clear: Stmt,
    kg_get: Stmt,
    kg_set: Stmt,
    ix_add: Stmt,
    ix_del: Stmt,
    ix_clear: Stmt,
    begin: Stmt,
    commit: Stmt,
    rollback: Stmt,
    sp: Stmt,
    sp_release: Stmt,
    sp_rollback: Stmt,
}

/// Which bound clause a range statement carries: 0 none, 1 `>=`/`<=`, 2 `>`/`<`.
fn bk(b: &Bound<&[u8]>) -> u8 {
    match b {
        Bound::Unbounded => 0,
        Bound::Included(_) => 1,
        Bound::Excluded(_) => 2,
    }
}

struct SqliteDb {
    // Statements before the connection: fields drop in order, and a
    // connection closes only once its statements are finalized.
    st: Stmts,
    ranges: HashMap<(bool, u8, u8, bool, bool), Stmt>,
    conn: Conn,
    meta: DbMeta,
    stem: String,
    shared: Arc<Mutex<Shared>>,
    in_tx: bool,
    strict: bool,
    /// `savepoint()` was asked for and nothing has written since: the actual
    /// SAVEPOINT is taken by the first write (a read-only request costs none).
    sp_wanted: bool,
    sp_open: bool,
    sp_schema: bool,
    tx_bytes: u64,
}

fn io(e: SqlErr) -> BackendError {
    match e.primary() {
        BUSY | LOCKED => BackendError::Locked(e.to_string()),
        _ => BackendError::Io(e.to_string()),
    }
}

impl SqliteDb {
    fn write_err(&self, e: SqlErr) -> BackendError {
        if e.primary() == FULL {
            let mut s = self.shared.lock().unwrap();
            let quota = s.quota(true);
            return BackendError::Quota { quota, requested: self.tx_bytes };
        }
        io(e)
    }

    /// Before any write: the savepoint a request asked for, taken now.
    fn touch(&mut self) -> R<()> {
        if self.sp_wanted && self.in_tx {
            if self.sp_open {
                self.conn.run(&self.st.sp_release, &[], |_| true).map_err(io)?;
            }
            self.conn.run(&self.st.sp, &[], |_| true).map_err(io)?;
            self.sp_open = true;
            self.sp_schema = false;
        }
        self.sp_wanted = false;
        Ok(())
    }

    fn schema_write(&mut self, sql: &str, params: &[P]) -> R<()> {
        self.touch()?;
        self.sp_schema = true;
        self.conn.once(sql, params).map_err(|e| self.write_err(e))
    }

    fn reload_meta(&mut self) {
        if let Ok(m) = load_meta(&self.conn) {
            self.meta = m;
        }
        self.publish();
    }

    fn publish(&self) {
        let mut s = self.shared.lock().unwrap();
        s.open.insert(self.stem.clone(), (self.meta.name.clone(), self.meta.version));
    }

    fn range(&mut self, index: bool, lo: &Bound<&[u8]>, hi: &Bound<&[u8]>, reverse: bool, count: bool) -> R<&Stmt> {
        let k = (index, bk(lo), bk(hi), reverse, count);
        if !self.ranges.contains_key(&k) {
            let (table, owner, col) = if index { ("entry", "index_id", "entry") } else { ("record", "store_id", "key") };
            let mut sql = if count {
                format!("SELECT count(*) FROM {table} WHERE {owner}=?1")
            } else {
                format!("SELECT {col} FROM {table} WHERE {owner}=?1")
            };
            let mut n = 1;
            match k.1 {
                1 => { n += 1; sql.push_str(&format!(" AND {col}>=?{n}")) }
                2 => { n += 1; sql.push_str(&format!(" AND {col}>?{n}")) }
                _ => {}
            }
            match k.2 {
                1 => { n += 1; sql.push_str(&format!(" AND {col}<=?{n}")) }
                2 => { n += 1; sql.push_str(&format!(" AND {col}<?{n}")) }
                _ => {}
            }
            if !count {
                sql.push_str(&format!(" ORDER BY {col}{} LIMIT ?{}", if reverse { " DESC" } else { "" }, n + 1));
            }
            let s = self.conn.prepare(&sql).map_err(io)?;
            self.ranges.insert(k, s);
        }
        Ok(&self.ranges[&k])
    }

    fn scan(&mut self, index: bool, owner: u64, lo: Bound<&[u8]>, hi: Bound<&[u8]>, reverse: bool, limit: usize) -> R<Vec<Vec<u8>>> {
        let lim = if limit >= i64::MAX as usize { -1 } else { limit as i64 };
        let mut params = vec![P::I(owner as i64)];
        if let Bound::Included(b) | Bound::Excluded(b) = lo {
            params.push(P::B(b));
        }
        if let Bound::Included(b) | Bound::Excluded(b) = hi {
            params.push(P::B(b));
        }
        params.push(P::I(lim));
        self.range(index, &lo, &hi, reverse, false)?;
        let s = &self.ranges[&(index, bk(&lo), bk(&hi), reverse, false)];
        let mut out = Vec::new();
        self.conn
            .run(s, &params, |r| {
                out.push(r.blob(0).to_vec());
                true
            })
            .map_err(io)?;
        Ok(out)
    }

    fn count_range(&mut self, index: bool, owner: u64, lo: Bound<&[u8]>, hi: Bound<&[u8]>) -> R<u64> {
        let mut params = vec![P::I(owner as i64)];
        if let Bound::Included(b) | Bound::Excluded(b) = lo {
            params.push(P::B(b));
        }
        if let Bound::Included(b) | Bound::Excluded(b) = hi {
            params.push(P::B(b));
        }
        self.range(index, &lo, &hi, false, true)?;
        let s = &self.ranges[&(index, bk(&lo), bk(&hi), false, true)];
        let mut n = 0u64;
        self.conn
            .run(s, &params, |r| {
                n = r.i64(0) as u64;
                false
            })
            .map_err(io)?;
        Ok(n)
    }

    fn exec_st(&self, which: fn(&Stmts) -> &Stmt, params: &[P]) -> SR<()> {
        self.conn.run(which(&self.st), params, |_| true)
    }
}

impl Drop for SqliteDb {
    fn drop(&mut self) {
        if self.in_tx || self.conn.in_transaction() {
            let _ = self.conn.run(&self.st.rollback, &[], |_| true);
        }
        self.shared.lock().unwrap().open.remove(&self.stem);
        // Then the fields drop: statements first, then the connection, whose
        // close checkpoints the WAL into the main file and removes it.
    }
}

fn prepare_all(c: &Conn) -> SR<Stmts> {
    Ok(Stmts {
        get: c.prepare("SELECT value FROM record WHERE store_id=?1 AND key=?2")?,
        put: c.prepare(
            "INSERT INTO record(store_id,key,value) VALUES(?1,?2,?3) \
             ON CONFLICT(store_id,key) DO UPDATE SET value=excluded.value",
        )?,
        del: c.prepare("DELETE FROM record WHERE store_id=?1 AND key=?2")?,
        clear: c.prepare("DELETE FROM record WHERE store_id=?1")?,
        kg_get: c.prepare("SELECT key_gen FROM store WHERE id=?1")?,
        kg_set: c.prepare("UPDATE store SET key_gen=?2 WHERE id=?1")?,
        ix_add: c.prepare("INSERT OR IGNORE INTO entry(index_id,entry) VALUES(?1,?2)")?,
        ix_del: c.prepare("DELETE FROM entry WHERE index_id=?1 AND entry=?2")?,
        ix_clear: c.prepare("DELETE FROM entry WHERE index_id=?1")?,
        begin: c.prepare("BEGIN IMMEDIATE")?,
        commit: c.prepare("COMMIT")?,
        rollback: c.prepare("ROLLBACK")?,
        sp: c.prepare("SAVEPOINT rq")?,
        sp_release: c.prepare("RELEASE rq")?,
        sp_rollback: c.prepare("ROLLBACK TO rq")?,
    })
}

const SCHEMA: &str = "\
    CREATE TABLE meta (k TEXT PRIMARY KEY, v) WITHOUT ROWID;\
    CREATE TABLE store (id INTEGER PRIMARY KEY, name BLOB NOT NULL, key_path BLOB, \
        auto_increment INTEGER NOT NULL, key_gen REAL NOT NULL DEFAULT 1);\
    CREATE TABLE idx (id INTEGER PRIMARY KEY, store_id INTEGER NOT NULL, name BLOB NOT NULL, \
        key_path BLOB NOT NULL, is_unique INTEGER NOT NULL, multi_entry INTEGER NOT NULL);\
    CREATE TABLE record (store_id INTEGER NOT NULL, key BLOB NOT NULL, value BLOB NOT NULL, \
        PRIMARY KEY (store_id, key));\
    CREATE TABLE entry (index_id INTEGER NOT NULL, entry BLOB NOT NULL, \
        PRIMARY KEY (index_id, entry)) WITHOUT ROWID;";

fn load_meta(c: &Conn) -> SR<DbMeta> {
    let mut m = DbMeta::default();
    let s = c.prepare("SELECT k, v FROM meta")?;
    c.run(&s, &[], |r| {
        match r.blob(0) {
            b"name" => m.name = from_u16le(r.blob(1)),
            b"version" => m.version = r.i64(1) as u64,
            _ => {}
        }
        true
    })?;
    let s = c.prepare("SELECT id, name, key_path, auto_increment FROM store ORDER BY id")?;
    c.run(&s, &[], |r| {
        m.stores.push(StoreMeta {
            id: r.i64(0) as u64,
            name: from_u16le(r.blob(1)),
            key_path: if r.is_null(2) { None } else { dec_key_path(r.blob(2)) },
            auto_increment: r.i64(3) != 0,
            indexes: Vec::new(),
        });
        true
    })?;
    let s = c.prepare("SELECT id, store_id, name, key_path, is_unique, multi_entry FROM idx ORDER BY id")?;
    let mut ixs = Vec::new();
    c.run(&s, &[], |r| {
        ixs.push((
            r.i64(1) as u64,
            IndexMeta {
                id: r.i64(0) as u64,
                name: from_u16le(r.blob(2)),
                key_path: dec_key_path(r.blob(3)).unwrap_or(KeyPath::String(Vec::new())),
                unique: r.i64(4) != 0,
                multi_entry: r.i64(5) != 0,
            },
        ));
        true
    })?;
    for (store, ix) in ixs {
        if let Some(s) = m.stores.iter_mut().find(|s| s.id == store) {
            s.indexes.push(ix);
        }
    }
    Ok(m)
}

/// Opens `path` (keyed if `key`), with the connection settings every use
/// shares, and checks that it is readable. `Kind::Absent`/`Empty` files are
/// created (keyed, if a key is given).
fn connect(path: &Path, key: Option<&[u8; 32]>) -> SR<Conn> {
    let c = Conn::open(path)?;
    if let Some(k) = key {
        c.key(k)?;
    }
    // EXCLUSIVE before WAL: the WAL index then lives in heap memory, no -shm.
    c.exec("PRAGMA locking_mode=EXCLUSIVE")?;
    // The first read: a wrong key (or not a database) fails HERE, NOTADB.
    c.query_i64("SELECT count(*) FROM sqlite_schema")?;
    Ok(c)
}

/// The settings of a connection that serves requests.
fn configure(c: &Conn) -> SR<()> {
    c.exec("PRAGMA journal_mode=WAL")?;
    c.exec("PRAGMA synchronous=NORMAL")?;
    c.exec("PRAGMA foreign_keys=OFF")?;
    c.exec("PRAGMA cache_size=-4096")?;
    Ok(())
}

/// Creates the schema of a new database (inside one transaction), or checks
/// the format of an existing one.
fn ensure_schema(c: &Conn, name: &[u16]) -> SR<()> {
    let v = c.query_i64("PRAGMA user_version")?.unwrap_or(0);
    if v == FORMAT {
        return Ok(());
    }
    if v > FORMAT {
        return Err(SqlErr { code: 1, msg: format!("the database is in IndexedDB format {v}; this build reads format {FORMAT}") });
    }
    c.exec("BEGIN IMMEDIATE")?;
    let r = (|| {
        c.exec(SCHEMA)?;
        c.once("INSERT INTO meta(k,v) VALUES('format',?1)", &[P::I(FORMAT)])?;
        c.once("INSERT INTO meta(k,v) VALUES('name',?1)", &[P::B(&u16le(name))])?;
        c.once("INSERT INTO meta(k,v) VALUES('version',0)", &[])?;
        c.exec(&format!("PRAGMA user_version={FORMAT}"))?;
        c.exec("COMMIT")
    })();
    if r.is_err() {
        let _ = c.exec("ROLLBACK");
    }
    r
}

impl Database for SqliteDb {
    fn meta(&self) -> &DbMeta {
        &self.meta
    }

    fn begin_write(&mut self, durability: Durability) -> R<()> {
        if self.in_tx {
            return Err(BackendError::Io("a write transaction is already open".into()));
        }
        self.strict = durability == Durability::Strict;
        if self.strict {
            self.conn.exec("PRAGMA synchronous=FULL").map_err(io)?;
        }
        self.exec_st(|s| &s.begin, &[]).map_err(io)?;
        self.in_tx = true;
        self.sp_wanted = false;
        self.sp_open = false;
        self.tx_bytes = 0;
        Ok(())
    }

    fn commit_write(&mut self) -> R<()> {
        let r = self.exec_st(|s| &s.commit, &[]);
        let strict = self.strict;
        self.in_tx = false;
        self.sp_wanted = false;
        self.sp_open = false;
        self.strict = false;
        if strict {
            let _ = self.conn.exec("PRAGMA synchronous=NORMAL");
        }
        match r {
            Ok(()) => {
                // Usage is ESTIMATED between measurements (what was written
                // is added; `usage()` and the start re-measure the files):
                // four metadata calls per commit would cost more than the
                // commit itself on a filter-driver-laden NTFS.
                let written = std::mem::take(&mut self.tx_bytes);
                let mut s = self.shared.lock().unwrap();
                *s.sizes.entry(self.stem.clone()).or_insert(0) += written;
                s.open.insert(self.stem.clone(), (self.meta.name.clone(), self.meta.version));
                Ok(())
            }
            Err(e) => {
                let err = self.write_err(e);
                if self.conn.in_transaction() {
                    let _ = self.exec_st(|s| &s.rollback, &[]);
                }
                self.tx_bytes = 0;
                self.reload_meta();
                Err(err)
            }
        }
    }

    fn rollback_write(&mut self) {
        if self.in_tx || self.conn.in_transaction() {
            let _ = self.exec_st(|s| &s.rollback, &[]);
        }
        if self.strict {
            let _ = self.conn.exec("PRAGMA synchronous=NORMAL");
        }
        self.in_tx = false;
        self.strict = false;
        self.sp_wanted = false;
        self.sp_open = false;
        self.tx_bytes = 0;
        self.reload_meta();
    }

    fn savepoint(&mut self) -> usize {
        if self.in_tx {
            self.sp_wanted = true;
        }
        0
    }

    fn rollback_to(&mut self, _sp: usize) {
        if self.sp_wanted || !self.in_tx {
            // Nothing was written since it was asked for.
            self.sp_wanted = false;
            return;
        }
        if self.sp_open {
            let _ = self.exec_st(|s| &s.sp_rollback, &[]);
            if self.sp_schema {
                self.reload_meta();
            }
        }
    }

    fn set_version(&mut self, v: u64) -> R<()> {
        self.schema_write("UPDATE meta SET v=?1 WHERE k='version'", &[P::I(v as i64)])?;
        self.meta.version = v;
        self.publish();
        Ok(())
    }

    fn create_store(&mut self, meta: StoreMeta) -> R<()> {
        let kp = meta.key_path.as_ref().map(enc_key_path);
        self.touch()?;
        self.sp_schema = true;
        let r = match &kp {
            Some(kp) => self.conn.once(
                "INSERT INTO store(id,name,key_path,auto_increment,key_gen) VALUES(?1,?2,?3,?4,1)",
                &[P::I(meta.id as i64), P::B(&u16le(&meta.name)), P::B(kp), P::I(meta.auto_increment as i64)],
            ),
            None => self.conn.once(
                "INSERT INTO store(id,name,key_path,auto_increment,key_gen) VALUES(?1,?2,NULL,?3,1)",
                &[P::I(meta.id as i64), P::B(&u16le(&meta.name)), P::I(meta.auto_increment as i64)],
            ),
        };
        r.map_err(|e| self.write_err(e))?;
        for ix in &meta.indexes {
            self.conn
                .once(
                    "INSERT INTO idx(id,store_id,name,key_path,is_unique,multi_entry) VALUES(?1,?2,?3,?4,?5,?6)",
                    &[
                        P::I(ix.id as i64),
                        P::I(meta.id as i64),
                        P::B(&u16le(&ix.name)),
                        P::B(&enc_key_path(&ix.key_path)),
                        P::I(ix.unique as i64),
                        P::I(ix.multi_entry as i64),
                    ],
                )
                .map_err(|e| self.write_err(e))?;
        }
        self.meta.stores.push(meta);
        Ok(())
    }

    fn delete_store(&mut self, id: StoreId) -> R<()> {
        let i = id as i64;
        self.schema_write("DELETE FROM entry WHERE index_id IN (SELECT id FROM idx WHERE store_id=?1)", &[P::I(i)])?;
        self.schema_write("DELETE FROM idx WHERE store_id=?1", &[P::I(i)])?;
        self.schema_write("DELETE FROM record WHERE store_id=?1", &[P::I(i)])?;
        self.schema_write("DELETE FROM store WHERE id=?1", &[P::I(i)])?;
        self.meta.stores.retain(|s| s.id != id);
        Ok(())
    }

    fn rename_store(&mut self, id: StoreId, name: Vec<u16>) -> R<()> {
        self.schema_write("UPDATE store SET name=?2 WHERE id=?1", &[P::I(id as i64), P::B(&u16le(&name))])?;
        if let Some(s) = self.meta.stores.iter_mut().find(|s| s.id == id) {
            s.name = name;
        }
        Ok(())
    }

    fn create_index(&mut self, store: StoreId, meta: IndexMeta) -> R<()> {
        self.schema_write(
            "INSERT INTO idx(id,store_id,name,key_path,is_unique,multi_entry) VALUES(?1,?2,?3,?4,?5,?6)",
            &[
                P::I(meta.id as i64),
                P::I(store as i64),
                P::B(&u16le(&meta.name)),
                P::B(&enc_key_path(&meta.key_path)),
                P::I(meta.unique as i64),
                P::I(meta.multi_entry as i64),
            ],
        )?;
        if let Some(s) = self.meta.stores.iter_mut().find(|s| s.id == store) {
            s.indexes.push(meta);
        }
        Ok(())
    }

    fn delete_index(&mut self, store: StoreId, id: IndexId) -> R<()> {
        self.schema_write("DELETE FROM entry WHERE index_id=?1", &[P::I(id as i64)])?;
        self.schema_write("DELETE FROM idx WHERE id=?1", &[P::I(id as i64)])?;
        if let Some(s) = self.meta.stores.iter_mut().find(|s| s.id == store) {
            s.indexes.retain(|i| i.id != id);
        }
        Ok(())
    }

    fn rename_index(&mut self, store: StoreId, id: IndexId, name: Vec<u16>) -> R<()> {
        self.schema_write("UPDATE idx SET name=?2 WHERE id=?1", &[P::I(id as i64), P::B(&u16le(&name))])?;
        if let Some(s) = self.meta.stores.iter_mut().find(|s| s.id == store) {
            if let Some(i) = s.indexes.iter_mut().find(|i| i.id == id) {
                i.name = name;
            }
        }
        Ok(())
    }

    fn key_generator(&mut self, store: StoreId) -> R<f64> {
        let mut v = None;
        self.conn
            .run(&self.st.kg_get, &[P::I(store as i64)], |r| {
                v = Some(r.f64(0));
                false
            })
            .map_err(io)?;
        v.ok_or_else(|| BackendError::Io(format!("no object store #{store}")))
    }

    fn set_key_generator(&mut self, store: StoreId, v: f64) -> R<()> {
        self.touch()?;
        self.exec_st(|s| &s.kg_set, &[P::I(store as i64), P::F(v)]).map_err(|e| self.write_err(e))
    }

    fn get(&mut self, store: StoreId, key: &[u8]) -> R<Option<Value>> {
        let mut out = None;
        self.conn
            .run(&self.st.get, &[P::I(store as i64), P::B(key)], |r| {
                out = Some(Arc::new(r.blob(0).to_vec()));
                false
            })
            .map_err(io)?;
        Ok(out)
    }

    fn put(&mut self, store: StoreId, key: &[u8], value: Value) -> R<()> {
        let requested = (key.len() + value.len()) as u64;
        {
            let mut s = self.shared.lock().unwrap();
            let quota = s.quota(false);
            if s.usage().saturating_add(self.tx_bytes).saturating_add(requested) > quota {
                return Err(BackendError::Quota { quota, requested });
            }
        }
        self.touch()?;
        self.exec_st(|s| &s.put, &[P::I(store as i64), P::B(key), P::B(&value)]).map_err(|e| self.write_err(e))?;
        self.tx_bytes += requested;
        Ok(())
    }

    fn delete(&mut self, store: StoreId, key: &[u8]) -> R<()> {
        self.touch()?;
        self.exec_st(|s| &s.del, &[P::I(store as i64), P::B(key)]).map_err(|e| self.write_err(e))
    }

    fn keys(&mut self, store: StoreId, lo: Bound<&[u8]>, hi: Bound<&[u8]>, reverse: bool, limit: usize) -> R<Vec<Vec<u8>>> {
        self.scan(false, store, lo, hi, reverse, limit)
    }

    fn count(&mut self, store: StoreId, lo: Bound<&[u8]>, hi: Bound<&[u8]>) -> R<u64> {
        self.count_range(false, store, lo, hi)
    }

    fn clear(&mut self, store: StoreId) -> R<()> {
        self.touch()?;
        self.exec_st(|s| &s.clear, &[P::I(store as i64)]).map_err(|e| self.write_err(e))
    }

    fn index_add(&mut self, index: IndexId, entry: Vec<u8>) -> R<()> {
        self.touch()?;
        self.exec_st(|s| &s.ix_add, &[P::I(index as i64), P::B(&entry)]).map_err(|e| self.write_err(e))?;
        self.tx_bytes += entry.len() as u64;
        Ok(())
    }

    fn index_remove(&mut self, index: IndexId, entry: &[u8]) -> R<()> {
        self.touch()?;
        self.exec_st(|s| &s.ix_del, &[P::I(index as i64), P::B(entry)]).map_err(|e| self.write_err(e))
    }

    fn index_scan(&mut self, index: IndexId, lo: Bound<&[u8]>, hi: Bound<&[u8]>, reverse: bool, limit: usize) -> R<Vec<Vec<u8>>> {
        self.scan(true, index, lo, hi, reverse, limit)
    }

    fn index_count(&mut self, index: IndexId, lo: Bound<&[u8]>, hi: Bound<&[u8]>) -> R<u64> {
        self.count_range(true, index, lo, hi)
    }

    fn index_clear(&mut self, index: IndexId) -> R<()> {
        self.touch()?;
        self.exec_st(|s| &s.ix_clear, &[P::I(index as i64)]).map_err(|e| self.write_err(e))
    }
}

// ---------------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------------

/// What the backend is told at its start.
pub struct SqliteConfig {
    pub folder: PathBuf,
    pub encryption: IdbEncryption,
    /// Bytes the app may use, given current usage.
    pub quota: QuotaFn,
    /// Called once the start-up migration left every file in the requested
    /// format (the host may then retire an old key).
    pub migrated: Option<Box<dyn FnOnce() + Send>>,
}

pub struct SqliteBackend {
    folder: PathBuf,
    /// The key new and existing files are in, once the start-up migration ran
    /// (`Decrypting` becomes `None` after it).
    key: Option<[u8; 32]>,
    mode: IdbEncryption,
    /// `<folder>\lock`, once it is ours (held open until the process ends).
    lock: Option<File>,
    shared: Arc<Mutex<Shared>>,
    migrated: Option<Box<dyn FnOnce() + Send>>,
    /// A host that could not say where its databases live: every open fails
    /// with this (never a silent memory fallback).
    broken: Option<String>,
}

impl SqliteBackend {
    fn bare(cfg: SqliteConfig) -> SqliteBackend {
        let shared = Arc::new(Mutex::new(Shared {
            open: HashMap::new(),
            sizes: HashMap::new(),
            quota: super::DEFAULT_QUOTA,
            quota_at: Instant::now() - Duration::from_secs(3600),
            quota_fn: cfg.quota,
        }));
        let key = match &cfg.encryption {
            IdbEncryption::Encrypted(k) => Some(*k),
            _ => None,
        };
        SqliteBackend {
            folder: cfg.folder,
            key,
            mode: cfg.encryption,
            lock: None,
            shared,
            migrated: cfg.migrated,
            broken: None,
        }
    }

    /// Creates the folder, takes its lock and runs the start-up migration
    /// (or remembers who holds the lock, and tries again at the next open).
    pub fn new(cfg: SqliteConfig) -> SqliteBackend {
        let mut b = SqliteBackend::bare(cfg);
        if let Err(e) = std::fs::create_dir_all(&b.folder) {
            b.broken = Some(format!("cannot create {}: {e}", b.folder.display()));
            return b;
        }
        let _ = b.ensure_lock();
        b
    }

    /// A backend that refuses everything with `why` — what a host whose folder
    /// or key could not be read gets, instead of a silent memory fallback.
    pub fn broken(folder: PathBuf, why: String) -> SqliteBackend {
        let mut b = SqliteBackend::bare(SqliteConfig { folder, encryption: IdbEncryption::Plaintext, quota: Arc::new(|_| 0), migrated: None });
        b.broken = Some(why);
        b
    }

    pub fn folder(&self) -> &Path {
        &self.folder
    }

    pub fn encrypted(&self) -> bool {
        self.key.is_some()
    }

    fn path_of(&self, name: &[u16]) -> PathBuf {
        self.folder.join(format!("{}.sqlite3", file_stem(name)))
    }

    /// The folder lock, taken now if it was not; the start-up work runs once
    /// it is.
    fn ensure_lock(&mut self) -> R<()> {
        if let Some(b) = &self.broken {
            return Err(BackendError::Io(b.clone()));
        }
        if self.lock.is_some() {
            return Ok(());
        }
        let f = take_lock(&self.folder).map_err(BackendError::Locked)?;
        self.lock = Some(f);
        self.start();
        Ok(())
    }

    /// Every database file, as `(path, stem)`.
    fn files(&self) -> Vec<(PathBuf, String)> {
        let Ok(rd) = std::fs::read_dir(&self.folder) else { return Vec::new() };
        let mut v: Vec<(PathBuf, String)> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                let stem = n.strip_suffix(".sqlite3")?;
                (stem.len() == 32 && stem.bytes().all(|b| b.is_ascii_hexdigit())).then(|| (e.path(), stem.to_string()))
            })
            .collect();
        v.sort();
        v
    }

    /// Start-up (the lock is ours): every file brought to the requested
    /// format, usage measured.
    fn start(&mut self) {
        let mut clean = true;
        let old_key = match &self.mode {
            IdbEncryption::Decrypting { old_key } => Some(*old_key),
            _ => None,
        };
        for (path, _) in self.files() {
            match (classify(&path), &self.mode) {
                (Kind::Plain, IdbEncryption::Encrypted(k)) => {
                    if let Err(e) = rekey(&path, None, Some(k)) {
                        clean = false;
                        super::advise(&format!("rekey:{}", path.display()), format!("IndexedDB: could not encrypt {}: {e}", path.display()));
                    }
                }
                (Kind::Encrypted, IdbEncryption::Encrypted(k)) => {
                    if let Err(e) = connect(&path, Some(k)) {
                        if e.primary() == NOTADB || e.primary() == CORRUPT {
                            move_aside(&path, "it is encrypted under a key this run does not have");
                        } else {
                            clean = false;
                        }
                    }
                }
                (Kind::Encrypted, IdbEncryption::Decrypting { old_key }) => {
                    let k = *old_key;
                    match rekey(&path, Some(&k), None) {
                        Ok(()) => {}
                        Err(e) if e.primary() == NOTADB || e.primary() == CORRUPT => {
                            move_aside(&path, "it is encrypted under a key this run does not have");
                        }
                        Err(e) => {
                            clean = false;
                            super::advise(&format!("rekey:{}", path.display()), format!("IndexedDB: could not decrypt {}: {e}", path.display()));
                        }
                    }
                }
                (Kind::Encrypted, IdbEncryption::Plaintext) => {
                    move_aside(&path, "it is encrypted and this run has no key (storage is not encrypted)");
                }
                _ => {}
            }
        }
        if old_key.is_some() && clean {
            self.mode = IdbEncryption::Plaintext;
        }
        if clean {
            if let Some(f) = self.migrated.take() {
                f();
            }
        }
        let mut sizes = HashMap::new();
        for (path, stem) in self.files() {
            sizes.insert(stem, file_bytes(&path));
        }
        let mut s = self.shared.lock().unwrap();
        s.sizes = sizes;
        s.quota(true);
    }

    /// The key this file opens with, deciding (and doing) the per-file
    /// migration the start-up could not.
    fn key_for(&self, path: &Path) -> Option<[u8; 32]> {
        match (classify(path), &self.mode) {
            (Kind::Encrypted, IdbEncryption::Decrypting { old_key }) => Some(*old_key),
            (Kind::Plain, _) => None,
            _ => self.key,
        }
    }

    fn open_db(&mut self, name: &[u16]) -> SR<SqliteDb> {
        let path = self.path_of(name);
        let stem = file_stem(name);
        if let (IdbEncryption::Encrypted(k), Kind::Plain) = (&self.mode, classify(&path)) {
            // The start-up could not encrypt it: try again, and REFUSE rather
            // than serve a plaintext file in an encrypted run.
            let k = *k;
            rekey(&path, None, Some(&k))?;
        }
        let mut tried_aside = false;
        let conn = loop {
            let key = self.key_for(&path);
            match connect(&path, key.as_ref()) {
                Ok(c) => break c,
                Err(e) if !tried_aside && (e.primary() == NOTADB || e.primary() == CORRUPT) => {
                    move_aside(&path, &e.to_string());
                    tried_aside = true;
                }
                Err(e) => return Err(e),
            }
        };
        configure(&conn)?;
        ensure_schema(&conn, name)?;
        let meta = load_meta(&conn)?;
        let st = prepare_all(&conn)?;
        let db = SqliteDb {
            st,
            ranges: HashMap::new(),
            conn,
            meta,
            stem: stem.clone(),
            shared: self.shared.clone(),
            in_tx: false,
            strict: false,
            sp_wanted: false,
            sp_open: false,
            sp_schema: false,
            tx_bytes: 0,
        };
        db.publish();
        self.shared.lock().unwrap().sizes.insert(stem, file_bytes(&path));
        Ok(db)
    }

    /// `(name, version)` from a database that is not open, briefly opened.
    fn peek(&self, path: &Path) -> Option<(Vec<u16>, u64)> {
        let key = self.key_for(path);
        let c = connect(path, key.as_ref()).ok()?;
        if c.query_i64("PRAGMA user_version").ok()?? != FORMAT {
            return None;
        }
        let m = load_meta(&c).ok()?;
        Some((m.name, m.version))
    }
}

/// Re-encrypts `path` from `from` (None = plaintext) to `to` (None =
/// plaintext). sqlite3mc rewrites every page; plaintext → encrypted is
/// refused in WAL mode, so the file is switched to a rollback journal around
/// it (which also folds any WAL into the main file first).
fn rekey(path: &Path, from: Option<&[u8; 32]>, to: Option<&[u8; 32]>) -> SR<()> {
    let c = connect(path, from)?;
    c.exec("PRAGMA journal_mode=DELETE")?;
    c.rekey(to)?;
    c.exec("PRAGMA journal_mode=WAL")?;
    Ok(())
}

impl Backend for SqliteBackend {
    fn describe(&self) -> String {
        format!("{}{}", self.folder.display(), if self.key.is_some() { " (encrypted)" } else { "" })
    }

    fn persistent(&self) -> bool {
        true
    }

    fn databases(&mut self) -> R<Vec<(Vec<u16>, u64)>> {
        self.ensure_lock()?;
        let open = self.shared.lock().unwrap().open.clone();
        let mut out = Vec::new();
        for (path, stem) in self.files() {
            if let Some((n, v)) = open.get(&stem) {
                out.push((n.clone(), *v));
            } else if let Some(nv) = self.peek(&path) {
                out.push(nv);
            }
        }
        out.sort();
        Ok(out)
    }

    fn open(&mut self, name: &[u16]) -> R<Box<dyn Database>> {
        self.ensure_lock()?;
        if self.shared.lock().unwrap().open.contains_key(&file_stem(name)) {
            return Err(BackendError::Io("this database is already open in this process".into()));
        }
        match self.open_db(name) {
            Ok(db) => Ok(Box::new(db)),
            Err(e) => Err(io(e)),
        }
    }

    fn delete(&mut self, name: &[u16]) -> R<u64> {
        self.ensure_lock()?;
        let path = self.path_of(name);
        if classify(&path) == Kind::Absent {
            return Ok(0);
        }
        let version = self.peek(&path).map(|(_, v)| v).unwrap_or(0);
        let mut err = None;
        for p in std::iter::once(path.clone()).chain(sidecars(&path)) {
            if let Err(e) = std::fs::remove_file(&p) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    err = Some(format!("could not delete {}: {e}", p.display()));
                }
            }
        }
        self.shared.lock().unwrap().sizes.remove(&file_stem(name));
        match err {
            Some(e) => Err(BackendError::Io(e)),
            None => Ok(version),
        }
    }

    fn usage(&mut self) -> u64 {
        // Measured, not tracked: a closed database's WAL has been folded away.
        let sizes: HashMap<String, u64> = self.files().into_iter().map(|(p, s)| (s, file_bytes(&p))).collect();
        let mut s = self.shared.lock().unwrap();
        s.sizes = sizes;
        s.usage()
    }

    fn quota(&mut self) -> u64 {
        self.shared.lock().unwrap().quota(true)
    }
}

/// The backend for an installed host: its folder and key, read now (on the
/// server thread, at the first IndexedDB use). A host that cannot answer gets
/// a backend that refuses every open with the reason.
pub fn from_host(h: &'static dyn super::IndexedDbHost) -> Box<dyn Backend> {
    let folder = match h.folder() {
        Ok(f) => f,
        Err(e) => {
            super::advise("host-folder", format!("IndexedDB: the storage host has no folder ({e}); every open() fails with UnknownError."));
            return Box::new(SqliteBackend::broken(PathBuf::new(), format!("no IndexedDB folder: {e}")));
        }
    };
    let encryption = match h.encryption() {
        Ok(e) => e,
        Err(e) => {
            super::advise(
                "host-key",
                format!("IndexedDB: the storage host could not provide its key ({e}); every open() fails with UnknownError."),
            );
            return Box::new(SqliteBackend::broken(folder, format!("no IndexedDB key: {e}")));
        }
    };
    Box::new(SqliteBackend::new(SqliteConfig {
        folder,
        encryption,
        quota: Arc::new(move |usage| h.quota_bytes(usage)),
        migrated: Some(Box::new(move || h.migrated())),
    }))
}

/// `PRAGMA integrity_check` over the database `name` in `folder` (opened
/// with `key`): `Ok("ok")` for a sound file. For diagnostics and the
/// crash-safety test; never called on a database the server has open.
pub fn integrity_check(folder: &Path, name: &[u16], key: Option<&[u8; 32]>) -> Result<String, String> {
    let path = folder.join(format!("{}.sqlite3", file_stem(name)));
    let c = connect(&path, key).map_err(|e| e.to_string())?;
    let s = c.prepare("PRAGMA integrity_check").map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    c.run(&s, &[], |r| {
        out.push(String::from_utf8_lossy(r.blob(0)).into_owned());
        true
    })
    .map_err(|e| e.to_string())?;
    Ok(out.join("; "))
}

/// A factory for [`super::set_backend_factory`]: SQLite in `folder` (tests,
/// conformance runs, embedders that configure through the C ABI).
pub fn factory(folder: PathBuf, encryption: IdbEncryption, quota: u64) -> super::BackendFactory {
    Box::new(move || Box::new(SqliteBackend::new(SqliteConfig { folder, encryption, quota: Arc::new(move |_| quota), migrated: None })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn dir() -> Dir {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!("redjs-idb-sqlite-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&p);
        Dir(p)
    }
    fn backend(d: &Dir, enc: IdbEncryption, quota: u64) -> SqliteBackend {
        SqliteBackend::new(SqliteConfig { folder: d.0.clone(), encryption: enc, quota: Arc::new(move |_| quota), migrated: None })
    }
    fn st(id: u64, name: &str) -> StoreMeta {
        StoreMeta { id, name: name.encode_utf16().collect(), key_path: Some(KeyPath::List(vec![vec![b'a' as u16], vec![]])), auto_increment: true, indexes: vec![] }
    }
    fn n(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }
    fn bytes_of(d: &Dir) -> Vec<u8> {
        let mut all = Vec::new();
        for e in std::fs::read_dir(&d.0).unwrap().flatten() {
            all.extend(std::fs::read(e.path()).unwrap_or_default());
        }
        all
    }
    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn rollback_savepoints_and_a_restart() {
        let d = dir();
        {
            let mut b = backend(&d, IdbEncryption::Plaintext, 1 << 30);
            let mut db = b.open(&n("x")).unwrap();
            assert_eq!(db.meta().version, 0);
            db.begin_write(Durability::Default).unwrap();
            db.set_version(1).unwrap();
            db.create_store(st(1, "s")).unwrap();
            db.put(1, b"k1", Arc::new(vec![1, 2, 3])).unwrap();
            db.commit_write().unwrap();

            db.begin_write(Durability::Strict).unwrap();
            db.put(1, b"k1", Arc::new(vec![9])).unwrap();
            let sp = db.savepoint();
            db.put(1, b"k2", Arc::new(vec![9])).unwrap();
            db.set_key_generator(1, 7.0).unwrap();
            db.rollback_to(sp);
            assert_eq!(db.get(1, b"k2").unwrap(), None, "the savepoint undid only its own request");
            assert_eq!(db.get(1, b"k1").unwrap().as_deref(), Some(&vec![9]));
            assert_eq!(db.key_generator(1).unwrap(), 1.0);
            let sp = db.savepoint();
            db.create_index(1, IndexMeta { id: 2, name: n("i"), key_path: KeyPath::String(n("a.b")), unique: true, multi_entry: false }).unwrap();
            db.rollback_to(sp);
            assert!(db.meta().stores[0].indexes.is_empty(), "schema reverts with its savepoint");
            db.clear(1).unwrap();
            db.set_version(2).unwrap();
            db.rollback_write();
            assert_eq!(db.meta().version, 1);
            assert_eq!(db.get(1, b"k1").unwrap().as_deref(), Some(&vec![1, 2, 3]));
            assert_eq!(db.key_generator(1).unwrap(), 1.0);

            db.begin_write(Durability::Default).unwrap();
            db.create_index(1, IndexMeta { id: 2, name: n("i"), key_path: KeyPath::String(n("a.b")), unique: true, multi_entry: true }).unwrap();
            db.index_add(2, b"ab".to_vec()).unwrap();
            db.set_key_generator(1, f64::INFINITY).unwrap();
            db.commit_write().unwrap();
        }
        // A new backend on the same folder: everything committed is back.
        let mut b = backend(&d, IdbEncryption::Plaintext, 1 << 30);
        assert_eq!(b.databases().unwrap(), vec![(n("x"), 1)]);
        let mut db = b.open(&n("x")).unwrap();
        let m = db.meta().clone();
        assert_eq!(m.name, n("x"));
        assert_eq!(m.version, 1);
        assert_eq!(m.stores[0].key_path, Some(KeyPath::List(vec![vec![b'a' as u16], vec![]])));
        assert!(m.stores[0].auto_increment);
        assert_eq!(m.stores[0].indexes[0].key_path, KeyPath::String(n("a.b")));
        assert!(m.stores[0].indexes[0].unique && m.stores[0].indexes[0].multi_entry);
        assert_eq!(db.get(1, b"k1").unwrap().as_deref(), Some(&vec![1, 2, 3]));
        assert_eq!(db.key_generator(1).unwrap(), f64::INFINITY);
        assert_eq!(db.index_scan(2, Bound::Unbounded, Bound::Unbounded, false, 10).unwrap(), vec![b"ab".to_vec()]);
        drop(db);
        assert!(!sidecars(&b.path_of(&n("x")))[0].exists(), "closing checkpoints the WAL away");
        assert_eq!(b.delete(&n("x")).unwrap(), 1);
        assert!(b.databases().unwrap().is_empty());
    }

    #[test]
    fn ranges_counts_order_and_quota() {
        let d = dir();
        let mut b = backend(&d, IdbEncryption::Plaintext, 1 << 30);
        let mut db = b.open(&n("q")).unwrap();
        db.begin_write(Durability::Default).unwrap();
        db.create_store(st(1, "s")).unwrap();
        for k in [&b"a"[..], b"b", b"c", b"d", b"", b"\xff", b"b\x00"] {
            db.put(1, k, Arc::new(vec![0])).unwrap();
        }
        let all = db.keys(1, Bound::Unbounded, Bound::Unbounded, false, usize::MAX).unwrap();
        assert_eq!(all, vec![b"".to_vec(), b"a".to_vec(), b"b".to_vec(), b"b\x00".to_vec(), b"c".to_vec(), b"d".to_vec(), b"\xff".to_vec()], "memcmp order, prefixes first");
        assert_eq!(db.keys(1, Bound::Excluded(b"a"), Bound::Included(b"c"), false, 10).unwrap(), vec![b"b".to_vec(), b"b\x00".to_vec(), b"c".to_vec()]);
        assert_eq!(db.keys(1, Bound::Unbounded, Bound::Excluded(b"\xff"), true, 1).unwrap(), vec![b"d".to_vec()]);
        assert_eq!(db.count(1, Bound::Included(b"b"), Bound::Unbounded).unwrap(), 5);
        assert!(db.keys(1, Bound::Included(b"z"), Bound::Included(b"a"), false, 10).unwrap().is_empty());
        assert_eq!(db.get(1, b"").unwrap().as_deref(), Some(&vec![0]), "an empty key is a key, not NULL");
        db.delete(1, b"a").unwrap();
        assert_eq!(db.count(1, Bound::Unbounded, Bound::Unbounded).unwrap(), 6);
        db.commit_write().unwrap();
        drop(db);
        drop(b);

        let d2 = dir();
        let mut b = backend(&d2, IdbEncryption::Plaintext, 64 << 10);
        let mut db = b.open(&n("small")).unwrap();
        db.begin_write(Durability::Default).unwrap();
        db.create_store(st(1, "s")).unwrap();
        match db.put(1, b"big", Arc::new(vec![7; 256 << 10])) {
            Err(BackendError::Quota { quota, .. }) => assert_eq!(quota, 64 << 10),
            other => panic!("{other:?}"),
        }
        db.commit_write().unwrap();
    }

    #[test]
    fn encrypted_at_rest_and_the_key_matters() {
        let d = dir();
        let key = [0x5a; 32];
        let secret = b"vn-secret-value-0123456789";
        {
            let mut b = backend(&d, IdbEncryption::Encrypted(key), 1 << 30);
            assert!(b.encrypted());
            let mut db = b.open(&n("vn-secret-db")).unwrap();
            db.begin_write(Durability::Default).unwrap();
            db.set_version(3).unwrap();
            db.create_store(st(1, "vn-secret-store")).unwrap();
            db.put(1, b"vn-secret-key-abcdef", Arc::new(secret.to_vec())).unwrap();
            db.commit_write().unwrap();
            // While open: the WAL holds the commit — it must be ciphertext too.
            let raw = bytes_of(&d);
            assert!(!contains(&raw, secret) && !contains(&raw, b"vn-secret-key-abcdef"), "plaintext in the WAL");
            assert!(!contains(&raw, b"SQLite format 3"));
        }
        let raw = bytes_of(&d);
        assert!(!contains(&raw, secret) && !contains(&raw, b"vn-secret-key") && !contains(&raw, &u16le(&n("vn-secret-store"))));
        assert!(!contains(&raw, b"SQLite format 3"), "not a plaintext SQLite file");
        // The same key reads it back.
        {
            let mut b = backend(&d, IdbEncryption::Encrypted(key), 1 << 30);
            assert_eq!(b.databases().unwrap(), vec![(n("vn-secret-db"), 3)]);
            let mut db = b.open(&n("vn-secret-db")).unwrap();
            assert_eq!(db.get(1, b"vn-secret-key-abcdef").unwrap().as_deref(), Some(&secret.to_vec()));
        }
        // Another key cannot, and the file is moved aside — never deleted.
        {
            let mut b = backend(&d, IdbEncryption::Encrypted([1; 32]), 1 << 30);
            assert!(b.databases().unwrap().is_empty());
            let db = b.open(&n("vn-secret-db")).unwrap();
            assert_eq!(db.meta().version, 0, "a fresh database in its place");
        }
        let names: Vec<String> = std::fs::read_dir(&d.0).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(names.iter().any(|n| n.contains(".unreadable-")), "{names:?}");
    }

    #[test]
    fn plaintext_files_are_encrypted_and_decrypted_at_start() {
        let d = dir();
        let key = [7u8; 32];
        {
            let mut b = backend(&d, IdbEncryption::Plaintext, 1 << 30);
            let mut db = b.open(&n("m")).unwrap();
            db.begin_write(Durability::Default).unwrap();
            db.set_version(1).unwrap();
            db.create_store(st(1, "s")).unwrap();
            db.put(1, b"k", Arc::new(b"plain-then-sealed".to_vec())).unwrap();
            db.commit_write().unwrap();
        }
        assert!(contains(&bytes_of(&d), b"plain-then-sealed"));
        let migrated = Arc::new(AtomicUsize::new(0));
        let m2 = migrated.clone();
        {
            let mut b = SqliteBackend::new(SqliteConfig {
                folder: d.0.clone(),
                encryption: IdbEncryption::Encrypted(key),
                quota: Arc::new(|_| 1 << 30),
                migrated: Some(Box::new(move || {
                    m2.fetch_add(1, Ordering::Relaxed);
                })),
            });
            assert_eq!(migrated.load(Ordering::Relaxed), 1);
            assert!(!contains(&bytes_of(&d), b"plain-then-sealed"), "encrypted at start, before any open");
            let mut db = b.open(&n("m")).unwrap();
            assert_eq!(db.get(1, b"k").unwrap().as_deref(), Some(&b"plain-then-sealed".to_vec()));
        }
        {
            let mut b = backend(&d, IdbEncryption::Decrypting { old_key: key }, 1 << 30);
            assert!(!b.encrypted());
            assert!(contains(&bytes_of(&d), b"plain-then-sealed"), "decrypted at start");
            let mut db = b.open(&n("m")).unwrap();
            assert_eq!(db.get(1, b"k").unwrap().as_deref(), Some(&b"plain-then-sealed".to_vec()));
        }
        // An encrypted file and no key at all: moved aside.
        {
            let mut b = backend(&d, IdbEncryption::Encrypted(key), 1 << 30);
            drop(b.open(&n("m")).unwrap());
        }
        let b = backend(&d, IdbEncryption::Plaintext, 1 << 30);
        drop(b);
        let names: Vec<String> = std::fs::read_dir(&d.0).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(names.iter().any(|n| n.contains(".unreadable-")), "{names:?}");
    }

    #[test]
    fn a_second_holder_of_the_folder_is_locked_out_until_the_first_goes() {
        let d = dir();
        let mut a = backend(&d, IdbEncryption::Plaintext, 1 << 30);
        drop(a.open(&n("x")).unwrap());
        let mut b = backend(&d, IdbEncryption::Plaintext, 1 << 30);
        match b.open(&n("x")) {
            Err(BackendError::Locked(holder)) => assert!(holder.contains(&format!("pid {}", std::process::id())), "{holder}"),
            other => panic!("{:?}", other.map(|_| ())),
        }
        assert!(matches!(b.databases(), Err(BackendError::Locked(_))));
        assert!(matches!(b.delete(&n("x")), Err(BackendError::Locked(_))));
        drop(a);
        assert!(b.open(&n("x")).is_ok(), "the next open after the holder is gone takes the lock");
    }

    #[test]
    fn point_reads_cost_microseconds() {
        let d = dir();
        let mut b = backend(&d, IdbEncryption::Plaintext, 1 << 30);
        let mut db = b.open(&n("speed")).unwrap();
        db.begin_write(Durability::Default).unwrap();
        db.create_store(st(1, "s")).unwrap();
        for i in 0..1000u32 {
            db.put(1, &i.to_be_bytes(), Arc::new(vec![1; 100])).unwrap();
        }
        db.commit_write().unwrap();
        let t = Instant::now();
        for i in 0..1000u32 {
            assert!(db.get(1, &i.to_be_bytes()).unwrap().is_some());
        }
        let outside = t.elapsed().as_secs_f64() * 1e6 / 1000.0;
        db.begin_write(Durability::Default).unwrap();
        let t = Instant::now();
        for i in 0..1000u32 {
            assert!(db.get(1, &i.to_be_bytes()).unwrap().is_some());
        }
        let inside = t.elapsed().as_secs_f64() * 1e6 / 1000.0;
        let t = Instant::now();
        for i in 0..200u32 {
            db.put(1, &i.to_be_bytes(), Arc::new(vec![2; 100])).unwrap();
        }
        let put = t.elapsed().as_secs_f64() * 1e6 / 200.0;
        db.commit_write().unwrap();
        let t = Instant::now();
        for i in 0..50u32 {
            db.begin_write(Durability::Default).unwrap();
            db.put(1, &i.to_be_bytes(), Arc::new(vec![3; 100])).unwrap();
            db.commit_write().unwrap();
        }
        let commit = t.elapsed().as_secs_f64() * 1e6 / 50.0;
        let t = Instant::now();
        for i in 0..20u32 {
            db.begin_write(Durability::Strict).unwrap();
            db.put(1, &i.to_be_bytes(), Arc::new(vec![4; 100])).unwrap();
            db.commit_write().unwrap();
        }
        let strict = t.elapsed().as_secs_f64() * 1e6 / 20.0;
        eprintln!(
            "[sqlite] get {outside:.1} µs (autocommit), {inside:.1} µs (in a write tx); put {put:.1} µs; \
             relaxed commit {commit:.0} µs; strict commit {strict:.0} µs"
        );
    }

    #[test]
    fn names_hash_to_stable_file_names() {
        assert_eq!(file_stem(&[]).len(), 32);
        assert_ne!(file_stem(&n("a")), file_stem(&n("A")));
        assert_eq!(file_stem(&[0xD800]), file_stem(&[0xD800]), "lone surrogates are names too");
        assert_eq!(dec_key_path(&enc_key_path(&KeyPath::List(vec![vec![], n("x.y")]))), Some(KeyPath::List(vec![vec![], n("x.y")])));
        assert_eq!(dec_key_path(&enc_key_path(&KeyPath::String(vec![]))), Some(KeyPath::String(vec![])));
        assert!(utc_stamp().ends_with('Z') && utc_stamp().len() == 16);
        assert!(version().contains("3.54.0") && version().contains("2.6.1"), "{}", version());
    }
}
