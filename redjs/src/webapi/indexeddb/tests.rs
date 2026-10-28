//! JS-level tests on the memory backend: real contexts, real pumps (held
//! under the API lock, as vue-native's frame does, so every drain is a
//! hook-running checkpoint). Database names are unique per test: the server
//! is process-wide and tests run in parallel.

use std::time::{Duration, Instant};

use crate::api::Context;
use crate::web::{PUMP_CHECKPOINT_PER_TASK, PUMP_MICROTASKS, PUMP_TIMERS};
use crate::webapi::testing::{ctx, ok, s};

const SPEC: u32 = PUMP_TIMERS | PUMP_MICROTASKS | PUMP_CHECKPOINT_PER_TASK;
const FRAME: u32 = PUMP_TIMERS | PUMP_MICROTASKS;

/// Pumps (lock held) until `globalThis.__r` is set; returns it. `flags` picks
/// the browser-exact loop or vue-native's frame loop.
fn wait_flags(c: &Context, flags: u32, ms: u64) -> (String, u32) {
    let deadline = Instant::now() + Duration::from_millis(ms);
    let mut pumps = 0;
    unsafe { crate::ffi::redjs_lock(c.raw()) };
    let r = loop {
        unsafe { crate::ffi::redjs_pump(c.raw(), flags, core::ptr::null_mut()) };
        pumps += 1;
        let r = s(c, "String(globalThis.__r)");
        if r != "undefined" {
            break r;
        }
        if Instant::now() > deadline {
            break format!("TIMEOUT; log: {}", s(c, "JSON.stringify(globalThis.__log || [])"));
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    unsafe { crate::ffi::redjs_unlock(c.raw()) };
    (r, pumps)
}

fn wait(c: &Context) -> String {
    wait_flags(c, SPEC, 10_000).0
}

/// Runs an async function body; its return value (stringified) or
/// `ERR <name>: <message>`.
fn run(c: &Context, body: &str) -> String {
    let src = format!(
        "globalThis.__r = undefined; globalThis.__log = [];\n\
         {{\n\
         const log = (x) => globalThis.__log.push(String(x));\n\
         const req = (r) => new Promise((res, rej) => {{ r.onsuccess = () => res(r.result); r.onerror = () => rej(r.error); }});\n\
         const done = (tx) => new Promise((res, rej) => {{ tx.oncomplete = () => res('complete'); tx.onabort = () => rej(tx.error || new Error('abort')); }});\n\
         (async () => {{ {body} }})().then(v => {{ globalThis.__r = String(v); }}, e => {{ globalThis.__r = 'ERR ' + (e && e.name) + ': ' + (e && e.message); }});\n\
         }} 1"
    );
    s(c, &src);
    wait(c)
}

/// A context with timers too (`setTimeout`), as the player has.
fn wctx() -> Context {
    let c = Context::new();
    unsafe {
        crate::ffi::redjs_web_install(c.raw());
        crate::ffi::redjs_webshim_install(c.raw(), core::ptr::null(), 0, core::ptr::null(), 0);
    }
    c
}

fn advised(needle: &str) -> bool {
    super::advisories().iter().any(|a| a.contains(needle))
}

#[test]
fn class_strings_of_subclass_instances() {
    let c = ctx();
    let r = run(
        &c,
        "const open = indexedDB.open('t-tags', 1);
         const tag = Object.prototype.toString.call(open);
         open.onupgradeneeded = () => { open.result.createObjectStore('s').put(1, 1); };
         const db = await req(open);
         const st = db.transaction('s').objectStore('s');
         const kc = await req(st.openKeyCursor());
         const st2 = db.transaction('s').objectStore('s');
         const vc = await req(st2.openCursor());
         db.close();
         return [tag, Object.prototype.toString.call(kc), Object.prototype.toString.call(vc),
                 open instanceof IDBRequest, vc instanceof IDBCursor, String(db), String(st)].join('|');",
    );
    assert_eq!(
        r,
        "[object IDBOpenDBRequest]|[object IDBCursor]|[object IDBCursorWithValue]|true|true|[object IDBDatabase]|[object IDBObjectStore]"
    );
}

#[test]
fn error_events_cancel_or_abort() {
    let c = ctx();
    let r = run(
        &c,
        "const open = indexedDB.open('t-errors', 1);
         open.onupgradeneeded = () => { open.result.createObjectStore('s'); };
         const db = await req(open);
         // preventDefault keeps the transaction alive: the first add commits.
         let tx = db.transaction('s', 'readwrite');
         tx.objectStore('s').add(1, 'k');
         const dup = tx.objectStore('s').add(2, 'k');
         dup.onerror = (e) => { log(dup.error.name); e.preventDefault(); };
         log(await done(tx));
         // Not canceled: the transaction aborts with the request's error, and
         // the abort bubbles to the database — after the transaction's own
         // handler, whose promise reactions run in between (a checkpoint
         // between listeners), as in a browser.
         tx = db.transaction('s', 'readwrite');
         tx.objectStore('s').put(3, 'other');
         tx.objectStore('s').add(4, 'k');
         db.addEventListener('abort', (e) => log('db saw abort of ' + (e.target === tx)));
         try { await done(tx); } catch (e) { log('aborted ' + e.name); }
         const t3 = db.transaction('s');
         const v = await req(t3.objectStore('s').get('k'));
         const other = await req(t3.objectStore('s').get('other'));
         db.close();
         return globalThis.__log.join() + '|' + v + '|' + other;",
    );
    assert_eq!(r, "ConstraintError,complete,aborted ConstraintError,db saw abort of true|1|undefined");
}

#[test]
fn awaits_inside_a_transaction_keep_it_alive() {
    // idb's shape: every await resolves in a success listener's microtask
    // checkpoint, while the transaction is active.
    let c = ctx();
    let r = run(
        &c,
        "const open = indexedDB.open('t-await', 1);
         open.onupgradeneeded = () => { open.result.createObjectStore('s'); };
         const db = await req(open);
         const tx = db.transaction('s', 'readwrite');
         const st = tx.objectStore('s');
         await req(st.put(1, 'a'));
         const a = await req(st.get('a'));
         await req(st.put(a + 1, 'b'));
         const keys = await req(st.getAllKeys());
         log(await done(tx));
         db.close();
         return keys.join() + '|' + globalThis.__log.join();",
    );
    assert_eq!(r, "a,b|complete");
}

#[test]
fn versionchange_blocked_and_delete() {
    let c = ctx();
    let r = run(
        &c,
        "const o1 = indexedDB.open('t-vc', 1);
         const db1 = await req(o1);
         db1.onversionchange = (e) => { log('vc ' + e.oldVersion + '>' + e.newVersion); db1.close(); };
         const o2 = indexedDB.open('t-vc', 2);
         o2.onupgradeneeded = (e) => log('up ' + e.oldVersion + '>' + e.newVersion);
         const db2 = await req(o2);
         // A connection that does not close blocks the delete until it does.
         const del = indexedDB.deleteDatabase('t-vc');
         del.onblocked = (e) => { log('blocked ' + e.oldVersion + '>' + e.newVersion); db2.close(); };
         const ev = await new Promise((res, rej) => { del.onsuccess = res; del.onerror = rej; });
         log('deleted ' + ev.oldVersion + ' ' + ev.newVersion + ' ' + (del.result === undefined));
         const names = (await indexedDB.databases()).map(d => d.name);
         return globalThis.__log.join() + '|' + names.includes('t-vc');",
    );
    assert_eq!(r, "vc 1>2,up 1>2,blocked 2>null,deleted 2 null true|false");
}

#[test]
fn upgrade_abort_reverts_and_close_in_upgrade_fails_the_open() {
    let c = ctx();
    let r = run(
        &c,
        "let store;
         const o1 = indexedDB.open('t-revert', 1);
         o1.onupgradeneeded = () => { store = o1.result.createObjectStore('s'); store.createIndex('i', 'x'); o1.transaction.abort(); };
         try { await req(o1); } catch (e) { log(e.name); }
         log(o1.result === undefined && o1.transaction === null);
         log(store.indexNames.length);
         log((await indexedDB.databases()).some(d => d.name === 't-revert'));
         const o2 = indexedDB.open('t-revert2', 1);
         o2.onupgradeneeded = () => { o2.result.createObjectStore('s'); o2.result.close(); };
         let completed = false;
         o2.onupgradeneeded = ((f) => (e) => { f(e); o2.transaction.oncomplete = () => completed = true; })(o2.onupgradeneeded);
         try { await req(o2); } catch (e) { log(e.name + ' after complete ' + completed); }
         return globalThis.__log.join();",
    );
    assert_eq!(r, "AbortError,true,0,false,AbortError after complete true");
}

#[test]
fn blob_values_round_trip() {
    let c = ctx();
    let r = run(
        &c,
        "const open = indexedDB.open('t-blob', 1);
         open.onupgradeneeded = () => { open.result.createObjectStore('s'); };
         const db = await req(open);
         let tx = db.transaction('s', 'readwrite');
         tx.objectStore('s').put({b: new Blob(['hello'], {type: 'text/x'}), n: 1}, 1);
         await done(tx);
         tx = db.transaction('s');
         const v = await req(tx.objectStore('s').get(1));
         db.close();
         return [v.b instanceof Blob, v.b.size, v.b.type, await v.b.text()].join();",
    );
    assert_eq!(r, "true,5,text/x,hello");
}

#[test]
fn storage_manager() {
    let c = ctx();
    let r = run(
        &c,
        "const e = await navigator.storage.estimate();
         return [typeof e.usage, e.quota, await navigator.storage.persist(), await navigator.storage.persisted()].join();",
    );
    assert_eq!(r, format!("number,{},true,true", super::DEFAULT_QUOTA));
}

#[test]
fn a_dropped_connection_still_gets_versionchange() {
    // Open connections are protected: GC cannot take one that is still open,
    // so its versionchange handler runs and the upgrade is not blocked.
    let c = ctx();
    s(
        &c,
        "globalThis.__r = undefined; (() => { const o = indexedDB.open('t-gc', 1);
           o.onsuccess = () => { o.result.onversionchange = (e) => { e.target.close(); }; globalThis.__r = 'open'; }; })(); 1",
    );
    assert_eq!(wait(&c), "open");
    c.gc();
    let r = run(&c, "const db = await req(indexedDB.open('t-gc', 2)); const v = db.version; db.close(); return v;");
    assert_eq!(r, "2");
}

#[test]
fn second_instance_on_the_same_folder_is_unknown_error_and_advised() {
    let c = ctx();
    let name: Vec<u16> = "t-locked".encode_utf16().collect();
    super::memory::fail_next_open(&name, super::backend::BackendError::Locked("pid 4242".into()));
    let r = run(&c, "try { await req(indexedDB.open('t-locked', 1)); return 'opened'; } catch (e) { return e.name + ': ' + e.message; }");
    assert!(r.starts_with("UnknownError: ") && r.contains("another instance"), "{r}");
    assert!(advised("\"t-locked\" is open in another instance"), "{:?}", super::advisories());
    // The lock is per attempt: the next open works.
    assert_eq!(run(&c, "const db = await req(indexedDB.open('t-locked', 1)); db.close(); return 'ok';"), "ok");
}

#[test]
fn late_config_change_is_advised_once() {
    let c = ctx();
    // Make sure the server has started (latched).
    assert_eq!(run(&c, "const db = await req(indexedDB.open('t-latch', 1)); db.close(); return 'ok';"), "ok");
    super::config_changed();
    super::config_changed();
    let n = super::advisories().iter().filter(|a| a.contains("applies at the next start")).count();
    assert_eq!(n, 1, "{:?}", super::advisories());
}

#[test]
fn opaque_origin_is_a_security_error() {
    let c = ctx();
    unsafe { super::set_opaque_origin(c.raw(), true) };
    ok(&c, "(() => { try { indexedDB.open('x'); return false; } catch (e) { return e.name === 'SecurityError'; } })()");
    ok(&c, "(() => { try { indexedDB.deleteDatabase('x'); return false; } catch (e) { return e.name === 'SecurityError'; } })()");
    let r = run(&c, "try { await indexedDB.databases(); return 'listed'; } catch (e) { return e.name; }");
    assert_eq!(r, "SecurityError");
}

#[test]
fn a_transaction_made_in_a_coalesced_timer_is_advised() {
    // IDB-D1: vue-native's frame drains once per pump (no checkpoint per
    // task); a transaction created in a timer callback there is named.
    let c = wctx();
    s(
        &c,
        "globalThis.__r = undefined;
         const o = indexedDB.open('t-d1', 1);
         o.onupgradeneeded = () => o.result.createObjectStore('s');
         o.onsuccess = () => setTimeout(() => {
           const tx = o.result.transaction('s');
           tx.oncomplete = () => { o.result.close(); globalThis.__r = 'done'; };
         }, 0); 1",
    );
    assert_eq!(wait_flags(&c, FRAME, 10_000).0, "done");
    assert!(advised("IDB-D1"), "{:?}", super::advisories());
}

#[test]
fn a_gone_context_releases_its_connections() {
    // context_gone runs at teardown with no JS: the server closes the dead
    // context's connection, so another context's upgrade is not blocked.
    let a = ctx();
    let b = ctx();
    assert_eq!(run(&a, "globalThis.keep = await req(indexedDB.open('t-gone', 1)); return 'held';"), "held");
    let r = s(
        &b,
        "globalThis.__r = undefined;
         const o = indexedDB.open('t-gone', 2);
         o.onsuccess = () => { globalThis.__r = 'v' + o.result.version; o.result.close(); }; 1",
    );
    assert_eq!(r, "1");
    // `a` is never pumped, so it never fires (or acks) its versionchange: b
    // waits, exactly as a page waits on another page's event loop.
    let until = Instant::now() + Duration::from_millis(200);
    unsafe { crate::ffi::redjs_lock(b.raw()) };
    while Instant::now() < until {
        unsafe { crate::ffi::redjs_pump(b.raw(), SPEC, core::ptr::null_mut()) };
        std::thread::sleep(Duration::from_millis(1));
    }
    unsafe { crate::ffi::redjs_unlock(b.raw()) };
    assert_eq!(s(&b, "String(globalThis.__r)"), "undefined", "the upgrade must wait for a");
    // Tear a down: no JS runs, the server forgets its connection.
    crate::context::forget(a.raw());
    assert_eq!(wait(&b), "v2");
}

#[test]
fn surface_and_brands() {
    let c = ctx();
    ok(&c, "typeof indexedDB === 'object' && indexedDB instanceof IDBFactory && indexedDB === globalThis.indexedDB");
    ok(&c, "Object.prototype.toString.call(indexedDB) === '[object IDBFactory]'");
    ok(&c, "Object.getPrototypeOf(IDBOpenDBRequest.prototype) === IDBRequest.prototype && Object.getPrototypeOf(IDBOpenDBRequest) === IDBRequest");
    ok(&c, "Object.getPrototypeOf(IDBRequest.prototype) === EventTarget.prototype");
    ok(&c, "Object.getPrototypeOf(IDBCursorWithValue.prototype) === IDBCursor.prototype");
    ok(&c, "Object.getPrototypeOf(IDBVersionChangeEvent.prototype) === Event.prototype");
    ok(&c, "'onsuccess' in IDBRequest.prototype && 'onupgradeneeded' in IDBOpenDBRequest.prototype");
    ok(&c, "(() => { try { new IDBFactory(); return false; } catch (e) { return e instanceof TypeError; } })()");
    ok(&c, "(() => { const e = new IDBVersionChangeEvent('versionchange', {oldVersion: 1, newVersion: 2}); return e.oldVersion === 1 && e.newVersion === 2 && e.type === 'versionchange'; })()");
    ok(&c, "new IDBVersionChangeEvent('x').newVersion === null");
    ok(&c, "typeof navigator.storage.estimate === 'function' && navigator.storage === navigator.storage");
    ok(&c, "indexedDB.cmp(1, 2) === -1 && indexedDB.cmp('a', 1) === 1 && indexedDB.cmp([1], [1]) === 0 && indexedDB.cmp(-0, 0) === 0");
    ok(&c, "(() => { try { indexedDB.cmp({}, 1); return false; } catch (e) { return e.name === 'DataError'; } })()");
    ok(&c, "(() => { const r = IDBKeyRange.bound(1, 5, true); return r.lower === 1 && r.upper === 5 && r.lowerOpen && !r.upperOpen && r.includes(3) && !r.includes(1); })()");
    ok(&c, "(() => { try { IDBKeyRange.bound(5, 1); return false; } catch (e) { return e.name === 'DataError'; } })()");
}

#[test]
fn open_put_get_complete() {
    let c = ctx();
    let r = run(
        &c,
        "const open = indexedDB.open('t-basic', 1);
         let upgraded = 0;
         open.onupgradeneeded = (e) => { upgraded = e.oldVersion + ':' + e.newVersion; const db = open.result; db.createObjectStore('s'); };
         const db = await req(open);
         log('version ' + db.version + ' up ' + upgraded + ' names ' + Array.from(db.objectStoreNames).join());
         const tx = db.transaction('s', 'readwrite');
         const st = tx.objectStore('s');
         st.put({a: 1, d: new Date(5)}, 'k1');
         st.add('two', 'k2');
         const ok2 = await done(tx);
         const tx2 = db.transaction('s');
         const v = await req(tx2.objectStore('s').get('k1'));
         const all = await req(tx2.objectStore('s').getAllKeys());
         const n = await req(tx2.objectStore('s').count());
         db.close();
         return [globalThis.__log.join(), ok2, v.a, v.d.getTime(), all.join(), n].join('|');",
    );
    assert_eq!(r, "version 1 up 0:1 names s|complete|1|5|k1,k2|2");
}
