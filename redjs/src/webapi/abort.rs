//! Native `AbortController` / `AbortSignal` (WHATWG DOM).
//!
//! `AbortSignal` is a real `EventTarget` (its prototype chains to the native
//! EventTarget), so `addEventListener('abort', …)`, `onabort` and `signal
//! .aborted/.reason/.throwIfAborted()` all behave per spec. Statics:
//! `AbortSignal.abort(reason)` and `AbortSignal.timeout(ms)` (fires through
//! the native scheduler with a spec `TimeoutError`-named error).

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::events;
use super::util::*;
use crate::glue;

struct AbortClasses {
    signal: Class,
    controller: Class,
    /// Private slot: {aborted, reason} bag on signals / {signal} on controllers.
    s_abort: *mut c_void,
}

unsafe impl Send for AbortClasses {}
unsafe impl Sync for AbortClasses {}

fn registry() -> &'static Mutex<HashMap<usize, &'static AbortClasses>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static AbortClasses>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> &'static AbortClasses {
    registry().lock().unwrap().get(&(g as usize)).copied().expect("webapi::abort not installed")
}

pub(crate) fn forget(gone: crate::context::Gone) {
    registry().lock().unwrap().remove(&gone.key);
}

unsafe fn slots_of(g: *mut c_void, obj: u64) -> u64 {
    get_a(g, obj, classes(g).s_abort)
}

/// Creates a signal object (proto → AbortSignal.prototype → EventTarget).
unsafe fn new_signal(g: *mut c_void, aborted: bool, reason: u64) -> u64 {
    let c = classes(g);
    let sig = new_object(g, c.signal.proto);
    let slots = new_object(g, UNDEF);
    define(g, slots, "aborted", enc_bool(aborted), 0);
    define(g, slots, "reason", reason, 0);
    glue::rjs_define(g, sig, c.s_abort, slots, 0);
    sig
}

/// Flips a signal to aborted and fires the `abort` event. No-op if already
/// aborted (spec).
pub unsafe fn abort_signal(g: *mut c_void, sig: u64, reason: u64) {
    let slots = slots_of(g, sig);
    if !is_cell(slots) || get(g, slots, "aborted") == 0x7 {
        return;
    }
    let reason = if reason == UNDEF { default_reason(g, "AbortError", "signal is aborted without reason") } else { reason };
    define(g, slots, "aborted", enc_bool(true), 0);
    define(g, slots, "reason", reason, 0);
    let ev = events::new_trusted_event(g, "abort");
    events::dispatch(g, sig, ev);
}

unsafe fn default_reason(g: *mut c_void, name: &str, msg: &str) -> u64 {
    let m: Vec<u16> = msg.encode_utf16().collect();
    let mut err = UNDEF;
    glue::rjs_error_create(g, 0, m.as_ptr(), m.len(), &mut err);
    define(g, err, "name", js_string(g, name), 0);
    err
}

unsafe fn this_signal_slots(g: *mut c_void, cf: *mut c_void) -> Option<u64> {
    let slots = slots_of(g, cf_this(cf));
    is_cell(slots).then_some(slots)
}

// ---- AbortSignal ------------------------------------------------------------

unsafe extern "sysv64" fn sig_get_aborted(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_signal_slots(g, cf) {
        Some(s) => get(g, s, "aborted"),
        None => throw_type_error(g, "AbortSignal.aborted on wrong this"),
    }
}

unsafe extern "sysv64" fn sig_get_reason(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_signal_slots(g, cf) {
        Some(s) => get(g, s, "reason"),
        None => throw_type_error(g, "AbortSignal.reason on wrong this"),
    }
}

unsafe extern "sysv64" fn sig_throw_if_aborted(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(s) = this_signal_slots(g, cf) else {
        return throw_type_error(g, "AbortSignal.throwIfAborted on wrong this");
    };
    if get(g, s, "aborted") == 0x7 {
        glue::rjs_throw(g, get(g, s, "reason"));
    }
    UNDEF
}

/// AbortSignal.abort(reason) — a pre-aborted signal.
unsafe extern "sysv64" fn sig_static_abort(g: *mut c_void, cf: *mut c_void) -> u64 {
    let reason = cf_arg(cf, 0);
    let reason = if reason == UNDEF { default_reason(g, "AbortError", "signal is aborted without reason") } else { reason };
    new_signal(g, true, reason)
}

/// AbortSignal.timeout(ms) — aborts with a TimeoutError via the scheduler.
unsafe extern "sysv64" fn sig_static_timeout(g: *mut c_void, cf: *mut c_void) -> u64 {
    let ms = dec_f64(cf_arg(cf, 0)).unwrap_or(0.0).max(0.0);
    let sig = new_signal(g, false, UNDEF);
    crate::ffi::redjs_protect(g, sig);
    crate::web::schedule_native(g, ms, Box::new(move |g| {
        let reason = default_reason(g, "TimeoutError", "signal timed out");
        abort_signal(g, sig, reason);
        crate::ffi::redjs_unprotect(g, sig);
    }));
    sig
}

// ---- AbortController --------------------------------------------------------

unsafe extern "sysv64" fn ctrl_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let obj = ordinary_create(g, cf_new_target(cf), c.controller.proto);
    let slots = new_object(g, UNDEF);
    define(g, slots, "signal", new_signal(g, false, UNDEF), 0);
    glue::rjs_define(g, obj, c.s_abort, slots, 0);
    obj
}

unsafe extern "sysv64" fn ctrl_get_signal(g: *mut c_void, cf: *mut c_void) -> u64 {
    let slots = slots_of(g, cf_this(cf));
    if !is_cell(slots) {
        return throw_type_error(g, "AbortController.signal on wrong this");
    }
    get(g, slots, "signal")
}

unsafe extern "sysv64" fn ctrl_abort(g: *mut c_void, cf: *mut c_void) -> u64 {
    let slots = slots_of(g, cf_this(cf));
    if !is_cell(slots) {
        return throw_type_error(g, "AbortController.abort on wrong this");
    }
    let sig = get(g, slots, "signal");
    abort_signal(g, sig, cf_arg(cf, 0));
    UNDEF
}

// ---- install -----------------------------------------------------------------

pub unsafe fn install(g: *mut c_void, event_target_proto: u64) {
    let s_abort = atom_priv(g, "redjs#abort");

    let signal = define_class(g, &ClassSpec {
        name: "AbortSignal",
        arity: 0,
        construct: None,
        parent_proto: event_target_proto,
        methods: &[("throwIfAborted", 0, sig_throw_if_aborted)],
        getters: &[("aborted", sig_get_aborted), ("reason", sig_get_reason)],
        accessors: &[],
    });
    define(g, signal.ctor, "abort", make_fn(g, "abort", 1, sig_static_abort), 2);
    define(g, signal.ctor, "timeout", make_fn(g, "timeout", 1, sig_static_timeout), 2);

    let controller = define_class(g, &ClassSpec {
        name: "AbortController",
        arity: 0,
        construct: Some(ctrl_construct),
        parent_proto: 0,
        methods: &[("abort", 1, ctrl_abort)],
        getters: &[("signal", ctrl_get_signal)],
        accessors: &[],
    });

    let boxed: &'static AbortClasses = Box::leak(Box::new(AbortClasses { signal, controller, s_abort }));
    registry().lock().unwrap().insert(g as usize, boxed);
}
