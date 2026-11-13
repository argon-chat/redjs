//! Event interfaces whose members are their init dictionary's, read back by getters: `TransitionEvent`,
//! `AnimationEvent`, `CompositionEvent`, `HashChangeEvent`, `PopStateEvent`, `PageTransitionEvent`,
//! `PromiseRejectionEvent`, `ToggleEvent`, `MediaQueryListEvent`, `FormDataEvent`, `SubmitEvent`,
//! `ClipboardEvent`, `TouchEvent`, and `Touch` / `TouchList`.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::events;
use super::util::*;
use crate::glue;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Str,
    Num,
    Bool,
    /// Any value, `null` when absent.
    OrNull,
    /// Any value, `undefined` when absent.
    Any,
    /// A `sequence<Touch>`, as a `TouchList`.
    Touches,
    /// `long`.
    Long,
    /// Any value, required.
    Required,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Parent {
    Event,
    UiEvent,
    None,
}

struct Iface {
    name: &'static str,
    parent: Parent,
    construct: HostFn,
    members: &'static [(&'static str, Kind, HostFn)],
}

struct State {
    members: usize,
    items: usize,
    protos: Vec<u64>,
    touch_list: u64,
}

unsafe impl Send for State {}
unsafe impl Sync for State {}

fn registry() -> &'static Mutex<HashMap<usize, &'static State>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static State>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn st(g: *mut c_void) -> &'static State {
    registry().lock().unwrap().get(&(g as usize)).copied().expect("webapi::event_interfaces not installed")
}

pub(crate) fn forget(gone: crate::context::Gone) {
    registry().lock().unwrap().remove(&gone.key);
}

const IFACE_KEY: &str = "#iface";

/// `this`'s members if it was made by interface `id`'s constructor.
unsafe fn members_of(g: *mut c_void, this: u64, id: usize) -> Option<u64> {
    if !is_cell(this) {
        return None;
    }
    let m = get_a(g, this, st(g).members as *mut c_void);
    (is_cell(m) && dec_f64(get(g, m, IFACE_KEY)) == Some(id as f64)).then_some(m)
}

macro_rules! member {
    ($fn_name:ident, $id:expr, $key:literal) => {
        #[crate::host_fn]
        unsafe fn $fn_name(g: *mut c_void, cf: *mut c_void) -> u64 {
            match members_of(g, cf_this(cf), $id) {
                Some(m) => get(g, m, $key),
                None => throw_type_error(g, "Illegal invocation"),
            }
        }
    };
}

macro_rules! construct {
    ($fn_name:ident, $id:expr) => {
        #[crate::host_fn]
        unsafe fn $fn_name(g: *mut c_void, cf: *mut c_void) -> u64 {
            construct(g, cf, $id)
        }
    };
}

const TRANSITION: usize = 0;
const ANIMATION: usize = 1;
const COMPOSITION: usize = 2;
const HASH_CHANGE: usize = 3;
const POP_STATE: usize = 4;
const PAGE_TRANSITION: usize = 5;
const PROMISE_REJECTION: usize = 6;
const TOGGLE: usize = 7;
const MEDIA_QUERY_LIST: usize = 8;
const FORM_DATA: usize = 9;
const SUBMIT: usize = 10;
const CLIPBOARD: usize = 11;
const TOUCH_EVENT: usize = 12;
const TOUCH: usize = 13;

member!(tr_property_name, TRANSITION, "propertyName");
member!(tr_elapsed_time, TRANSITION, "elapsedTime");
member!(tr_pseudo_element, TRANSITION, "pseudoElement");
member!(an_animation_name, ANIMATION, "animationName");
member!(an_elapsed_time, ANIMATION, "elapsedTime");
member!(an_pseudo_element, ANIMATION, "pseudoElement");
member!(co_data, COMPOSITION, "data");
member!(hc_old_url, HASH_CHANGE, "oldURL");
member!(hc_new_url, HASH_CHANGE, "newURL");
member!(ps_state, POP_STATE, "state");
member!(pt_persisted, PAGE_TRANSITION, "persisted");
member!(pr_promise, PROMISE_REJECTION, "promise");
member!(pr_reason, PROMISE_REJECTION, "reason");
member!(to_old_state, TOGGLE, "oldState");
member!(to_new_state, TOGGLE, "newState");
member!(mq_media, MEDIA_QUERY_LIST, "media");
member!(mq_matches, MEDIA_QUERY_LIST, "matches");
member!(fd_form_data, FORM_DATA, "formData");
member!(su_submitter, SUBMIT, "submitter");
member!(cl_clipboard_data, CLIPBOARD, "clipboardData");
member!(te_touches, TOUCH_EVENT, "touches");
member!(te_target_touches, TOUCH_EVENT, "targetTouches");
member!(te_changed_touches, TOUCH_EVENT, "changedTouches");
member!(te_alt_key, TOUCH_EVENT, "altKey");
member!(te_meta_key, TOUCH_EVENT, "metaKey");
member!(te_ctrl_key, TOUCH_EVENT, "ctrlKey");
member!(te_shift_key, TOUCH_EVENT, "shiftKey");
member!(t_identifier, TOUCH, "identifier");
member!(t_target, TOUCH, "target");
member!(t_client_x, TOUCH, "clientX");
member!(t_client_y, TOUCH, "clientY");
member!(t_screen_x, TOUCH, "screenX");
member!(t_screen_y, TOUCH, "screenY");
member!(t_page_x, TOUCH, "pageX");
member!(t_page_y, TOUCH, "pageY");
member!(t_radius_x, TOUCH, "radiusX");
member!(t_radius_y, TOUCH, "radiusY");
member!(t_rotation_angle, TOUCH, "rotationAngle");
member!(t_force, TOUCH, "force");
member!(t_altitude_angle, TOUCH, "altitudeAngle");
member!(t_azimuth_angle, TOUCH, "azimuthAngle");
member!(t_touch_type, TOUCH, "touchType");

construct!(c_transition, TRANSITION);
construct!(c_animation, ANIMATION);
construct!(c_composition, COMPOSITION);
construct!(c_hash_change, HASH_CHANGE);
construct!(c_pop_state, POP_STATE);
construct!(c_page_transition, PAGE_TRANSITION);
construct!(c_promise_rejection, PROMISE_REJECTION);
construct!(c_toggle, TOGGLE);
construct!(c_media_query_list, MEDIA_QUERY_LIST);
construct!(c_form_data, FORM_DATA);
construct!(c_submit, SUBMIT);
construct!(c_clipboard, CLIPBOARD);
construct!(c_touch_event, TOUCH_EVENT);
construct!(c_touch, TOUCH);

use Kind::*;

const IFACES: &[Iface] = &[
    Iface {
        name: "TransitionEvent",
        parent: Parent::Event,
        construct: c_transition,
        members: &[("propertyName", Str, tr_property_name), ("elapsedTime", Num, tr_elapsed_time), ("pseudoElement", Str, tr_pseudo_element)],
    },
    Iface {
        name: "AnimationEvent",
        parent: Parent::Event,
        construct: c_animation,
        members: &[("animationName", Str, an_animation_name), ("elapsedTime", Num, an_elapsed_time), ("pseudoElement", Str, an_pseudo_element)],
    },
    Iface { name: "CompositionEvent", parent: Parent::UiEvent, construct: c_composition, members: &[("data", Str, co_data)] },
    Iface { name: "HashChangeEvent", parent: Parent::Event, construct: c_hash_change, members: &[("oldURL", Str, hc_old_url), ("newURL", Str, hc_new_url)] },
    Iface { name: "PopStateEvent", parent: Parent::Event, construct: c_pop_state, members: &[("state", OrNull, ps_state)] },
    Iface { name: "PageTransitionEvent", parent: Parent::Event, construct: c_page_transition, members: &[("persisted", Bool, pt_persisted)] },
    Iface {
        name: "PromiseRejectionEvent",
        parent: Parent::Event,
        construct: c_promise_rejection,
        members: &[("promise", Any, pr_promise), ("reason", Any, pr_reason)],
    },
    Iface { name: "ToggleEvent", parent: Parent::Event, construct: c_toggle, members: &[("oldState", Str, to_old_state), ("newState", Str, to_new_state)] },
    Iface {
        name: "MediaQueryListEvent",
        parent: Parent::Event,
        construct: c_media_query_list,
        members: &[("media", Str, mq_media), ("matches", Bool, mq_matches)],
    },
    Iface { name: "FormDataEvent", parent: Parent::Event, construct: c_form_data, members: &[("formData", Any, fd_form_data)] },
    Iface { name: "SubmitEvent", parent: Parent::Event, construct: c_submit, members: &[("submitter", OrNull, su_submitter)] },
    Iface { name: "ClipboardEvent", parent: Parent::Event, construct: c_clipboard, members: &[("clipboardData", OrNull, cl_clipboard_data)] },
    Iface {
        name: "TouchEvent",
        parent: Parent::UiEvent,
        construct: c_touch_event,
        members: &[
            ("touches", Touches, te_touches),
            ("targetTouches", Touches, te_target_touches),
            ("changedTouches", Touches, te_changed_touches),
            ("altKey", Bool, te_alt_key),
            ("metaKey", Bool, te_meta_key),
            ("ctrlKey", Bool, te_ctrl_key),
            ("shiftKey", Bool, te_shift_key),
        ],
    },
    Iface {
        name: "Touch",
        parent: Parent::None,
        construct: c_touch,
        members: &[
            ("identifier", Long, t_identifier),
            ("target", Required, t_target),
            ("clientX", Num, t_client_x),
            ("clientY", Num, t_client_y),
            ("screenX", Num, t_screen_x),
            ("screenY", Num, t_screen_y),
            ("pageX", Num, t_page_x),
            ("pageY", Num, t_page_y),
            ("radiusX", Num, t_radius_x),
            ("radiusY", Num, t_radius_y),
            ("rotationAngle", Num, t_rotation_angle),
            ("force", Num, t_force),
            ("altitudeAngle", Num, t_altitude_angle),
            ("azimuthAngle", Num, t_azimuth_angle),
            ("touchType", Str, t_touch_type),
        ],
    },
];

fn to_int32(n: f64) -> i32 {
    if !n.is_finite() {
        return 0;
    }
    n.trunc().rem_euclid(4294967296.0) as u32 as i32
}

unsafe fn touch_list(g: *mut c_void, items: &[u64]) -> u64 {
    let s = st(g);
    let list = new_object(g, s.touch_list);
    let arr = new_array(g);
    let object = get(g, glue::rjs_global(g), "Object");
    let define_property = get(g, object, "defineProperty");
    for (i, &t) in items.iter().enumerate() {
        let desc = new_object(g, UNDEF);
        define(g, desc, "value", t, 0);
        define(g, desc, "enumerable", TRUE, 0);
        define(g, desc, "configurable", TRUE, 0);
        let _ = call(g, define_property, object, &[list, enc_f64(i as f64), desc]);
        set_i(g, arr, i as u32, t);
    }
    glue::rjs_define(g, list, s.items as *mut c_void, arr, 0);
    list
}

unsafe fn list_items(g: *mut c_void, list: u64) -> Option<u64> {
    let a = if is_cell(list) { get_a(g, list, st(g).items as *mut c_void) } else { UNDEF };
    is_cell(a).then_some(a)
}

#[crate::host_fn]
unsafe fn tl_length(g: *mut c_void, cf: *mut c_void) -> u64 {
    match list_items(g, cf_this(cf)) {
        Some(a) => enc_f64(array_len(g, a) as f64),
        None => throw_type_error(g, "Illegal invocation"),
    }
}

#[crate::host_fn]
unsafe fn tl_item(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(a) = list_items(g, cf_this(cf)) else { return throw_type_error(g, "Illegal invocation") };
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "Failed to execute 'item' on 'TouchList': 1 argument required, but only 0 present.");
    }
    let Ok(n) = to_number(g, cf_arg(cf, 0)) else { return UNDEF };
    let i = to_int32(n) as u32;
    if i < array_len(g, a) { get_i(g, a, i) } else { NULL }
}

/// A `sequence<Touch>` from the init dictionary; `Err` with an exception pending.
unsafe fn touches(g: *mut c_void, v: u64, member: &str) -> Result<u64, ()> {
    if v == UNDEF || v == NULL {
        return Ok(touch_list(g, &[]));
    }
    let global = glue::rjs_global(g);
    let reflect = get(g, global, "Reflect");
    let iterator = get(g, get(g, global, "Symbol"), "iterator");
    let iter_fn = if is_cell(v) { call(g, get(g, reflect, "get"), reflect, &[v, iterator]).map_err(|e| glue::rjs_throw(g, e))? } else { UNDEF };
    if !events::is_callable(g, iter_fn) {
        throw_type_error(
            g,
            &format!("Failed to construct 'TouchEvent': Failed to read the '{member}' property from 'TouchEventInit': The provided value cannot be converted to a sequence."),
        );
        return Err(());
    }
    let array = get(g, global, "Array");
    let arr = call(g, get(g, array, "from"), array, &[v]).map_err(|e| glue::rjs_throw(g, e))?;
    let mut items = Vec::new();
    for i in 0..array_len(g, arr) {
        let t = get_i(g, arr, i);
        if members_of(g, t, TOUCH).is_none() {
            throw_type_error(
                g,
                &format!("Failed to construct 'TouchEvent': Failed to read the '{member}' property from 'TouchEventInit': Failed to convert value to 'Touch'."),
            );
            return Err(());
        }
        items.push(t);
    }
    Ok(touch_list(g, &items))
}

unsafe fn construct(g: *mut c_void, cf: *mut c_void, id: usize) -> u64 {
    let iface = &IFACES[id];
    let init = cf_arg(cf, if iface.parent == Parent::None { 0 } else { 1 });
    let missing = || format!("Failed to construct '{}': 1 argument required, but only 0 present.", iface.name);
    if cf_argc(cf) < 1 || (iface.parent == Parent::None && !is_cell(init)) {
        return throw_type_error(g, &missing());
    }
    let s = st(g);
    let obj = ordinary_create(g, cf_new_target(cf), s.protos[id]);
    match iface.parent {
        Parent::Event => events::init_event(g, obj, cf_arg(cf, 0), init, false),
        Parent::UiEvent => super::uievents::init_ui_event(g, obj, cf_arg(cf, 0), init),
        Parent::None => {}
    }
    let m = new_object(g, UNDEF);
    define(g, m, IFACE_KEY, enc_f64(id as f64), 0);
    for &(key, kind, _) in iface.members {
        let raw = if is_cell(init) {
            match try_get(g, init, key) {
                Ok(v) => v,
                Err(()) => return UNDEF,
            }
        } else {
            UNDEF
        };
        let v = match kind {
            Str if raw == UNDEF => js_string(g, if key == "touchType" { "direct" } else { "" }),
            Str => match to_js_string(g, raw) {
                Ok(v) => v,
                Err(()) => return UNDEF,
            },
            Num | Long if raw == UNDEF => enc_f64(0.0),
            Num | Long => match to_number(g, raw) {
                Ok(n) => enc_f64(if kind == Long { to_int32(n) as f64 } else { n }),
                Err(()) => return UNDEF,
            },
            Bool => enc_bool(raw != UNDEF && glue::rjs_to_boolean(g, raw) != 0),
            OrNull if raw == UNDEF => NULL,
            OrNull | Any => raw,
            Touches => match touches(g, raw, key) {
                Ok(v) => v,
                Err(()) => return UNDEF,
            },
            Required => raw,
        };
        if raw == UNDEF && (kind == Required || (kind == Long && iface.parent == Parent::None)) {
            let dict = format!("{}Init", iface.name);
            return throw_type_error(
                g,
                &format!("Failed to construct '{}': Failed to read the '{key}' property from '{dict}': Required member is undefined.", iface.name),
            );
        }
        define(g, m, key, v, 0);
    }
    glue::rjs_define(g, obj, s.members as *mut c_void, m, 0);
    obj
}

pub unsafe fn install(g: *mut c_void, event_proto: u64) {
    let global = glue::rjs_global(g);
    let ui_ctor = get(g, global, "UIEvent");
    let ui_proto = if is_cell(ui_ctor) { get(g, ui_ctor, "prototype") } else { event_proto };

    let touch_list = define_class(g, &ClassSpec {
        name: "TouchList",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[("item", 1, tl_item)],
        getters: &[("length", tl_length)],
        accessors: &[],
    });
    let values = get(g, get(g, get(g, global, "Array"), "prototype"), "values");
    let object = get(g, global, "Object");
    let desc = new_object(g, UNDEF);
    define(g, desc, "value", values, 0);
    define(g, desc, "writable", TRUE, 0);
    define(g, desc, "configurable", TRUE, 0);
    let iterator = get(g, get(g, global, "Symbol"), "iterator");
    let _ = call(g, get(g, object, "defineProperty"), object, &[touch_list.proto, iterator, desc]);
    crate::ffi::redjs_protect(g, touch_list.proto);

    let mut protos = Vec::with_capacity(IFACES.len());
    for iface in IFACES {
        let getters: Vec<(&str, HostFn)> = iface.members.iter().map(|&(k, _, f)| (k, f)).collect();
        let parent_proto = match iface.parent {
            Parent::Event => event_proto,
            Parent::UiEvent => ui_proto,
            Parent::None => 0,
        };
        let c = define_class(g, &ClassSpec {
            name: iface.name,
            arity: 1,
            construct: Some(iface.construct),
            parent_proto,
            methods: &[],
            getters: &getters,
            accessors: &[],
        });
        crate::ffi::redjs_protect(g, c.proto);
        protos.push(c.proto);
    }

    let state: &'static State = Box::leak(Box::new(State {
        members: atom_priv(g, "redjs#eventMembers") as usize,
        items: atom_priv(g, "redjs#touchListItems") as usize,
        protos,
        touch_list: touch_list.proto,
    }));
    registry().lock().unwrap().insert(g as usize, state);
}

#[cfg(test)]
mod tests {
    use crate::webapi::testing::{ctx, ok, s};

    #[test]
    fn init_members_are_read_back_coerced_with_their_defaults() {
        let c = ctx();
        ok(&c, "new TransitionEvent('transitionend') instanceof Event");
        assert_eq!(
            s(&c, "(() => { const e = new TransitionEvent('transitionend', { propertyName: 7, elapsedTime: '1.5', bubbles: true }); \
                    return [e.type, e.propertyName, e.elapsedTime, e.pseudoElement, e.bubbles].join('|'); })()"),
            "transitionend|7|1.5||true"
        );
        ok(&c, "new CompositionEvent('compositionend', { data: 'あ' }).data === 'あ' && new CompositionEvent('x') instanceof UIEvent");
        ok(&c, "new PopStateEvent('popstate').state === null && new PopStateEvent('popstate', { state: { a: 1 } }).state.a === 1");
        ok(&c, "new PromiseRejectionEvent('unhandledrejection').reason === undefined");
        ok(&c, "new MediaQueryListEvent('change', { matches: 1, media: '(x)' }).matches === true");
        ok(&c, "new SubmitEvent('submit').submitter === null && new ClipboardEvent('copy').clipboardData === null");
        ok(&c, "(() => { try { new HashChangeEvent(); return false; } catch (e) { return e instanceof TypeError; } })()");
        ok(&c, "(() => { try { Object.getOwnPropertyDescriptor(AnimationEvent.prototype, 'elapsedTime').get.call(new TransitionEvent('x')); return false; } \
                 catch (e) { return e.message === 'Illegal invocation'; } })()");
        ok(&c, "(() => { class Mine extends ToggleEvent {} const e = new Mine('toggle', { newState: 'open' }); return e instanceof Mine && e.newState === 'open'; })()");
    }

    #[test]
    fn touches_are_checked_and_listed() {
        let c = ctx();
        assert_eq!(
            s(&c, "(() => { try { new Touch({ identifier: 1 }); } catch (e) { return e.message; } })()"),
            "Failed to construct 'Touch': Failed to read the 'target' property from 'TouchInit': Required member is undefined."
        );
        assert_eq!(
            s(&c, "(() => { const t = new Touch({ identifier: 4.9, target: globalThis, clientX: 3 }); \
                    return [t.identifier, t.clientX, t.force, t.touchType, t.target === globalThis].join('|'); })()"),
            "4|3|0|direct|true"
        );
        assert_eq!(
            s(&c, "(() => { const t = new Touch({ identifier: 1, target: globalThis }); \
                    const e = new TouchEvent('touchstart', { touches: [t], ctrlKey: true }); \
                    return [e instanceof UIEvent, e.touches instanceof TouchList, e.touches.length, e.touches[0] === t, e.touches.item(0) === t, \
                            e.touches.item(5), [...e.touches].length, e.changedTouches.length, e.ctrlKey, e.altKey].join('|'); })()"),
            "true|true|1|true|true||1|0|true|false"
        );
        assert_eq!(
            s(&c, "(() => { try { new TouchEvent('x', { touches: [{}] }); } catch (e) { return e.message; } })()"),
            "Failed to construct 'TouchEvent': Failed to read the 'touches' property from 'TouchEventInit': Failed to convert value to 'Touch'."
        );
        ok(&c, "(() => { try { new TouchList(); return false; } catch (e) { return e instanceof TypeError; } })()");
    }
}
