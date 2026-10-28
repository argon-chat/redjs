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

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, RwLock};

use super::util::*;
use crate::glue;

/// What `navigator.userAgent` says until the embedder sets something else.
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) RedVue/1.0";

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

struct NavClasses {
    /// Brand slot: only the real `navigator` answers the getters.
    s_nav: *mut c_void,
}

unsafe impl Send for NavClasses {}
unsafe impl Sync for NavClasses {}

fn registry() -> &'static Mutex<HashMap<usize, &'static NavClasses>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static NavClasses>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn forget(gone: crate::context::Gone) {
    registry().lock().unwrap().remove(&gone.key);
}

unsafe fn is_navigator(g: *mut c_void, v: u64) -> bool {
    let Some(c) = registry().lock().unwrap().get(&(g as usize)).copied() else { return false };
    is_cell(v) && get_a(g, v, c.s_nav) == TRUE
}

macro_rules! nav_getter {
    ($name:ident, |$ua:ident| $body:expr) => {
        unsafe extern "sysv64" fn $name(g: *mut c_void, cf: *mut c_void) -> u64 {
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

unsafe extern "sysv64" fn get_hardware_concurrency(g: *mut c_void, cf: *mut c_void) -> u64 {
    if !is_navigator(g, cf_this(cf)) {
        return throw_type_error(g, "Illegal invocation");
    }
    enc_i32(hardware_concurrency().min(i32::MAX as usize) as i32)
}

pub unsafe fn install(g: *mut c_void) {
    let s_nav = atom_priv(g, "redjs#navigator");
    registry().lock().unwrap().insert(g as usize, Box::leak(Box::new(NavClasses { s_nav })));

    let class = define_class(g, &ClassSpec {
        name: "Navigator",
        arity: 0,
        construct: None, // "Illegal constructor", as in a browser
        parent_proto: 0,
        methods: &[],
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
        ],
        accessors: &[],
    });
    let to_string_tag = glue::rjs_atom_wellknown(g, 1);
    glue::rjs_define(g, class.proto, to_string_tag, js_string(g, "Navigator"), 1 | 2);

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
}

#[cfg(test)]
mod tests {
    use crate::webapi::testing::{ctx, ok, s};

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
        assert_eq!(s(&c, "navigator.userAgent"), "Mozilla/5.0 (Windows NT 10.0; Win64; x64) RedVue/1.0");
        assert_eq!(s(&c, "navigator.appVersion"), "5.0 (Windows NT 10.0; Win64; x64) RedVue/1.0");
        assert_eq!(s(&c, "navigator.platform"), "Win32");

        let ua: Vec<u16> = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) Embedder/2".encode_utf16().collect();
        unsafe { crate::ffi::redjs_navigator_set_user_agent(ua.as_ptr(), ua.len()) };
        let got = s(&c, "[navigator.userAgent, navigator.appVersion, navigator.platform].join('|')");
        unsafe { crate::ffi::redjs_navigator_set_user_agent(core::ptr::null(), 0) };
        assert_eq!(got, "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) Embedder/2|5.0 (Macintosh; Intel Mac OS X 14_0) Embedder/2|MacIntel");
        assert_eq!(s(&c, "navigator.userAgent"), super::DEFAULT_USER_AGENT);
    }
}
