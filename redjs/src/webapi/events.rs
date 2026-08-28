//! Native `Event`, `CustomEvent` and `EventTarget` (WHATWG DOM §2), the
//! dispatch machinery the rest of the web API builds on.
//!
//! Internal state lives in private-name slots (invisible to JS):
//! - Event:      [[s_event]] → plain object {type,bubbles,cancelable,composed,
//!               defaultPrevented,stopImmediate,target,currentTarget,detail,
//!               timeStamp,isTrusted}
//! - EventTarget:[[s_listeners]] → JS Array of {type,cb,once} entries
//!
//! Spec behaviors covered: readonly prototype getters, canceled-only-if-
//! cancelable preventDefault, listener once/duplicate-suppression,
//! `handleEvent` objects, stopImmediatePropagation, dispatch snapshotting,
//! return value !defaultPrevented, constructor newTarget subclassing.
//! Not covered (no DOM tree): capture/bubble phases — dispatch is single-
//! phase AT_TARGET, `capture` only participates in listener identity.

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
// Event
// ---------------------------------------------------------------------------

unsafe fn event_state(g: *mut c_void, this: u64) -> Option<u64> {
    let st = state(g);
    let s = get_a(g, this, st.s_event);
    if is_cell(s) { Some(s) } else { None }
}

macro_rules! event_getter {
    ($fn_name:ident, $field:literal, $default:expr) => {
        pub unsafe extern "sysv64" fn $fn_name(g: *mut c_void, cf: *mut c_void) -> u64 {
            match event_state(g, cf_this(cf)) {
                Some(s) => get(g, s, $field),
                None => throw_type_error(g, concat!("Event.prototype.", $field, " called on a non-Event")),
            }
        }
    };
}

event_getter!(get_type, "type", UNDEF);
event_getter!(get_bubbles, "bubbles", FALSE);
event_getter!(get_cancelable, "cancelable", FALSE);
event_getter!(get_composed, "composed", FALSE);
event_getter!(get_default_prevented, "defaultPrevented", FALSE);
event_getter!(get_target, "target", NULL);
event_getter!(get_current_target, "currentTarget", NULL);
event_getter!(get_time_stamp, "timeStamp", UNDEF);
event_getter!(get_is_trusted, "isTrusted", FALSE);
event_getter!(get_detail, "detail", NULL);

unsafe extern "sysv64" fn get_event_phase(g: *mut c_void, cf: *mut c_void) -> u64 {
    match event_state(g, cf_this(cf)) {
        Some(s) => {
            // 0 NONE before/after dispatch, 2 AT_TARGET while dispatching.
            let dispatching = truthy_flag(get(g, s, "dispatching"));
            enc_i32(if dispatching { 2 } else { 0 })
        }
        None => throw_type_error(g, "Event.prototype.eventPhase called on a non-Event"),
    }
}

unsafe extern "sysv64" fn prevent_default(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Some(s) = event_state(g, cf_this(cf)) {
        if truthy_flag(get(g, s, "cancelable")) {
            define(g, s, "defaultPrevented", TRUE, 0);
        }
    }
    UNDEF
}

unsafe extern "sysv64" fn stop_propagation(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Some(s) = event_state(g, cf_this(cf)) {
        define(g, s, "stopProp", TRUE, 0);
    }
    UNDEF
}

unsafe extern "sysv64" fn stop_immediate(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Some(s) = event_state(g, cf_this(cf)) {
        define(g, s, "stopProp", TRUE, 0);
        define(g, s, "stopImmediate", TRUE, 0);
    }
    UNDEF
}

unsafe extern "sysv64" fn composed_path(g: *mut c_void, cf: *mut c_void) -> u64 {
    let arr = new_array(g);
    if let Some(s) = event_state(g, cf_this(cf)) {
        let t = get(g, s, "currentTarget");
        if is_cell(t) {
            set_i(g, arr, 0, t);
        }
    }
    arr
}

/// Shared init for Event/CustomEvent constructors. `this_obj` is the fresh
/// ordinary object; fills the [[s_event]] slot from (type, initDict).
pub unsafe fn init_event(g: *mut c_void, this_obj: u64, ty: u64, init: u64, detail_from_init: bool) {
    let st = state(g);
    let s = new_object(g, UNDEF);
    let type_str = read_string(g, ty).unwrap_or_default();
    define(g, s, "type", js_string(g, &type_str), 0);

    let mut flag = |name: &str| {
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

unsafe extern "sysv64" fn event_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "Event constructor requires a type argument");
    }
    let st = state(g);
    let obj = ordinary_create(g, cf_new_target(cf), st.event.proto);
    init_event(g, obj, cf_arg(cf, 0), cf_arg(cf, 1), false);
    obj
}

unsafe extern "sysv64" fn custom_event_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "CustomEvent constructor requires a type argument");
    }
    let st = state(g);
    let obj = ordinary_create(g, cf_new_target(cf), st.custom_event.proto);
    init_event(g, obj, cf_arg(cf, 0), cf_arg(cf, 1), true);
    obj
}

/// Creates a trusted Event of `type` (native dispatches: load/resize/…).
pub unsafe fn new_trusted_event(g: *mut c_void, ty: &str) -> u64 {
    let st = state(g);
    let obj = new_object(g, st.event.proto);
    init_event(g, obj, js_string(g, ty), UNDEF, false);
    let s = get_a(g, obj, st.s_event);
    define(g, s, "isTrusted", TRUE, 0);
    obj
}

// ---------------------------------------------------------------------------
// EventTarget
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

/// options: boolean (capture) or {once, capture} — capture only participates
/// in listener identity here (no tree, no capture phase).
unsafe fn parse_once(g: *mut c_void, options: u64) -> bool {
    if is_cell(options) {
        let v = get(g, options, "once");
        return v != UNDEF && glue::rjs_to_boolean(g, v) != 0;
    }
    false
}

pub unsafe extern "sysv64" fn add_event_listener(g: *mut c_void, cf: *mut c_void) -> u64 {
    let target = target_this(g, cf);
    let ty = cf_arg(cf, 0);
    let cb = cf_arg(cf, 1);
    if !is_cell(cb) {
        return UNDEF; // spec: null callback → do nothing
    }
    let st = state(g);
    let type_str = read_string(g, ty).unwrap_or_default();
    note_listener_added(&type_str);
    let list = listeners_of(g, target, true);
    let n = array_len(g, list);
    // Spec: suppress duplicate (type, callback, capture) registrations.
    for i in 0..n {
        let e = get_i(g, list, i);
        if get_a(g, e, st.a_cb) == cb {
            let et = read_string(g, get_a(g, e, st.a_type)).unwrap_or_default();
            if et == type_str {
                return UNDEF;
            }
        }
    }
    let entry = new_object(g, UNDEF);
    glue::rjs_define(g, entry, st.a_type, js_string(g, &type_str), 0);
    glue::rjs_define(g, entry, st.a_cb, cb, 0);
    glue::rjs_define(g, entry, st.a_once, enc_bool(parse_once(g, cf_arg(cf, 2))), 0);
    set_i(g, list, n, entry);
    UNDEF
}

pub unsafe extern "sysv64" fn remove_event_listener(g: *mut c_void, cf: *mut c_void) -> u64 {
    let target = target_this(g, cf);
    let cb = cf_arg(cf, 1);
    let list = listeners_of(g, target, false);
    if !is_cell(list) {
        return UNDEF;
    }
    let st = state(g);
    let type_str = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let n = array_len(g, list);
    let mut kept: Vec<u64> = Vec::with_capacity(n as usize);
    let mut removed = false;
    for i in 0..n {
        let e = get_i(g, list, i);
        if !removed && get_a(g, e, st.a_cb) == cb {
            let et = read_string(g, get_a(g, e, st.a_type)).unwrap_or_default();
            if et == type_str {
                removed = true;
                continue;
            }
        }
        kept.push(e);
    }
    if removed {
        rebuild(g, target, &kept);
    }
    UNDEF
}

unsafe fn rebuild(g: *mut c_void, target: u64, entries: &[u64]) {
    let st = state(g);
    let arr = new_array(g);
    for (i, e) in entries.iter().enumerate() {
        set_i(g, arr, i as u32, *e);
    }
    glue::rjs_define(g, target, st.s_listeners, arr, 0);
}

/// Core dispatch, also used by native event sources (image load, resize…).
/// Invokes the listener snapshot, then the `on<type>` handler property.
pub unsafe fn dispatch(g: *mut c_void, target: u64, event: u64) -> bool {
    let st = state(g);
    let s = get_a(g, event, st.s_event);
    let has_state = is_cell(s);
    if has_state {
        define(g, s, "target", target, 0);
        define(g, s, "currentTarget", target, 0);
        define(g, s, "dispatching", TRUE, 0);
        define(g, s, "stopImmediate", FALSE, 0);
    }
    let type_str = if has_state {
        read_string(g, get(g, s, "type")).unwrap_or_default()
    } else {
        read_string(g, get(g, event, "type")).unwrap_or_default()
    };

    // Snapshot matching listeners (spec: mutation during dispatch doesn't
    // affect the current dispatch), honoring once-removal up front.
    let list = listeners_of(g, target, false);
    let mut snapshot: Vec<(u64, bool)> = Vec::new();
    if is_cell(list) {
        let n = array_len(g, list);
        let mut kept: Vec<u64> = Vec::with_capacity(n as usize);
        let mut any_once = false;
        for i in 0..n {
            let e = get_i(g, list, i);
            let et = read_string(g, get_a(g, e, st.a_type)).unwrap_or_default();
            if et == type_str {
                let once = truthy_flag(get_a(g, e, st.a_once));
                snapshot.push((get_a(g, e, st.a_cb), once));
                if once {
                    any_once = true;
                    continue;
                }
            }
            kept.push(e);
        }
        if any_once {
            rebuild(g, target, &kept);
        }
    }

    for (cb, _once) in &snapshot {
        if has_state && truthy_flag(get(g, s, "stopImmediate")) {
            break;
        }
        invoke_listener(g, target, *cb, event);
    }

    // Event-handler IDL attribute (onload, onresize, …).
    let handler = get(g, target, &format!("on{type_str}"));
    if is_cell(handler) && glue::rjs_kind(g, handler) == 9 {
        if !(has_state && truthy_flag(get(g, s, "stopImmediate"))) {
            if let Err(exc) = call(g, handler, target, &[event]) {
                report(g, exc);
            }
        }
    }

    let mut not_canceled = true;
    if has_state {
        define(g, s, "dispatching", FALSE, 0);
        define(g, s, "currentTarget", NULL, 0);
        not_canceled = !truthy_flag(get(g, s, "defaultPrevented"));
    }
    not_canceled
}

unsafe fn invoke_listener(g: *mut c_void, target: u64, cb: u64, event: u64) {
    let st = state(g);
    let result = if glue::rjs_kind(g, cb) == 9 {
        call(g, cb, target, &[event])
    } else {
        // Spec: listener object with handleEvent(event).
        let he = get_a(g, cb, st.a_handle_event);
        if is_cell(he) { call(g, he, cb, &[event]) } else { Ok(UNDEF) }
    };
    if let Err(exc) = result {
        report(g, exc);
    }
}

unsafe fn report(g: *mut c_void, exc: u64) {
    let (mut msg, mut stack) = (0u64, 0u64);
    glue::rjs_error_info(g, exc, &mut msg, &mut stack);
    let m = read_string(g, msg).unwrap_or_default();
    let s = read_string(g, stack).unwrap_or_default();
    eprintln!("[js:listener] {m}\n{s}");
}

pub unsafe extern "sysv64" fn dispatch_event(g: *mut c_void, cf: *mut c_void) -> u64 {
    let target = target_this(g, cf);
    let event = cf_arg(cf, 0);
    if !is_cell(event) {
        return throw_type_error(g, "dispatchEvent requires an Event");
    }
    enc_bool(dispatch(g, target, event))
}

unsafe extern "sysv64" fn event_target_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let st = state(g);
    ordinary_create(g, cf_new_target(cf), st.event_target.proto)
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
        accessors: &[],
    });

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
