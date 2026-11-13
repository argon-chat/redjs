//! Native `Event`, `CustomEvent` and `EventTarget` (WHATWG DOM §2), the
//! dispatch machinery the rest of the web API builds on.
//!
//! Internal state lives in private-name slots (invisible to JS):
//! - Event:       [[s_event]] → plain object {type, bubbles, cancelable,
//!                composed, defaultPrevented, stopProp, stopImmediate,
//!                dispatching, inPassive, initialized, phase, path, target,
//!                currentTarget, detail, timeStamp, isTrusted}
//! - EventTarget: [[s_listeners]] → JS Array of listener entries
//!                {type, callback, flags} (flags: once/capture/passive/removed)
//!
//! # Dispatch is the DOM algorithm, path and all
//!
//! [`dispatch_path`] takes an explicit event path — `[target, parent, …]` —
//! and runs DOM §2.9 over it: a capturing pass root→target, then a bubbling
//! pass target→root (the target itself in both, at `AT_TARGET`, capture
//! listeners first), `stopPropagation` between nodes, `stopImmediate…` between
//! listeners, the `removed` flag so a listener removed mid-dispatch never runs,
//! `once`, `passive` (preventDefault is a no-op inside one), and listener
//! identity `(type, callback, capture)`. `eventPhase`, `currentTarget` and
//! `composedPath()` read what a browser's do during and after dispatch.
//!
//! The path is the CALLER's: non-DOM targets have their own "get the parent"
//! (IndexedDB: request → transaction → database), so the module that owns
//! the targets builds it. [`dispatch`] — what every native event source and
//! `dispatchEvent` uses — builds the one parent this runtime has: `document`'s
//! is the global (DOM "get the parent", except for `load`). Elements are not
//! in a tree here, so an element's path is itself; vue-native's mirror tree
//! does its own walk in `runtime.ts`.
//!
//! # Between listeners
//!
//! A listener invoked with no JS on the stack — a native dispatch from the
//! pump — is followed by a microtask checkpoint, as HTML's "clean up after
//! running script" does after every callback. From `dispatchEvent` the stack
//! is not empty and nothing runs between listeners, as in a browser.
//!
//! A throwing listener is reported through [`crate::errors::report`] as
//! [`crate::errors::ErrKind::Listener`] and the dispatch goes on; the caller
//! learns it happened ([`DispatchOutcome::listener_threw`], the DOM's
//! "legacyOutputDidListenersThrowFlag" that IndexedDB aborts on).
//!
//! # Event handler attributes
//!
//! `on<type>` is read off each node in the bubbling pass and called after that
//! node's listeners, with `this` = the node; returning `false` cancels a
//! cancelable event. A browser registers a handler as a listener at the point
//! it was first assigned, so the two orders differ only for a node that has
//! both a handler and listeners — unchanged from before this module knew
//! about paths.

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};

use super::util::*;
use crate::glue;

// ---------------------------------------------------------------------------
// "somebody is listening for X" — the advisory seam
// ---------------------------------------------------------------------------

/// Set once a hook exists, so the common case (none) is a relaxed load rather
/// than a lock on a path Vue walks for every element it patches.
static ANY_HOOK: AtomicBool = AtomicBool::new(false);

fn hooks() -> &'static RwLock<Vec<fn(&str)>> {
    static H: OnceLock<RwLock<Vec<fn(&str)>>> = OnceLock::new();
    H.get_or_init(|| RwLock::new(Vec::new()))
}

/// Registers `f` to be told the type of every listener added, anywhere.
///
/// Exists for one reason: a module that knows this runtime never fires some
/// event needs to say so, and the only moment worth saying it is when an
/// application actually listens for that event. An advisory printed at install
/// would fire for every app including the ones that never touch the API; a
/// silent no-op would be exactly the divergence-without-a-word this project
/// refuses. See [`crate::webapi::storage`]'s `storage` advisory.
///
/// Process-global and never removed — hooks are registered at install by
/// modules that live as long as the process.
pub fn on_listener_added(f: fn(&str)) {
    hooks().write().unwrap().push(f);
    ANY_HOOK.store(true, Ordering::Relaxed);
}

fn note_listener_added(ty: &str) {
    if !ANY_HOOK.load(Ordering::Relaxed) {
        return;
    }
    for f in hooks().read().unwrap().iter() {
        f(ty);
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const NONE: i32 = 0;
pub const CAPTURING_PHASE: i32 = 1;
pub const AT_TARGET: i32 = 2;
pub const BUBBLING_PHASE: i32 = 3;

const L_ONCE: i32 = 1;
const L_CAPTURE: i32 = 2;
const L_PASSIVE: i32 = 4;
const L_REMOVED: i32 = 8;

const KIND_OBJECT_MIN: u32 = 8; // rjs_kind: 8 object … 13 promise are all objects
const KIND_FUNCTION: u32 = 9;

unsafe fn is_object(g: *mut c_void, v: u64) -> bool {
    is_cell(v) && glue::rjs_kind(g, v) >= KIND_OBJECT_MIN
}

pub unsafe fn is_callable(g: *mut c_void, v: u64) -> bool {
    is_cell(v) && glue::rjs_kind(g, v) == KIND_FUNCTION
}

// ---------------------------------------------------------------------------
// Event
// ---------------------------------------------------------------------------

unsafe fn event_state(g: *mut c_void, this: u64) -> Option<u64> {
    if !is_cell(this) {
        return None;
    }
    let st = state(g);
    let s = get_a(g, this, st.s_event);
    if is_cell(s) { Some(s) } else { None }
}

/// The event's internal-slot bag, for modules that build Event subclasses.
pub unsafe fn state_of(g: *mut c_void, event: u64) -> Option<u64> {
    event_state(g, event)
}

macro_rules! event_getter {
    ($fn_name:ident, $field:literal) => {
        #[crate::host_fn]
        pub unsafe fn $fn_name(g: *mut c_void, cf: *mut c_void) -> u64 {
            match event_state(g, cf_this(cf)) {
                Some(s) => get(g, s, $field),
                None => throw_type_error(g, concat!("Event.prototype.", $field, " called on a non-Event")),
            }
        }
    };
}

event_getter!(get_type, "type");
event_getter!(get_bubbles, "bubbles");
event_getter!(get_cancelable, "cancelable");
event_getter!(get_composed, "composed");
event_getter!(get_default_prevented, "defaultPrevented");
event_getter!(get_target, "target");
event_getter!(get_current_target, "currentTarget");
event_getter!(get_time_stamp, "timeStamp");
event_getter!(get_is_trusted, "isTrusted");
event_getter!(get_detail, "detail");
event_getter!(get_event_phase, "phase");

/// DOM "set the canceled flag": only for a cancelable event, never from
/// inside a passive listener.
unsafe fn set_canceled(g: *mut c_void, s: u64) {
    if truthy_flag(get(g, s, "cancelable")) && !truthy_flag(get(g, s, "inPassive")) {
        define(g, s, "defaultPrevented", TRUE, 0);
    }
}

#[crate::host_fn]
unsafe fn prevent_default(g: *mut c_void, cf: *mut c_void) -> u64 {
    match event_state(g, cf_this(cf)) {
        Some(s) => {
            set_canceled(g, s);
            UNDEF
        }
        None => throw_type_error(g, "Event.prototype.preventDefault called on a non-Event"),
    }
}

#[crate::host_fn]
unsafe fn stop_propagation(g: *mut c_void, cf: *mut c_void) -> u64 {
    match event_state(g, cf_this(cf)) {
        Some(s) => {
            define(g, s, "stopProp", TRUE, 0);
            UNDEF
        }
        None => throw_type_error(g, "Event.prototype.stopPropagation called on a non-Event"),
    }
}

#[crate::host_fn]
unsafe fn stop_immediate(g: *mut c_void, cf: *mut c_void) -> u64 {
    match event_state(g, cf_this(cf)) {
        Some(s) => {
            define(g, s, "stopProp", TRUE, 0);
            define(g, s, "stopImmediate", TRUE, 0);
            UNDEF
        }
        None => throw_type_error(g, "Event.prototype.stopImmediatePropagation called on a non-Event"),
    }
}

/// The invocation targets of the path, target first — a fresh array each call,
/// empty outside dispatch (DOM: "if path is empty, return composedPath").
#[crate::host_fn]
unsafe fn composed_path(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(s) = event_state(g, cf_this(cf)) else {
        return throw_type_error(g, "Event.prototype.composedPath called on a non-Event");
    };
    let out = new_array(g);
    let path = get(g, s, "path");
    if is_cell(path) {
        let n = array_len(g, path);
        for i in 0..n {
            set_i(g, out, i, get_i(g, path, i));
        }
    }
    out
}

/// Legacy `cancelBubble`: reads the stop-propagation flag; setting `true` sets it.
#[crate::host_fn]
unsafe fn get_cancel_bubble(g: *mut c_void, cf: *mut c_void) -> u64 {
    match event_state(g, cf_this(cf)) {
        Some(s) => get(g, s, "stopProp"),
        None => throw_type_error(g, "Event.prototype.cancelBubble called on a non-Event"),
    }
}

#[crate::host_fn]
unsafe fn set_cancel_bubble(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Some(s) = event_state(g, cf_this(cf)) {
        if glue::rjs_to_boolean(g, cf_arg(cf, 0)) != 0 {
            define(g, s, "stopProp", TRUE, 0);
        }
    }
    UNDEF
}

/// Legacy `returnValue`: `!defaultPrevented`; setting `false` cancels.
#[crate::host_fn]
unsafe fn get_return_value(g: *mut c_void, cf: *mut c_void) -> u64 {
    match event_state(g, cf_this(cf)) {
        Some(s) => enc_bool(!truthy_flag(get(g, s, "defaultPrevented"))),
        None => throw_type_error(g, "Event.prototype.returnValue called on a non-Event"),
    }
}

#[crate::host_fn]
unsafe fn set_return_value(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Some(s) = event_state(g, cf_this(cf)) {
        if glue::rjs_to_boolean(g, cf_arg(cf, 0)) == 0 {
            set_canceled(g, s);
        }
    }
    UNDEF
}

/// Legacy `initEvent(type, bubbles = false, cancelable = false)`.
#[crate::host_fn]
unsafe fn init_event_legacy(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(s) = event_state(g, cf_this(cf)) else {
        return throw_type_error(g, "Event.prototype.initEvent called on a non-Event");
    };
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "Failed to execute 'initEvent' on 'Event': 1 argument required, but only 0 present.");
    }
    let Ok(ty) = to_js_string(g, cf_arg(cf, 0)) else { return UNDEF };
    if truthy_flag(get(g, s, "dispatching")) {
        return UNDEF;
    }
    define(g, s, "initialized", TRUE, 0);
    define(g, s, "stopProp", FALSE, 0);
    define(g, s, "stopImmediate", FALSE, 0);
    define(g, s, "defaultPrevented", FALSE, 0);
    define(g, s, "isTrusted", FALSE, 0);
    define(g, s, "target", NULL, 0);
    define(g, s, "type", ty, 0);
    define(g, s, "bubbles", enc_bool(glue::rjs_to_boolean(g, cf_arg(cf, 1)) != 0), 0);
    define(g, s, "cancelable", enc_bool(glue::rjs_to_boolean(g, cf_arg(cf, 2)) != 0), 0);
    UNDEF
}

/// A constructor's fresh event/state are not reachable from script yet. Init
/// getters may reenter JS and collect, so root the whole input graph until the
/// state bag is attached, including callers which pass temporary native values.
struct EventInitRoots {
    global: *mut c_void,
    values: [u64; 4],
}
impl EventInitRoots {
    unsafe fn new(global: *mut c_void, object: u64, ty: u64, init: u64) -> Self {
        let values = [object, ty, init, UNDEF];
        for value in values {
            if is_cell(value) { crate::ffi::redjs_protect(global, value); }
        }
        Self { global, values }
    }
    unsafe fn state(&mut self, value: u64) {
        self.values[3] = value;
        if is_cell(value) { crate::ffi::redjs_protect(self.global, value); }
    }
}
impl Drop for EventInitRoots {
    fn drop(&mut self) {
        for &value in self.values.iter().rev() {
            if is_cell(value) { unsafe { crate::ffi::redjs_unprotect(self.global, value); } }
        }
    }
}

/// Shared init for Event/CustomEvent constructors. `this_obj` is the fresh
/// ordinary object; fills the [[s_event]] slot from (type, initDict).
pub unsafe fn init_event(g: *mut c_void, this_obj: u64, ty: u64, init: u64, detail_from_init: bool) {
    let mut roots = EventInitRoots::new(g, this_obj, ty, init);
    let st = state(g);
    let s = new_object(g, UNDEF);
    roots.state(s);
    let type_str = read_string(g, ty).unwrap_or_default();
    define(g, s, "type", js_string(g, &type_str), 0);

    let flag = |name: &str| {
        if is_cell(init) {
            let v = get(g, init, name);
            let mut b = FALSE;
            if v != UNDEF {
                b = enc_bool(glue::rjs_to_boolean(g, v) != 0);
            }
            b
        } else {
            FALSE
        }
    };
    let bubbles = flag("bubbles");
    let cancelable = flag("cancelable");
    let composed = flag("composed");
    define(g, s, "bubbles", bubbles, 0);
    define(g, s, "cancelable", cancelable, 0);
    define(g, s, "composed", composed, 0);
    define(g, s, "defaultPrevented", FALSE, 0);
    define(g, s, "stopProp", FALSE, 0);
    define(g, s, "stopImmediate", FALSE, 0);
    define(g, s, "dispatching", FALSE, 0);
    define(g, s, "inPassive", FALSE, 0);
    define(g, s, "initialized", TRUE, 0);
    define(g, s, "phase", enc_i32(NONE), 0);
    define(g, s, "path", UNDEF, 0);
    define(g, s, "target", NULL, 0);
    define(g, s, "currentTarget", NULL, 0);
    define(g, s, "isTrusted", FALSE, 0);
    define(g, s, "timeStamp", enc_f64(crate::web::now_ms()), 0);
    if detail_from_init {
        let detail = if is_cell(init) { get(g, init, "detail") } else { UNDEF };
        define(g, s, "detail", if detail == UNDEF { NULL } else { detail }, 0);
    } else {
        define(g, s, "detail", NULL, 0);
    }

    glue::rjs_define(g, this_obj, st.s_event, s, 0);
}

#[crate::host_fn]
unsafe fn event_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "Event constructor requires a type argument");
    }
    let st = state(g);
    let obj = ordinary_create(g, cf_new_target(cf), st.event.proto);
    init_event(g, obj, cf_arg(cf, 0), cf_arg(cf, 1), false);
    obj
}

#[crate::host_fn]
unsafe fn custom_event_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "CustomEvent constructor requires a type argument");
    }
    let st = state(g);
    let obj = ordinary_create(g, cf_new_target(cf), st.custom_event.proto);
    init_event(g, obj, cf_arg(cf, 0), cf_arg(cf, 1), true);
    obj
}

/// Creates a trusted Event of `type` (native dispatches: load/resize/…) that
/// neither bubbles nor can be canceled — the shape of most UA-fired events.
pub unsafe fn new_trusted_event(g: *mut c_void, ty: &str) -> u64 {
    new_trusted_event_with(g, ty, false, false)
}

/// A trusted Event with explicit `bubbles` / `cancelable` (IndexedDB's
/// `error` bubbles and cancels; `DOMContentLoaded` bubbles).
pub unsafe fn new_trusted_event_with(g: *mut c_void, ty: &str, bubbles: bool, cancelable: bool) -> u64 {
    let st = state(g);
    let obj = new_object(g, st.event.proto);
    init_event(g, obj, js_string(g, ty), UNDEF, false);
    let s = get_a(g, obj, st.s_event);
    define(g, s, "isTrusted", TRUE, 0);
    define(g, s, "bubbles", enc_bool(bubbles), 0);
    define(g, s, "cancelable", enc_bool(cancelable), 0);
    obj
}

// ---------------------------------------------------------------------------
// EventTarget: listener lists
// ---------------------------------------------------------------------------

/// `this` for the target methods: undefined/null (bare `addEventListener(...)`
/// call at global scope) means the global object — window semantics.
unsafe fn target_this(g: *mut c_void, cf: *mut c_void) -> u64 {
    let t = cf_this(cf);
    if !is_cell(t) {
        return glue::rjs_global(g);
    }
    // window/globalThis is a proxy over the JSGlobalObject; normalize so
    // window.addEventListener and bare addEventListener share one slot.
    let st = state(g);
    if t == st.global_proxy { glue::rjs_global(g) } else { t }
}

/// What JS sees for a path node: the global object's proxy (`globalThis`),
/// never the raw JSGlobalObject the slots are kept on.
unsafe fn expose(g: *mut c_void, t: u64) -> u64 {
    if t == glue::rjs_global(g) { state(g).global_proxy } else { t }
}

unsafe fn listeners_of(g: *mut c_void, target: u64, create: bool) -> u64 {
    let st = state(g);
    let l = get_a(g, target, st.s_listeners);
    if is_cell(l) {
        return l;
    }
    if !create {
        return UNDEF;
    }
    let arr = new_array(g);
    glue::rjs_define(g, target, st.s_listeners, arr, 0);
    arr
}

unsafe fn entry_flags(g: *mut c_void, e: u64) -> i32 {
    dec_f64(get_a(g, e, state(g).a_flags)).unwrap_or(0.0) as i32
}

/// Is anything (a listener or the `on<type>` handler) listening for `ty` on
/// `target`? For modules that only do work when somebody is.
pub unsafe fn has_listener(g: *mut c_void, target: u64, ty: &str) -> bool {
    let st = state(g);
    let list = listeners_of(g, target, false);
    if is_cell(list) {
        for i in 0..array_len(g, list) {
            let e = get_i(g, list, i);
            if read_string(g, get_a(g, e, st.a_type)).as_deref() == Some(ty) {
                return true;
            }
        }
    }
    is_callable(g, get(g, target, &format!("on{ty}")))
}

/// DOM "remove an event listener": set `removed` (so a dispatch that already
/// cloned the list skips it) and drop it from the list. The list is replaced,
/// never mutated, so a clone in flight keeps its entries alive.
pub unsafe fn remove_entry(g: *mut c_void, target: u64, entry: u64) {
    let st = state(g);
    glue::rjs_define(g, entry, st.a_flags, enc_i32(entry_flags(g, entry) | L_REMOVED), 0);
    let list = listeners_of(g, target, false);
    if !is_cell(list) {
        return;
    }
    let arr = new_array(g);
    let mut j = 0u32;
    for i in 0..array_len(g, list) {
        let e = get_i(g, list, i);
        if e != entry {
            set_i(g, arr, j, e);
            j += 1;
        }
    }
    glue::rjs_define(g, target, st.s_listeners, arr, 0);
}

struct Flattened {
    capture: bool,
    once: bool,
    /// `None` = not given (the default passive value applies).
    passive: Option<bool>,
    signal: Option<u64>,
}

/// DOM "flatten more" over `boolean or AddEventListenerOptions` (with `more`
/// false it is plain "flatten": only `capture`). Dictionary members are read
/// in WebIDL order — inherited `capture`, then `once`, `passive`, `signal` —
/// and what a getter throws propagates.
unsafe fn flatten(g: *mut c_void, options: u64, more: bool, method: &str) -> Result<Flattened, ()> {
    let mut f = Flattened { capture: false, once: false, passive: None, signal: None };
    if options == UNDEF || options == NULL {
        return Ok(f);
    }
    if !is_object(g, options) {
        f.capture = glue::rjs_to_boolean(g, options) != 0;
        return Ok(f);
    }
    f.capture = glue::rjs_to_boolean(g, try_get(g, options, "capture")?) != 0;
    if !more {
        return Ok(f);
    }
    f.once = glue::rjs_to_boolean(g, try_get(g, options, "once")?) != 0;
    let p = try_get(g, options, "passive")?;
    if p != UNDEF {
        f.passive = Some(glue::rjs_to_boolean(g, p) != 0);
    }
    let sig = try_get(g, options, "signal")?;
    if sig != UNDEF {
        if !super::abort::is_signal(g, sig) {
            throw_type_error(
                g,
                &format!(
                    "Failed to execute '{method}' on 'EventTarget': Failed to read the 'signal' property from \
                     'AddEventListenerOptions': Failed to convert value to 'AbortSignal'."
                ),
            );
            return Err(());
        }
        f.signal = Some(sig);
    }
    Ok(f)
}

/// The listener callback argument: an object (callable or with handleEvent),
/// or null/undefined (→ `None`, nothing happens). Anything else is the IDL's
/// TypeError.
unsafe fn callback_arg(g: *mut c_void, cb: u64, method: &str) -> Result<Option<u64>, ()> {
    if cb == UNDEF || cb == NULL {
        return Ok(None);
    }
    if !is_object(g, cb) {
        throw_type_error(
            g,
            &format!("Failed to execute '{method}' on 'EventTarget': parameter 2 is not of type 'Object'."),
        );
        return Err(());
    }
    Ok(Some(cb))
}

/// DOM "default passive value": true for touch/wheel listeners on the window
/// or the document, false otherwise.
unsafe fn default_passive(g: *mut c_void, target: u64, ty: &str) -> bool {
    matches!(ty, "touchstart" | "touchmove" | "wheel" | "mousewheel")
        && (target == glue::rjs_global(g) || target == state(g).document)
}

unsafe fn find_entry(g: *mut c_void, list: u64, ty: u64, cb: u64, capture: bool) -> Option<u64> {
    let st = state(g);
    for i in 0..array_len(g, list) {
        let e = get_i(g, list, i);
        if get_a(g, e, st.a_cb) == cb
            && ((entry_flags(g, e) & L_CAPTURE) != 0) == capture
            && glue::rjs_strict_equals(g, get_a(g, e, st.a_type), ty) != 0
        {
            return Some(e);
        }
    }
    None
}

#[crate::host_fn]
pub unsafe fn add_event_listener(g: *mut c_void, cf: *mut c_void) -> u64 {
    let target = target_this(g, cf);
    if cf_argc(cf) < 2 {
        return throw_type_error(
            g,
            &format!("Failed to execute 'addEventListener' on 'EventTarget': 2 arguments required, but only {} present.", cf_argc(cf)),
        );
    }
    let Ok(ty) = to_js_string(g, cf_arg(cf, 0)) else { return UNDEF };
    let Ok(cb) = callback_arg(g, cf_arg(cf, 1), "addEventListener") else { return UNDEF };
    let Ok(opts) = flatten(g, cf_arg(cf, 2), true, "addEventListener") else { return UNDEF };
    if let Some(sig) = opts.signal {
        if super::abort::is_aborted(g, sig) {
            return UNDEF;
        }
    }
    let Some(cb) = cb else { return UNDEF };
    let st = state(g);
    let type_str = read_string(g, ty).unwrap_or_default();
    note_listener_added(&type_str);
    let list = listeners_of(g, target, true);
    if find_entry(g, list, ty, cb, opts.capture).is_some() {
        return UNDEF; // DOM: an identical (type, callback, capture) is not added twice
    }
    let passive = opts.passive.unwrap_or_else(|| default_passive(g, target, &type_str));
    let flags = (if opts.once { L_ONCE } else { 0 })
        | (if opts.capture { L_CAPTURE } else { 0 })
        | (if passive { L_PASSIVE } else { 0 });
    let entry = new_object(g, NULL);
    glue::rjs_define(g, entry, st.a_type, ty, 0);
    glue::rjs_define(g, entry, st.a_cb, cb, 0);
    glue::rjs_define(g, entry, st.a_flags, enc_i32(flags), 0);
    set_i(g, list, array_len(g, list), entry);
    if let Some(sig) = opts.signal {
        super::abort::on_abort_remove_listener(g, sig, target, entry);
    }
    UNDEF
}

#[crate::host_fn]
pub unsafe fn remove_event_listener(g: *mut c_void, cf: *mut c_void) -> u64 {
    let target = target_this(g, cf);
    if cf_argc(cf) < 2 {
        return throw_type_error(
            g,
            &format!("Failed to execute 'removeEventListener' on 'EventTarget': 2 arguments required, but only {} present.", cf_argc(cf)),
        );
    }
    let Ok(ty) = to_js_string(g, cf_arg(cf, 0)) else { return UNDEF };
    let Ok(cb) = callback_arg(g, cf_arg(cf, 1), "removeEventListener") else { return UNDEF };
    let Ok(opts) = flatten(g, cf_arg(cf, 2), false, "removeEventListener") else { return UNDEF };
    let Some(cb) = cb else { return UNDEF };
    let list = listeners_of(g, target, false);
    if !is_cell(list) {
        return UNDEF;
    }
    if let Some(e) = find_entry(g, list, ty, cb, opts.capture) {
        remove_entry(g, target, e);
    }
    UNDEF
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// What a dispatch observed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DispatchOutcome {
    /// The event's canceled flag at the end (`preventDefault()` took).
    pub canceled: bool,
    /// Some listener or handler threw (and was reported) — DOM's
    /// "legacyOutputDidListenersThrowFlag".
    pub listener_threw: bool,
}

/// The path a dispatch at `target` takes in this runtime: the target, then
/// DOM's "get the parent" for the one target that has one — the document's
/// parent is the global, except for `load`.
unsafe fn path_for(g: *mut c_void, target: u64, ty: u64) -> Vec<u64> {
    let st = state(g);
    let mut path = vec![target];
    if target == st.document && is_cell(st.document) && read_string(g, ty).as_deref() != Some("load") {
        path.push(glue::rjs_global(g));
    }
    path
}

/// Native dispatch at `target` (image load, resize, abort, socket events…):
/// the path [`path_for`] builds. Returns `!canceled`, as `dispatchEvent` does.
pub unsafe fn dispatch(g: *mut c_void, target: u64, event: u64) -> bool {
    let Some(s) = event_state(g, event) else { return true };
    let path = path_for(g, target, get(g, s, "type"));
    !dispatch_path(g, event, &path).canceled
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    Capturing,
    Bubbling,
}

/// DOM §2.9 "dispatch" over an explicit path, `path[0]` being the target.
/// See the module header for exactly what is and is not modelled.
///
/// # Safety
/// `g` live, on its VM thread; `event` a redjs Event; `path` non-empty and
/// every node reachable for the duration (the caller holds them).
pub unsafe fn dispatch_path(g: *mut c_void, event: u64, path: &[u64]) -> DispatchOutcome {
    let mut out = DispatchOutcome::default();
    let Some(s) = event_state(g, event) else { return out };
    let Some(&target) = path.first() else { return out };

    define(g, s, "dispatching", TRUE, 0);
    define(g, s, "target", expose(g, target), 0);
    let js_path = new_array(g);
    for (i, n) in path.iter().enumerate() {
        set_i(g, js_path, i as u32, expose(g, *n));
    }
    define(g, s, "path", js_path, 0);

    let ty = get(g, s, "type");
    let type_str = read_string(g, ty).unwrap_or_default();
    let bubbles = truthy_flag(get(g, s, "bubbles"));

    for (i, node) in path.iter().enumerate().rev() {
        let phase = if i == 0 { AT_TARGET } else { CAPTURING_PHASE };
        define(g, s, "phase", enc_i32(phase), 0);
        invoke(g, s, event, *node, ty, &type_str, Pass::Capturing, &mut out);
    }
    for (i, node) in path.iter().enumerate() {
        let phase = if i == 0 {
            AT_TARGET
        } else if bubbles {
            BUBBLING_PHASE
        } else {
            continue;
        };
        define(g, s, "phase", enc_i32(phase), 0);
        invoke(g, s, event, *node, ty, &type_str, Pass::Bubbling, &mut out);
    }

    define(g, s, "phase", enc_i32(NONE), 0);
    define(g, s, "currentTarget", NULL, 0);
    define(g, s, "path", UNDEF, 0);
    define(g, s, "dispatching", FALSE, 0);
    define(g, s, "stopProp", FALSE, 0);
    define(g, s, "stopImmediate", FALSE, 0);
    out.canceled = truthy_flag(get(g, s, "defaultPrevented"));
    out
}

/// DOM "invoke" + "inner invoke" for one path node and one pass.
#[allow(clippy::too_many_arguments)]
unsafe fn invoke(
    g: *mut c_void,
    s: u64,
    event: u64,
    node: u64,
    ty: u64,
    type_str: &str,
    pass: Pass,
    out: &mut DispatchOutcome,
) {
    if truthy_flag(get(g, s, "stopProp")) {
        return;
    }
    let st = state(g);
    let this = expose(g, node);
    define(g, s, "currentTarget", this, 0);

    // The clone DOM takes: the list as it is NOW, by length. Additions land
    // past `n` (or in a replacement array) and do not run; removals set
    // `removed` on the entry, which is checked per call.
    let list = listeners_of(g, node, false);
    if is_cell(list) {
        let n = array_len(g, list);
        // Removal replaces the list, so this array may become unreachable from
        // the target while its entries are still being called.
        crate::ffi::redjs_protect(g, list);
        for i in 0..n {
            let e = get_i(g, list, i);
            let flags = entry_flags(g, e);
            if flags & L_REMOVED != 0 || glue::rjs_strict_equals(g, get_a(g, e, st.a_type), ty) == 0 {
                continue;
            }
            let capture = flags & L_CAPTURE != 0;
            if (pass == Pass::Capturing) != capture {
                continue;
            }
            if flags & L_ONCE != 0 {
                remove_entry(g, node, e);
            }
            if flags & L_PASSIVE != 0 {
                define(g, s, "inPassive", TRUE, 0);
            }
            if let Err(exc) = call_listener(g, get_a(g, e, st.a_cb), this, event) {
                crate::errors::report(g, exc, crate::errors::ErrKind::Listener);
                out.listener_threw = true;
            }
            define(g, s, "inPassive", FALSE, 0);
            between_listeners(g);
            if truthy_flag(get(g, s, "stopImmediate")) {
                break;
            }
        }
        crate::ffi::redjs_unprotect(g, list);
    }

    // The event handler IDL attribute — a non-capture listener of its own.
    if pass == Pass::Bubbling && !truthy_flag(get(g, s, "stopImmediate")) {
        let handler = get(g, node, &format!("on{type_str}"));
        if is_callable(g, handler) {
            match call(g, handler, this, &[event]) {
                // HTML "process return value": `false` cancels (a cancelable event).
                Ok(rv) if rv == FALSE => set_canceled(g, s),
                Ok(_) => {}
                Err(exc) => {
                    crate::errors::report(g, exc, crate::errors::ErrKind::Listener);
                    out.listener_threw = true;
                }
            }
            between_listeners(g);
        }
    }
}

/// WebIDL "call a user object's operation": a callable is called with `this`
/// = the current target; any other object has its `handleEvent` looked up NOW
/// (a getter runs, and what it throws is the listener's exception).
unsafe fn call_listener(g: *mut c_void, cb: u64, this: u64, event: u64) -> Result<u64, u64> {
    if is_callable(g, cb) {
        let _profile = crate::profiling::js_span(g, cb, "Event listener callback");
        return call(g, cb, this, &[event]);
    }
    let st = state(g);
    let mut he = UNDEF;
    if glue::rjs_get(g, cb, st.a_handle_event, &mut he) != 0 {
        return Err(he);
    }
    if !is_callable(g, he) {
        let m: Vec<u16> = "The listener's handleEvent is not a function".encode_utf16().collect();
        let mut err = UNDEF;
        glue::rjs_error_create(g, 1, m.as_ptr(), m.len(), &mut err);
        return Err(err);
    }
    let _profile = crate::profiling::js_span(g, he, "handleEvent callback");
    call(g, he, cb, &[event])
}

/// HTML "clean up after running script": a callback run with nothing else on
/// the JS stack is followed by a microtask checkpoint. Nothing happens for a
/// dispatch that script started (`dispatchEvent`): its stack is not empty.
unsafe fn between_listeners(g: *mut c_void) {
    if glue::rjs_in_js(g) == 0 {
        crate::web::checkpoint(g);
    }
}

#[crate::host_fn]
pub unsafe fn dispatch_event(g: *mut c_void, cf: *mut c_void) -> u64 {
    let target = target_this(g, cf);
    let event = cf_arg(cf, 0);
    let Some(s) = event_state(g, event) else {
        return throw_type_error(g, "Failed to execute 'dispatchEvent' on 'EventTarget': parameter 1 is not of type 'Event'.");
    };
    if truthy_flag(get(g, s, "dispatching")) || !truthy_flag(get(g, s, "initialized")) {
        return super::domexception::throw(
            g,
            "InvalidStateError",
            "Failed to execute 'dispatchEvent' on 'EventTarget': The event is already being dispatched.",
        );
    }
    define(g, s, "isTrusted", FALSE, 0);
    let path = path_for(g, target, get(g, s, "type"));
    enc_bool(!dispatch_path(g, event, &path).canceled)
}

#[crate::host_fn]
unsafe fn event_target_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let st = state(g);
    let obj = ordinary_create(g, cf_new_target(cf), st.event_target.proto);
    // The listener list up front: it is also what makes `new EventTarget()`
    // (and every subclass instance) a platform object to the structured
    // serializer — a DataCloneError, as in a browser, not a cloned `{}`.
    listeners_of(g, obj, true);
    obj
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

pub unsafe fn install(g: *mut c_void) -> (Class, Class, Class) {
    let event = define_class(g, &ClassSpec {
        name: "Event",
        arity: 1,
        construct: Some(event_construct),
        parent_proto: 0,
        methods: &[
            ("preventDefault", 0, prevent_default),
            ("stopPropagation", 0, stop_propagation),
            ("stopImmediatePropagation", 0, stop_immediate),
            ("composedPath", 0, composed_path),
            ("initEvent", 1, init_event_legacy),
        ],
        getters: &[
            ("type", get_type),
            ("bubbles", get_bubbles),
            ("cancelable", get_cancelable),
            ("composed", get_composed),
            ("defaultPrevented", get_default_prevented),
            ("target", get_target),
            ("currentTarget", get_current_target),
            ("srcElement", get_target),
            ("timeStamp", get_time_stamp),
            ("isTrusted", get_is_trusted),
            ("eventPhase", get_event_phase),
        ],
        accessors: &[
            ("cancelBubble", get_cancel_bubble, set_cancel_bubble),
            ("returnValue", get_return_value, set_return_value),
        ],
    });
    for (name, v) in [("NONE", NONE), ("CAPTURING_PHASE", CAPTURING_PHASE), ("AT_TARGET", AT_TARGET), ("BUBBLING_PHASE", BUBBLING_PHASE)] {
        // WebIDL constants: { writable: false, enumerable: true, configurable: false }.
        define(g, event.ctor, name, enc_i32(v), 1 | 4);
        define(g, event.proto, name, enc_i32(v), 1 | 4);
    }

    let custom_event = define_class(g, &ClassSpec {
        name: "CustomEvent",
        arity: 1,
        construct: Some(custom_event_construct),
        parent_proto: event.proto,
        methods: &[],
        getters: &[("detail", get_detail)],
        accessors: &[],
    });

    let event_target = define_class(g, &ClassSpec {
        name: "EventTarget",
        arity: 0,
        construct: Some(event_target_construct),
        parent_proto: 0,
        methods: &[
            ("addEventListener", 2, add_event_listener),
            ("removeEventListener", 2, remove_event_listener),
            ("dispatchEvent", 1, dispatch_event),
        ],
        getters: &[],
        accessors: &[],
    });

    (event, custom_event, event_target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webapi::testing::*;

    #[crate::host_fn]
    unsafe fn force_init_gc(g: *mut c_void, _cf: *mut c_void) -> u64 {
        glue::rjs_gc(g, 0);
        UNDEF
    }

    #[test]
    fn constructor_state_survives_gc_and_reentry_in_init_getters() {
        let c = ctx();
        let collect = unsafe { c.function_sysv("__eventInitGc", 0, force_init_gc as *const c_void) }.unwrap();
        c.global_set("__eventInitGc", collect).unwrap();
        ok(&c, r#"(() => {
            for (let index = 0; index < 12; index++) {
                const type = 'event-' + index + '-' + Array.from({length: 512}, (_, i) => i).join('x');
                const seen = [];
                const event = new Event({toString() {__eventInitGc(); return type;}}, {
                    get bubbles() {
                        const nested = new Event('nested', {get cancelable() {__eventInitGc(); return true;}});
                        if (!nested.cancelable) throw Error('nested event lost state');
                        __eventInitGc(); seen.push('bubbles'); return true;
                    },
                    get cancelable() {__eventInitGc(); seen.push('cancelable'); return true;},
                    get composed() {__eventInitGc(); seen.push('composed'); return true;}
                });
                if (event.type !== type || !event.bubbles || !event.cancelable || !event.composed || seen.join() !== 'bubbles,cancelable,composed') return false;
                const custom = new CustomEvent(type, {
                    get bubbles() {__eventInitGc(); return true;},
                    get cancelable() {__eventInitGc(); return true;},
                    get detail() {const detail = {marker: 'detail-' + index, bytes: new Uint8Array([index, 7, 9])}; __eventInitGc(); return detail;}
                });
                __eventInitGc();
                const target = new EventTarget(); let dispatched = false;
                target.addEventListener(type, function (received) {
                    __eventInitGc();
                    dispatched = received === custom && received.target === target && received.currentTarget === target && received.type === type && received.detail.marker === 'detail-' + index && received.detail.bytes[0] === index;
                    received.preventDefault();
                });
                if (target.dispatchEvent(custom) !== false || !dispatched || !custom.defaultPrevented || custom.currentTarget !== null || custom.eventPhase !== 0) return false;
            }
            return true;
        })()"#);
    }

    #[test]
    fn phases_and_order_over_an_explicit_path() {
        // request → transaction → database, the IndexedDB shape.
        let c = ctx();
        s(
            &c,
            "globalThis.log = [];
             globalThis.req = new EventTarget(); globalThis.tx = new EventTarget(); globalThis.db = new EventTarget();
             const rec = (who, tag) => e => log.push(who + ':' + tag + ':' + e.eventPhase + ':' +
                 (e.currentTarget === globalThis[who]) + ':' + (e.target === req));
             for (const who of ['req', 'tx', 'db']) {
               globalThis[who].addEventListener('error', rec(who, 'cap'), true);
               globalThis[who].addEventListener('error', rec(who, 'bub'));
             }
             db.onerror = e => log.push('db:handler:' + e.eventPhase);
             globalThis.ev = new Event('error', { bubbles: true, cancelable: true }); 0",
        );
        let g = c.raw();
        let out = unsafe {
            let global = glue::rjs_global(g);
            let path = [get(g, global, "req"), get(g, global, "tx"), get(g, global, "db")];
            dispatch_path(g, get(g, global, "ev"), &path)
        };
        assert_eq!(out, DispatchOutcome { canceled: false, listener_threw: false });
        assert_eq!(
            s(&c, "log.join(' ')"),
            "db:cap:1:true:true tx:cap:1:true:true req:cap:2:true:true req:bub:2:true:true \
             tx:bub:3:true:true db:bub:3:true:true db:handler:3"
        );
        // After dispatch: phase NONE, currentTarget null, target kept, path empty.
        ok(&c, "ev.eventPhase === 0 && ev.currentTarget === null && ev.target === req && ev.composedPath().length === 0");
    }

    #[test]
    fn a_non_bubbling_event_still_captures_through_ancestors() {
        let c = ctx();
        s(
            &c,
            "globalThis.log = [];
             globalThis.a = new EventTarget(); globalThis.b = new EventTarget();
             a.addEventListener('x', () => log.push('a'));
             b.addEventListener('x', () => log.push('b-cap'), { capture: true });
             b.addEventListener('x', () => log.push('b-bub'));
             globalThis.ev = new Event('x'); 0",
        );
        let g = c.raw();
        unsafe {
            let global = glue::rjs_global(g);
            dispatch_path(g, get(g, global, "ev"), &[get(g, global, "a"), get(g, global, "b")]);
        }
        assert_eq!(s(&c, "log.join()"), "b-cap,a");
    }

    #[test]
    fn stop_propagation_and_stop_immediate() {
        let c = ctx();
        s(
            &c,
            "globalThis.log = [];
             globalThis.a = new EventTarget(); globalThis.b = new EventTarget();
             b.addEventListener('x', e => { log.push('b-cap'); e.stopPropagation(); log.push(e.cancelBubble); }, true);
             b.addEventListener('x', () => log.push('b-cap2'), true);
             a.addEventListener('x', () => log.push('a'));
             globalThis.ev = new Event('x', { bubbles: true }); 0",
        );
        let g = c.raw();
        unsafe {
            let global = glue::rjs_global(g);
            dispatch_path(g, get(g, global, "ev"), &[get(g, global, "a"), get(g, global, "b")]);
        }
        // stopPropagation lets the rest of THIS node run, nothing after it.
        assert_eq!(s(&c, "log.join()"), "b-cap,true,b-cap2");
        ok(&c, "ev.cancelBubble === false"); // cleared at the end of dispatch
        ok(
            &c,
            "(() => { const t = new EventTarget(); const l = [];
               t.addEventListener('y', e => { l.push(1); e.stopImmediatePropagation(); });
               t.addEventListener('y', () => l.push(2));
               t.dispatchEvent(new Event('y')); return l.join() === '1'; })()",
        );
    }

    #[test]
    fn listener_identity_is_type_callback_capture() {
        let c = ctx();
        ok(
            &c,
            "(() => { const t = new EventTarget(); let n = 0; const f = () => n++;
               t.addEventListener('x', f); t.addEventListener('x', f); t.addEventListener('x', f, true);
               t.dispatchEvent(new Event('x'));
               if (n !== 2) return 'twice? ' + n;
               t.removeEventListener('x', f, { capture: true });
               t.dispatchEvent(new Event('x'));
               if (n !== 3) return 'after removing capture: ' + n;
               t.removeEventListener('x', f);
               t.dispatchEvent(new Event('x'));
               return n === 3; })()",
        );
    }

    #[test]
    fn a_listener_removed_mid_dispatch_does_not_run_and_one_added_does_not_either() {
        let c = ctx();
        ok(
            &c,
            "(() => { const t = new EventTarget(); const l = [];
               const second = () => l.push('second');
               t.addEventListener('x', () => { l.push('first'); t.removeEventListener('x', second);
                                               t.addEventListener('x', () => l.push('added')); });
               t.addEventListener('x', second);
               t.dispatchEvent(new Event('x'));
               return l.join() === 'first'; })()",
        );
    }

    #[test]
    fn once_passive_and_signal() {
        let c = ctx();
        ok(
            &c,
            "(() => { const t = new EventTarget(); let n = 0;
               t.addEventListener('x', () => n++, { once: true });
               t.dispatchEvent(new Event('x')); t.dispatchEvent(new Event('x'));
               return n === 1; })()",
        );
        ok(
            &c,
            "(() => { const t = new EventTarget(); let prevented = null;
               t.addEventListener('x', e => { e.preventDefault(); prevented = e.defaultPrevented; }, { passive: true });
               const r = t.dispatchEvent(new Event('x', { cancelable: true }));
               return prevented === false && r === true; })()",
        );
        ok(
            &c,
            "(() => { const t = new EventTarget(); const ac = new AbortController(); let n = 0;
               t.addEventListener('x', () => n++, { signal: ac.signal });
               t.dispatchEvent(new Event('x')); ac.abort(); t.dispatchEvent(new Event('x'));
               const pre = AbortSignal.abort();
               t.addEventListener('x', () => n += 100, { signal: pre });
               t.dispatchEvent(new Event('x'));
               return n === 1; })()",
        );
        let m = throws(&c, "new EventTarget().addEventListener('x', () => {}, { signal: {} })");
        assert!(m.contains("AbortSignal"), "{m}");
        // Option getters run in WebIDL order.
        ok(
            &c,
            "(() => { const order = []; const o = {};
               for (const k of ['signal', 'passive', 'once', 'capture'])
                 Object.defineProperty(o, k, { get() { order.push(k); return undefined; } });
               new EventTarget().addEventListener('x', () => {}, o);
               return order.join() === 'capture,once,passive,signal'; })()",
        );
    }

    #[test]
    fn handle_event_objects_and_argument_checks() {
        let c = ctx();
        ok(
            &c,
            "(() => { const t = new EventTarget(); let self = null;
               const o = { handleEvent(e) { self = this; } };
               t.addEventListener('x', o); t.dispatchEvent(new Event('x')); return self === o; })()",
        );
        let m = throws(&c, "new EventTarget().addEventListener('x', 5)");
        assert!(m.contains("not of type 'Object'"), "{m}");
        ok(&c, "new EventTarget().addEventListener('x', null) === undefined");
        let m = throws(&c, "new EventTarget().dispatchEvent({})");
        assert!(m.contains("not of type 'Event'"), "{m}");
    }

    #[test]
    fn redispatching_an_event_in_flight_is_an_invalid_state_error() {
        let c = ctx();
        ok(
            &c,
            "(() => { const t = new EventTarget(); let err = null; const ev = new Event('x');
               t.addEventListener('x', e => { try { t.dispatchEvent(e); } catch (x) { err = x; } });
               t.dispatchEvent(ev);
               return err instanceof DOMException && err.name === 'InvalidStateError'; })()",
        );
    }

    #[test]
    fn handlers_return_false_cancels_and_this_is_the_target() {
        let c = ctx();
        ok(
            &c,
            "(() => { const t = new EventTarget(); let self = null;
               t.onx = function () { self = this; return false; };
               const r = t.dispatchEvent(new Event('x', { cancelable: true }));
               return r === false && self === t; })()",
        );
    }

    #[test]
    fn a_throwing_listener_is_reported_and_the_rest_still_run() {
        let c = ctx();
        s(
            &c,
            "globalThis.log = []; globalThis.t = new EventTarget();
             t.addEventListener('x', () => { throw new Error('boom'); });
             t.addEventListener('x', () => log.push('after'));
             globalThis.ev = new Event('x'); 0",
        );
        let g = c.raw();
        let out = unsafe {
            let global = glue::rjs_global(g);
            dispatch_path(g, get(g, global, "ev"), &[get(g, global, "t")])
        };
        assert!(out.listener_threw);
        assert_eq!(s(&c, "log.join()"), "after");
    }

    #[test]
    fn a_native_dispatch_checkpoints_between_listeners_and_script_dispatch_does_not() {
        let c = ctx();
        s(
            &c,
            "globalThis.log = []; globalThis.t = new EventTarget();
             t.addEventListener('x', () => { log.push('l1'); Promise.resolve().then(() => log.push('micro')); });
             t.addEventListener('x', () => log.push('l2'));
             0",
        );
        // The embedder's frame holds the lock, as vue-native's pump does — so
        // the only drains are the dispatcher's own.
        let g = c.raw();
        unsafe {
            glue::rjs_lock(g);
            let global = glue::rjs_global(g);
            let ev = new_trusted_event(g, "x");
            dispatch(g, get(g, global, "t"), ev);
            glue::rjs_unlock(g);
        }
        assert_eq!(s(&c, "log.join()"), "l1,micro,l2");
        // From script: microtasks wait for the script to finish.
        assert_eq!(s(&c, "log = []; t.dispatchEvent(new Event('x')); log.join()"), "l1,l2");
    }

    #[test]
    fn document_events_reach_the_window_except_load() {
        let c = ctx();
        ok(
            &c,
            "(() => { const l = [];
               window.addEventListener('ping', e => l.push('win:' + e.eventPhase + ':' + (e.currentTarget === window)));
               document.addEventListener('ping', () => l.push('doc'));
               document.dispatchEvent(new Event('ping', { bubbles: true }));
               window.addEventListener('load', () => l.push('win-load'));
               document.dispatchEvent(new Event('load', { bubbles: true }));
               return l.join() === 'doc,win:3:true'; })()",
        );
    }

    #[test]
    fn legacy_members_and_constants() {
        let c = ctx();
        ok(&c, "Event.AT_TARGET === 2 && Event.prototype.BUBBLING_PHASE === 3 && Event.NONE === 0");
        ok(
            &c,
            "(() => { const e = new Event('a'); e.initEvent('b', true, true);
               return e.type === 'b' && e.bubbles && e.cancelable && e.returnValue === true; })()",
        );
        ok(
            &c,
            "(() => { const t = new EventTarget(); t.addEventListener('x', e => { e.returnValue = false; });
               return t.dispatchEvent(new Event('x', { cancelable: true })) === false; })()",
        );
    }
}
