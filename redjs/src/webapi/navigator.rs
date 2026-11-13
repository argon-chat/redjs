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
//! `language` / `languages` are the OS's display languages unless the embedder calls
//! [`set_languages`].
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

static LANGUAGES: RwLock<Option<Vec<String>>> = RwLock::new(None);
static LANGUAGES_EPOCH: AtomicUsize = AtomicUsize::new(0);

/// `None`, or no valid tag, restores [`system_languages`]. No `languagechange` event fires.
pub fn set_languages(langs: Option<Vec<String>>) {
    let langs = langs.map(|l| dedup_tags(l.iter().map(String::as_str))).filter(|l| !l.is_empty());
    *LANGUAGES.write().unwrap() = langs;
    LANGUAGES_EPOCH.fetch_add(1, Ordering::AcqRel);
}

pub fn languages() -> Vec<String> {
    if let Some(l) = LANGUAGES.read().unwrap().as_ref() {
        return l.clone();
    }
    system_languages().to_vec()
}

pub fn accept_language() -> String {
    accept_language_for(&languages())
}

/// Chromium's: `ru-RU,en-US` -> `ru-RU,ru;q=0.9,en-US;q=0.8,en;q=0.7`.
pub fn accept_language_for(langs: &[String]) -> String {
    let base = |t: &str| t.split('-').next().unwrap_or(t).to_ascii_lowercase();
    let mut expanded: Vec<String> = Vec::new();
    for (i, tag) in langs.iter().enumerate() {
        if !expanded.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
            expanded.push(tag.clone());
        }
        let b = base(tag);
        let run_continues = langs.get(i + 1).is_some_and(|next| base(next) == b);
        let named = langs.iter().chain(&expanded).any(|t| t.eq_ignore_ascii_case(&b));
        if !run_continues && !named {
            expanded.push(b);
        }
    }
    let mut out = String::new();
    let mut q10 = 10;
    for tag in &expanded {
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(tag);
        if q10 < 10 {
            out.push_str(&format!(";q=0.{q10}"));
        }
        q10 = (q10 - 1).max(1);
    }
    out
}

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

/// `pt_BR.UTF-8@euro` -> `pt-BR`; `None` for `C`, `POSIX` and non-tags.
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

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn apple_languages(out: &str) -> Vec<String> {
    out.lines()
        .map(|l| l.trim().trim_end_matches(',').trim_matches('"'))
        .filter(|l| !l.is_empty() && *l != "(" && *l != ")")
        .map(str::to_owned)
        .collect()
}

/// gettext's order: `LANGUAGE` unless the locale is `C`, then `LC_ALL`, `LC_MESSAGES`, `LANG`.
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
    s_languages: *mut c_void,
    s_languages_epoch: *mut c_void,
    s_ua_data: *mut c_void,
    s_ua: *mut c_void,
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

/// HTML: the same frozen array until the value changes.
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

const GREASE_BRAND: &str = "Not)A;Brand";

/// `(brand, full version)` of the user agent's last product token: `RedVue/1.0` -> `("RedVue", "1.0")`.
pub fn ua_product(ua: &str) -> (String, String) {
    ua.split_whitespace()
        .rev()
        .find_map(|t| t.split_once('/').filter(|(n, v)| !n.is_empty() && !v.is_empty() && !n.starts_with('(')))
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .unwrap_or_else(|| ("RedVue".into(), "1.0".into()))
}

fn ua_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else {
        ""
    }
}

/// Chromium's Windows `platformVersion`: 13 and up is Windows 11, 1 to 10 Windows 10.
pub fn windows_platform_version(build: u32) -> &'static str {
    match build {
        26100.. => "19.0.0",
        22621.. => "15.0.0",
        22000.. => "14.0.0",
        19041.. => "10.0.0",
        18362.. => "8.0.0",
        17763.. => "7.0.0",
        10240.. => "1.0.0",
        _ => "0.0.0",
    }
}

#[cfg_attr(windows, allow(dead_code))]
fn three_part(v: &str) -> String {
    let mut parts: Vec<&str> = v.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).take(3).collect();
    while parts.len() < 3 {
        parts.push("0");
    }
    parts.join(".")
}

#[cfg(target_os = "windows")]
fn platform_version() -> String {
    #[repr(C)]
    struct OsVersionInfo {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        csd: [u16; 128],
    }
    #[link(name = "ntdll")]
    extern "system" {
        fn RtlGetVersion(info: *mut OsVersionInfo) -> i32;
    }
    let mut info = OsVersionInfo {
        size: core::mem::size_of::<OsVersionInfo>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        csd: [0; 128],
    };
    // SAFETY: `info` is a correctly sized OSVERSIONINFOW.
    let build = if unsafe { RtlGetVersion(&mut info) } == 0 { info.build } else { 0 };
    windows_platform_version(build).to_string()
}

#[cfg(target_os = "macos")]
fn platform_version() -> String {
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: `buf` holds `len` bytes; sysctlbyname writes at most that.
    let rc = unsafe {
        libc::sysctlbyname(c"kern.osproductversion".as_ptr(), buf.as_mut_ptr().cast(), &mut len, core::ptr::null_mut(), 0)
    };
    if rc != 0 {
        return String::new();
    }
    three_part(&String::from_utf8_lossy(&buf[..len.saturating_sub(1)]))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_version() -> String {
    // SAFETY: `uname` fills a zeroed utsname; `release` is NUL-terminated.
    unsafe {
        let mut u: libc::utsname = core::mem::zeroed();
        if libc::uname(&mut u) != 0 {
            return String::new();
        }
        three_part(&std::ffi::CStr::from_ptr(u.release.as_ptr()).to_string_lossy())
    }
}

#[cfg(not(any(windows, unix)))]
fn platform_version() -> String {
    String::new()
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn brands_json(full: bool) -> String {
    let (name, version) = ua_product(&user_agent());
    let (v, grease) = if full {
        (version.clone(), "99.0.0.0")
    } else {
        (version.split('.').next().unwrap_or("").to_string(), "99")
    };
    format!(
        "[{{\"brand\":{},\"version\":{}}},{{\"brand\":{},\"version\":{}}}]",
        json_str(GREASE_BRAND),
        json_str(grease),
        json_str(&name),
        json_str(&v)
    )
}

/// The `getHighEntropyValues` answer for `hints`: the low-entropy three, plus what was asked for.
pub fn high_entropy_json(hints: &[String]) -> String {
    let mut fields = vec![
        format!("\"brands\":{}", brands_json(false)),
        "\"mobile\":false".to_string(),
        format!("\"platform\":{}", json_str(ua_platform())),
    ];
    let arch = if cfg!(any(target_arch = "aarch64", target_arch = "arm")) { "arm" } else { "x86" };
    let bitness = if cfg!(target_pointer_width = "64") { "64" } else { "32" };
    for hint in hints {
        let value = match hint.as_str() {
            "architecture" => json_str(arch),
            "bitness" => json_str(bitness),
            "formFactors" => "[\"Desktop\"]".to_string(),
            "fullVersionList" => brands_json(true),
            "model" => json_str(""),
            "platformVersion" => json_str(&platform_version()),
            "uaFullVersion" => json_str(&ua_product(&user_agent()).1),
            "wow64" => "false".to_string(),
            _ => continue,
        };
        let field = format!("{}:{value}", json_str(hint));
        if !fields.contains(&field) {
            fields.push(field);
        }
    }
    format!("{{{}}}", fields.join(","))
}

unsafe fn parse_json(g: *mut c_void, json: &str) -> u64 {
    call(g, state(g).json_parse, UNDEF, &[js_string(g, json)]).unwrap_or(UNDEF)
}

unsafe fn is_ua_data(g: *mut c_void, v: u64) -> bool {
    let Some(c) = registry().lock().unwrap().get(&(g as usize)).copied() else { return false };
    is_cell(v) && get_a(g, v, c.s_ua) == TRUE
}

macro_rules! ua_check {
    ($g:expr, $cf:expr) => {
        if !is_ua_data($g, cf_this($cf)) {
            return throw_type_error($g, "Illegal invocation");
        }
    };
}

#[crate::host_fn]
unsafe fn ua_brands(g: *mut c_void, cf: *mut c_void) -> u64 {
    ua_check!(g, cf);
    let arr = parse_json(g, &brands_json(false));
    let object = get(g, glue::rjs_global(g), "Object");
    let _ = call(g, get(g, object, "freeze"), object, &[arr]);
    arr
}

#[crate::host_fn]
unsafe fn ua_mobile(g: *mut c_void, cf: *mut c_void) -> u64 {
    ua_check!(g, cf);
    enc_bool(false)
}

#[crate::host_fn]
unsafe fn ua_platform_get(g: *mut c_void, cf: *mut c_void) -> u64 {
    ua_check!(g, cf);
    js_string(g, ua_platform())
}

#[crate::host_fn]
unsafe fn ua_to_json(g: *mut c_void, cf: *mut c_void) -> u64 {
    ua_check!(g, cf);
    parse_json(g, &high_entropy_json(&[]))
}

#[crate::host_fn]
unsafe fn ua_high_entropy(g: *mut c_void, cf: *mut c_void) -> u64 {
    ua_check!(g, cf);
    let array = get(g, glue::rjs_global(g), "Array");
    let list = match call(g, get(g, array, "from"), array, &[cf_arg(cf, 0)]) {
        Ok(l) => l,
        Err(e) => {
            glue::rjs_throw(g, e);
            return UNDEF;
        }
    };
    let mut hints = Vec::new();
    for i in 0..array_len(g, list) {
        let Ok(s) = to_js_string(g, get_i(g, list, i)) else { return UNDEF };
        hints.push(read_string(g, s).unwrap_or_default());
    }
    resolved_promise(g, parse_json(g, &high_entropy_json(&hints)))
}

#[crate::host_fn]
unsafe fn get_user_agent_data(g: *mut c_void, cf: *mut c_void) -> u64 {
    let nav = cf_this(cf);
    if !is_navigator(g, nav) {
        return throw_type_error(g, "Illegal invocation");
    }
    let Some(c) = registry().lock().unwrap().get(&(g as usize)).copied() else { return UNDEF };
    get_a(g, nav, c.s_ua_data)
}

unsafe fn install_ua_data(g: *mut c_void, nav: u64) {
    let Some(c) = registry().lock().unwrap().get(&(g as usize)).copied() else { return };
    let class = define_class(g, &ClassSpec {
        name: "NavigatorUAData",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[("getHighEntropyValues", 1, ua_high_entropy), ("toJSON", 0, ua_to_json)],
        getters: &[("brands", ua_brands), ("mobile", ua_mobile), ("platform", ua_platform_get)],
        accessors: &[],
    });
    let data = new_object(g, class.proto);
    glue::rjs_define(g, data, c.s_ua, TRUE, 1 | 2 | 4);
    glue::rjs_define(g, nav, c.s_ua_data, data, 1 | 2 | 4);
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
        s_ua_data: atom_priv(g, "redjs#uaData"),
        s_ua: atom_priv(g, "redjs#uaDataBrand"),
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
            ("userAgentData", get_user_agent_data),
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
    install_ua_data(g, nav);

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
    fn accept_language_is_chromiums() {
        let h = |l: &[&str]| super::accept_language_for(&l.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(h(&["ru-RU", "en-US"]), "ru-RU,ru;q=0.9,en-US;q=0.8,en;q=0.7");
        assert_eq!(h(&["en-US", "en-GB"]), "en-US,en-GB;q=0.9,en;q=0.8");
        assert_eq!(h(&["de", "en-US", "en"]), "de,en-US;q=0.9,en;q=0.8");
        assert_eq!(h(&["zh-Hans-CN"]), "zh-Hans-CN,zh;q=0.9");
        let many: Vec<String> = (0..12).map(|i| format!("x{}", (b'a' + i) as char)).collect();
        assert!(super::accept_language_for(&many).ends_with("xk;q=0.1,xl;q=0.1"), "the floor is 0.1");
    }

    #[test]
    fn user_agent_data_follows_the_user_agent() {
        let c = ctx();
        assert_eq!(super::ua_product("Mozilla/5.0 (Windows NT 10.0; Win64; x64) RedVue/1.0"), ("RedVue".into(), "1.0".into()));
        assert_eq!(super::windows_platform_version(26100), "19.0.0");
        assert_eq!(super::windows_platform_version(19045), "10.0.0");
        ok(&c, "navigator.userAgentData instanceof NavigatorUAData && navigator.userAgentData === navigator.userAgentData");
        ok(&c, "navigator.userAgentData.mobile === false && Object.isFrozen(navigator.userAgentData.brands)");
        ok(&c, "navigator.userAgentData.brands.some(b => b.brand === 'RedVue' && b.version === '1')");
        ok(&c, "JSON.stringify(navigator.userAgentData) === JSON.stringify({ brands: navigator.userAgentData.brands, mobile: false, platform: navigator.userAgentData.platform })");
        #[cfg(target_os = "windows")]
        ok(&c, "navigator.userAgentData.platform === 'Windows'");
        s(&c, "navigator.userAgentData.getHighEntropyValues(['platformVersion', 'architecture', 'bogus']).then(v => { globalThis.__he = v; }); 0");
        c.drain_microtasks();
        ok(&c, "/^\\d+\\.\\d+\\.\\d+$/.test(__he.platformVersion) && __he.architecture.length > 0 && !('bogus' in __he) && __he.mobile === false");
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

    /// One test: the state is process-wide.
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
