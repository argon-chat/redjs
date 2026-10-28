//! `navigator.clipboard` — the async Clipboard API over a host-supplied
//! platform backend.
//!
//! # Why the backend is a trait and not code in this file
//!
//! Every embedder that draws a window already owns the OS clipboard: it has to,
//! because Ctrl+C in a text field is table stakes and that path cannot go
//! through a second, private Win32 handle without the two fighting over
//! `OpenClipboard`. So this module does not open a clipboard — it asks whoever
//! already has one. Same seam, same reasoning as
//! [`crate::webapi::permissions::PermissionHost`].
//!
//! With no host installed the API still exists and every call **rejects**, with
//! a `NotAllowedError` naming the missing backend. Deliberately not a missing
//! `navigator.clipboard`: code that feature-detects it would otherwise take a
//! different path here than in a browser, and a rejected promise is the failure
//! mode such code is already written to handle.
//!
//! # Divergence 1: `read()` and `write()` are text-only, and refuse
//!
//! The item-based half of the API (`ClipboardItem`, `image/png`, multiple
//! representations) is not implemented in v1, and `ClipboardItem` is therefore
//! **not defined at all** — so `typeof ClipboardItem === "undefined"` tells the
//! truth and the feature detection every real image-paste implementation
//! already does takes the right branch. `read()`/`write()` exist, reject with a
//! `NotSupportedError`, and advise once, pointing at `readText`/`writeText`.
//!
//! # Divergence 2: there is no user-gesture requirement and no focus check
//!
//! A browser rejects `readText()` outside a user gesture, or when the document
//! is not focused, because a web page is untrusted code from the network. An
//! embedded application is not: it is the local program the user launched, and
//! it already has every OS API this call would gate. So the gesture and focus
//! checks are not reproduced — the observable difference is that a call a
//! browser would have rejected succeeds here. That is a *superset*, and the one
//! direction of divergence that cannot break an app written for a browser.
//!
//! # Divergence 3: `document.execCommand` is inert
//!
//! `execCommand("copy" | "cut" | "paste")` exists, does nothing, returns
//! `false`, and advises once naming `navigator.clipboard`. It is deprecated in
//! every browser, its behaviour depends on a live selection in a contenteditable
//! DOM that this runtime does not have, and silently returning `true` would be a
//! lie an app cannot detect. `false` is what a browser returns when the command
//! could not be executed, so an app that checks the return value already has the
//! branch.
//!
//! # Permissions
//!
//! `clipboard-read` / `clipboard-write` are answered by
//! [`crate::webapi::permissions`], through whatever `PermissionHost` the
//! embedder installed — this module never second-guesses it. On a platform
//! whose backend reports `granted` unconditionally (Windows has no consent gate
//! on the clipboard at all), `query()` saying `granted` and `readText()`
//! resolving are the same fact reported twice, which is exactly the consistency
//! a permissions layer is for.

use core::ffi::c_void;
use std::sync::{OnceLock, RwLock};

use super::util::*;
use crate::glue;

// ---------------------------------------------------------------------------
// The platform contract
// ---------------------------------------------------------------------------

/// The platform backend behind `navigator.clipboard`.
///
/// Synchronous on purpose. Every desktop clipboard API is, the JS surface is
/// promise-shaped either way, and an asynchronous trait would force every
/// backend to invent a completion queue for an operation that takes
/// microseconds. A backend that genuinely needs to wait (a Wayland portal) can
/// still block here — the alternative is a promise that never settles.
pub trait ClipboardHost: Send + Sync {
    /// A human name for this platform, for advisories: `"Windows"`, `"macOS"`.
    fn platform(&self) -> &'static str;

    /// The clipboard's text, or a reason the read failed. `Ok("")` is an empty
    /// clipboard; `Err` is a real failure, and becomes a rejected promise.
    ///
    /// A clipboard holding only non-text data is `Ok(String::new())` — the same
    /// answer a browser's `readText()` gives, because there is no text to read
    /// and nothing went wrong.
    fn read_text(&self) -> Result<String, String>;

    /// Replaces the clipboard's contents with `text`.
    fn write_text(&self, text: &str) -> Result<(), String>;
}

static HOST: RwLock<Option<&'static dyn ClipboardHost>> = RwLock::new(None);

/// Install the platform backend. Last writer wins; an embedder calls this once
/// at boot, before or after [`crate::webapi::install`].
pub fn set_host(host: &'static dyn ClipboardHost) {
    *HOST.write().unwrap() = Some(host);
}

/// The installed backend, if any.
pub fn host() -> Option<&'static dyn ClipboardHost> {
    *HOST.read().unwrap()
}

fn advise_no_host() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        eprintln!(
            "[redjs] navigator.clipboard: no ClipboardHost installed — every call rejects. \
             Call redjs::webapi::clipboard::set_host()."
        );
    });
}

// ---------------------------------------------------------------------------
// Rejections
// ---------------------------------------------------------------------------

/// A promise rejected with a `name` DOMException — the spec's "reject p with
/// a `NotAllowedError` DOMException".
unsafe fn rejected_named(g: *mut c_void, name: &str, msg: &str) -> u64 {
    super::domexception::rejected(g, name, msg)
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

unsafe extern "sysv64" fn clipboard_read_text(g: *mut c_void, _cf: *mut c_void) -> u64 {
    match host() {
        None => {
            advise_no_host();
            rejected_named(
                g,
                "NotAllowedError",
                "Failed to execute 'readText' on 'Clipboard': no clipboard backend is installed \
                 in this runtime.",
            )
        }
        Some(h) => match h.read_text() {
            Ok(text) => resolved_promise(g, js_string(g, &text)),
            Err(why) => rejected_named(
                g,
                "NotAllowedError",
                &format!("Failed to execute 'readText' on 'Clipboard': {why}"),
            ),
        },
    }
}

unsafe extern "sysv64" fn clipboard_write_text(g: *mut c_void, cf: *mut c_void) -> u64 {
    // `data` is a `DOMString`, so it is ToString-coerced — `writeText(42)`
    // writes "42" in a browser and writes "42" here. A conversion that throws
    // (a Symbol, a throwing toString) rejects with WHAT it threw, as WebIDL
    // does for a promise-returning operation.
    let arg = cf_arg(cf, 0);
    let mut sv = arg;
    if glue::rjs_kind(g, arg) != 5 && glue::rjs_to_string(g, arg, &mut sv) != 0 {
        return super::domexception::rejected_with(g, sv);
    }
    let text = read_string(g, sv).unwrap_or_default();
    match host() {
        None => {
            advise_no_host();
            rejected_named(
                g,
                "NotAllowedError",
                "Failed to execute 'writeText' on 'Clipboard': no clipboard backend is installed \
                 in this runtime.",
            )
        }
        Some(h) => match h.write_text(&text) {
            Ok(()) => resolved_promise(g, UNDEF),
            Err(why) => rejected_named(
                g,
                "NotAllowedError",
                &format!("Failed to execute 'writeText' on 'Clipboard': {why}"),
            ),
        },
    }
}

fn advise_items(verb: &str) {
    static ONCE: OnceLock<()> = OnceLock::new();
    let platform = host().map(|h| h.platform()).unwrap_or("this platform");
    ONCE.get_or_init(|| {
        eprintln!(
            "[redjs] navigator.clipboard.{verb}() is not implemented on {platform}: this runtime's \
             clipboard is text-only in v1, and `ClipboardItem` is deliberately undefined so \
             feature detection reports it honestly. Use readText()/writeText()."
        );
    });
}

unsafe extern "sysv64" fn clipboard_read(g: *mut c_void, _cf: *mut c_void) -> u64 {
    advise_items("read");
    rejected_named(
        g,
        "NotSupportedError",
        "Failed to execute 'read' on 'Clipboard': this runtime's clipboard is text-only. Use \
         navigator.clipboard.readText().",
    )
}

unsafe extern "sysv64" fn clipboard_write(g: *mut c_void, _cf: *mut c_void) -> u64 {
    advise_items("write");
    rejected_named(
        g,
        "NotSupportedError",
        "Failed to execute 'write' on 'Clipboard': this runtime's clipboard is text-only. Use \
         navigator.clipboard.writeText().",
    )
}

// ---------------------------------------------------------------------------
// document.execCommand
// ---------------------------------------------------------------------------

unsafe extern "sysv64" fn exec_command(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cmd = read_string(g, cf_arg(cf, 0)).unwrap_or_default().to_ascii_lowercase();
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        eprintln!(
            "[redjs] document.execCommand({cmd:?}) did nothing and returned false. execCommand is \
             deprecated in every browser and depends on a contenteditable selection this runtime \
             does not have; for the clipboard commands use navigator.clipboard.writeText() / \
             readText() instead."
        );
    });
    FALSE
}

/// `document.queryCommandSupported` / `queryCommandEnabled` — `false` for
/// everything, which is the answer that makes the usual
/// `if (document.queryCommandSupported("copy"))` guard take the branch that
/// actually works here.
unsafe extern "sysv64" fn query_command_false(_g: *mut c_void, _cf: *mut c_void) -> u64 {
    FALSE
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

pub unsafe fn install(g: *mut c_void, event_target_proto: u64) {
    let clipboard_class = define_class(g, &ClassSpec {
        name: "Clipboard",
        arity: 0,
        construct: None,
        // A browser's `Clipboard` extends `EventTarget`; it is where the
        // (unimplemented, and unimplementable here) `clipboardchange` event
        // would land, and `instanceof EventTarget` is observable.
        parent_proto: event_target_proto,
        methods: &[
            ("readText", 0, clipboard_read_text),
            ("writeText", 1, clipboard_write_text),
            ("read", 0, clipboard_read),
            ("write", 1, clipboard_write),
        ],
        getters: &[],
        accessors: &[],
    });

    let clipboard = new_object(g, clipboard_class.proto);
    crate::ffi::redjs_protect(g, clipboard);

    // `navigator` may already exist (permissions, mediaDevices, or the host's
    // own); only `clipboard` is ours, and it goes onto whatever object is there.
    let global = glue::rjs_global(g);
    let existing = get(g, global, "navigator");
    let navigator = if is_cell(existing) {
        existing
    } else {
        let n = new_object(g, UNDEF);
        define(g, global, "navigator", n, 2 /* DontEnum */);
        n
    };
    define(g, navigator, "clipboard", clipboard, 2);

    // `document` exists by now (webapi::install runs `dom::install` first); on
    // a context where it somehow does not, skip rather than invent one.
    let document = state(g).document;
    if is_cell(document) {
        define(g, document, "execCommand", make_fn(g, "execCommand", 3, exec_command), 2);
        define(
            g,
            document,
            "queryCommandSupported",
            make_fn(g, "queryCommandSupported", 1, query_command_false),
            2,
        );
        define(
            g,
            document,
            "queryCommandEnabled",
            make_fn(g, "queryCommandEnabled", 1, query_command_false),
            2,
        );
    }
}

#[cfg(test)]
mod jsc_tests {
    use super::*;
    use crate::api::Context;
    use crate::webapi::testing::*;

    /// A clipboard that is a `Mutex<String>` — the OS, without an OS.
    struct FakeClipboard;
    static TEXT: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
    static FAIL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    impl ClipboardHost for FakeClipboard {
        fn platform(&self) -> &'static str {
            "scripted"
        }
        fn read_text(&self) -> Result<String, String> {
            if FAIL.load(std::sync::atomic::Ordering::Relaxed) {
                return Err("the clipboard is held by another process".into());
            }
            Ok(TEXT.lock().unwrap().clone())
        }
        fn write_text(&self, text: &str) -> Result<(), String> {
            if FAIL.load(std::sync::atomic::Ordering::Relaxed) {
                return Err("the clipboard is held by another process".into());
            }
            *TEXT.lock().unwrap() = text.to_string();
            Ok(())
        }
    }

    /// `set_host` and the fake clipboard behind it are process-global — which
    /// is exactly what a real clipboard is — so these tests run one at a time.
    /// Poison-tolerant, so one failure does not hide itself behind lock panics
    /// in the others.
    fn install_fake() -> std::sync::MutexGuard<'static, ()> {
        static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = L.lock().unwrap_or_else(|e| e.into_inner());
        static H: FakeClipboard = FakeClipboard;
        set_host(&H);
        FAIL.store(false, std::sync::atomic::Ordering::Relaxed);
        TEXT.lock().unwrap().clear();
        guard
    }

    fn run(c: &Context, body: &str) -> String {
        s(
            c,
            &format!(
                "globalThis.__out = '<pending>'; (async () => {{ {body} }})().then(\
                   v => {{ globalThis.__out = String(v); }}, \
                   e => {{ globalThis.__out = 'ERROR: ' + (e instanceof DOMException ? '' : 'plain ') + \
                     e.name + ': ' + ((e && e.message) || e); }}); 'started'"
            ),
        );
        for _ in 0..64 {
            c.drain_microtasks();
        }
        s(c, "globalThis.__out")
    }

    #[test]
    fn clipboard_is_a_singleton_on_navigator_and_an_event_target() {
        let c = ctx();
        ok(&c, "navigator.clipboard === navigator.clipboard");
        ok(&c, "navigator.clipboard instanceof Clipboard");
        ok(&c, "navigator.clipboard instanceof EventTarget");
        ok(&c, "typeof navigator.clipboard.readText === 'function'");
        ok(&c, "typeof navigator.clipboard.writeText === 'function'");
        // permissions and clipboard must share ONE navigator, whichever
        // installs first.
        ok(&c, "typeof navigator.permissions === 'object'");
        assert!(throws(&c, "new Clipboard()").contains("Illegal constructor"));
    }

    #[test]
    fn write_then_read_round_trips_through_the_host() {
        let _g = install_fake();
        let c = ctx();
        assert_eq!(
            run(
                &c,
                "await navigator.clipboard.writeText('hello native');
                 return await navigator.clipboard.readText();"
            ),
            "hello native"
        );
        assert_eq!(*TEXT.lock().unwrap(), "hello native");
    }

    #[test]
    fn write_text_coerces_its_argument_like_a_dom_string() {
        let _g = install_fake();
        let c = ctx();
        assert_eq!(
            run(
                &c,
                "await navigator.clipboard.writeText(42);
                 return await navigator.clipboard.readText();"
            ),
            "42"
        );
        assert_eq!(run(&c, "return await navigator.clipboard.writeText('');"), "undefined");
        // A conversion that throws rejects with the TypeError itself.
        let got = run(&c, "return await navigator.clipboard.writeText(Symbol('s'));");
        assert!(got.starts_with("ERROR: plain TypeError"), "{got}");
    }

    #[test]
    fn a_backend_failure_is_a_not_allowed_error_not_a_silent_empty_string() {
        // The failure this prevents: an app pasting "" because the read failed
        // and nobody said so.
        let _g = install_fake();
        FAIL.store(true, std::sync::atomic::Ordering::Relaxed);
        let c = ctx();
        let got = run(&c, "return await navigator.clipboard.readText();");
        assert!(got.starts_with("ERROR: NotAllowedError"), "{got}");
        assert!(got.contains("held by another process"), "{got}");
        let got = run(&c, "return await navigator.clipboard.writeText('x');");
        assert!(got.starts_with("ERROR: NotAllowedError"), "{got}");
    }

    #[test]
    fn read_and_write_reject_and_clipboard_item_does_not_exist() {
        // v1's whole item story, pinned: the honest "no" plus the feature
        // detection that lets an app take the text branch.
        let _g = install_fake();
        let c = ctx();
        ok(&c, "typeof ClipboardItem === 'undefined'");
        let got = run(&c, "return await navigator.clipboard.read();");
        assert!(got.starts_with("ERROR: NotSupportedError"), "{got}");
        assert!(got.contains("readText"), "the message must name the alternative: {got}");
        let got = run(&c, "return await navigator.clipboard.write([]);");
        assert!(got.starts_with("ERROR: NotSupportedError"), "{got}");
        assert!(got.contains("writeText"), "{got}");
    }

    #[test]
    fn exec_command_is_inert_and_says_so_through_its_return_value() {
        let c = ctx();
        assert_eq!(s(&c, "document.execCommand('copy')"), "false");
        assert_eq!(s(&c, "document.execCommand('paste')"), "false");
        assert_eq!(s(&c, "document.queryCommandSupported('copy')"), "false");
        assert_eq!(s(&c, "document.queryCommandEnabled('copy')"), "false");
    }
}
