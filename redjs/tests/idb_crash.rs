//! Crash safety of the SQLite backend (m25 P2): a process KILLED in the middle
//! of a transaction leaves the database at its last committed state — every
//! transaction whose `complete` fired is there, whole; the one in flight is
//! not there at all; the file passes `PRAGMA integrity_check`.
//!
//! The child is this same binary (`REDJS_IDB_CRASH_CHILD=<folder>`): a real
//! context on the SQLite backend, pumped like vue-native's frame, writing
//! transactions of 3000 records × ~2 KB (6 MB, more than the 4 MiB page
//! cache, so SQLite spills uncommitted pages into the WAL before COMMIT —
//! the case where "nothing partial" is actually at stake). It prints `begin k`
//! when transaction k starts and `committed k` once its `complete` fired.
//! The parent kills it (TerminateProcess) a random few ms after a `begin`,
//! then opens the folder itself and checks.

use std::io::{BufRead, BufReader, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use redjs::api::{Context, Error};
use redjs::ffi;
use redjs::webapi::indexeddb::backend::Backend;
use redjs::webapi::indexeddb::key::{self, Key};
use redjs::webapi::indexeddb::{set_backend_factory, sqlite, IdbEncryption};

const PER_TX: u32 = 3000;
const KEY: [u8; 32] = [0x3c; 32];

fn enc(encrypted: bool) -> IdbEncryption {
    if encrypted {
        IdbEncryption::Encrypted(KEY)
    } else {
        IdbEncryption::Plaintext
    }
}

fn child(dir: &Path, encrypted: bool) -> ! {
    set_backend_factory(sqlite::factory(dir.to_path_buf(), enc(encrypted), 1 << 40));
    let c = Context::new();
    unsafe {
        ffi::redjs_web_install(c.raw());
        ffi::redjs_webshim_install(c.raw(), core::ptr::null(), 0, core::ptr::null(), 0);
    }
    let src = format!(
        "globalThis.__begun = -1; globalThis.__committed = -1; globalThis.__err = '';
         (async () => {{
           const open = indexedDB.open('crash', 1);
           open.onupgradeneeded = () => {{ open.result.createObjectStore('r'); open.result.createObjectStore('done'); }};
           const db = await new Promise((res, rej) => {{ open.onsuccess = () => res(open.result); open.onerror = () => rej(open.error); }});
           const payload = 'p'.repeat(2000);
           for (let k = 0; ; k++) {{
             const tx = db.transaction(['r', 'done'], 'readwrite');
             const r = tx.objectStore('r');
             for (let i = 0; i < {PER_TX}; i++) r.put({{ k, i, payload }}, k + ':' + i);
             tx.objectStore('done').put(k, k);
             globalThis.__begun = k;
             await new Promise((res, rej) => {{ tx.oncomplete = res; tx.onabort = () => rej(tx.error); }});
             globalThis.__committed = k;
           }}
         }})().catch(e => {{ globalThis.__err = String(e && e.name) + ': ' + String(e && e.message); }});
         'ok'"
    );
    match c.eval_named(&src, "crash-child.js") {
        Ok(_) => {}
        Err(Error::Exception(e)) => panic!("child script threw: {}", c.error_info(e).0),
        Err(e) => panic!("{e}"),
    }
    let read = |what: &str| -> String {
        match c.eval_named(what, "poll.js") {
            Ok(v) => c.to_rust_string(v).unwrap_or_default(),
            Err(_) => String::new(),
        }
    };
    let (mut begun, mut committed) = (String::from("-1"), String::from("-1"));
    let mut out = std::io::stdout();
    unsafe { ffi::redjs_lock(c.raw()) };
    loop {
        unsafe { ffi::redjs_pump(c.raw(), 1 | 2 | 4, core::ptr::null_mut()) };
        let b = read("String(globalThis.__begun)");
        let k = read("String(globalThis.__committed)");
        let e = read("globalThis.__err");
        if k != committed {
            committed = k;
            let _ = writeln!(out, "committed {committed}");
        }
        if b != begun {
            begun = b;
            let _ = writeln!(out, "begin {begun}");
        }
        if !e.is_empty() {
            let _ = writeln!(out, "error {e}");
        }
        let _ = out.flush();
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What the parent finds after the kill: (transactions present, records).
fn inspect(dir: &Path, encrypted: bool) -> (u64, u64) {
    let ok = sqlite::integrity_check(dir, &units("crash"), encrypted.then_some(&KEY)).expect("integrity_check runs");
    assert_eq!(ok, "ok", "PRAGMA integrity_check after the kill");
    let mut b = sqlite::SqliteBackend::new(sqlite::SqliteConfig {
        folder: dir.to_path_buf(),
        encryption: enc(encrypted),
        quota: std::sync::Arc::new(|_| 1 << 40),
        migrated: None,
    });
    let mut db = b.open(&units("crash")).expect("the database opens after the kill");
    let meta = db.meta().clone();
    let id = |n: &str| meta.stores.iter().find(|s| s.name == units(n)).unwrap_or_else(|| panic!("store {n}")).id;
    let (r, done) = (id("r"), id("done"));
    let n_done = db.count(done, Bound::Unbounded, Bound::Unbounded).unwrap();
    let keys = db.keys(done, Bound::Unbounded, Bound::Unbounded, false, usize::MAX).unwrap();
    for (i, k) in keys.iter().enumerate() {
        assert_eq!(key::decode(k), Some(Key::Number(i as f64)), "transactions 0..n, contiguous");
    }
    let n_rec = db.count(r, Bound::Unbounded, Bound::Unbounded).unwrap();
    // Every present transaction is whole.
    for k in 0..n_done {
        let lo = Key::String(units(&format!("{k}:"))).encode();
        let hi = Key::String(units(&format!("{k};"))).encode();
        let n = db.count(r, Bound::Included(&lo), Bound::Excluded(&hi)).unwrap();
        assert_eq!(n, PER_TX as u64, "transaction {k} has {n} of its {PER_TX} records");
    }
    (n_done, n_rec)
}

#[test]
fn a_kill_mid_transaction_leaves_the_committed_state() {
    if let Ok(dir) = std::env::var("REDJS_IDB_CRASH_CHILD") {
        child(Path::new(&dir), std::env::var("REDJS_IDB_CRASH_ENCRYPTED").is_ok());
    }
    let exe = std::env::current_exe().unwrap();
    // Deterministic "random" kill delays, spread over a transaction's write.
    let delays_ms = [3u64, 11, 27, 6, 41, 17];
    for (round, delay) in delays_ms.iter().enumerate() {
        let encrypted = round % 2 == 1;
        let dir = Dir(std::env::temp_dir().join(format!("redjs-idb-crash-{}-{round}", std::process::id())));
        let _ = std::fs::remove_dir_all(&dir.0);
        let mut child = Command::new(&exe)
            .env("REDJS_IDB_CRASH_CHILD", &dir.0)
            .envs(encrypted.then_some(("REDJS_IDB_CRASH_ENCRYPTED", "1")))
            .args(["--exact", "a_kill_mid_transaction_leaves_the_committed_state", "--nocapture"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let (tx_lines, rx_lines) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            for l in lines.map_while(Result::ok) {
                if tx_lines.send(l).is_err() {
                    break;
                }
            }
        });
        // Let a couple of transactions commit (round + 1 of them), wait for the
        // next one to BEGIN, then kill it `delay` ms into its writes.
        let target = round as i64 + 1;
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut last_committed = -1i64;
        let mut killed_in: Option<i64> = None;
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            let Ok(l) = rx_lines.recv_timeout(Duration::from_millis(200)) else { continue };
            seen.push(l.clone());
            if let Some(e) = l.strip_prefix("error ") {
                let _ = child.kill();
                panic!("the child failed: {e}");
            }
            if let Some(k) = l.strip_prefix("committed ") {
                last_committed = k.parse().unwrap();
            }
            if let Some(k) = l.strip_prefix("begin ") {
                let k: i64 = k.parse().unwrap();
                if k > target {
                    std::thread::sleep(Duration::from_millis(*delay));
                    child.kill().unwrap();
                    killed_in = Some(k);
                    break;
                }
            }
        }
        let _ = child.wait();
        while let Ok(l) = rx_lines.try_recv() {
            if let Some(k) = l.strip_prefix("committed ") {
                last_committed = last_committed.max(k.parse().unwrap());
            }
        }
        let killed_in = killed_in.unwrap_or_else(|| panic!("round {round}: the child never got going: {seen:?}"));
        let wal = std::fs::read_dir(&dir.0)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with("-wal"))
            .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
            .sum::<u64>();
        let (n_done, n_rec) = inspect(&dir.0, encrypted);
        eprintln!(
            "[crash] round {round} ({}): killed {delay} ms into transaction {killed_in}; last `complete` seen: {last_committed}; \
             WAL at the kill {:.1} MB; on reopen: {n_done} transactions, {n_rec} records, integrity ok",
            if encrypted { "encrypted" } else { "plaintext" },
            wal as f64 / 1048576.0
        );
        assert_eq!(n_rec, n_done * PER_TX as u64, "only whole transactions");
        // Everything whose `complete` fired is there (relaxed durability
        // survives a process kill: the WAL write reached the OS) ...
        assert!(n_done as i64 >= last_committed + 1, "committed transactions lost: {n_done} present, {} completed", last_committed + 1);
        // ... and nothing past the transaction the kill interrupted.
        assert!(n_done as i64 <= killed_in + 1, "{n_done} transactions present, the kill was in {killed_in}");
    }
}
