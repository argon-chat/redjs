//! The compatibility proof (m25 indexeddb-design §8.3): Dexie 4, idb 8,
//! idb-keyval 6 and localForage 1.10, vendored UNMODIFIED in `tests/libs/`,
//! loaded into a redjs context and driven the way vue-native drives a frame:
//! `redjs_pump(TIMERS | RAF | MICROTASKS)` under the API lock, once per
//! simulated frame — NOT the browser-exact loop the WPT runner uses. Passing
//! here is what "Dexie and idb work unmodified" means.
//!
//! Plus the latency guard and the P1 benchmark: 1000 sequential awaited
//! `get`s must finish in ≤ 30 pumps. Numbers are printed (`--nocapture`).
//!
//! Database names are unique per test: the server is process-wide and the
//! tests run in parallel.

use std::time::{Duration, Instant};

use redjs::api::{Context, Error};
use redjs::ffi;

/// vue-native's frame: PUMP_TIMERS | PUMP_RAF | PUMP_MICROTASKS.
const FRAME: u32 = 1 | 2 | 4;

fn lib(path: &str) -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("libs").join(path);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn ctx(libs: &[&str]) -> Context {
    let c = Context::new();
    unsafe {
        ffi::redjs_web_install(c.raw());
        ffi::redjs_webshim_install(c.raw(), core::ptr::null(), 0, core::ptr::null(), 0);
    }
    for l in libs {
        eval(&c, &lib(l), l);
    }
    c
}

fn eval(c: &Context, src: &str, url: &str) -> String {
    match c.eval_named(src, url) {
        Ok(v) => c.to_rust_string(v).unwrap_or_default(),
        Err(Error::Exception(e)) => panic!("{url} threw: {}", c.error_info(e).0),
        Err(e) => panic!("{url}: {e}"),
    }
}

struct Run {
    out: String,
    pumps: u32,
    ms: f64,
}

/// Runs an async body as vue-native would: one pump per ~frame (a 1 ms sleep
/// between pumps stands in for the rest of the frame), under the API lock.
fn run(c: &Context, body: &str) -> Run {
    let src = format!(
        "globalThis.__r = undefined;\n\
         {{\n\
         const until = async (f, what) => {{ for (let i = 0; i < 2000; i++) {{ if (f()) return; await new Promise(r => setTimeout(r, 1)); }} throw new Error('timed out waiting for ' + what); }};\n\
         (async () => {{ {body} }})().then(v => {{ globalThis.__r = typeof v === 'string' ? v : JSON.stringify(v); }}, e => {{ globalThis.__r = 'ERR ' + (e && e.name) + ': ' + (e && e.message) + (e && e.failures ? ' failures=' + e.failures.length : ''); }});\n\
         }} 1"
    );
    eval(c, &src, "scenario.js");
    let start = Instant::now();
    let deadline = start + Duration::from_secs(60);
    let mut pumps = 0;
    unsafe { ffi::redjs_lock(c.raw()) };
    let out = loop {
        unsafe { ffi::redjs_pump(c.raw(), FRAME, core::ptr::null_mut()) };
        pumps += 1;
        let r = eval(c, "String(globalThis.__r)", "poll.js");
        if r != "undefined" {
            break r;
        }
        if Instant::now() > deadline {
            break "TIMEOUT".to_string();
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    unsafe { ffi::redjs_unlock(c.raw()) };
    Run { out, pumps, ms: start.elapsed().as_secs_f64() * 1000.0 }
}

fn expect(r: &Run, want: &str) {
    assert_eq!(r.out, want, "({} pumps, {:.1} ms)", r.pumps, r.ms);
}

// ---------------------------------------------------------------------------
// Dexie 4.4.7
// ---------------------------------------------------------------------------

#[test]
fn dexie_schema_bulk_queries_transactions() {
    let c = ctx(&["dexie-4.4.7/dexie.js"]);
    let r = run(
        &c,
        "const name = 'dx-main';
         const schema1 = { friends: '++id, name, age, [first+last], *tags' };
         let db = new Dexie(name);
         db.version(1).stores(schema1);
         await db.open();
         const rows = [];
         for (let i = 0; i < 10000; i++) rows.push({ name: 'n' + i, age: i % 100, first: 'f' + (i % 10), last: 'l' + (i % 7), tags: ['t' + (i % 3), 't' + (i % 5)] });
         const t0 = performance.now();
         await db.friends.bulkPut(rows);
         const bulkMs = performance.now() - t0;
         const count = await db.friends.count();
         // Compound index between().
         const between = await db.friends.where('[first+last]').between(['f1', 'l0'], ['f1', 'l3'], true, true).count();
         const wantBetween = rows.filter(r => r.first === 'f1' && r.last >= 'l0' && r.last <= 'l3').length;
         // multiEntry.
         const tagged = await db.friends.where('tags').equals('t4').count();
         const wantTagged = rows.filter(r => r.tags.includes('t4')).length;
         // rw transaction with native awaits inside: no PrematureCommitError.
         await db.transaction('rw', db.friends, async () => {
           const a = await db.friends.get(1);
           await Promise.resolve();
           await db.friends.update(1, { age: a.age + 1000 });
           const b = await db.friends.get(1);
           if (b.age !== a.age + 1000) throw new Error('update not visible inside the transaction');
         });
         const after = (await db.friends.get(1)).age;
         // bulkAdd with a duplicate: BulkError, the rest committed.
         let bulk = 'no error';
         try { await db.friends.bulkAdd([{ id: 1, name: 'dup' }, { name: 'fresh1' }, { name: 'fresh2' }]); }
         catch (e) { bulk = e.name + ':' + e.failures.length; }
         const fresh = await db.friends.where('name').anyOf('fresh1', 'fresh2').count();
         // Schema v2 adds an index over existing rows.
         db.close();
         db = new Dexie(name);
         db.version(1).stores(schema1);
         db.version(2).stores({ friends: '++id, name, age, [first+last], *tags, last' });
         await db.open();
         const byLast = await db.friends.where('last').equals('l3').count();
         const wantByLast = rows.filter(r => r.last === 'l3').length;
         const verno = db.verno;
         // getDatabaseNames, delete.
         const names = await Dexie.getDatabaseNames();
         await db.delete();
         const exists = await Dexie.exists(name);
         globalThis.__bulk = bulkMs;
         return [count, between === wantBetween, tagged === wantTagged, after, bulk, fresh, byLast === wantByLast, verno,
                 names.includes(name), exists].join('|');",
    );
    eprintln!("[bench] Dexie bulkPut(10 000): {} ms (scenario {:.0} ms, {} pumps)", eval(&c, "Math.round(globalThis.__bulk)", "b.js"), r.ms, r.pumps);
    expect(&r, "10000|true|true|1000|BulkError:1|2|true|2|true|false");
}

#[test]
fn dexie_live_query_refires_after_a_write() {
    let c = ctx(&["dexie-4.4.7/dexie.js"]);
    let r = run(
        &c,
        "const db = new Dexie('dx-live');
         db.version(1).stores({ items: '++id, kind' });
         await db.items.bulkAdd([{ kind: 'a' }, { kind: 'b' }]);
         const seen = [];
         const sub = Dexie.liveQuery(() => db.items.where('kind').equals('a').count())
           .subscribe({ next: v => seen.push(v), error: e => seen.push('E:' + e) });
         await until(() => seen.length >= 1, 'the first result');
         await db.items.add({ kind: 'a' });
         await until(() => seen.length >= 2, 'the re-run');
         sub.unsubscribe();
         await db.delete();
         return seen.join(',');",
    );
    expect(&r, "1,2");
}

#[test]
fn dexie_collections_cursors_and_versionchange() {
    let c = ctx(&["dexie-4.4.7/dexie.js"]);
    let r = run(
        &c,
        "const db = new Dexie('dx-coll');
         db.version(1).stores({ people: '++id, name, age' });
         const names = ['Ann', 'anna', 'Bob', 'bobby', 'ANNIKA', 'Carl', 'dave', 'Eve'];
         await db.people.bulkAdd(names.map((name, i) => ({ name, age: 20 + i })));
         // startsWithIgnoreCase walks the index with cursor.continue(nextKey).
         const ann = (await db.people.where('name').startsWithIgnoreCase('ann').toArray()).map(p => p.name).sort().join(',');
         const anyOfIc = await db.people.where('name').anyOfIgnoreCase('bob', 'EVE').count();
         const page = (await db.people.orderBy('age').reverse().offset(2).limit(3).toArray()).map(p => p.age).join(',');
         const pks = (await db.people.where('age').between(22, 25).primaryKeys()).join(',');
         // modify() updates through the cursor; Collection.delete() deletes through it.
         const modified = await db.people.where('age').above(25).modify(p => { p.name = p.name.toUpperCase(); });
         const upper = (await db.people.where('age').above(25).toArray()).every(p => p.name === p.name.toUpperCase());
         const removed = await db.people.where('name').equals('Bob').delete();
         const bulkGet = (await db.people.bulkGet([1, 3, 99])).map(p => p ? p.name : 'none').join(',');
         await db.people.bulkDelete([1, 2]);
         const left = await db.people.count();
         // A second instance upgrading the same database: the first closes itself on versionchange.
         const db2 = new Dexie('dx-coll');
         db2.version(1).stores({ people: '++id, name, age' });
         db2.version(2).stores({ people: '++id, name, age, [name+age]' });
         await db2.open();
         const firstClosed = !db.isOpen();
         const v = db2.verno;
         await db2.delete();
         return [ann, anyOfIc, page, pks, modified, upper, removed, bulkGet, left, firstClosed, v].join('|');",
    );
    expect(&r, "ANNIKA,Ann,anna|2|25,24,23|3,4,5|2|true|1|Ann,none,none|5|true|2");
}

// ---------------------------------------------------------------------------
// idb 8.0.4
// ---------------------------------------------------------------------------

#[test]
fn idb_upgrade_iteration_done_and_index_ranges() {
    let c = ctx(&["idb-8.0.4/umd.js"]);
    let r = run(
        &c,
        "const { openDB, deleteDB } = idb;
         const branches = [];
         const up = (db, oldVersion, newVersion) => {
           if (oldVersion < 1) { const s = db.createObjectStore('kv', { keyPath: 'id' }); s.createIndex('byN', 'n'); branches.push('v1'); }
           if (oldVersion < 2 && newVersion >= 2) { db.createObjectStore('meta'); branches.push('v2'); }
         };
         let db = await openDB('idb-main', 1, { upgrade: up });
         const tx = db.transaction('kv', 'readwrite');
         for (let i = 0; i < 20; i++) tx.store.put({ id: i, n: i % 4 });
         await tx.done;
         db.close();
         db = await openDB('idb-main', 2, { upgrade: up });
         let seen = 0, sum = 0;
         for await (const cursor of db.transaction('kv').store) { seen++; sum += cursor.value.id; }
         const ranged = await db.getAllFromIndex('kv', 'byN', IDBKeyRange.bound(1, 2));
         const onlyThree = await db.transaction('kv').store.index('byN').getAll(IDBKeyRange.only(3));
         const t2 = db.transaction(['kv', 'meta'], 'readwrite');
         await Promise.all([t2.objectStore('meta').put('x', 'k'), t2.objectStore('kv').delete(0), t2.done]);
         const n = await db.count('kv');
         const ok = db instanceof IDBDatabase && typeof db.get === 'function';
         db.close();
         await deleteDB('idb-main');
         return [branches.join(','), seen, sum, ranged.length, onlyThree.length, n, ok].join('|');",
    );
    expect(&r, "v1,v2|20|190|10|5|19|true");
}

// ---------------------------------------------------------------------------
// idb-keyval 6.3.0
// ---------------------------------------------------------------------------

#[test]
fn idb_keyval_get_set_update_entries_delmany_reopen() {
    let c = ctx(&["idb-keyval-6.3.0/umd.js"]);
    let r = run(
        &c,
        "const { set, get, update, entries, delMany, keys, createStore, setMany, values } = idbKeyval;
         const st = createStore('kv-main', 'store');
         await set('a', 1, st);
         await set('b', { x: 2 }, st);
         await update('a', (v) => v + 1, st);
         const a = await get('a', st);
         await setMany([['c', 3], ['d', 4]], st);
         const e = (await entries(st)).map(([k, v]) => k + '=' + JSON.stringify(v)).join(',');
         await delMany(['c', 'd'], st);
         // A second createStore on the same names is a second connection: a reopen.
         const again = createStore('kv-main', 'store');
         const ks = (await keys(again)).join(',');
         const vs = (await values(again)).length;
         return [a, e, ks, vs].join('|');",
    );
    expect(&r, "2|a=2,b={\"x\":2},c=3,d=4|a,b|2");
}

// ---------------------------------------------------------------------------
// localForage 1.10.0
// ---------------------------------------------------------------------------

#[test]
fn localforage_blobs_keys_iterate() {
    let c = ctx(&["localforage-1.10.0/localforage.js"]);
    let r = run(
        &c,
        "const lf = localforage.createInstance({ name: 'lf-main', storeName: 's' });
         await lf.ready();
         const driver = lf.driver();
         await lf.setItem('blob', new Blob(['abc'], { type: 'text/plain' }));
         const b = await lf.getItem('blob');
         await lf.setItem('n', 5);
         await lf.setItem('o', { a: [1, 2] });
         const ks = (await lf.keys()).join(',');
         const it = [];
         await lf.iterate((v, k, i) => { it.push(k + '@' + i); });
         const len = await lf.length();
         await lf.removeItem('n');
         const after = await lf.getItem('n');
         return [driver, b instanceof Blob, b.type, await b.text(), ks, it.join(','), len, String(after)].join('|');",
    );
    expect(&r, "asyncStorage|true|text/plain|abc|blob,n,o|blob@1,n@2,o@3|3|null");
}

// ---------------------------------------------------------------------------
// The Sentry offline-queue pattern (raw IDB, promise-wrapped)
// ---------------------------------------------------------------------------

#[test]
fn sentry_offline_queue_pattern() {
    let c = ctx(&[]);
    let r = run(
        &c,
        "function promisify(request) {
           return new Promise((resolve, reject) => {
             request.oncomplete = request.onsuccess = () => resolve(request.result);
             request.onabort = request.onerror = () => reject(request.error);
           });
         }
         function createStore(dbName, storeName) {
           const request = indexedDB.open(dbName);
           request.onupgradeneeded = () => request.result.createObjectStore(storeName);
           const dbp = promisify(request);
           return (cb) => dbp.then((db) => cb(db.transaction(storeName, 'readwrite').objectStore(storeName)));
         }
         const keys = (store) => promisify(store.getAllKeys());
         function push(store, value, max) {
           return store((store) => keys(store).then((keys) => {
             if (keys.length >= max) return;
             store.put(value, Math.max(...keys, 0) + 1);
             return promisify(store.transaction);
           }));
         }
         function shift(store) {
           return store((store) => keys(store).then((keys) => {
             const first = keys[0];
             if (first == null) return undefined;
             return promisify(store.get(first)).then((value) => {
               store.delete(first);
               return promisify(store.transaction).then(() => value);
             });
           }));
         }
         const q = createStore('sentry-offline', 'queue');
         for (const v of ['e1', 'e2', 'e3']) await push(q, v, 2);
         const out = [await shift(q), await shift(q), String(await shift(q))];
         return out.join(',');",
    );
    expect(&r, "e1,e2,undefined");
}

// ---------------------------------------------------------------------------
// The latency guard and the P1 benchmark
// ---------------------------------------------------------------------------

fn setup_bench(c: &Context, name: &str, n: u32) {
    let r = run(
        c,
        &format!(
            "const open = indexedDB.open('{name}', 1);
             open.onupgradeneeded = () => {{ const s = open.result.createObjectStore('s'); for (let i = 0; i < {n}; i++) s.put({{ i, payload: 'x'.repeat(64) }}, i); }};
             globalThis.benchDb = await new Promise((res, rej) => {{ open.onsuccess = () => res(open.result); open.onerror = () => rej(open.error); }});
             return 'ready';"
        ),
    );
    expect(&r, "ready");
}

#[test]
fn bench_sequential_gets_take_few_pumps() {
    let c = ctx(&[]);
    setup_bench(&c, "bench-seq", 1000);
    // Separate transactions per get: the worst case (a Begin + Commit per hop).
    let r = run(
        &c,
        "const get = (k) => new Promise((res, rej) => { const q = benchDb.transaction('s').objectStore('s').get(k); q.onsuccess = () => res(q.result); q.onerror = () => rej(q.error); });
         const t0 = performance.now();
         let sum = 0;
         for (let i = 0; i < 1000; i++) sum += (await get(i)).i;
         globalThis.__ms = performance.now() - t0;
         return String(sum);",
    );
    eprintln!("[bench] 1000 sequential awaited gets, a transaction each: {} pumps, {:.1} ms", r.pumps, r.ms);
    expect(&r, "499500");
    assert!(r.pumps <= 30, "1000 sequential gets took {} pumps (guard: <= 30)", r.pumps);

    // One transaction, every get awaited in turn (idb's shape).
    let r = run(
        &c,
        "const st = benchDb.transaction('s').objectStore('s');
         const get = (k) => new Promise((res, rej) => { const q = st.get(k); q.onsuccess = () => res(q.result); q.onerror = () => rej(q.error); });
         let sum = 0;
         for (let i = 0; i < 1000; i++) sum += (await get(i)).i;
         return String(sum);",
    );
    eprintln!("[bench] 1000 sequential awaited gets, one transaction: {} pumps, {:.1} ms", r.pumps, r.ms);
    expect(&r, "499500");
    assert!(r.pumps <= 30, "1000 sequential gets in one transaction took {} pumps (guard: <= 30)", r.pumps);

    // Pipelined: 1000 gets issued at once, then awaited together.
    let r = run(
        &c,
        "const st = benchDb.transaction('s').objectStore('s');
         const qs = []; for (let i = 0; i < 1000; i++) qs.push(st.get(i));
         await new Promise((res) => { qs[qs.length - 1].onsuccess = res; });
         return String(qs.reduce((a, q) => a + q.result.i, 0));",
    );
    eprintln!("[bench] 1000 pipelined gets (one listener): {} pumps, {:.1} ms", r.pumps, r.ms);
    expect(&r, "499500");

    // 10 000 puts in one readwrite transaction (no listeners but the last).
    let r = run(
        &c,
        "const tx = benchDb.transaction('s', 'readwrite');
         const st = tx.objectStore('s');
         for (let i = 0; i < 10000; i++) st.put({ i, payload: 'y'.repeat(64) }, 'p' + i);
         await new Promise((res, rej) => { tx.oncomplete = res; tx.onabort = () => rej(tx.error); });
         return 'ok';",
    );
    eprintln!("[bench] 10 000 puts in one transaction: {} pumps, {:.1} ms", r.pumps, r.ms);
    expect(&r, "ok");

    // getAll over 11 000 records.
    let r = run(
        &c,
        "const q = benchDb.transaction('s').objectStore('s').getAll();
         const all = await new Promise((res) => { q.onsuccess = () => res(q.result); });
         return String(all.length);",
    );
    eprintln!("[bench] getAll() of 11 000 records: {} pumps, {:.1} ms", r.pumps, r.ms);
    expect(&r, "11000");
}
