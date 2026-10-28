//! `localStorage` / `sessionStorage` — Web Storage over a host-supplied
//! persistence backend.
//!
//! # Why the backend is a trait and not code in this file
//!
//! Where a key/value pair belongs on disk is not a browser question. A browser
//! answers it from an origin and a profile directory; a desktop app answers it
//! from a known folder, a portable install, or a folder the user picked, and it
//! may want the bytes encrypted at rest. None of that is portable and none of it
//! belongs in a JS engine. What *is* portable is the shape of Web Storage — an
//! ordered string→string map with six members and a quota — so that shape is
//! here and the bytes are somebody else's crate. Same seam as
//! [`crate::webapi::permissions::PermissionHost`], for the same reason.
//!
//! An embedder installs one with [`set_host`] before or after
//! [`crate::webapi::install`] — the hook is read at first touch, not at install
//! time. With no host installed `localStorage` still exists and still works,
//! **in memory only**, and says so once on stderr. That is deliberately not a
//! missing `localStorage`: code that feature-detects `typeof localStorage` would
//! otherwise take a silently different path here than in a browser, and the
//! failure it is guarding against (storage disabled) is not the failure that
//! actually happened (no backend wired up).
//!
//! # Divergence 1: the `storage` event never fires
//!
//! In a browser `storage` fires on *other* browsing contexts of the same origin
//! — it is how one tab learns that another tab wrote a key. This runtime is a
//! single context: there is no other tab, so there is nothing to notify and a
//! `storage` listener would never be called even if the machinery existed.
//!
//! We therefore fire nothing, and advise **once, only if an application
//! actually adds a `storage` listener** — an advisory nobody's code asked for is
//! noise. [`StorageEvent`] itself exists and is constructible, so an app that
//! synthesises its own cross-component notification
//! (`dispatchEvent(new StorageEvent("storage", {...}))`) behaves exactly as it
//! does in a browser.
//!
//! The honest consequence, stated plainly: **two processes sharing one storage
//! folder do not see each other's writes.** Each loads once and writes the whole
//! area; the last writer wins. In a browser that case is two tabs, and the
//! `storage` event is precisely what makes it survivable. Here it is two dev
//! instances, and the answer is to give them different folders — see the
//! embedder's documentation.
//!
//! # (Closed) Divergence 2: `QuotaExceededError`
//!
//! `setItem` past the quota throws a real `QuotaExceededError` — the WebIDL
//! derived interface, so `e instanceof DOMException`, `e.name ===
//! "QuotaExceededError"` and `e.code === 22` all hold, with Chrome's message
//! text and `quota`/`requested` null (Web Storage does not report them). Before
//! `webapi::domexception` existed this was an `Error` with that name.
//!
//! # Divergence 3: values round-trip through UTF-8
//!
//! JS strings are UTF-16 and may contain lone surrogates; the host trait speaks
//! `String`. A lone surrogate stored here comes back as U+FFFD. This is
//! redjs-wide (`read_string`), not specific to storage, and no real key or value
//! is affected.
//!
//! # The named-property surface is a `Proxy`
//!
//! `localStorage.token = "x"`, `delete localStorage.token`,
//! `Object.keys(localStorage)` and `JSON.stringify(localStorage)` are all part
//! of Web Storage, and JSC's C class API as redjs binds it has no
//! named-property hooks. So the native [`Storage`](install) object — six real
//! members on a real prototype — is wrapped in a `Proxy` whose traps forward to
//! `getItem`/`setItem`/`removeItem`/`key`/`length`. Builtins win over stored
//! keys, which is what WebIDL says for an interface without
//! `[LegacyOverrideBuiltIns]`: a key literally named `getItem` is reachable
//! through `getItem("getItem")` and not through `localStorage.getItem`, exactly
//! as in a browser.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, RwLock};

use super::events;
use super::util::*;
use crate::{capi, glue};

// ---------------------------------------------------------------------------
// The platform contract
// ---------------------------------------------------------------------------

/// Chrome's, Firefox's and Safari's shared answer, and therefore the number
/// application code is written against: 5 MiB per area.
pub const DEFAULT_QUOTA_BYTES: usize = 5 * 1024 * 1024;

/// The persistence backend behind `localStorage`.
///
/// One implementation per embedder. Deliberately free of any browser-shaped
/// type, so a backend can be written and tested with no JS context anywhere
/// near it — and deliberately *whole-area*, because the only file update that
/// is crash-safe is one that replaces the whole file by rename. A backend that
/// wants to coalesce writes does so on its own clock; this trait says "this is
/// the area now", nothing about when it must reach a disk.
///
/// `sessionStorage` never reaches a host: it is per-run by definition.
pub trait StorageHost: Send + Sync {
    /// The persisted area, in the order keys should enumerate. Called **once**,
    /// lazily, the first time anything touches `localStorage`.
    fn load(&self) -> Vec<(String, String)>;

    /// The area after a mutation. Called on every `setItem` / `removeItem` /
    /// `clear` that actually changed something.
    ///
    /// Must not call back into the storage API — it runs while the area lock is
    /// held. Errors are the host's to report: by the time this is called the
    /// mutation has already happened in memory, and a browser does not undo a
    /// `setItem` because a disk was full (it refuses up front, which is what
    /// [`quota_bytes`](StorageHost::quota_bytes) is for).
    fn store(&self, entries: &[(String, String)]);

    /// The cap `setItem` enforces, in bytes, counting each key and value as
    /// two bytes per UTF-16 code unit. Default [`DEFAULT_QUOTA_BYTES`].
    fn quota_bytes(&self) -> usize {
        DEFAULT_QUOTA_BYTES
    }

    /// Where this persists, for advisories: a path, or a phrase like
    /// `"the app's local data folder"`.
    fn describe(&self) -> String {
        String::from("the host's storage backend")
    }
}

static HOST: RwLock<Option<&'static dyn StorageHost>> = RwLock::new(None);

/// Install the persistence backend. Last writer wins; an embedder calls this
/// once at boot, before or after [`crate::webapi::install`].
///
/// Installing a host after `localStorage` has already been read does **not**
/// re-load the area — the in-memory copy is authoritative for the run, and
/// silently swapping the user's data underneath running code would be worse
/// than the missing-host advisory it replaces.
pub fn set_host(host: &'static dyn StorageHost) {
    *HOST.write().unwrap() = Some(host);
}

/// The installed backend, if any.
pub fn host() -> Option<&'static dyn StorageHost> {
    *HOST.read().unwrap()
}

fn advise_no_host() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        eprintln!(
            "[redjs] localStorage: no StorageHost installed — this run is IN MEMORY ONLY and \
             nothing survives a restart. Call redjs::webapi::storage::set_host()."
        );
    });
}

// ---------------------------------------------------------------------------
// The areas
// ---------------------------------------------------------------------------

/// One storage area: an insertion-ordered string→string map.
///
/// A `Vec` rather than a map because Web Storage is *ordered* (`key(i)` and
/// enumeration both depend on it) and because the realistic size of an area is
/// tens of keys, where a linear scan beats a hash plus a side table.
#[derive(Default)]
struct Area {
    entries: Vec<(String, String)>,
    /// `localStorage` only: has [`StorageHost::load`] run yet?
    loaded: bool,
}

impl Area {
    fn index_of(&self, key: &str) -> Option<usize> {
        self.entries.iter().position(|(k, _)| k == key)
    }

    /// Bytes charged against the quota: two per UTF-16 code unit of every key
    /// and every value. The count browsers use, so an app that fits in a
    /// browser fits here.
    fn size(&self) -> usize {
        self.entries.iter().map(|(k, v)| utf16_bytes(k) + utf16_bytes(v)).sum()
    }
}

fn utf16_bytes(s: &str) -> usize {
    s.encode_utf16().count() * 2
}

/// `localStorage` is PROCESS-global, not per-context — a browser gives every
/// same-origin context one storage area, and two contexts with private copies
/// of one file would corrupt it on the second write.
fn local_area() -> &'static Mutex<Area> {
    static A: OnceLock<Mutex<Area>> = OnceLock::new();
    A.get_or_init(|| Mutex::new(Area::default()))
}

/// `sessionStorage` is per-context and never persisted.
fn session_areas() -> &'static Mutex<HashMap<usize, Area>> {
    static A: OnceLock<Mutex<HashMap<usize, Area>>> = OnceLock::new();
    A.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Runs `f` against one area, loading `localStorage` from the host on first
/// touch and persisting afterwards if `f` reports a change.
fn with_area<R>(local: bool, g: usize, f: impl FnOnce(&mut Area, usize) -> (R, bool)) -> R {
    if local {
        let mut area = local_area().lock().unwrap();
        if !area.loaded {
            area.loaded = true;
            match host() {
                Some(h) => area.entries = h.load(),
                None => advise_no_host(),
            }
        }
        let quota = host().map(|h| h.quota_bytes()).unwrap_or(DEFAULT_QUOTA_BYTES);
        let (out, dirty) = f(&mut area, quota);
        if dirty {
            if let Some(h) = host() {
                h.store(&area.entries);
            }
        }
        out
    } else {
        let mut areas = session_areas().lock().unwrap();
        let area = areas.entry(g).or_default();
        f(area, DEFAULT_QUOTA_BYTES).0
    }
}

/// Test-only reset of the process-global `localStorage` area, so one test's
/// keys are not another's fixture. Not part of the web surface.
#[cfg(test)]
fn reset_local_for_tests() {
    let mut area = local_area().lock().unwrap();
    area.entries.clear();
    area.loaded = false;
}

// ---------------------------------------------------------------------------
// Per-context JS state
// ---------------------------------------------------------------------------

struct StorageData {
    local: bool,
    g: usize,
}

struct StorageClasses {
    storage_class: capi::JSClassRef,
    storage: Class,
    event: Class,
    /// Private slot bag on a StorageEvent ({ key, oldValue, newValue, url }).
    s_ev: *mut c_void,
}

unsafe impl Send for StorageClasses {}
unsafe impl Sync for StorageClasses {}

fn classes_reg() -> &'static Mutex<HashMap<usize, &'static StorageClasses>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static StorageClasses>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> &'static StorageClasses {
    classes_reg()
        .lock()
        .unwrap()
        .get(&(g as usize))
        .copied()
        .expect("webapi::storage not installed")
}

/// `(context, the Proxy object JS actually holds) -> is it localStorage?`
///
/// The price of the named-property `Proxy` (module header): `localStorage
/// .getItem(k)` calls the prototype method with `this` bound to the **proxy**,
/// not to the native object behind it, so the brand check has nothing to check.
///
/// The alternatives were worse. Returning `fn.bind(target)` from the `get` trap
/// makes `localStorage.getItem !== Storage.prototype.getItem` and mints a
/// closure per property read; making the methods lenient about `this` would let
/// `Storage.prototype.clear.call({})` wipe an area. So the two proxies — there
/// are exactly two, they are protected, and JSC's heap does not move objects —
/// are registered here as legitimate receivers, and everything else is still a
/// `TypeError`.
fn proxies() -> &'static Mutex<HashMap<(usize, u64), bool>> {
    static P: OnceLock<Mutex<HashMap<(usize, u64), bool>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `crate::context` fan-out. A gone context's `sessionStorage` dies with it —
/// a browsing context's session area does — instead of greeting the next
/// context that lands on the same address.
pub(crate) fn forget(gone: crate::context::Gone) {
    classes_reg().lock().unwrap().remove(&gone.key);
    session_areas().lock().unwrap().remove(&gone.key);
    proxies().lock().unwrap().retain(|(g, _), _| *g != gone.key);
}

/// `(is_local, context)` for a `this` that is either a native Storage or one of
/// this context's two proxies.
unsafe fn this_area(g: *mut c_void, cf: *mut c_void) -> Option<(bool, usize)> {
    let this = cf_this(cf);
    if let Some(d) = host_data::<StorageData>(g, classes(g).storage_class, this) {
        return Some((d.local, d.g));
    }
    proxies().lock().unwrap().get(&(g as usize, this)).map(|&local| (local, g as usize))
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

macro_rules! this_storage {
    ($g:expr, $cf:expr, $member:literal) => {
        match this_area($g, $cf) {
            Some(pair) => pair,
            None => {
                return throw_type_error(
                    $g,
                    concat!("Storage.", $member, " called on an object that is not a Storage"),
                )
            }
        }
    };
}

unsafe extern "sysv64" fn storage_length(g: *mut c_void, cf: *mut c_void) -> u64 {
    let (local, ctx) = this_storage!(g, cf, "length");
    let n = with_area(local, ctx, |a, _| (a.entries.len(), false));
    enc_i32(n as i32)
}

/// `key(n)` — the nth key in insertion order, or `null`.
unsafe extern "sysv64" fn storage_key(g: *mut c_void, cf: *mut c_void) -> u64 {
    let (local, ctx) = this_storage!(g, cf, "key");
    // WebIDL `unsigned long`: NaN/undefined → 0, negatives wrap. Anything past
    // the end is `null`, which is also where a wrapped negative lands.
    let n = read_number(g, cf_arg(cf, 0));
    let idx = if n.is_finite() && n >= 0.0 { n as u64 } else { 0 };
    let found = with_area(local, ctx, |a, _| {
        (a.entries.get(idx as usize).map(|(k, _)| k.clone()), false)
    });
    match found {
        Some(k) => js_string(g, &k),
        None => NULL,
    }
}

unsafe extern "sysv64" fn storage_get_item(g: *mut c_void, cf: *mut c_void) -> u64 {
    let (local, ctx) = this_storage!(g, cf, "getItem");
    let Some(key) = read_string(g, cf_arg(cf, 0)) else { return UNDEF };
    let found =
        with_area(local, ctx, |a, _| (a.index_of(&key).map(|i| a.entries[i].1.clone()), false));
    match found {
        Some(v) => js_string(g, &v),
        None => NULL,
    }
}

unsafe extern "sysv64" fn storage_set_item(g: *mut c_void, cf: *mut c_void) -> u64 {
    let (local, ctx) = this_storage!(g, cf, "setItem");
    // Both arguments are `DOMString`, so both are ToString-coerced: the browser
    // behaviour every "I stored an object and got [object Object] back" bug
    // report depends on. A missing second argument really is the string
    // "undefined", not a removal.
    let Some(key) = read_string(g, cf_arg(cf, 0)) else { return UNDEF };
    let Some(value) = read_string(g, cf_arg(cf, 1)) else { return UNDEF };

    enum Outcome {
        Unchanged,
        Stored,
        Quota(usize),
    }
    let outcome = with_area(local, ctx, |a, quota| {
        match a.index_of(&key) {
            Some(i) => {
                if a.entries[i].1 == value {
                    return (Outcome::Unchanged, false);
                }
                let after =
                    a.size() - utf16_bytes(&a.entries[i].1) + utf16_bytes(&value);
                if after > quota {
                    return (Outcome::Quota(quota), false);
                }
                a.entries[i].1 = value;
            }
            None => {
                let after = a.size() + utf16_bytes(&key) + utf16_bytes(&value);
                if after > quota {
                    return (Outcome::Quota(quota), false);
                }
                a.entries.push((key.clone(), value));
            }
        }
        (Outcome::Stored, true)
    });

    match outcome {
        // A no-op write is not a write: it must not rewrite the file, and in a
        // browser it does not fire `storage` either.
        Outcome::Unchanged | Outcome::Stored => UNDEF,
        Outcome::Quota(quota) => throw_quota_exceeded(
            g,
            &format!(
                "Failed to execute 'setItem' on 'Storage': Setting the value of '{key}' exceeded \
                 the quota ({} KiB).",
                quota / 1024
            ),
        ),
    }
}

unsafe extern "sysv64" fn storage_remove_item(g: *mut c_void, cf: *mut c_void) -> u64 {
    let (local, ctx) = this_storage!(g, cf, "removeItem");
    let Some(key) = read_string(g, cf_arg(cf, 0)) else { return UNDEF };
    with_area(local, ctx, |a, _| match a.index_of(&key) {
        Some(i) => {
            a.entries.remove(i);
            ((), true)
        }
        None => ((), false),
    });
    UNDEF
}

unsafe extern "sysv64" fn storage_clear(g: *mut c_void, cf: *mut c_void) -> u64 {
    let (local, ctx) = this_storage!(g, cf, "clear");
    with_area(local, ctx, |a, _| {
        let had = !a.entries.is_empty();
        a.entries.clear();
        ((), had)
    });
    UNDEF
}

/// The `QuotaExceededError` a browser throws — the real derived interface
/// (WebIDL `QuotaExceededError`), `quota`/`requested` null as Web Storage
/// leaves them.
unsafe fn throw_quota_exceeded(g: *mut c_void, msg: &str) -> u64 {
    glue::rjs_throw(g, super::domexception::create_quota_exceeded(g, msg, None, None));
    UNDEF
}

// ---------------------------------------------------------------------------
// StorageEvent
// ---------------------------------------------------------------------------

unsafe extern "sysv64" fn storage_event_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "StorageEvent constructor requires a type argument");
    }
    let c = classes(g);
    let obj = ordinary_create(g, cf_new_target(cf), c.event.proto);
    events::init_event(g, obj, cf_arg(cf, 0), cf_arg(cf, 1), false);

    let init = cf_arg(cf, 1);
    let slots = new_object(g, UNDEF);
    // Every member is `DOMString?` with a `null` default, so an absent one is
    // `null` and an explicitly-passed `undefined` is… also null. WebIDL
    // nullable string conversion: `undefined` → the default.
    let member = |name: &str| {
        let raw = if is_cell(init) { get(g, init, name) } else { UNDEF };
        if raw == UNDEF || raw == NULL {
            NULL
        } else {
            match read_string(g, raw) {
                Some(s) => js_string(g, &s),
                None => NULL,
            }
        }
    };
    define(g, slots, "key", member("key"), 0);
    define(g, slots, "oldValue", member("oldValue"), 0);
    define(g, slots, "newValue", member("newValue"), 0);
    // `url` is a plain `USVString` with a `""` default, not nullable.
    let url = if is_cell(init) { get(g, init, "url") } else { UNDEF };
    let url = if url == UNDEF {
        js_string(g, "")
    } else {
        read_string(g, url).map(|s| js_string(g, &s)).unwrap_or_else(|| js_string(g, ""))
    };
    define(g, slots, "url", url, 0);
    let area = if is_cell(init) { get(g, init, "storageArea") } else { UNDEF };
    define(g, slots, "storageArea", if is_cell(area) { area } else { NULL }, 0);

    glue::rjs_define(g, obj, c.s_ev, slots, 0);
    obj
}

unsafe fn ev_member(g: *mut c_void, cf: *mut c_void, name: &str) -> u64 {
    let slots = get_a(g, cf_this(cf), classes(g).s_ev);
    if !is_cell(slots) {
        return throw_type_error(g, "StorageEvent accessor on wrong this");
    }
    get(g, slots, name)
}

unsafe extern "sysv64" fn ev_key(g: *mut c_void, cf: *mut c_void) -> u64 {
    ev_member(g, cf, "key")
}
unsafe extern "sysv64" fn ev_old_value(g: *mut c_void, cf: *mut c_void) -> u64 {
    ev_member(g, cf, "oldValue")
}
unsafe extern "sysv64" fn ev_new_value(g: *mut c_void, cf: *mut c_void) -> u64 {
    ev_member(g, cf, "newValue")
}
unsafe extern "sysv64" fn ev_url(g: *mut c_void, cf: *mut c_void) -> u64 {
    ev_member(g, cf, "url")
}
unsafe extern "sysv64" fn ev_storage_area(g: *mut c_void, cf: *mut c_void) -> u64 {
    ev_member(g, cf, "storageArea")
}

/// The `storage` advisory (module header, divergence 1). Registered with
/// [`events::on_listener_added`] at install, so it costs one relaxed atomic
/// load per `addEventListener` in a build that has this module, and literally
/// nothing in a build that does not.
fn listener_added(ty: &str) {
    if ty != "storage" {
        return;
    }
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        eprintln!(
            "[redjs] a 'storage' listener was added, and this runtime never fires one: the event \
             exists to tell one browsing context that ANOTHER wrote a key, and there is only one \
             context here. Writes made by this process are visible immediately through \
             localStorage itself. `new StorageEvent(...)` + dispatchEvent still works if you want \
             to notify your own components."
        );
    });
}

// ---------------------------------------------------------------------------
// The named-property surface
// ---------------------------------------------------------------------------

/// Wraps the two native Storage objects in the `Proxy` that gives Web Storage
/// its named properties (module header). Evaluated once at install, with the
/// raw objects already published under their final names.
const STORAGE_JS: &str = r#"(() => {
  const proto = Storage.prototype;
  // Spec: no [LegacyOverrideBuiltIns], so `length`/`getItem`/… shadow a stored
  // key of the same name. `Reflect.has(proto, p)` IS that rule.
  const builtin = (p) => Reflect.has(proto, p);
  const wrap = (store) => new Proxy(store, {
    get(t, p) {
      if (typeof p === "symbol" || builtin(p)) return Reflect.get(t, p, t);
      const v = t.getItem(p);
      return v === null ? undefined : v;
    },
    set(t, p, v) {
      if (typeof p === "symbol") return Reflect.set(t, p, v, t);
      t.setItem(p, v);
      return true;
    },
    has(t, p) {
      if (typeof p === "symbol") return Reflect.has(t, p);
      return builtin(p) || t.getItem(p) !== null;
    },
    deleteProperty(t, p) {
      if (typeof p === "symbol") return Reflect.deleteProperty(t, p);
      t.removeItem(p);
      return true;
    },
    ownKeys(t) {
      const n = t.length;
      const out = new Array(n);
      for (let i = 0; i < n; i++) out[i] = t.key(i);
      return out;
    },
    getOwnPropertyDescriptor(t, p) {
      if (typeof p === "symbol") return Reflect.getOwnPropertyDescriptor(t, p);
      const v = t.getItem(p);
      if (v === null) return undefined;
      return { value: v, writable: true, enumerable: true, configurable: true };
    },
    defineProperty(t, p, d) {
      if (typeof p === "symbol") return Reflect.defineProperty(t, p, d);
      if (!("value" in d)) return false;
      t.setItem(p, d.value);
      return true;
    },
  });
  // `Object.prototype.toString.call(localStorage)` is "[object Storage]" in a
  // browser; without this it would be "[object Object]".
  Object.defineProperty(proto, Symbol.toStringTag, {
    value: "Storage", writable: false, enumerable: false, configurable: true,
  });
  for (const name of ["localStorage", "sessionStorage"]) {
    Object.defineProperty(globalThis, name, {
      value: wrap(globalThis[name]), writable: false, enumerable: true, configurable: true,
    });
  }
})();"#;

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

pub unsafe fn install(g: *mut c_void, event_proto: u64) -> i32 {
    let storage = define_class(g, &ClassSpec {
        name: "Storage",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[
            ("key", 1, storage_key),
            ("getItem", 1, storage_get_item),
            ("setItem", 2, storage_set_item),
            ("removeItem", 1, storage_remove_item),
            ("clear", 0, storage_clear),
        ],
        getters: &[("length", storage_length)],
        accessors: &[],
    });

    let event = define_class(g, &ClassSpec {
        name: "StorageEvent",
        arity: 1,
        construct: Some(storage_event_construct),
        parent_proto: event_proto,
        methods: &[],
        getters: &[
            ("key", ev_key),
            ("oldValue", ev_old_value),
            ("newValue", ev_new_value),
            ("url", ev_url),
            ("storageArea", ev_storage_area),
        ],
        accessors: &[],
    });

    let storage_class = make_host_class::<StorageData>("Storage");
    let s_ev = atom_priv(g, "redjs#storageEvent");
    let boxed: &'static StorageClasses =
        Box::leak(Box::new(StorageClasses { storage_class, storage, event, s_ev }));
    classes_reg().lock().unwrap().insert(g as usize, boxed);

    let global = glue::rjs_global(g);
    for (name, local) in [("localStorage", true), ("sessionStorage", false)] {
        let obj = host_create(
            g,
            storage_class,
            boxed.storage.proto,
            StorageData { local, g: g as usize },
        );
        crate::ffi::redjs_protect(g, obj);
        // Enumerable, like every `Window` interface attribute; the `Proxy` that
        // replaces it below keeps the same shape.
        define(g, global, name, obj, 0);
    }

    events::on_listener_added(listener_added);

    let url_name = "redjs/storage.js";
    let mut out = 0u64;
    if glue::rjs_eval8(
        g,
        STORAGE_JS.as_ptr(),
        STORAGE_JS.len(),
        url_name.as_ptr(),
        url_name.len(),
        &mut out,
    ) == 1
    {
        let (mut msg, mut stack) = (0u64, 0u64);
        glue::rjs_error_info(g, out, &mut msg, &mut stack);
        eprintln!("[redjs] storage.js failed: {}", read_string(g, msg).unwrap_or_default());
        return 1;
    }

    // The two objects JS now holds are the Proxies, not the natives. Register
    // them as legitimate receivers (see `proxies`) and protect them: from here
    // on they are what `localStorage` means.
    let global_proxy = state(g).global_proxy;
    for (name, local) in [("localStorage", true), ("sessionStorage", false)] {
        let p = get(g, global_proxy, name);
        if !is_cell(p) {
            eprintln!("[redjs] storage.js did not publish {name}");
            return 1;
        }
        crate::ffi::redjs_protect(g, p);
        proxies().lock().unwrap().insert((g as usize, p), local);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_quota_is_counted_in_utf16_bytes_like_a_browser() {
        // The number an app's "am I near the limit" check is written against.
        // ASCII is 2 bytes/char here, not 1 — a browser charges the same.
        assert_eq!(utf16_bytes("abc"), 6);
        // Astral characters are surrogate PAIRS: 2 code units, 4 bytes.
        assert_eq!(utf16_bytes("\u{1F600}"), 4);
        assert_eq!(utf16_bytes(""), 0);
    }

    #[test]
    fn an_areas_size_is_keys_plus_values() {
        let mut a = Area::default();
        a.entries.push(("ab".into(), "cd".into()));
        a.entries.push(("e".into(), "".into()));
        assert_eq!(a.size(), 4 + 4 + 2);
    }

    #[test]
    fn lookup_is_by_exact_key_and_order_is_insertion_order() {
        // `key(i)` and enumeration both depend on this, and a map would give
        // neither a stable nor a browser-like order.
        let mut a = Area::default();
        for k in ["z", "a", "m"] {
            a.entries.push((k.into(), k.into()));
        }
        assert_eq!(a.index_of("z"), Some(0));
        assert_eq!(a.index_of("a"), Some(1));
        assert_eq!(a.index_of("m"), Some(2));
        assert_eq!(a.index_of("Z"), None, "keys are case-sensitive");
        assert_eq!(a.index_of(""), None);
    }

    /// A host that records what it was asked to persist — the shape every
    /// backend test uses.
    struct RecordingHost {
        seed: Vec<(String, String)>,
        writes: Mutex<Vec<Vec<(String, String)>>>,
        quota: usize,
    }

    impl StorageHost for RecordingHost {
        fn load(&self) -> Vec<(String, String)> {
            self.seed.clone()
        }
        fn store(&self, entries: &[(String, String)]) {
            self.writes.lock().unwrap().push(entries.to_vec());
        }
        fn quota_bytes(&self) -> usize {
            self.quota
        }
    }

    #[test]
    fn the_default_quota_is_the_browsers_five_mebibytes() {
        // Pinned: an app that fits in a browser must fit here, and this is the
        // number that decides it.
        struct Bare;
        impl StorageHost for Bare {
            fn load(&self) -> Vec<(String, String)> {
                Vec::new()
            }
            fn store(&self, _: &[(String, String)]) {}
        }
        assert_eq!(Bare.quota_bytes(), 5 * 1024 * 1024);
        assert_eq!(DEFAULT_QUOTA_BYTES, 5_242_880);
    }

    #[test]
    fn a_host_describes_itself_even_without_overriding() {
        struct Bare;
        impl StorageHost for Bare {
            fn load(&self) -> Vec<(String, String)> {
                Vec::new()
            }
            fn store(&self, _: &[(String, String)]) {}
        }
        assert!(!Bare.describe().is_empty(), "an advisory has to be able to name it");
    }

    #[test]
    fn store_receives_the_whole_area_not_a_delta() {
        // The invariant the atomic-rename discipline depends on: a host can
        // always write the file it was just handed, with no read-modify-write.
        let h = RecordingHost {
            seed: vec![("a".into(), "1".into())],
            writes: Mutex::new(Vec::new()),
            quota: 1024,
        };
        let mut area = Area { entries: h.load(), loaded: true };
        area.entries.push(("b".into(), "2".into()));
        h.store(&area.entries);
        let writes = h.writes.lock().unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0], vec![("a".into(), "1".into()), ("b".into(), "2".into())]);
    }
}

#[cfg(test)]
mod jsc_tests {
    use super::*;
    use crate::api::Context;
    use crate::webapi::testing::*;

    /// A host whose area lives in a `Mutex<Vec<..>>` — a file, without a disk.
    struct MemoryHost;
    static SEED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
    static WRITES: Mutex<u32> = Mutex::new(0);

    impl StorageHost for MemoryHost {
        fn load(&self) -> Vec<(String, String)> {
            SEED.lock().unwrap().clone()
        }
        fn store(&self, entries: &[(String, String)]) {
            *SEED.lock().unwrap() = entries.to_vec();
            *WRITES.lock().unwrap() += 1;
        }
        fn quota_bytes(&self) -> usize {
            // Small on purpose: the quota path has to be reachable from a test
            // without allocating five megabytes of JS string.
            256
        }
    }

    /// `localStorage` is process-global by design (module header: one origin,
    /// one area), and so is the host — so these tests are one at a time.
    /// Poison-tolerant, so one failure does not turn the rest into lock
    /// panics that hide it.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static L: Mutex<()> = Mutex::new(());
        L.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn fresh() -> (Context, std::sync::MutexGuard<'static, ()>) {
        let guard = serial();
        static H: MemoryHost = MemoryHost;
        set_host(&H);
        SEED.lock().unwrap().clear();
        *WRITES.lock().unwrap() = 0;
        reset_local_for_tests();
        (ctx(), guard)
    }

    #[test]
    fn both_areas_are_storage_instances_on_the_global() {
        let (c, _g) = fresh();
        ok(&c, "localStorage instanceof Storage");
        ok(&c, "sessionStorage instanceof Storage");
        ok(&c, "localStorage !== sessionStorage");
        ok(&c, "Object.prototype.toString.call(localStorage) === '[object Storage]'");
        // Same object every time — a browser's `localStorage` is one object.
        ok(&c, "localStorage === localStorage");
        assert!(throws(&c, "new Storage()").contains("Illegal constructor"));
    }

    #[test]
    fn the_six_members_behave_like_a_browsers() {
        let (c, _g) = fresh();
        assert_eq!(s(&c, "localStorage.length"), "0");
        assert_eq!(s(&c, "localStorage.getItem('nope')"), "null");
        assert_eq!(s(&c, "localStorage.key(0)"), "null");
        s(&c, "localStorage.setItem('a', '1'); localStorage.setItem('b', '2')");
        assert_eq!(s(&c, "localStorage.length"), "2");
        assert_eq!(s(&c, "localStorage.getItem('a')"), "1");
        assert_eq!(s(&c, "localStorage.key(0) + ',' + localStorage.key(1)"), "a,b");
        assert_eq!(s(&c, "localStorage.key(2)"), "null");
        s(&c, "localStorage.removeItem('a')");
        assert_eq!(s(&c, "localStorage.getItem('a')"), "null");
        assert_eq!(s(&c, "localStorage.length"), "1");
        // Removing something that is not there is not an error.
        s(&c, "localStorage.removeItem('a')");
        s(&c, "localStorage.clear()");
        assert_eq!(s(&c, "localStorage.length"), "0");
    }

    #[test]
    fn everything_is_coerced_to_a_string_on_the_way_in() {
        // The single most common source of "why is my object [object Object]"
        // — and the behaviour an app written against a browser depends on.
        let (c, _g) = fresh();
        s(&c, "localStorage.setItem('n', 42)");
        s(&c, "localStorage.setItem('o', { a: 1 })");
        s(&c, "localStorage.setItem('u')");
        s(&c, "localStorage.setItem(7, 'seven')");
        assert_eq!(s(&c, "typeof localStorage.getItem('n')"), "string");
        assert_eq!(s(&c, "localStorage.getItem('n')"), "42");
        assert_eq!(s(&c, "localStorage.getItem('o')"), "[object Object]");
        assert_eq!(s(&c, "localStorage.getItem('u')"), "undefined");
        assert_eq!(s(&c, "localStorage.getItem('7')"), "seven");
    }

    #[test]
    fn named_properties_are_the_same_store_as_the_methods() {
        let (c, _g) = fresh();
        s(&c, "localStorage.token = 'abc'");
        assert_eq!(s(&c, "localStorage.getItem('token')"), "abc");
        s(&c, "localStorage.setItem('other', 'x')");
        assert_eq!(s(&c, "localStorage.other"), "x");
        ok(&c, "'token' in localStorage");
        ok(&c, "!('missing' in localStorage)");
        assert_eq!(s(&c, "localStorage.missing"), "undefined");
        assert_eq!(s(&c, "Object.keys(localStorage).join(',')"), "token,other");
        assert_eq!(s(&c, "JSON.stringify(localStorage)"), r#"{"token":"abc","other":"x"}"#);
        s(&c, "delete localStorage.token");
        assert_eq!(s(&c, "localStorage.length"), "1");
        ok(&c, "!('token' in localStorage)");
    }

    #[test]
    fn a_builtin_name_is_never_shadowed_by_a_stored_key() {
        // WebIDL: Storage has no [LegacyOverrideBuiltIns], so a key called
        // "getItem" is reachable only through getItem(). Getting this backwards
        // would break the API the moment an app stored such a key.
        let (c, _g) = fresh();
        s(&c, "localStorage.setItem('getItem', 'not a function')");
        assert_eq!(s(&c, "typeof localStorage.getItem"), "function");
        assert_eq!(s(&c, "localStorage.getItem('getItem')"), "not a function");
        s(&c, "localStorage.setItem('length', 'nine')");
        assert_eq!(s(&c, "localStorage.length"), "2");
        assert_eq!(s(&c, "localStorage.getItem('length')"), "nine");
    }

    #[test]
    fn session_storage_is_separate_and_never_reaches_the_host() {
        let (c, _g) = fresh();
        s(&c, "localStorage.setItem('k', 'local')");
        s(&c, "sessionStorage.setItem('k', 'session')");
        assert_eq!(s(&c, "localStorage.getItem('k')"), "local");
        assert_eq!(s(&c, "sessionStorage.getItem('k')"), "session");
        // The host saw the localStorage write and only that one.
        assert_eq!(SEED.lock().unwrap().clone(), vec![("k".to_string(), "local".to_string())]);
    }

    #[test]
    fn a_write_reaches_the_host_and_a_no_op_write_does_not() {
        // Rewriting a file because somebody re-set the value it already holds
        // is the difference between an idle app and one that touches the disk
        // on every frame.
        let (c, _g) = fresh();
        s(&c, "localStorage.setItem('a', '1')");
        assert_eq!(*WRITES.lock().unwrap(), 1);
        s(&c, "localStorage.setItem('a', '1')");
        assert_eq!(*WRITES.lock().unwrap(), 1, "an identical write is not a write");
        s(&c, "localStorage.setItem('a', '2')");
        assert_eq!(*WRITES.lock().unwrap(), 2);
        s(&c, "localStorage.removeItem('zzz')");
        assert_eq!(*WRITES.lock().unwrap(), 2, "removing nothing is not a write");
        s(&c, "localStorage.clear()");
        assert_eq!(*WRITES.lock().unwrap(), 3);
        s(&c, "localStorage.clear()");
        assert_eq!(*WRITES.lock().unwrap(), 3, "clearing an empty area is not a write");
    }

    #[test]
    fn the_area_is_loaded_from_the_host_exactly_once_and_lazily() {
        // Persistence, seen from the JS side: whatever the host has is what
        // `localStorage` starts with.
        let (c, _g) = fresh();
        *SEED.lock().unwrap() = vec![("restored".into(), "yes".into())];
        assert_eq!(s(&c, "localStorage.getItem('restored')"), "yes");
        assert_eq!(s(&c, "localStorage.length"), "1");
    }

    #[test]
    fn past_the_quota_setitem_throws_a_quota_exceeded_error_and_stores_nothing() {
        let (c, _g) = fresh();
        let msg = throws(&c, "localStorage.setItem('big', 'x'.repeat(1000))");
        assert!(msg.contains("exceeded the quota"), "{msg}");
        assert_eq!(s(&c, "localStorage.length"), "0", "a refused write stores nothing");
        // The check every real catch block does.
        assert_eq!(
            s(
                &c,
                "(() => { try { localStorage.setItem('b', 'x'.repeat(1000)); return 'no throw'; } \
                  catch (e) { return e.name + '/' + e.code + '/' + (e instanceof QuotaExceededError) \
                    + '/' + (e instanceof DOMException) + '/' + e.quota; } })()"
            ),
            "QuotaExceededError/22/true/true/null"
        );
        // And what fits still fits.
        s(&c, "localStorage.setItem('small', 'ok')");
        assert_eq!(s(&c, "localStorage.getItem('small')"), "ok");
    }

    #[test]
    fn storage_event_is_constructible_and_dispatchable_even_though_we_never_fire_it() {
        // The escape hatch named in the module header: an app that wants
        // same-process notification builds the event itself.
        let (c, _g) = fresh();
        assert_eq!(
            s(
                &c,
                "let seen = null;
                 addEventListener('storage', (e) => { seen = e.key + '=' + e.newValue + '|' + \
                   e.oldValue + '|' + e.url; });
                 dispatchEvent(new StorageEvent('storage', { key: 'k', newValue: 'new', \
                   oldValue: 'old' }));
                 seen"
            ),
            "k=new|old|"
        );
        ok(&c, "new StorageEvent('storage') instanceof Event");
        ok(&c, "new StorageEvent('storage').key === null");
        ok(&c, "new StorageEvent('storage').storageArea === null");
        ok(&c, "new StorageEvent('storage', { storageArea: localStorage }).storageArea === localStorage");
        assert!(throws(&c, "new StorageEvent()").contains("requires a type"));
    }

    #[test]
    fn the_members_refuse_a_foreign_this() {
        let (c, _g) = fresh();
        let msg = throws(&c, "Storage.prototype.getItem.call({}, 'k')");
        assert!(msg.contains("not a Storage"), "{msg}");
        let msg = throws(
            &c,
            "Object.getOwnPropertyDescriptor(Storage.prototype, 'length').get.call({})",
        );
        assert!(msg.contains("not a Storage"), "{msg}");
    }
}
