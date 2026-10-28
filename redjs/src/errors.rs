//! The runtime's error-reporting funnel: every exception that nobody in JS is
//! going to see goes through [`report`] and out to the one host handler.
//!
//! Two things live here.
//!
//! **The funnel.** `redjs_set_uncaught_handler` registers a single process-wide
//! `void f(ctx, exception)` (MS ABI, C#-friendly), called with the VM lock held.
//! Every call site passes an [`ErrKind`] saying *why* the exception has nobody
//! left to catch it. The kind is not on the ABI yet — the host handler still
//! takes two arguments — but every reporter already carries one, so the
//! `redjs_set_error_handler(ctx, exc, kind)` collapse sketched in
//! `.refactor/DESKTOP-DEVTOOLS-DESIGN.md` §6.5 is a signature change here, not
//! a redesign. Listener throws are routed in (`webapi::events`).
//!
//! **Unhandled promise rejections.** JSC has always had the hook — this build
//! exports `JSGlobalContextSetUnhandledRejectionCallback` (public C API,
//! `JSContextRefPrivate.h`) — and the slot was simply never armed, which is why
//! a rejection routed into a handler an application never registered produced a
//! mounted-but-empty view and *no* log line anywhere. [`install_rejection`] arms
//! it at context creation, so no embedder can forget to.
//!
//! # The callback must never throw back into JSC
//! A JS exception propagating out of a host callback into the VM turns a
//! reporting mechanism into a crash, so the rejection callback: takes no path
//! that can leave a pending exception un-taken (`rjs_error_info` and `rjs_get`
//! both clear), guards against re-entering itself (a report that itself rejects
//! must not recurse), and clears the VM's exception state before returning.

use core::ffi::c_void;
use std::cell::Cell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::capi;
use crate::glue;

/// Why an exception reached the host instead of JS. Carried by every reporter;
/// see the module docs for what it is preparing for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ErrKind {
    /// Thrown by a callback the host pumped (timer, rAF, idle).
    PumpCallback = 1,
    /// Thrown by an `EventTarget` listener or `on*` handler. Dispatch catches
    /// it so one bad listener cannot abort the rest (DOM "inner invoke":
    /// report the exception, keep going), reports it here — HTML's "report
    /// the exception" — and tells its caller a listener threw
    /// (`webapi::events::DispatchOutcome::listener_threw`).
    Listener = 2,
    /// A promise rejected with nothing attached to observe it.
    UnhandledRejection = 3,
}

impl ErrKind {
    /// The tag the native fallback prints. Same shape as `webapi`'s `[js:*]`.
    fn tag(self) -> &'static str {
        match self {
            ErrKind::PumpCallback => "[js:uncaught]",
            ErrKind::Listener => "[js:listener]",
            ErrKind::UnhandledRejection => "[js:unhandled-rejection]",
        }
    }
}

/// `void handler(ctx, exception_value)` — MS ABI. Called with the VM lock held;
/// extracting error info is fine, re-entering JS is not.
static HANDLER: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());

pub fn set_handler(handler: *const c_void) {
    HANDLER.store(handler as *mut c_void, Ordering::Release);
}

thread_local! {
    /// Re-entrancy guard: a report that itself throws or rejects must not
    /// recurse into the funnel (that is a stack overflow, i.e. the reporting
    /// mechanism becoming the crash).
    static REPORTING: Cell<bool> = const { Cell::new(false) };
}

/// Hands `exc` to the host handler, or — when no host has registered one —
/// writes it to stderr rather than dropping it.
///
/// # Safety
/// `g` must be a live context and the caller must be on its VM thread.
pub(crate) unsafe fn report(g: *mut c_void, exc: u64, kind: ErrKind) {
    if REPORTING.with(|r| r.replace(true)) {
        return; // already inside a report on this thread
    }
    let h = HANDLER.load(Ordering::Acquire);
    if h.is_null() {
        let (header, stack) = describe(g, exc);
        if stack.is_empty() {
            eprintln!("{} {header}", kind.tag());
        } else {
            eprintln!("{} {header}\n{stack}", kind.tag());
        }
    } else {
        let f: unsafe extern "C" fn(*mut c_void, u64) = core::mem::transmute(h);
        f(g, exc);
    }
    REPORTING.with(|r| r.set(false));
}

/// `(header, stack)` of an exception value, where `header` is
/// `"TypeError: undefined is not an object"` — **name and message both**.
///
/// JSC's `Error.prototype.stack` is frames only: it carries no `Name: message`
/// line the way V8's does, so a formatter that prints `stack || message`
/// (`bootstrap.js:17`, `JSConsole.cs:216`) prints a call stack that never says
/// what went wrong. This is the join those two are missing.
///
/// A rejection reason is often *not* an `Error` — `Promise.reject({code: 500})`
/// is ordinary code. When there is no `name`/`message` to join, the header is
/// the value's own `ToString`, so a non-`Error` reason reports as itself rather
/// than as `undefined`. Never throws: every glue call on this path clears.
pub(crate) unsafe fn describe(g: *mut c_void, exc: u64) -> (String, String) {
    let mut msg_v = 0u64;
    let mut stack_v = 0u64;
    glue::rjs_error_info(g, exc, &mut msg_v, &mut stack_v);
    let stack = read_string(g, stack_v);

    let name = read_string_prop(g, exc, "name");
    let msg = read_string_prop(g, exc, "message");
    let header = match (name.is_empty(), msg.is_empty()) {
        (false, false) => format!("{name}: {msg}"),
        (false, true) => name,
        (true, false) => msg,
        // Neither: a thrown/rejected non-Error. Say what it actually was.
        (true, true) => {
            let mut s = 0u64;
            if glue::rjs_to_string(g, exc, &mut s) == 0 { read_string(g, s) } else { String::new() }
        }
    };
    (header, stack)
}

/// `obj[key]` when it is a string, else empty. Clears any exception the read
/// provoked (an accessor on an exotic reason object).
unsafe fn read_string_prop(g: *mut c_void, obj: u64, key: &str) -> String {
    const KIND_STRING: u32 = 5;
    let atom = glue::rjs_atom8(g, key.as_ptr(), key.len());
    if atom.is_null() {
        return String::new();
    }
    let mut v = 0u64;
    let rc = glue::rjs_get(g, obj, atom, &mut v);
    glue::rjs_atom_free(g, atom);
    if rc != 0 || glue::rjs_kind(g, v) != KIND_STRING {
        return String::new();
    }
    read_string(g, v)
}

unsafe fn read_string(g: *mut c_void, v: u64) -> String {
    let mut info = crate::glue::RjsStr::default();
    if glue::rjs_string_info(g, v, &mut info) != 0 || info.len == 0 {
        return String::new();
    }
    if info.is8bit != 0 {
        let bytes = core::slice::from_raw_parts(info.ptr as *const u8, info.len as usize);
        bytes.iter().map(|&b| b as char).collect()
    } else {
        let units = core::slice::from_raw_parts(info.ptr as *const u16, info.len as usize);
        String::from_utf16_lossy(units)
    }
}

// ---------------------------------------------------------------------------
// Unhandled promise rejections
// ---------------------------------------------------------------------------

/// `(promise, reason)` — JSC calls this with the rejected promise and its
/// reason once the microtask queue has settled and nothing has attached a
/// handler. SysV, like every other Rust host function here (no trampoline).
unsafe extern "sysv64" fn on_unhandled_rejection(g: *mut c_void, cf: *mut c_void) -> u64 {
    let args = crate::api::HostArgs::new(cf);
    let reason = args.arg(1).0;
    report(g, reason, ErrKind::UnhandledRejection);
    // Belt and braces: whatever happened above, JSC must not resume with an
    // exception we planted. Everything on the path clears already; this makes
    // "never throws back into the VM" unconditional rather than a code review.
    let mut pending = 0u64;
    glue::rjs_exception_pending(g, &mut pending);
    crate::api::Value::undefined().0
}

fn installed() -> &'static Mutex<HashSet<usize>> {
    static SET: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

/// `crate::context` fan-out: the address no longer means the context it was
/// armed for, so a newcomer there must be armed again rather than skipped.
pub(crate) fn forget(gone: crate::context::Gone) {
    installed().lock().unwrap().remove(&gone.key);
}

/// Whether [`install_rejection`] has run for this address (tests).
#[cfg(test)]
pub(crate) fn rejection_armed(g: *mut c_void) -> bool {
    installed().lock().unwrap().contains(&(g as usize))
}

/// Arms JSC's unhandled-rejection hook on `g`. Called from
/// `redjs_context_create`, so every context has it — an embedder cannot forget
/// and get silence. Idempotent per context.
///
/// # Safety
/// `g` must be the `JSGlobalContextRef` from `JSGlobalContextCreate`.
pub unsafe fn install_rejection(g: *mut c_void) -> i32 {
    // A recycled address purges its stale registration here, before the
    // "already armed" test below could mistake it for this context's.
    crate::context::ensure(g);
    if !installed().lock().unwrap().insert(g as usize) {
        return 0;
    }
    let name: Vec<u16> = "onUnhandledRejection".encode_utf16().collect();
    let mut f = 0u64;
    let rc = glue::rjs_function_create(
        g,
        name.as_ptr(),
        name.len(),
        2,
        on_unhandled_rejection as *const c_void,
        &mut f,
    );
    if rc != 0 {
        return rc;
    }
    // JSC stores the callback on the global object (with a write barrier), so
    // it is reachable — but this one lives for the process, and a protect is
    // cheaper than depending on that detail of the prebuilt.
    capi::JSValueProtect(g, f as usize as capi::JSValueRef);
    let mut exc: capi::JSValueRef = core::ptr::null();
    capi::JSGlobalContextSetUnhandledRejectionCallback(g, f as usize as capi::JSObjectRef, &mut exc);
    if !exc.is_null() {
        return crate::ffi::REDJS_EXCEPTION;
    }
    0
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! The half a naive implementation gets wrong is the *negative*: a
    //! `.catch()`-handled rejection must produce nothing. Every test below
    //! asserts both directions on one context.

    use super::*;
    use crate::api::Context;
    use std::collections::HashMap;

    /// Reports are keyed by context: `HANDLER` is process-global and `cargo
    /// test` runs these in parallel, so a test must only ever read its own.
    fn reports() -> &'static Mutex<HashMap<usize, Vec<String>>> {
        static R: OnceLock<Mutex<HashMap<usize, Vec<String>>>> = OnceLock::new();
        R.get_or_init(|| Mutex::new(HashMap::new()))
    }

    unsafe extern "C" fn record(g: *mut c_void, exc: u64) {
        let (header, stack) = describe(g, exc);
        reports().lock().unwrap().entry(g as usize).or_default().push(format!("{header}\u{1}{stack}"));
    }

    /// A context with the funnel pointed at [`record`]. Setting the global
    /// handler is idempotent — every test installs the same function pointer.
    fn ctx() -> Context {
        let c = Context::new();
        set_handler(record as *const c_void);
        c
    }

    /// Everything reported for `c` since it was created, and drains it.
    fn taken(c: &Context) -> Vec<String> {
        c.drain_microtasks();
        reports().lock().unwrap().remove(&(c.raw() as usize)).unwrap_or_default()
    }

    fn headers(v: &[String]) -> Vec<&str> {
        v.iter().map(|s| s.split('\u{1}').next().unwrap()).collect()
    }

    #[test]
    fn an_unawaited_async_throw_is_reported() {
        let c = ctx();
        c.eval("(async () => { throw new Error('async boom'); })();").unwrap();
        let got = taken(&c);
        assert_eq!(headers(&got), vec!["Error: async boom"], "got {got:?}");
    }

    #[test]
    fn a_bare_promise_rejection_is_reported() {
        let c = ctx();
        c.eval("Promise.reject(new TypeError('bare boom'));").unwrap();
        let got = taken(&c);
        assert_eq!(headers(&got), vec!["TypeError: bare boom"], "got {got:?}");
    }

    #[test]
    fn a_handled_rejection_is_not_reported() {
        let c = ctx();
        c.eval("Promise.reject(new Error('caught')).catch(() => {});").unwrap();
        assert_eq!(taken(&c), Vec::<String>::new(), "a .catch()-ed rejection must stay silent");

        // …and the same context still reports the unhandled one right after,
        // so "silent" cannot be passing because reporting is simply broken.
        c.eval("Promise.reject(new Error('loud'));").unwrap();
        assert_eq!(headers(&taken(&c)), vec!["Error: loud"]);
    }

    #[test]
    fn an_awaited_throw_inside_a_try_is_not_reported() {
        let c = ctx();
        c.eval("(async () => { try { await Promise.reject(new Error('handled')); } catch (e) {} })();").unwrap();
        assert_eq!(taken(&c), Vec::<String>::new());
    }

    #[test]
    fn the_report_carries_the_message_and_the_stack_separately() {
        let c = ctx();
        c.eval_named("function boom() { throw new Error('has a message'); }\nPromise.resolve().then(boom);", "game/main.js")
            .unwrap();
        let got = taken(&c);
        assert_eq!(got.len(), 1, "got {got:?}");
        let (header, stack) = got[0].split_once('\u{1}').unwrap();
        // The defect this must not inherit: both console formatters print
        // `stack || message`, and JSC's stack has no `Name: message` header —
        // so the message vanishes. Assert it is present.
        assert_eq!(header, "Error: has a message");
        assert!(stack.contains("boom"), "stack should name the frame, got {stack:?}");
        assert!(!stack.contains("has a message"), "JSC stacks carry no message — if this fires, the join above is now double-printing");
    }

    #[test]
    fn a_non_error_reason_still_reports() {
        let c = ctx();
        // `null`/`undefined` matter: a property read on them THROWS, so the
        // describe path has to survive its own lookups failing.
        c.eval("Promise.reject(42); Promise.reject('a string'); Promise.reject({ nope: 1 }); Promise.reject(); Promise.reject(null);")
            .unwrap();
        let got = taken(&c);
        assert_eq!(
            headers(&got),
            vec!["42", "a string", "[object Object]", "undefined", "null"],
            "got {got:?}"
        );
    }

    #[test]
    fn a_rejection_from_a_pumped_callback_is_reported_too() {
        // The path the benchmark actually took: nothing throws synchronously,
        // the rejection is born inside a frame callback.
        let c = ctx();
        unsafe { crate::ffi::redjs_web_install(c.raw()) };
        c.eval("requestAnimationFrame(async () => { throw new Error('frame boom'); });").unwrap();
        unsafe { crate::ffi::redjs_pump(c.raw(), 7, core::ptr::null_mut()) };
        assert_eq!(headers(&taken(&c)), vec!["Error: frame boom"]);
    }
}
