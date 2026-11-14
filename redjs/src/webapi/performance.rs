//! The native scheduling clock's existing singleton, exposed through the
//! Performance WebIDL interface before an AOT host seals its global surface.
use super::util::*;
use crate::{capi, glue};
use core::ffi::c_void;

unsafe fn data(g: *mut c_void, receiver: u64) -> Option<u64> {
    if !is_cell(receiver) {
        return None;
    }
    let bag = get_a(g, receiver, state(g).s_performance);
    (is_cell(bag) && get(g, bag, "receiver") == receiver).then_some(bag)
}

#[crate::host_fn]
unsafe fn now(g: *mut c_void, frame: *mut c_void) -> u64 {
    if data(g, cf_this(frame)).is_none() {
        return throw_type_error(g, "Illegal invocation");
    }
    // Same native clock as timers, rAF and the former own performance.now.
    enc_f64(crate::web::now_ms())
}

#[crate::host_fn]
unsafe fn time_origin(g: *mut c_void, frame: *mut c_void) -> u64 {
    let Some(bag) = data(g, cf_this(frame)) else {
        return throw_type_error(g, "Illegal invocation");
    };
    get(g, bag, "timeOrigin")
}

#[crate::host_fn]
unsafe fn to_json(g: *mut c_void, frame: *mut c_void) -> u64 {
    let Some(bag) = data(g, cf_this(frame)) else {
        return throw_type_error(g, "Illegal invocation");
    };
    let result = new_object(g, UNDEF);
    define(g, result, "timeOrigin", get(g, bag, "timeOrigin"), 0);
    result
}

pub(super) unsafe fn clock_installed(g: *mut c_void) -> bool {
    let performance = get(g, glue::rjs_global(g), "performance");
    glue::rjs_kind(g, performance) == 8
        && dec_f64(get(g, performance, "timeOrigin")).is_some_and(f64::is_finite)
}

pub unsafe fn install(g: *mut c_void, event_target_prototype: u64) {
    let global = glue::rjs_global(g);
    // web::install ran first. Preserve the actual singleton and its native
    // clock origin instead of creating a placeholder or a second clock.
    let performance = get(g, global, "performance");
    let origin = get(g, performance, "timeOrigin");
    let class = define_class(
        g,
        &ClassSpec {
            name: "Performance",
            arity: 0,
            construct: None,
            parent_proto: event_target_prototype,
            methods: &[("now", 0, now), ("toJSON", 0, to_json)],
            getters: &[("timeOrigin", time_origin)],
            accessors: &[],
        },
    );
    let parent_constructor = get(g, event_target_prototype, "constructor");
    capi::JSObjectSetPrototype(
        g,
        class.ctor as usize as capi::JSObjectRef,
        parent_constructor as usize as capi::JSValueRef,
    );
    let bag = new_object(g, UNDEF);
    define(g, bag, "receiver", performance, 0);
    define(g, bag, "timeOrigin", origin, 0);
    glue::rjs_define(g, performance, state(g).s_performance, bag, 0);
    for name in ["now", "timeOrigin"] {
        let key = atom(g, name);
        glue::rjs_delete(g, performance, key);
        glue::rjs_atom_free(g, key);
    }
    capi::JSObjectSetPrototype(
        g,
        performance as usize as capi::JSObjectRef,
        class.proto as usize as capi::JSValueRef,
    );
}

#[cfg(test)]
mod tests {
    use super::super::testing::{ctx, ok};

    #[test]
    fn missing_clock_is_refused_without_partial_host_state() {
        let context = crate::api::Context::new();
        unsafe {
            assert_eq!(crate::ffi::redjs_webshim_install(context.raw(), core::ptr::null(), 0, core::ptr::null(), 0), 2);
            assert!(!super::super::util::has_state(context.raw()));
            assert_eq!(crate::ffi::redjs_web_install(context.raw()), 0);
            assert_eq!(crate::ffi::redjs_webshim_install(context.raw(), core::ptr::null(), 0, core::ptr::null(), 0), 0);
        }
        ok(&context, "performance instanceof Performance");
    }

    #[test]
    fn native_performance_has_its_real_interface_and_event_target_chain() {
        let context = ctx();
        ok(&context, "performance instanceof Performance && performance instanceof EventTarget && Object.getPrototypeOf(performance) === Performance.prototype && Object.getPrototypeOf(Performance.prototype) === EventTarget.prototype && Object.getPrototypeOf(Performance) === EventTarget");
        ok(&context, "Object.prototype.toString.call(performance) === '[object Performance]' && performance.constructor === Performance");
        ok(&context, "(() => { const a = performance.now(), b = performance.now(); const json = performance.toJSON(); return Number.isFinite(a) && b >= a && json.timeOrigin === performance.timeOrigin && performance.timeOrigin > 0 && !Object.hasOwn(performance, 'now') && !Object.hasOwn(performance, 'timeOrigin'); })()");
        ok(&context, "(() => { let calls = 0; const callback = () => calls++; performance.addEventListener('sk-probe', callback); performance.dispatchEvent(new Event('sk-probe')); performance.removeEventListener('sk-probe', callback); performance.dispatchEvent(new Event('sk-probe')); return calls === 1; })()");
    }

    #[test]
    fn illegal_construction_and_forged_receivers_do_not_gain_a_performance_brand() {
        let context = ctx();
        ok(&context, "(() => { const attempts = [() => Performance(), () => new Performance(), () => Performance.prototype.now.call({}), () => Performance.prototype.now.call(Object.create(performance)), () => Performance.prototype.toJSON.call(Object.create(Performance.prototype)), () => Object.getOwnPropertyDescriptor(Performance.prototype, 'timeOrigin').get.call({})]; return attempts.every(f => { try { f(); return false; } catch (e) { return e instanceof TypeError; } }); })()");
    }
}
