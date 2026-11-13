//! `navigator` — a real `Navigator` (HTML §8.9.1): the NavigatorID members
//! and `hardwareConcurrency`. `navigator.permissions` / `.clipboard` /
//! `.mediaDevices` hang off this same object (their modules install after
//! this one and attach to whatever `navigator` is there).
//!
//! `userAgent` is [`DEFAULT_USER_AGENT`] unless the embedder said otherwise
//! ([`set_user_agent`] / `redjs_navigator_set_user_agent`). It is read at
//! call time and is process-wide — every context, a worker's included, tells
//! the same story — and `appVersion` and `platform` derive from it, so an
//! override can never leave the three disagreeing.
//!
//! The remaining NavigatorID members are the constants HTML freezes them to
//! for web compatibility (`appCodeName` "Mozilla", `appName` "Netscape",
//! `product` "Gecko", `productSub` "20030107"); `vendor` / `vendorSub` are
//! empty, as HTML allows for an engine that is not one of the three.
//!
//! `language` / `languages` (NavigatorLanguage) are the OS's display languages
//! ([`system_languages`]) unless the embedder calls [`set_languages`] /
//! `redjs_navigator_set_languages`.
//!
//! # `onLine`, and the `online` / `offline` events (HTML's NavigatorOnLine)
//!
//! The value is "the OS believes there is a network", not reachability —
//! Chromium's meaning: on Windows its `NetworkChangeNotifier` reports
//! offline only when the OS says there is no connection at all, so a LAN
//! without internet, a captive portal and a VPN are all `true`.
//!
//! The answer comes from the embedder, which owns the OS: it calls
//! [`set_on_line`] (or `redjs_navigator_set_on_line`) from any thread, once at
//! boot and again on every OS change notification. The state is process-wide,
//! like the user agent. Each change is fanned out to every context's queue
//! (keyed by [`crate::context::ContextId`], so it can never land in a context
//! that merely reused a dead one's address) and the context is woken
//! ([`crate::web::wake`]). A [`crate::web::PumpSource`] then delivers it on the
//! context's own thread: a trusted `Event("online")` / `Event("offline")` on
//! the global, one task each, with a microtask checkpoint after it — HTML's
//! "queue a global task on the networking task source".
//!
//! With no provider (redjs's own tests, an embedder that never says), `onLine`
//! is `true` — what Chromium answers when it cannot tell — and the first read
//! says once on stderr that nobody is providing it. No event ever fires then.
//!
//! A context that has not pumped through several changes gets the net effect
//! with at most one round trip: offline → online → offline arrives as
//! `offline`, offline → online arrives as `offline`, `online`. `onLine` itself
//! is always the latest state, as in Chromium, so a listener reading it while
//! an older event is being delivered sees where things ended up.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

use super::events;
use super::util::*;
use crate::glue;

/// What `navigator.userAgent` says until the embedder sets something else.
/// Use the host OS so navigator.platform, keyboard shortcuts and HTTP requests
/// do not claim Windows on every supported desktop. The Mac tokens follow the
/// browser compatibility convention, including on Apple Silicon.
#[cfg(target_os = "windows")]
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) RedVue/1.0";
#[cfg(target_os = "macos")]
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) RedVue/1.0";
#[cfg(target_os = "linux")]
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux) RedVue/1.0";
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 RedVue/1.0";

static USER_AGENT: RwLock<Option<String>> = RwLock::new(None);

/// Overrides `navigator.userAgent` for every context in the process, from
/// the next read on. `None` restores [`DEFAULT_USER_AGENT`].
pub fn set_user_agent(ua: Option<&str>) {
    *USER_AGENT.write().unwrap() = ua.map(str::to_owned);
}

/// The user agent string `navigator.userAgent` reports right now.
pub fn user_agent() -> String {
    USER_AGENT.read().unwrap().clone().unwrap_or_else(|| DEFAULT_USER_AGENT.to_owned())
}

/// HTML: `appVersion` is the user agent string minus its leading "Mozilla/".
pub fn app_version(ua: &str) -> &str {
    ua.strip_prefix("Mozilla/").unwrap_or(ua)
}

/// `platform`, consistent with the user agent: the values browsers report on
/// each desktop OS, or "" (allowed) for anything else.
pub fn platform(ua: &str) -> &'static str {
    if ua.contains("Windows") {
        "Win32"
    } else if ua.contains("Macintosh") || ua.contains("Mac OS X") {
        "MacIntel"
    } else if ua.contains("Linux") || ua.contains("X11") {
        "Linux x86_64"
    } else {
        ""
    }
}

pub fn hardware_concurrency() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

// ---------------------------------------------------------------------------
// NavigatorLanguage
// ---------------------------------------------------------------------------

static LANGUAGES: RwLock<Option<Vec<String>>> = RwLock::new(None);
/// Bumped by [`set_languages`]: a context whose cached `languages` array is older builds a new one.
static LANGUAGES_EPOCH: AtomicUsize = AtomicUsize::new(0);

/// Overrides `navigator.languages` (and `language`, its first entry) for every context, from the
/// next read on. Entries that are not language tags are dropped; `None`, or nothing left, restores
/// [`system_languages`]. No `languagechange` event fires.
pub fn set_languages(langs: Option<Vec<String>>) {
    let langs = langs.map(|l| dedup_tags(l.iter().map(String::as_str))).filter(|l| !l.is_empty());
    *LANGUAGES.write().unwrap() = langs;
    LANGUAGES_EPOCH.fetch_add(1, Ordering::AcqRel);
}

/// What `navigator.languages` reports right now: never empty, most preferred first.
pub fn languages() -> Vec<String> {
    if let Some(l) = LANGUAGES.read().unwrap().as_ref() {
        return l.clone();
    }
    system_languages().to_vec()
}

/// The user's display languages as BCP 47 tags, most preferred first, read once per process;
/// `["en-US"]` when the OS says nothing usable.
pub fn system_languages() -> &'static [String] {
    static SYSTEM: OnceLock<Vec<String>> = OnceLock::new();
    SYSTEM.get_or_init(|| {
        let tags = dedup_tags(os_languages().iter().map(String::as_str));
        if tags.is_empty() { vec!["en-US".to_owned()] } else { tags }
    })
}

fn dedup_tags<'a>(raw: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tag in raw.filter_map(canonical_tag) {
        if !out.iter().any(|t| t.eq_ignore_ascii_case(&tag)) {
            out.push(tag);
        }
    }
    out
}

/// A BCP 47 tag from an OS spelling (`pt_BR.UTF-8@euro`, `zh-hans-cn`), cased as browsers report
/// it (`pt-BR`, `zh-Hans-CN`); `None` for `C`, `POSIX` and anything that is not a tag.
pub fn canonical_tag(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let raw = raw.split(['.', '@']).next().unwrap_or("");
    if raw.is_empty() || raw.eq_ignore_ascii_case("c") || raw.eq_ignore_ascii_case("posix") {
        return None;
    }
    let mut parts = Vec::new();
    for (i, sub) in raw.split(['-', '_']).enumerate() {
        if sub.is_empty() || sub.len() > 8 || !sub.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        let part = if i == 0 {
            if !(2..=3).contains(&sub.len()) || !sub.bytes().all(|b| b.is_ascii_alphabetic()) {
                return None;
            }
            sub.to_ascii_lowercase()
        } else if sub.len() == 4 && sub.bytes().all(|b| b.is_ascii_alphabetic()) {
            let lower = sub.to_ascii_lowercase();
            lower[..1].to_ascii_uppercase() + &lower[1..]
        } else if sub.len() == 2 && sub.bytes().all(|b| b.is_ascii_alphabetic()) {
            sub.to_ascii_uppercase()
        } else {
            sub.to_ascii_lowercase()
        };
        parts.push(part);
    }
    Some(parts.join("-"))
}

#[cfg(target_os = "windows")]
fn os_languages() -> Vec<String> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetUserPreferredUILanguages(flags: u32, num: *mut u32, buffer: *mut u16, len: *mut u32) -> i32;
    }
    const MUI_LANGUAGE_NAME: u32 = 0x8;
    let (mut num, mut len) = (0u32, 0u32);
    // SAFETY: the first call only reports the length; the second writes at most `len` units.
    unsafe {
        if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut num, core::ptr::null_mut(), &mut len) == 0 || len == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u16; len as usize];
        if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut num, buf.as_mut_ptr(), &mut len) == 0 {
            return Vec::new();
        }
        buf.split(|&u| u == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
    }
}

/// An app started from Finder has no `LANG`: the list is the user's `AppleLanguages` default.
#[cfg(target_os = "macos")]
fn os_languages() -> Vec<String> {
    let out = std::process::Command::new("/usr/bin/defaults").args(["read", "-g", "AppleLanguages"]).output();
    match out {
        Ok(o) if o.status.success() => apple_languages(&String::from_utf8_lossy(&o.stdout)),
        _ => posix_languages(|k| std::env::var(k).ok()),
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn os_languages() -> Vec<String> {
    posix_languages(|k| std::env::var(k).ok())
}

/// `defaults read`'s old-style plist array: `(\n    "en-US",\n    "ru-RU"\n)`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn apple_languages(out: &str) -> Vec<String> {
    out.lines()
        .map(|l| l.trim().trim_end_matches(',').trim_matches('"'))
        .filter(|l| !l.is_empty() && *l != "(" && *l != ")")
        .map(str::to_owned)
        .collect()
}

/// gettext's order: `LANGUAGE` (a colon list) unless the locale is `C`, then the first of `LC_ALL`,
/// `LC_MESSAGES`, `LANG`.
#[cfg_attr(target_os = "windows", allow(dead_code))]
fn posix_languages(var: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"].into_iter().find_map(|k| var(k).filter(|v| !v.is_empty()));
    let mut out = Vec::new();
    if locale.as_deref().and_then(canonical_tag).is_some() {
        if let Some(list) = var("LANGUAGE") {
            out.extend(list.split(':').filter(|s| !s.is_empty()).map(str::to_owned));
        }
    }
    out.extend(locale);
    out
}

// ---------------------------------------------------------------------------
// NavigatorOnLine — the state, the fan-out, the delivery (module header)
// ---------------------------------------------------------------------------

const NO_PROVIDER: u8 = 0;
const OFFLINE: u8 = 1;
const ONLINE: u8 = 2;

/// What the embedder last said: [`NO_PROVIDER`], [`OFFLINE`] or [`ONLINE`].
/// Written only under [`online_queues`]' lock, so a change and a context's
/// first queue cannot interleave.
static ON_LINE: AtomicU8 = AtomicU8::new(NO_PROVIDER);

/// Events not yet delivered, summed over every context: with none anywhere,
/// the pump source costs one atomic load.
static ONLINE_PENDING: AtomicUsize = AtomicUsize::new(0);

/// Set once the no-provider advisory has been said (kept for the test).
static ADVISED_NO_PROVIDER: AtomicBool = AtomicBool::new(false);

fn effective(state: u8) -> bool {
    state != OFFLINE
}

/// Per context: the `onLine` values whose events are still to be delivered,
/// oldest first. They alternate, and the last one is the current state, so
/// a context whose queue is empty has seen everything.
fn online_queues() -> &'static Mutex<HashMap<u64, Vec<bool>>> {
    static Q: OnceLock<Mutex<HashMap<u64, Vec<bool>>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Sets what `navigator.onLine` reports, for every context in the process,
/// from any thread. `Some(true)` / `Some(false)` is the OS's answer; `None`
/// means "no provider" again, which reads as `true`.
///
/// When the value a page would read changes, every context gets an
/// `online` / `offline` event at its next pump. Saying the same thing twice
/// is not a change, so a provider may simply forward every OS notification.
pub fn set_on_line(state: Option<bool>) {
    let new = match state {
        None => NO_PROVIDER,
        Some(false) => OFFLINE,
        Some(true) => ONLINE,
    };
    let mut woken = Vec::new();
    {
        let mut queues = online_queues().lock().unwrap();
        let old = ON_LINE.swap(new, Ordering::AcqRel);
        let now = effective(new);
        if effective(old) == now {
            return;
        }
        for (&id, q) in queues.iter_mut() {
            q.push(now);
            ONLINE_PENDING.fetch_add(1, Ordering::AcqRel);
            if q.len() > 2 {
                // [a, !a, a]: the round trip in the middle collapses.
                q.drain(1..);
                ONLINE_PENDING.fetch_sub(2, Ordering::AcqRel);
            }
            woken.push(id);
        }
    }
    for id in woken {
        crate::web::wake(crate::context::ContextId(id));
    }
}

/// What the embedder last said (`None`: nothing, or `set_on_line(None)`).
pub fn on_line_state() -> Option<bool> {
    match ON_LINE.load(Ordering::Acquire) {
        OFFLINE => Some(false),
        ONLINE => Some(true),
        _ => None,
    }
}

/// The value `navigator.onLine` reports right now.
pub fn on_line() -> bool {
    effective(ON_LINE.load(Ordering::Acquire))
}

fn advise_no_provider() {
    if ADVISED_NO_PROVIDER.swap(true, Ordering::AcqRel) {
        return;
    }
    eprintln!(
        "[redjs] navigator.onLine: no network-state provider is installed, so it reads true \
         (what Chromium reports when it cannot tell) and online/offline never fire. An embedder \
         calls redjs::webapi::navigator::set_on_line() (or redjs_navigator_set_on_line) from its \
         OS network-change notifier."
    );
}

unsafe fn online_queue_len(g: *mut c_void) -> usize {
    let Some(id) = crate::context::id_of(g) else { return 0 };
    online_queues().lock().unwrap().get(&id.0).map(Vec::len).unwrap_or(0)
}

/// Never in flight: a change is queued whole, so there is nothing to wait for.
unsafe fn online_inflight(_g: *mut c_void) -> bool {
    false
}

unsafe fn online_ready(g: *mut c_void) -> bool {
    ONLINE_PENDING.load(Ordering::Acquire) != 0 && online_queue_len(g) != 0
}

/// Delivers this context's queued events, each one task with a checkpoint
/// after it (and one before the first: microtasks left by earlier tasks run
/// first, HTML's order).
unsafe fn online_drain(g: *mut c_void) -> usize {
    if ONLINE_PENDING.load(Ordering::Acquire) == 0 {
        return 0;
    }
    let Some(id) = crate::context::id_of(g) else { return 0 };
    let mut n = 0usize;
    loop {
        let next = {
            let mut queues = online_queues().lock().unwrap();
            match queues.get_mut(&id.0) {
                Some(q) if !q.is_empty() => {
                    ONLINE_PENDING.fetch_sub(1, Ordering::AcqRel);
                    q.remove(0)
                }
                _ => break,
            }
        };
        if n == 0 {
            crate::web::checkpoint(g);
        }
        let ev = events::new_trusted_event(g, if next { "online" } else { "offline" });
        events::dispatch(g, glue::rjs_global(g), ev);
        crate::web::checkpoint(g);
        n += 1;
    }
    n
}

struct NavClasses {
    /// Brand slot: only the real `navigator` answers the getters.
    s_nav: *mut c_void,
    /// The cached `languages` array and the [`LANGUAGES_EPOCH`] it was built at.
    s_languages: *mut c_void,
    s_languages_epoch: *mut c_void,
}

unsafe impl Send for NavClasses {}
unsafe impl Sync for NavClasses {}

fn registry() -> &'static Mutex<HashMap<usize, &'static NavClasses>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static NavClasses>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn forget(gone: crate::context::Gone) {
    registry().lock().unwrap().remove(&gone.key);
    if let Some(q) = online_queues().lock().unwrap().remove(&gone.id.0) {
        ONLINE_PENDING.fetch_sub(q.len(), Ordering::AcqRel);
    }
}

unsafe fn is_navigator(g: *mut c_void, v: u64) -> bool {
    let Some(c) = registry().lock().unwrap().get(&(g as usize)).copied() else { return false };
    is_cell(v) && get_a(g, v, c.s_nav) == TRUE
}

macro_rules! nav_getter {
    ($name:ident, |$ua:ident| $body:expr) => {
        #[crate::host_fn]
        unsafe fn $name(g: *mut c_void, cf: *mut c_void) -> u64 {
            if !is_navigator(g, cf_this(cf)) {
                return throw_type_error(g, "Illegal invocation");
            }
            let $ua = user_agent();
            let s: &str = $body;
            js_string(g, s)
        }
    };
}

nav_getter!(get_user_agent, |ua| &ua);
nav_getter!(get_app_version, |ua| app_version(&ua));
nav_getter!(get_platform, |ua| platform(&ua));
nav_getter!(get_app_code_name, |_ua| "Mozilla");
nav_getter!(get_app_name, |_ua| "Netscape");
nav_getter!(get_product, |_ua| "Gecko");
nav_getter!(get_product_sub, |_ua| "20030107");
nav_getter!(get_vendor, |_ua| "");
nav_getter!(get_vendor_sub, |_ua| "");

#[crate::host_fn]
unsafe fn get_hardware_concurrency(g: *mut c_void, cf: *mut c_void) -> u64 {
    if !is_navigator(g, cf_this(cf)) {
        return throw_type_error(g, "Illegal invocation");
    }
    enc_i32(hardware_concurrency().min(i32::MAX as usize) as i32)
}

#[crate::host_fn]
unsafe fn get_language(g: *mut c_void, cf: *mut c_void) -> u64 {
    if !is_navigator(g, cf_this(cf)) {
        return throw_type_error(g, "Illegal invocation");
    }
    js_string(g, languages().first().map_or("en-US", String::as_str))
}

/// HTML: the same frozen array until the value changes, so `navigator.languages === navigator.languages`.
#[crate::host_fn]
unsafe fn get_languages(g: *mut c_void, cf: *mut c_void) -> u64 {
    let nav = cf_this(cf);
    if !is_navigator(g, nav) {
        return throw_type_error(g, "Illegal invocation");
    }
    let Some(c) = registry().lock().unwrap().get(&(g as usize)).copied() else { return UNDEF };
    let (s_array, s_epoch) = (c.s_languages, c.s_languages_epoch);
    let epoch = enc_i32((LANGUAGES_EPOCH.load(Ordering::Acquire) & 0x7fff_ffff) as i32);
    let cached = get_a(g, nav, s_array);
    if is_cell(cached) && get_a(g, nav, s_epoch) == epoch {
        return cached;
    }
    let arr = new_array(g);
    for (i, tag) in languages().iter().enumerate() {
        set_i(g, arr, i as u32, js_string(g, tag));
    }
    let object = get(g, glue::rjs_global(g), "Object");
    let _ = call(g, get(g, object, "freeze"), object, &[arr]);
    glue::rjs_define(g, nav, s_array, arr, 2);
    glue::rjs_define(g, nav, s_epoch, epoch, 2);
    arr
}

#[crate::host_fn]
unsafe fn get_on_line(g: *mut c_void, cf: *mut c_void) -> u64 {
    if !is_navigator(g, cf_this(cf)) {
        return throw_type_error(g, "Illegal invocation");
    }
    let state = ON_LINE.load(Ordering::Acquire);
    if state == NO_PROVIDER {
        advise_no_provider();
    }
    enc_bool(effective(state))
}

pub unsafe fn install(g: *mut c_void) {
    let s_nav = atom_priv(g, "redjs#navigator");
    registry().lock().unwrap().insert(g as usize, Box::leak(Box::new(NavClasses {
        s_nav,
        s_languages: atom_priv(g, "redjs#languages"),
        s_languages_epoch: atom_priv(g, "redjs#languagesEpoch"),
    })));

    let class = define_class(g, &ClassSpec {
        name: "Navigator",
        arity: 0,
        construct: None, // "Illegal constructor", as in a browser
        parent_proto: 0,
        methods: &[
            #[cfg(feature = "battery")]
            ("getBattery", 0, super::battery::get_battery),
        ],
        getters: &[
            ("appCodeName", get_app_code_name),
            ("appName", get_app_name),
            ("appVersion", get_app_version),
            ("platform", get_platform),
            ("product", get_product),
            ("productSub", get_product_sub),
            ("userAgent", get_user_agent),
            ("vendor", get_vendor),
            ("vendorSub", get_vendor_sub),
            ("hardwareConcurrency", get_hardware_concurrency),
            ("language", get_language),
            ("languages", get_languages),
            ("onLine", get_on_line),
        ],
        accessors: &[],
    });

    // A host that put its own `navigator` up before install keeps its object
    // (and whatever it hung on it); it just becomes a Navigator underneath.
    let global = glue::rjs_global(g);
    let existing = get(g, global, "navigator");
    let nav = if is_cell(existing) {
        crate::capi::JSObjectSetPrototype(g, existing as usize as crate::capi::JSObjectRef, class.proto as usize as crate::capi::JSValueRef);
        existing
    } else {
        let n = new_object(g, class.proto);
        define(g, global, "navigator", n, 2 /* DontEnum */);
        n
    };
    glue::rjs_define(g, nav, s_nav, TRUE, 1 | 2 | 4);

    // NavigatorOnLine: this context hears every change from now on (an empty
    // queue: it starts at the current state), delivered by the pump source.
    let id = crate::context::ensure(g);
    online_queues().lock().unwrap().entry(id.0).or_default();
    crate::web::register_pump_source(crate::web::PumpSource {
        inflight: online_inflight,
        ready: online_ready,
        drain: online_drain,
    });
    // The window's `ononline` / `onoffline` event handler attributes, null
    // until assigned, so `'ononline' in window` is true as in a browser.
    for name in ["ononline", "onoffline"] {
        if get(g, global, name) == UNDEF {
            define(g, global, name, NULL, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::webapi::testing::{ctx, ok, s};

    #[cfg(not(feature = "battery"))]
    #[test]
    fn battery_feature_off_has_no_surface() {
        let c = ctx();
        ok(&c, "!('getBattery' in navigator) && typeof BatteryManager === 'undefined'");
    }

    #[test]
    fn navigator_is_a_real_navigator() {
        let c = ctx();
        ok(&c, "navigator instanceof Navigator && Object.prototype.toString.call(navigator) === '[object Navigator]'");
        ok(&c, "(() => { try { new Navigator(); return false; } catch (e) { return e instanceof TypeError; } })()");
        ok(&c, "(() => { try { Object.getOwnPropertyDescriptor(Navigator.prototype, 'userAgent').get.call({}); return false; } \
                catch (e) { return e instanceof TypeError; } })()");
        ok(&c, "!Object.hasOwn(navigator, 'userAgent') && typeof Object.getOwnPropertyDescriptor(Navigator.prototype, 'userAgent').get === 'function'");
        assert_eq!(
            s(&c, "[navigator.appCodeName, navigator.appName, navigator.product, navigator.productSub, \
                   JSON.stringify(navigator.vendor), JSON.stringify(navigator.vendorSub)].join('|')"),
            "Mozilla|Netscape|Gecko|20030107|\"\"|\"\""
        );
        ok(&c, "Number.isInteger(navigator.hardwareConcurrency) && navigator.hardwareConcurrency >= 1");
        // The other navigator.* surfaces hang off this same object.
        ok(&c, "typeof navigator.permissions === 'object' && navigator.permissions !== null");
    }

    /// The default, an override, and the reset — in one test, because the
    /// user agent is process-wide and tests run in parallel.
    #[test]
    fn user_agent_default_and_override() {
        let c = ctx();
        assert_eq!(s(&c, "navigator.userAgent"), super::DEFAULT_USER_AGENT);
        assert_eq!(s(&c, "navigator.appVersion"), super::app_version(super::DEFAULT_USER_AGENT));
        assert_eq!(s(&c, "navigator.platform"), super::platform(super::DEFAULT_USER_AGENT));
        #[cfg(target_os = "macos")]
        assert_eq!(s(&c, "navigator.platform"), "MacIntel");
        #[cfg(target_os = "windows")]
        assert_eq!(s(&c, "navigator.platform"), "Win32");

        let ua: Vec<u16> = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) Embedder/2".encode_utf16().collect();
        unsafe { crate::ffi::redjs_navigator_set_user_agent(ua.as_ptr(), ua.len()) };
        let got = s(&c, "[navigator.userAgent, navigator.appVersion, navigator.platform].join('|')");
        unsafe { crate::ffi::redjs_navigator_set_user_agent(core::ptr::null(), 0) };
        assert_eq!(got, "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) Embedder/2|5.0 (Macintosh; Intel Mac OS X 14_0) Embedder/2|MacIntel");
        assert_eq!(s(&c, "navigator.userAgent"), super::DEFAULT_USER_AGENT);
    }

    #[test]
    fn os_spellings_become_bcp47_tags() {
        use super::canonical_tag as t;
        assert_eq!(t("pt_BR.UTF-8@euro").as_deref(), Some("pt-BR"));
        assert_eq!(t("zh-hans-cn").as_deref(), Some("zh-Hans-CN"));
        assert_eq!(t("EN-us").as_deref(), Some("en-US"));
        assert_eq!(t("es-419").as_deref(), Some("es-419"));
        assert_eq!(t("sr_RS@latin").as_deref(), Some("sr-RS"));
        for bad in ["C", "POSIX", "C.UTF-8", "", "e", "1x", "en--US", "english"] {
            assert_eq!(t(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn posix_and_apple_lists_parse() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        assert_eq!(super::posix_languages(env(&[("LANG", "ru_RU.UTF-8")])), ["ru_RU.UTF-8"]);
        assert_eq!(
            super::posix_languages(env(&[("LANGUAGE", "uk:ru"), ("LC_ALL", "uk_UA.UTF-8"), ("LANG", "en_US")])),
            ["uk", "ru", "uk_UA.UTF-8"]
        );
        assert!(super::posix_languages(env(&[("LANGUAGE", "de"), ("LANG", "C")])) == ["C"], "LANGUAGE is ignored under C");
        assert_eq!(super::apple_languages("(\n    \"en-US\",\n    \"ru-RU\"\n)\n"), ["en-US", "ru-RU"]);
    }

    /// The system list, an override, its normalisation and the reset in ONE test: process-wide state.
    #[test]
    fn language_is_the_first_of_a_stable_frozen_languages() {
        let c = ctx();
        assert!(!super::system_languages().is_empty());
        assert_eq!(s(&c, "navigator.languages.join()"), super::system_languages().join(","));
        ok(&c, "navigator.language === navigator.languages[0]");
        assert_eq!(s(&c, "[navigator.languages === navigator.languages, Object.isFrozen(navigator.languages), Array.isArray(navigator.languages)].join()"), "true,true,true");
        ok(&c, "!Object.hasOwn(navigator, 'language') && !Object.hasOwn(navigator, 'languages')");
        s(&c, "globalThis.__before = navigator.languages; 0");

        let list: Vec<u16> = "de_DE.UTF-8,C,de-de,fr".encode_utf16().collect();
        unsafe { crate::ffi::redjs_navigator_set_languages(list.as_ptr(), list.len()) };
        let got = s(&c, "[navigator.language, navigator.languages.join(), navigator.languages !== __before].join('|')");
        super::set_languages(Some(vec!["C".into()]));
        let nothing_valid = s(&c, "navigator.languages.join()");
        unsafe { crate::ffi::redjs_navigator_set_languages(core::ptr::null(), 0) };
        assert_eq!(got, "de-DE|de-DE,fr|true");
        assert_eq!(nothing_valid, super::system_languages().join(","), "no valid tag: the system's");
        assert_eq!(s(&c, "navigator.languages.join()"), super::system_languages().join(","));
    }

    fn pump(c: &crate::api::Context) {
        unsafe {
            crate::web::pump(c.raw(), crate::web::PUMP_TIMERS | crate::web::PUMP_MICROTASKS, core::ptr::null_mut())
        };
    }

    /// Records every `online` / `offline` as `[type, isTrusted, target is
    /// window, navigator.onLine at delivery]`, through both listener shapes.
    fn listen(c: &crate::api::Context) {
        s(
            c,
            "globalThis.__ev = [];
             addEventListener('online', e => __ev.push([e.type, e.isTrusted, e.target === window, navigator.onLine]));
             window.onoffline = e => __ev.push([e.type, e.isTrusted, e.target === window, navigator.onLine]);
             0",
        );
    }

    fn events(c: &crate::api::Context) -> String {
        s(c, "JSON.stringify(__ev.map(e => e[0]))")
    }

    /// The whole NavigatorOnLine story in ONE test, because the state is
    /// process-wide and tests run in parallel: the no-provider default, a
    /// provider's answers, delivery to every context, the collapse of a
    /// context that did not pump, and the reset.
    #[test]
    fn on_line_follows_the_provider_and_fires_in_every_context() {
        use std::sync::atomic::Ordering;
        let a = ctx();
        let b = ctx();
        listen(&a);
        listen(&b);

        // No provider: `true`, Chromium's "cannot tell", said once.
        assert_eq!(super::on_line_state(), None);
        ok(&a, "navigator.onLine === true && 'ononline' in window && window.onoffline !== null && window.ononline === null");
        assert!(super::ADVISED_NO_PROVIDER.load(Ordering::Acquire), "the no-provider read must advise");
        ok(&a, "!Object.hasOwn(navigator, 'onLine') && typeof Object.getOwnPropertyDescriptor(Navigator.prototype, 'onLine').get === 'function'");
        ok(&a, "(() => { try { Object.getOwnPropertyDescriptor(Navigator.prototype, 'onLine').get.call({}); return false; } \
                catch (e) { return e instanceof TypeError; } })()");

        // The provider says offline: the value at once, the event at the pump,
        // in both contexts, trusted, on window, with onLine already false.
        super::set_on_line(Some(false));
        ok(&a, "navigator.onLine === false && __ev.length === 0");
        pump(&a);
        pump(&b);
        for c in [&a, &b] {
            assert_eq!(s(c, "JSON.stringify(__ev)"), r#"[["offline",true,true,false]]"#);
        }
        // Saying it again is not a change; a second pump delivers nothing.
        super::set_on_line(Some(false));
        pump(&a);
        pump(&a);
        assert_eq!(events(&a), r#"["offline"]"#);

        // Back online, through the C ABI this time.
        crate::ffi::redjs_navigator_set_on_line(1);
        pump(&a);
        assert_eq!(s(&a, "JSON.stringify(__ev[1])"), r#"["online",true,true,true]"#);

        // `b` has not pumped since: it is owed `online`. Two more flips while
        // it still does not pump collapse into that one event.
        super::set_on_line(Some(false));
        super::set_on_line(Some(true));
        pump(&b);
        assert_eq!(events(&b), r#"["offline","online"]"#);
        // A single round trip is kept: offline, then online.
        super::set_on_line(Some(false));
        super::set_on_line(Some(true));
        pump(&b);
        assert_eq!(events(&b), r#"["offline","online","offline","online"]"#);

        // A context made while offline starts there, with no event.
        super::set_on_line(Some(false));
        let c = ctx();
        listen(&c);
        pump(&c);
        ok(&c, "navigator.onLine === false && __ev.length === 0");

        // No provider again reads true, which is a change.
        crate::ffi::redjs_navigator_set_on_line(-1);
        assert_eq!(super::on_line_state(), None);
        pump(&c);
        assert_eq!(events(&c), r#"["online"]"#);
        ok(&c, "navigator.onLine === true");
    }
}
