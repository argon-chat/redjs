//! Native `UIEvent` / `MouseEvent` / `KeyboardEvent` / `WheelEvent` /
//! `FocusEvent` / `InputEvent` / `PointerEvent` / `DragEvent` and the
//! `DataTransfer` a drag event carries (UI Events, Pointer Events, Input
//! Events, HTML §Drag and drop).
//!
//! These are data carriers. Dispatch belongs to `events.rs` and is untouched:
//! each of these is a real subclass of the existing native `Event`, so
//! `dispatchEvent(new MouseEvent('click'))` goes through the one dispatch path
//! that already exists, `instanceof Event` is true, and the listener sees the
//! same object it would in a browser.
//!
//! Field storage follows `events.rs` exactly — the private `[[s_event]]` slot
//! object — rather than a second Rust-side store. Dispatch already reads that
//! slot, and two stores for one event is how a `type` read and a `type` write
//! end up disagreeing.
//!
//! ## Deliberate narrowings, each because there is no layout or hit-testing
//!
//! - `offsetX` / `offsetY` are defined relative to the target's padding edge
//!   (CSSOM View §"offsetX"). That needs layout. They read back exactly what
//!   the init dictionary supplied, defaulting to 0 — never a guess derived from
//!   `clientX`, which would be a plausible wrong number.
//! - `pageX` / `pageY` are `clientX + scrollX` (CSSOM View §"pageX"). There is
//!   no scrolling viewport here, so `scrollX` is 0 and they equal `clientX` /
//!   `clientY`. That is the correct value for this environment, not a stub.
//! - `getCoalescedEvents()` / `getPredictedEvents()` return an empty list.
//!   For a script-constructed event that is what the specification requires
//!   (Pointer Events §"getCoalescedEvents" — the list is the UA's, and a
//!   synthesized event has none). A host that starts delivering *trusted*
//!   pointer events with coalescing would have to fill these.
//! - `DataTransfer.items` (a `DataTransferItemList`) and `DataTransfer.files`
//!   (a `FileList` of `File`s) have no implementation: there is no `File` class
//!   in this runtime. `items` REFUSES by name; `files` is an empty list, which
//!   is the truthful answer for a drag that carries no files.
//! - `KeyboardEvent` carries no legacy `keyCode` / `charCode`. Those are a
//!   layout-dependent mapping from a physical key to a Windows virtual-key
//!   code, and there is no layout table here — a made-up number would be
//!   plausible and wrong in exactly the way that breaks a shortcut handler.
//!   `key` (the character) and `code` (the physical key) are the members the
//!   host fills, and they are what current specifications define.
//! - `KeyboardEvent.getModifierState` answers the same four modifiers as
//!   `MouseEvent`'s, for the same reason: those are the only key states this
//!   runtime tracks.
//!
//! ## One deviation from the file's "every number goes through `init_num`" rule
//!
//! `KeyboardEvent.location` and `WheelEvent.deltaMode` are WebIDL
//! `unsigned long` *enumerations* — their whole use is `=== DOM_KEY_LOCATION_*`
//! / `=== DOM_DELTA_*`. A fractional or negative value there would silently
//! match no branch, so those two get a real ToUint32 (`init_u32`).
//! `button` / `buttons` / `detail` stay on `init_num`: they are opaque numbers,
//! never compared against a constant table.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::events;
use super::util::*;
use crate::capi;
use crate::glue;

// ---------------------------------------------------------------------------
// DataTransfer (the drag data store)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct DataTransferData {
    /// Format → data, in insertion order (`types` is order-sensitive).
    pub items: Vec<(String, String)>,
    pub drop_effect: String,
    pub effect_allowed: String,
}

/// HTML §"the DataTransfer interface": `"text"` is an alias for
/// `"text/plain"` and `"url"` for `"text/uri-list"`; otherwise the format is
/// ASCII-lowercased.
pub fn normalize_format(format: &str) -> String {
    let f = format.to_ascii_lowercase();
    match f.as_str() {
        "text" => "text/plain".to_string(),
        "url" => "text/uri-list".to_string(),
        _ => f,
    }
}

impl DataTransferData {
    pub fn get(&self, format: &str) -> String {
        let f = normalize_format(format);
        self.items.iter().find(|(k, _)| *k == f).map(|(_, v)| v.clone()).unwrap_or_default()
    }
    pub fn set(&mut self, format: &str, data: &str) {
        let f = normalize_format(format);
        match self.items.iter_mut().find(|(k, _)| *k == f) {
            Some(slot) => slot.1 = data.to_string(),
            None => self.items.push((f, data.to_string())),
        }
    }
    pub fn clear(&mut self, format: Option<&str>) {
        match format {
            Some(f) => {
                let f = normalize_format(f);
                self.items.retain(|(k, _)| *k != f);
            }
            None => self.items.clear(),
        }
    }
}

// ---------------------------------------------------------------------------
// Per-context classes
// ---------------------------------------------------------------------------

struct UiClasses {
    ui_event: Class,
    mouse: Class,
    keyboard: Class,
    wheel: Class,
    focus: Class,
    input: Class,
    pointer: Class,
    drag: Class,
    dt_cls: capi::JSClassRef,
    dt: Class,
}
unsafe impl Send for UiClasses {}
unsafe impl Sync for UiClasses {}

fn registry() -> &'static Mutex<HashMap<usize, &'static UiClasses>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static UiClasses>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> &'static UiClasses {
    registry().lock().unwrap().get(&(g as usize)).copied().expect("webapi::uievents not installed")
}

pub(crate) fn forget(gone: crate::context::Gone) {
    registry().lock().unwrap().remove(&gone.key);
}

/// The `[[s_event]]` slot object — the same one `events.rs` fills and dispatch
/// reads.
unsafe fn slots(g: *mut c_void, this: u64) -> Option<u64> {
    let s = get_a(g, this, state(g).s_event);
    if is_cell(s) { Some(s) } else { None }
}

/// A field's value from the init dictionary, or its default.
unsafe fn init_num(g: *mut c_void, init: u64, key: &str, default: f64) -> u64 {
    if !is_cell(init) {
        return enc_f64(default);
    }
    let v = get(g, init, key);
    if v == UNDEF {
        enc_f64(default)
    } else {
        enc_f64(read_number(g, v))
    }
}

unsafe fn init_bool(g: *mut c_void, init: u64, key: &str) -> u64 {
    if !is_cell(init) {
        return FALSE;
    }
    let v = get(g, init, key);
    if v == UNDEF {
        FALSE
    } else {
        enc_bool(glue::rjs_to_boolean(g, v) != 0)
    }
}

unsafe fn init_obj(g: *mut c_void, init: u64, key: &str) -> u64 {
    if !is_cell(init) {
        return NULL;
    }
    let v = get(g, init, key);
    if v == UNDEF { NULL } else { v }
}

unsafe fn init_str(g: *mut c_void, init: u64, key: &str, default: &str) -> u64 {
    if !is_cell(init) {
        return js_string(g, default);
    }
    let v = get(g, init, key);
    if v == UNDEF {
        js_string(g, default)
    } else {
        let s = read_string(g, v).unwrap_or_default();
        js_string(g, &s)
    }
}

/// A `DOMString?` member whose default is null (`InputEvent.data`): an explicit
/// `null` stays null rather than becoming the string `"null"`.
unsafe fn init_nullable_str(g: *mut c_void, init: u64, key: &str) -> u64 {
    if !is_cell(init) {
        return NULL;
    }
    let v = get(g, init, key);
    if v == UNDEF || v == NULL {
        return NULL;
    }
    let s = read_string(g, v).unwrap_or_default();
    js_string(g, &s)
}

/// ECMA-262 ToUint32, for the enumerated `unsigned long` members — see the
/// module header for why these two do not just use `init_num`.
fn to_uint32(d: f64) -> u32 {
    if !d.is_finite() || d == 0.0 {
        return 0;
    }
    d.trunc().rem_euclid(4_294_967_296.0) as u32
}

unsafe fn init_u32(g: *mut c_void, init: u64, key: &str, default: u32) -> u64 {
    if !is_cell(init) {
        return enc_f64(default as f64);
    }
    let v = get(g, init, key);
    if v == UNDEF {
        enc_f64(default as f64)
    } else {
        enc_f64(to_uint32(read_number(g, v)) as f64)
    }
}

// ---------------------------------------------------------------------------
// Field tables — one row per attribute, so a getter cannot exist without the
// constructor initialising the slot it reads.
// ---------------------------------------------------------------------------

/// `(attribute, init-dictionary member, default)`.
const UI_NUMS: &[(&str, f64)] = &[("detail", 0.0)];

const MOUSE_NUMS: &[(&str, f64)] = &[
    ("screenX", 0.0), ("screenY", 0.0),
    ("clientX", 0.0), ("clientY", 0.0),
    ("offsetX", 0.0), ("offsetY", 0.0),
    ("movementX", 0.0), ("movementY", 0.0),
    ("button", 0.0), ("buttons", 0.0),
];

const MOUSE_FLAGS: &[&str] = &["ctrlKey", "shiftKey", "altKey", "metaKey"];

/// UI Events §"KeyboardEvent": the four modifiers plus the two per-event flags.
const KEY_FLAGS: &[&str] = &["ctrlKey", "shiftKey", "altKey", "metaKey", "repeat", "isComposing"];

const WHEEL_NUMS: &[(&str, f64)] =
    &[("deltaX", 0.0), ("deltaY", 0.0), ("deltaZ", 0.0)];

/// WebIDL constants — on the interface object *and* the prototype.
const KEY_LOCATIONS: &[(&str, i32)] = &[
    ("DOM_KEY_LOCATION_STANDARD", 0),
    ("DOM_KEY_LOCATION_LEFT", 1),
    ("DOM_KEY_LOCATION_RIGHT", 2),
    ("DOM_KEY_LOCATION_NUMPAD", 3),
];

const DELTA_MODES: &[(&str, i32)] =
    &[("DOM_DELTA_PIXEL", 0), ("DOM_DELTA_LINE", 1), ("DOM_DELTA_PAGE", 2)];

const POINTER_NUMS: &[(&str, f64)] = &[
    ("pointerId", 0.0), ("width", 1.0), ("height", 1.0),
    ("pressure", 0.0), ("tangentialPressure", 0.0),
    ("tiltX", 0.0), ("tiltY", 0.0), ("twist", 0.0),
    // Spec defaults: altitude π/2 (perpendicular), azimuth 0.
    ("altitudeAngle", std::f64::consts::FRAC_PI_2), ("azimuthAngle", 0.0),
];

unsafe fn init_ui_event(g: *mut c_void, obj: u64, ty: u64, init: u64) {
    events::init_event(g, obj, ty, init, false);
    let Some(s) = slots(g, obj) else { return };
    define(g, s, "view", init_obj(g, init, "view"), 0);
    for (name, default) in UI_NUMS {
        define(g, s, name, init_num(g, init, name, *default), 0);
    }
    // Legacy `which`: 0 unless the init dict says otherwise (UI Events, legacy
    // section). Mouse events override it below.
    define(g, s, "which", init_num(g, init, "which", 0.0), 0);
}

unsafe fn init_mouse_event(g: *mut c_void, obj: u64, ty: u64, init: u64) {
    init_ui_event(g, obj, ty, init);
    let Some(s) = slots(g, obj) else { return };
    for (name, default) in MOUSE_NUMS {
        define(g, s, name, init_num(g, init, name, *default), 0);
    }
    for name in MOUSE_FLAGS {
        define(g, s, name, init_bool(g, init, name), 0);
    }
    define(g, s, "relatedTarget", init_obj(g, init, "relatedTarget"), 0);
    // `which` for a mouse event is button + 1 (legacy).
    let button = dec_f64(get(g, s, "button")).unwrap_or(0.0);
    define(g, s, "which", enc_f64(button + 1.0), 0);
}

unsafe fn init_keyboard_event(g: *mut c_void, obj: u64, ty: u64, init: u64) {
    init_ui_event(g, obj, ty, init);
    let Some(s) = slots(g, obj) else { return };
    define(g, s, "key", init_str(g, init, "key", ""), 0);
    define(g, s, "code", init_str(g, init, "code", ""), 0);
    define(g, s, "location", init_u32(g, init, "location", 0), 0);
    for name in KEY_FLAGS {
        define(g, s, name, init_bool(g, init, name), 0);
    }
}

unsafe fn init_wheel_event(g: *mut c_void, obj: u64, ty: u64, init: u64) {
    init_mouse_event(g, obj, ty, init);
    let Some(s) = slots(g, obj) else { return };
    for (name, default) in WHEEL_NUMS {
        define(g, s, name, init_num(g, init, name, *default), 0);
    }
    define(g, s, "deltaMode", init_u32(g, init, "deltaMode", 0), 0);
}

unsafe fn init_focus_event(g: *mut c_void, obj: u64, ty: u64, init: u64) {
    init_ui_event(g, obj, ty, init);
    let Some(s) = slots(g, obj) else { return };
    define(g, s, "relatedTarget", init_obj(g, init, "relatedTarget"), 0);
}

unsafe fn init_input_event(g: *mut c_void, obj: u64, ty: u64, init: u64) {
    init_ui_event(g, obj, ty, init);
    let Some(s) = slots(g, obj) else { return };
    define(g, s, "data", init_nullable_str(g, init, "data"), 0);
    define(g, s, "inputType", init_str(g, init, "inputType", ""), 0);
    define(g, s, "isComposing", init_bool(g, init, "isComposing"), 0);
}

unsafe fn init_pointer_event(g: *mut c_void, obj: u64, ty: u64, init: u64) {
    init_mouse_event(g, obj, ty, init);
    let Some(s) = slots(g, obj) else { return };
    for (name, default) in POINTER_NUMS {
        define(g, s, name, init_num(g, init, name, *default), 0);
    }
    define(g, s, "pointerType", init_str(g, init, "pointerType", ""), 0);
    define(g, s, "isPrimary", init_bool(g, init, "isPrimary"), 0);
}

// ---------------------------------------------------------------------------
// Getters
// ---------------------------------------------------------------------------

unsafe fn slot_get(g: *mut c_void, cf: *mut c_void, key: &str, label: &str) -> u64 {
    match slots(g, cf_this(cf)) {
        Some(s) => {
            let v = get(g, s, key);
            if v == UNDEF { NULL } else { v }
        }
        None => throw_type_error(g, &format!("{label} called on a non-Event")),
    }
}

macro_rules! slot_getters {
    ($($fn_name:ident, $key:literal, $label:literal;)*) => {
        $(
            unsafe extern "sysv64" fn $fn_name(g: *mut c_void, cf: *mut c_void) -> u64 {
                slot_get(g, cf, $key, $label)
            }
        )*
    };
}

slot_getters! {
    ui_view, "view", "UIEvent.view";
    ui_detail, "detail", "UIEvent.detail";
    ui_which, "which", "UIEvent.which";
    m_screen_x, "screenX", "MouseEvent.screenX";
    m_screen_y, "screenY", "MouseEvent.screenY";
    m_client_x, "clientX", "MouseEvent.clientX";
    m_client_y, "clientY", "MouseEvent.clientY";
    m_offset_x, "offsetX", "MouseEvent.offsetX";
    m_offset_y, "offsetY", "MouseEvent.offsetY";
    m_movement_x, "movementX", "MouseEvent.movementX";
    m_movement_y, "movementY", "MouseEvent.movementY";
    m_button, "button", "MouseEvent.button";
    m_buttons, "buttons", "MouseEvent.buttons";
    m_ctrl, "ctrlKey", "MouseEvent.ctrlKey";
    m_shift, "shiftKey", "MouseEvent.shiftKey";
    m_alt, "altKey", "MouseEvent.altKey";
    m_meta, "metaKey", "MouseEvent.metaKey";
    m_related, "relatedTarget", "MouseEvent.relatedTarget";
    k_key, "key", "KeyboardEvent.key";
    k_code, "code", "KeyboardEvent.code";
    k_location, "location", "KeyboardEvent.location";
    k_repeat, "repeat", "KeyboardEvent.repeat";
    k_composing, "isComposing", "KeyboardEvent.isComposing";
    k_ctrl, "ctrlKey", "KeyboardEvent.ctrlKey";
    k_shift, "shiftKey", "KeyboardEvent.shiftKey";
    k_alt, "altKey", "KeyboardEvent.altKey";
    k_meta, "metaKey", "KeyboardEvent.metaKey";
    w_delta_x, "deltaX", "WheelEvent.deltaX";
    w_delta_y, "deltaY", "WheelEvent.deltaY";
    w_delta_z, "deltaZ", "WheelEvent.deltaZ";
    w_delta_mode, "deltaMode", "WheelEvent.deltaMode";
    f_related, "relatedTarget", "FocusEvent.relatedTarget";
    i_data, "data", "InputEvent.data";
    i_input_type, "inputType", "InputEvent.inputType";
    i_composing, "isComposing", "InputEvent.isComposing";
    p_id, "pointerId", "PointerEvent.pointerId";
    p_width, "width", "PointerEvent.width";
    p_height, "height", "PointerEvent.height";
    p_pressure, "pressure", "PointerEvent.pressure";
    p_tangential, "tangentialPressure", "PointerEvent.tangentialPressure";
    p_tilt_x, "tiltX", "PointerEvent.tiltX";
    p_tilt_y, "tiltY", "PointerEvent.tiltY";
    p_twist, "twist", "PointerEvent.twist";
    p_altitude, "altitudeAngle", "PointerEvent.altitudeAngle";
    p_azimuth, "azimuthAngle", "PointerEvent.azimuthAngle";
    p_type, "pointerType", "PointerEvent.pointerType";
    p_primary, "isPrimary", "PointerEvent.isPrimary";
    d_data_transfer, "dataTransfer", "DragEvent.dataTransfer";
}

/// CSSOM View §"pageX": `clientX + scrollX`, and there is no scrolling
/// viewport here, so the scroll offset is 0.
unsafe extern "sysv64" fn m_page_x(g: *mut c_void, cf: *mut c_void) -> u64 {
    slot_get(g, cf, "clientX", "MouseEvent.pageX")
}
unsafe extern "sysv64" fn m_page_y(g: *mut c_void, cf: *mut c_void) -> u64 {
    slot_get(g, cf, "clientY", "MouseEvent.pageY")
}
/// `x`/`y` are defined as aliases of `clientX`/`clientY`.
unsafe extern "sysv64" fn m_x(g: *mut c_void, cf: *mut c_void) -> u64 {
    slot_get(g, cf, "clientX", "MouseEvent.x")
}
unsafe extern "sysv64" fn m_y(g: *mut c_void, cf: *mut c_void) -> u64 {
    slot_get(g, cf, "clientY", "MouseEvent.y")
}

/// `getModifierState` is the same operation on every event that mixes in
/// `KeyboardEventInit`'s modifiers — one body, one label per interface.
unsafe fn modifier_state(g: *mut c_void, cf: *mut c_void, label: &str) -> u64 {
    let key = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let Some(s) = slots(g, cf_this(cf)) else {
        return throw_type_error(g, &format!("{label} called on a non-Event"));
    };
    // UI Events §"modifier keys": only the four this runtime tracks can be
    // true. Every other named modifier is legitimately false — there is no key
    // state source for CapsLock or AltGraph here.
    let slot = match key.as_str() {
        "Control" => "ctrlKey",
        "Shift" => "shiftKey",
        "Alt" => "altKey",
        "Meta" => "metaKey",
        _ => return FALSE,
    };
    enc_bool(truthy_flag(get(g, s, slot)))
}

unsafe extern "sysv64" fn m_get_modifier_state(g: *mut c_void, cf: *mut c_void) -> u64 {
    modifier_state(g, cf, "MouseEvent.getModifierState")
}

unsafe extern "sysv64" fn k_get_modifier_state(g: *mut c_void, cf: *mut c_void) -> u64 {
    modifier_state(g, cf, "KeyboardEvent.getModifierState")
}

unsafe extern "sysv64" fn p_empty_list(g: *mut c_void, _cf: *mut c_void) -> u64 {
    new_array(g)
}

// ---------------------------------------------------------------------------
// Constructors
// ---------------------------------------------------------------------------

macro_rules! event_ctor {
    ($name:ident, $field:ident, $init:path, $label:literal) => {
        unsafe extern "sysv64" fn $name(g: *mut c_void, cf: *mut c_void) -> u64 {
            if cf_argc(cf) < 1 {
                return throw_type_error(g, concat!($label, " constructor requires a type argument"));
            }
            let c = classes(g);
            let obj = ordinary_create(g, cf_new_target(cf), c.$field.proto);
            $init(g, obj, cf_arg(cf, 0), cf_arg(cf, 1));
            obj
        }
    };
}

event_ctor!(ui_construct, ui_event, init_ui_event, "UIEvent");
event_ctor!(mouse_construct, mouse, init_mouse_event, "MouseEvent");
event_ctor!(keyboard_construct, keyboard, init_keyboard_event, "KeyboardEvent");
event_ctor!(wheel_construct, wheel, init_wheel_event, "WheelEvent");
event_ctor!(focus_construct, focus, init_focus_event, "FocusEvent");
event_ctor!(input_construct, input, init_input_event, "InputEvent");
event_ctor!(pointer_construct, pointer, init_pointer_event, "PointerEvent");

unsafe extern "sysv64" fn drag_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "DragEvent constructor requires a type argument");
    }
    let c = classes(g);
    let obj = ordinary_create(g, cf_new_target(cf), c.drag.proto);
    init_mouse_event(g, obj, cf_arg(cf, 0), cf_arg(cf, 1));
    if let Some(s) = slots(g, obj) {
        define(g, s, "dataTransfer", init_obj(g, cf_arg(cf, 1), "dataTransfer"), 0);
    }
    obj
}

// ---------------------------------------------------------------------------
// DataTransfer
// ---------------------------------------------------------------------------

unsafe fn this_dt<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut DataTransferData> {
    host_data::<DataTransferData>(g, classes(g).dt_cls, cf_this(cf))
}

unsafe extern "sysv64" fn dt_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let proto = ordinary_create_proto(g, cf_new_target(cf), c.dt.proto);
    host_create(
        g,
        c.dt_cls,
        proto,
        DataTransferData { items: Vec::new(), drop_effect: "none".into(), effect_allowed: "none".into() },
    )
}

unsafe fn ordinary_create_proto(g: *mut c_void, new_target: u64, fallback: u64) -> u64 {
    let p = if is_cell(new_target) { get(g, new_target, "prototype") } else { UNDEF };
    if is_cell(p) { p } else { fallback }
}

unsafe extern "sysv64" fn dt_get_data(g: *mut c_void, cf: *mut c_void) -> u64 {
    let format = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    match this_dt(g, cf) {
        Some(d) => {
            let v = d.get(&format);
            js_string(g, &v)
        }
        None => throw_type_error(g, "DataTransfer.getData called on a non-DataTransfer"),
    }
}

unsafe extern "sysv64" fn dt_set_data(g: *mut c_void, cf: *mut c_void) -> u64 {
    let format = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let data = read_string(g, cf_arg(cf, 1)).unwrap_or_default();
    match this_dt(g, cf) {
        Some(d) => {
            d.set(&format, &data);
            UNDEF
        }
        None => throw_type_error(g, "DataTransfer.setData called on a non-DataTransfer"),
    }
}

unsafe extern "sysv64" fn dt_clear_data(g: *mut c_void, cf: *mut c_void) -> u64 {
    let arg = cf_arg(cf, 0);
    let format = if arg == UNDEF { None } else { read_string(g, arg) };
    match this_dt(g, cf) {
        Some(d) => {
            d.clear(format.as_deref());
            UNDEF
        }
        None => throw_type_error(g, "DataTransfer.clearData called on a non-DataTransfer"),
    }
}

unsafe extern "sysv64" fn dt_types(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(d) = this_dt(g, cf) else {
        return throw_type_error(g, "DataTransfer.types called on a non-DataTransfer");
    };
    let formats: Vec<String> = d.items.iter().map(|(k, _)| k.clone()).collect();
    let arr = new_array(g);
    for (i, f) in formats.iter().enumerate() {
        let v = js_string(g, f);
        set_i(g, arr, i as u32, v);
    }
    arr
}

unsafe extern "sysv64" fn dt_files(g: *mut c_void, cf: *mut c_void) -> u64 {
    if this_dt(g, cf).is_none() {
        return throw_type_error(g, "DataTransfer.files called on a non-DataTransfer");
    }
    // Empty rather than absent: a drag in this runtime genuinely carries no
    // files, and `e.dataTransfer.files.length` must not throw.
    new_array(g)
}

unsafe extern "sysv64" fn dt_items(g: *mut c_void, _cf: *mut c_void) -> u64 {
    throw_type_error(
        g,
        "DataTransfer.items is a DataTransferItemList over File entries, and this runtime has no \
         File class — there is nothing truthful to put in the list. Use getData()/setData() for \
         string data; a host that gains file drops has to add File first.",
    )
}

macro_rules! dt_string_accessor {
    ($get:ident, $set:ident, $field:ident, $label:literal) => {
        unsafe extern "sysv64" fn $get(g: *mut c_void, cf: *mut c_void) -> u64 {
            match this_dt(g, cf) {
                Some(d) => {
                    let v = d.$field.clone();
                    js_string(g, &v)
                }
                None => throw_type_error(g, concat!($label, " called on a non-DataTransfer")),
            }
        }
        unsafe extern "sysv64" fn $set(g: *mut c_void, cf: *mut c_void) -> u64 {
            let v = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
            match this_dt(g, cf) {
                Some(d) => {
                    d.$field = v;
                    UNDEF
                }
                None => throw_type_error(g, concat!($label, " called on a non-DataTransfer")),
            }
        }
    };
}
dt_string_accessor!(dt_get_drop, dt_set_drop, drop_effect, "DataTransfer.dropEffect");
dt_string_accessor!(dt_get_effect, dt_set_effect, effect_allowed, "DataTransfer.effectAllowed");

unsafe extern "sysv64" fn dt_noop(_g: *mut c_void, _cf: *mut c_void) -> u64 {
    UNDEF
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

/// WebIDL §"Constants": a constant is defined on both the interface object and
/// the interface prototype object, read-only and non-configurable — but
/// enumerable, unlike every method and accessor above.
unsafe fn define_constants(g: *mut c_void, class: &Class, consts: &[(&str, i32)]) {
    for (name, value) in consts {
        define(g, class.ctor, name, enc_i32(*value), 1 | 4);
        define(g, class.proto, name, enc_i32(*value), 1 | 4);
    }
}

pub unsafe fn install(g: *mut c_void, event_proto: u64) {
    let ui_event = define_class(g, &ClassSpec {
        name: "UIEvent",
        arity: 1,
        construct: Some(ui_construct),
        parent_proto: event_proto,
        methods: &[],
        getters: &[("view", ui_view), ("detail", ui_detail), ("which", ui_which)],
        accessors: &[],
    });

    let mouse = define_class(g, &ClassSpec {
        name: "MouseEvent",
        arity: 1,
        construct: Some(mouse_construct),
        parent_proto: ui_event.proto,
        methods: &[("getModifierState", 1, m_get_modifier_state)],
        getters: &[
            ("screenX", m_screen_x), ("screenY", m_screen_y),
            ("clientX", m_client_x), ("clientY", m_client_y),
            ("x", m_x), ("y", m_y),
            ("pageX", m_page_x), ("pageY", m_page_y),
            ("offsetX", m_offset_x), ("offsetY", m_offset_y),
            ("movementX", m_movement_x), ("movementY", m_movement_y),
            ("button", m_button), ("buttons", m_buttons),
            ("ctrlKey", m_ctrl), ("shiftKey", m_shift), ("altKey", m_alt), ("metaKey", m_meta),
            ("relatedTarget", m_related),
        ],
        accessors: &[],
    });

    let keyboard = define_class(g, &ClassSpec {
        name: "KeyboardEvent",
        arity: 1,
        construct: Some(keyboard_construct),
        parent_proto: ui_event.proto,
        methods: &[("getModifierState", 1, k_get_modifier_state)],
        getters: &[
            ("key", k_key), ("code", k_code), ("location", k_location),
            ("repeat", k_repeat), ("isComposing", k_composing),
            ("ctrlKey", k_ctrl), ("shiftKey", k_shift), ("altKey", k_alt), ("metaKey", k_meta),
        ],
        accessors: &[],
    });
    define_constants(g, &keyboard, KEY_LOCATIONS);

    let wheel = define_class(g, &ClassSpec {
        name: "WheelEvent",
        arity: 1,
        construct: Some(wheel_construct),
        parent_proto: mouse.proto,
        methods: &[],
        getters: &[
            ("deltaX", w_delta_x), ("deltaY", w_delta_y), ("deltaZ", w_delta_z),
            ("deltaMode", w_delta_mode),
        ],
        accessors: &[],
    });
    define_constants(g, &wheel, DELTA_MODES);

    let focus = define_class(g, &ClassSpec {
        name: "FocusEvent",
        arity: 1,
        construct: Some(focus_construct),
        parent_proto: ui_event.proto,
        methods: &[],
        getters: &[("relatedTarget", f_related)],
        accessors: &[],
    });

    let input = define_class(g, &ClassSpec {
        name: "InputEvent",
        arity: 1,
        construct: Some(input_construct),
        parent_proto: ui_event.proto,
        methods: &[],
        getters: &[
            ("data", i_data), ("inputType", i_input_type), ("isComposing", i_composing),
        ],
        accessors: &[],
    });

    let pointer = define_class(g, &ClassSpec {
        name: "PointerEvent",
        arity: 1,
        construct: Some(pointer_construct),
        parent_proto: mouse.proto,
        methods: &[
            ("getCoalescedEvents", 0, p_empty_list),
            ("getPredictedEvents", 0, p_empty_list),
        ],
        getters: &[
            ("pointerId", p_id), ("width", p_width), ("height", p_height),
            ("pressure", p_pressure), ("tangentialPressure", p_tangential),
            ("tiltX", p_tilt_x), ("tiltY", p_tilt_y), ("twist", p_twist),
            ("altitudeAngle", p_altitude), ("azimuthAngle", p_azimuth),
            ("pointerType", p_type), ("isPrimary", p_primary),
        ],
        accessors: &[],
    });

    let dt_cls = make_host_class::<DataTransferData>("DataTransfer");
    let dt = define_class(g, &ClassSpec {
        name: "DataTransfer",
        arity: 0,
        construct: Some(dt_construct),
        parent_proto: 0,
        methods: &[
            ("getData", 1, dt_get_data),
            ("setData", 2, dt_set_data),
            ("clearData", 1, dt_clear_data),
            // Spec: sets the drag feedback image. There is no drag feedback
            // surface here, and the call is advisory — a no-op is the whole
            // behaviour, not a placeholder for one.
            ("setDragImage", 3, dt_noop),
        ],
        getters: &[("types", dt_types), ("files", dt_files), ("items", dt_items)],
        accessors: &[
            ("dropEffect", dt_get_drop, dt_set_drop),
            ("effectAllowed", dt_get_effect, dt_set_effect),
        ],
    });

    let drag = define_class(g, &ClassSpec {
        name: "DragEvent",
        arity: 1,
        construct: Some(drag_construct),
        parent_proto: mouse.proto,
        methods: &[],
        getters: &[("dataTransfer", d_data_transfer)],
        accessors: &[],
    });

    let boxed: &'static UiClasses = Box::leak(Box::new(UiClasses {
        ui_event,
        mouse,
        keyboard,
        wheel,
        focus,
        input,
        pointer,
        drag,
        dt_cls,
        dt,
    }));
    registry().lock().unwrap().insert(g as usize, boxed);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_aliases_normalise_the_way_the_spec_says() {
        assert_eq!(normalize_format("Text"), "text/plain");
        assert_eq!(normalize_format("URL"), "text/uri-list");
        assert_eq!(normalize_format("Text/HTML"), "text/html");
    }

    #[test]
    fn setting_the_same_format_twice_replaces_rather_than_appends() {
        let mut d = DataTransferData::default();
        d.set("text", "a");
        d.set("text/plain", "b");
        assert_eq!(d.items.len(), 1, "the alias created a second entry: {:?}", d.items);
        assert_eq!(d.get("Text"), "b");
    }

    #[test]
    fn a_missing_format_reads_back_as_empty_string_not_a_panic() {
        assert_eq!(DataTransferData::default().get("text/plain"), "");
    }

    #[test]
    fn clear_data_with_a_format_removes_only_that_one() {
        let mut d = DataTransferData::default();
        d.set("text", "a");
        d.set("text/html", "b");
        d.clear(Some("text"));
        assert_eq!(d.get("text/html"), "b");
        assert_eq!(d.get("text"), "");
        d.clear(None);
        assert!(d.items.is_empty());
    }
}

#[cfg(test)]
mod jsc_tests {
    use crate::webapi::testing::*;

    #[test]
    fn the_inheritance_chain_reaches_event() {
        let c = ctx();
        ok(&c, "new MouseEvent('click') instanceof MouseEvent");
        ok(&c, "new MouseEvent('click') instanceof UIEvent");
        ok(&c, "new MouseEvent('click') instanceof Event");
        ok(&c, "new PointerEvent('pointerdown') instanceof MouseEvent");
        ok(&c, "new DragEvent('drop') instanceof MouseEvent");
        ok(&c, "!(new MouseEvent('click') instanceof PointerEvent)");
    }

    #[test]
    fn init_dictionary_members_read_back() {
        let c = ctx();
        ok(&c, "(e => e.clientX === 10 && e.clientY === 20 && e.button === 2 && e.ctrlKey === true)\
                (new MouseEvent('click', { clientX: 10, clientY: 20, button: 2, ctrlKey: true }))");
        ok(&c, "(e => e.x === 10 && e.pageX === 10)(new MouseEvent('click', { clientX: 10 }))");
        ok(&c, "(e => e.pointerType === 'pen' && e.pressure === 0.5 && e.isPrimary === true)\
                (new PointerEvent('pointerdown', { pointerType: 'pen', pressure: 0.5, isPrimary: true }))");
    }

    #[test]
    fn unset_members_take_their_specified_defaults() {
        let c = ctx();
        ok(&c, "(e => e.clientX === 0 && e.button === 0 && e.buttons === 0 && e.ctrlKey === false && e.relatedTarget === null)(new MouseEvent('click'))");
        ok(&c, "(e => e.width === 1 && e.height === 1 && e.pointerType === '' && e.altitudeAngle === Math.PI / 2)(new PointerEvent('p'))");
    }

    #[test]
    fn offset_coordinates_come_from_the_init_dict_and_do_not_guess() {
        // The narrowing that matters: offsetX has no layout to be relative to,
        // so it must NOT silently mirror clientX — that is a plausible wrong
        // number, and a hit test built on it lands in the wrong place.
        let c = ctx();
        ok(&c, "new MouseEvent('click', { clientX: 100 }).offsetX === 0");
        ok(&c, "new MouseEvent('click', { offsetX: 7 }).offsetX === 7");
    }

    #[test]
    fn get_modifier_state_answers_the_four_tracked_keys() {
        let c = ctx();
        ok(&c, "new MouseEvent('c', { shiftKey: true }).getModifierState('Shift') === true");
        ok(&c, "new MouseEvent('c', { shiftKey: true }).getModifierState('Control') === false");
        ok(&c, "new MouseEvent('c').getModifierState('CapsLock') === false");
    }

    #[test]
    fn a_mouse_event_dispatches_through_the_existing_event_target() {
        // The reason these are subclasses and not a parallel system: the one
        // dispatch path in events.rs carries them unchanged.
        let c = ctx();
        s(
            &c,
            "globalThis.seen = null; const t = new EventTarget(); \
             t.addEventListener('click', e => { globalThis.seen = [e.type, e.clientX, e instanceof MouseEvent, e.target === t]; }); \
             t.dispatchEvent(new MouseEvent('click', { clientX: 42 })); 'ok'",
        );
        ok(&c, "globalThis.seen !== null");
        ok(&c, "globalThis.seen.join() === 'click,42,true,true'");
    }

    #[test]
    fn prevent_default_still_works_through_the_subclass() {
        let c = ctx();
        ok(&c, "(() => { const t = new EventTarget(); t.addEventListener('x', e => e.preventDefault()); \
                 return t.dispatchEvent(new MouseEvent('x', { cancelable: true })) === false; })()");
    }

    #[test]
    fn a_drag_event_carries_its_data_transfer() {
        let c = ctx();
        ok(
            &c,
            "(() => { const dt = new DataTransfer(); dt.setData('text', 'hello'); \
              const e = new DragEvent('drop', { dataTransfer: dt }); \
              return e.dataTransfer === dt && e.dataTransfer.getData('text/plain') === 'hello' \
                  && e.dataTransfer.types.join() === 'text/plain'; })()",
        );
        ok(&c, "new DragEvent('drop').dataTransfer === null");
    }

    #[test]
    fn data_transfer_items_refuses_by_name() {
        let c = ctx();
        let msg = throws(&c, "new DataTransfer().items");
        assert!(msg.contains("DataTransferItemList"), "{msg}");
        assert!(msg.contains("File"), "{msg}");
        // …and `files` does not, because empty is the truthful answer.
        ok(&c, "new DataTransfer().files.length === 0");
    }

    #[test]
    fn a_getter_on_a_foreign_object_is_a_type_error() {
        let c = ctx();
        let msg = throws(&c, "Object.getOwnPropertyDescriptor(MouseEvent.prototype,'clientX').get.call({})");
        assert!(msg.contains("non-Event"), "{msg}");
    }

    // -- KeyboardEvent ------------------------------------------------------

    #[test]
    fn a_keyboard_event_is_a_ui_event_and_an_event() {
        let c = ctx();
        ok(&c, "new KeyboardEvent('keydown') instanceof KeyboardEvent");
        ok(&c, "new KeyboardEvent('keydown') instanceof UIEvent");
        ok(&c, "new KeyboardEvent('keydown') instanceof Event");
        ok(&c, "!(new KeyboardEvent('keydown') instanceof MouseEvent)");
        ok(&c, "Object.getPrototypeOf(KeyboardEvent.prototype) === UIEvent.prototype");
        ok(&c, "new KeyboardEvent('keydown').type === 'keydown'");
    }

    #[test]
    fn keyboard_event_members_default_the_way_the_spec_says() {
        let c = ctx();
        ok(
            &c,
            "(e => e.key === '' && e.code === '' && e.location === 0 && e.repeat === false \
              && e.isComposing === false && e.ctrlKey === false && e.shiftKey === false \
              && e.altKey === false && e.metaKey === false)(new KeyboardEvent('keydown'))",
        );
        // …and the same with an empty dictionary, not just a missing one.
        ok(&c, "new KeyboardEvent('keydown', {}).key === ''");
    }

    #[test]
    fn keyboard_event_reads_every_member_back_from_its_init_dictionary() {
        let c = ctx();
        ok(
            &c,
            "(e => e.key === 'a' && e.code === 'KeyA' && e.location === 3 && e.repeat === true \
              && e.isComposing === true && e.ctrlKey === true && e.shiftKey === true \
              && e.altKey === true && e.metaKey === true)\
             (new KeyboardEvent('keydown', { key: 'a', code: 'KeyA', \
               location: KeyboardEvent.DOM_KEY_LOCATION_NUMPAD, repeat: true, isComposing: true, \
               ctrlKey: true, shiftKey: true, altKey: true, metaKey: true }))",
        );
        // Inherited UIEvent/Event members come along.
        ok(
            &c,
            "(e => e.bubbles === true && e.cancelable === true && e.detail === 7 && e.view === null)\
             (new KeyboardEvent('keydown', { bubbles: true, cancelable: true, detail: 7 }))",
        );
    }

    #[test]
    fn keyboard_event_get_modifier_state_answers_the_four_tracked_keys() {
        let c = ctx();
        ok(&c, "new KeyboardEvent('k', { ctrlKey: true }).getModifierState('Control') === true");
        ok(&c, "new KeyboardEvent('k', { shiftKey: true }).getModifierState('Shift') === true");
        ok(&c, "new KeyboardEvent('k', { altKey: true }).getModifierState('Alt') === true");
        ok(&c, "new KeyboardEvent('k', { metaKey: true }).getModifierState('Meta') === true");
        ok(&c, "new KeyboardEvent('k').getModifierState('Control') === false");
        ok(&c, "new KeyboardEvent('k').getModifierState('CapsLock') === false");
    }

    #[test]
    fn the_key_location_constants_sit_on_the_constructor_and_the_prototype() {
        let c = ctx();
        for owner in ["KeyboardEvent", "KeyboardEvent.prototype"] {
            ok(&c, &format!("{owner}.DOM_KEY_LOCATION_STANDARD === 0"));
            ok(&c, &format!("{owner}.DOM_KEY_LOCATION_LEFT === 1"));
            ok(&c, &format!("{owner}.DOM_KEY_LOCATION_RIGHT === 2"));
            ok(&c, &format!("{owner}.DOM_KEY_LOCATION_NUMPAD === 3"));
        }
        ok(&c, "new KeyboardEvent('k').DOM_KEY_LOCATION_NUMPAD === 3");
        // WebIDL constants are read-only and non-configurable.
        ok(
            &c,
            "(d => d.writable === false && d.configurable === false)\
             (Object.getOwnPropertyDescriptor(KeyboardEvent, 'DOM_KEY_LOCATION_LEFT'))",
        );
    }

    #[test]
    fn a_cancelable_keyboard_event_can_be_prevented() {
        let c = ctx();
        ok(&c, "new KeyboardEvent('k').cancelable === false");
        ok(
            &c,
            "(() => { const e = new KeyboardEvent('k', { cancelable: true }); \
              e.preventDefault(); return e.defaultPrevented === true; })()",
        );
        // Not cancelable → preventDefault is a no-op, per the spec.
        ok(
            &c,
            "(() => { const e = new KeyboardEvent('k'); e.preventDefault(); \
              return e.defaultPrevented === false; })()",
        );
        ok(
            &c,
            "(() => { const t = new EventTarget(); t.addEventListener('keydown', e => e.preventDefault()); \
              return t.dispatchEvent(new KeyboardEvent('keydown', { cancelable: true })) === false; })()",
        );
    }

    // -- WheelEvent ---------------------------------------------------------

    #[test]
    fn a_wheel_event_is_a_mouse_event_all_the_way_down() {
        let c = ctx();
        ok(&c, "new WheelEvent('wheel') instanceof WheelEvent");
        ok(&c, "new WheelEvent('wheel') instanceof MouseEvent");
        ok(&c, "new WheelEvent('wheel') instanceof UIEvent");
        ok(&c, "new WheelEvent('wheel') instanceof Event");
        ok(&c, "Object.getPrototypeOf(WheelEvent.prototype) === MouseEvent.prototype");
    }

    #[test]
    fn wheel_deltas_default_to_zero_and_read_back_from_the_init_dictionary() {
        let c = ctx();
        ok(
            &c,
            "(e => e.deltaX === 0 && e.deltaY === 0 && e.deltaZ === 0 && e.deltaMode === 0)\
             (new WheelEvent('wheel'))",
        );
        ok(
            &c,
            "(e => e.deltaX === -1.5 && e.deltaY === 120 && e.deltaZ === 3 && e.deltaMode === 1)\
             (new WheelEvent('wheel', { deltaX: -1.5, deltaY: 120, deltaZ: 3, \
               deltaMode: WheelEvent.DOM_DELTA_LINE }))",
        );
        // The MouseEvent members it inherits are real, not shadowed away.
        ok(
            &c,
            "(e => e.clientX === 4 && e.ctrlKey === true && e.buttons === 0 \
              && e.getModifierState('Control') === true)\
             (new WheelEvent('wheel', { clientX: 4, ctrlKey: true }))",
        );
    }

    #[test]
    fn delta_mode_is_an_enumeration_so_it_is_coerced_to_an_unsigned_long() {
        // The reason deltaMode does not go through the plain numeric path:
        // every use of it is `=== WheelEvent.DOM_DELTA_*`, and 1.5 matches none
        // of them while looking like a valid value.
        let c = ctx();
        ok(&c, "new WheelEvent('wheel', { deltaMode: 1.9 }).deltaMode === 1");
        ok(&c, "new WheelEvent('wheel', { deltaMode: NaN }).deltaMode === 0");
        // …while a plain double member keeps its fraction.
        ok(&c, "new WheelEvent('wheel', { deltaY: 1.9 }).deltaY === 1.9");
    }

    #[test]
    fn the_delta_mode_constants_sit_on_the_constructor_and_the_prototype() {
        let c = ctx();
        for owner in ["WheelEvent", "WheelEvent.prototype"] {
            ok(&c, &format!("{owner}.DOM_DELTA_PIXEL === 0"));
            ok(&c, &format!("{owner}.DOM_DELTA_LINE === 1"));
            ok(&c, &format!("{owner}.DOM_DELTA_PAGE === 2"));
        }
        ok(&c, "new WheelEvent('wheel').DOM_DELTA_PAGE === 2");
    }

    #[test]
    fn a_cancelable_wheel_event_can_be_prevented() {
        let c = ctx();
        ok(
            &c,
            "(() => { const t = new EventTarget(); t.addEventListener('wheel', e => e.preventDefault()); \
              return t.dispatchEvent(new WheelEvent('wheel', { cancelable: true, deltaY: 10 })) === false; })()",
        );
    }

    // -- FocusEvent ---------------------------------------------------------

    #[test]
    fn a_focus_event_is_a_ui_event_and_carries_its_related_target() {
        let c = ctx();
        ok(&c, "new FocusEvent('focus') instanceof FocusEvent");
        ok(&c, "new FocusEvent('focus') instanceof UIEvent");
        ok(&c, "new FocusEvent('focus') instanceof Event");
        ok(&c, "!(new FocusEvent('focus') instanceof MouseEvent)");
        ok(&c, "new FocusEvent('blur').relatedTarget === null");
        ok(&c, "new FocusEvent('blur', {}).relatedTarget === null");
        ok(
            &c,
            "(() => { const t = new EventTarget(); \
              return new FocusEvent('blur', { relatedTarget: t }).relatedTarget === t; })()",
        );
        ok(&c, "new FocusEvent('focus', { bubbles: true, detail: 2 }).detail === 2");
    }

    #[test]
    fn a_cancelable_focus_event_can_be_prevented() {
        let c = ctx();
        ok(
            &c,
            "(() => { const e = new FocusEvent('focusout', { cancelable: true }); \
              e.preventDefault(); return e.defaultPrevented === true; })()",
        );
    }

    // -- InputEvent ---------------------------------------------------------

    #[test]
    fn an_input_event_is_a_ui_event_with_null_data_by_default() {
        let c = ctx();
        ok(&c, "new InputEvent('input') instanceof InputEvent");
        ok(&c, "new InputEvent('input') instanceof UIEvent");
        ok(&c, "new InputEvent('input') instanceof Event");
        ok(
            &c,
            "(e => e.data === null && e.inputType === '' && e.isComposing === false)\
             (new InputEvent('input'))",
        );
    }

    #[test]
    fn input_event_members_read_back_from_the_init_dictionary() {
        let c = ctx();
        ok(
            &c,
            "(e => e.data === 'x' && e.inputType === 'insertText' && e.isComposing === true)\
             (new InputEvent('input', { data: 'x', inputType: 'insertText', isComposing: true }))",
        );
        // `data` is DOMString? — an explicit null stays null, and the empty
        // string stays the empty string rather than collapsing to null.
        ok(&c, "new InputEvent('input', { data: null }).data === null");
        ok(&c, "new InputEvent('input', { data: '' }).data === ''");
        ok(&c, "new InputEvent('beforeinput', { inputType: 'deleteContentBackward' }).data === null");
    }

    #[test]
    fn a_cancelable_input_event_can_be_prevented() {
        let c = ctx();
        ok(
            &c,
            "(() => { const t = new EventTarget(); t.addEventListener('beforeinput', e => e.preventDefault()); \
              return t.dispatchEvent(new InputEvent('beforeinput', { cancelable: true, data: 'q' })) === false; })()",
        );
    }

    // -- shared constructor behaviour ---------------------------------------

    #[test]
    fn the_new_classes_require_a_type_and_reject_a_bare_call() {
        let c = ctx();
        for name in ["KeyboardEvent", "WheelEvent", "FocusEvent", "InputEvent"] {
            let msg = throws(&c, &format!("new {name}()"));
            assert!(msg.contains("type argument"), "{name}: {msg}");
            let msg = throws(&c, &format!("{name}('x')"));
            assert!(msg.contains("new"), "{name}: {msg}");
        }
    }

    #[test]
    fn the_new_classes_subclass_through_new_target() {
        // ordinary_create takes the prototype from new.target, so a JS subclass
        // gets its own prototype and still reaches Event.
        let c = ctx();
        for (name, ty) in [
            ("KeyboardEvent", "keydown"),
            ("WheelEvent", "wheel"),
            ("FocusEvent", "focus"),
            ("InputEvent", "input"),
        ] {
            ok(
                &c,
                &format!(
                    "(() => {{ class Sub extends {name} {{ get tag() {{ return 'sub'; }} }} \
                      const e = new Sub('{ty}', {{ cancelable: true }}); \
                      return e instanceof Sub && e instanceof {name} && e instanceof Event \
                        && e.tag === 'sub' && e.type === '{ty}' && e.cancelable === true; }})()"
                ),
            );
        }
    }

    #[test]
    fn the_new_classes_dispatch_through_the_existing_event_target() {
        let c = ctx();
        s(
            &c,
            "globalThis.log = []; const t = new EventTarget(); \
             t.addEventListener('keydown', e => globalThis.log.push([e.key, e instanceof KeyboardEvent, e.target === t])); \
             t.addEventListener('wheel', e => globalThis.log.push([e.deltaY, e instanceof WheelEvent, e.target === t])); \
             t.addEventListener('input', e => globalThis.log.push([e.inputType, e instanceof InputEvent, e.target === t])); \
             t.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter' })); \
             t.dispatchEvent(new WheelEvent('wheel', { deltaY: 3 })); \
             t.dispatchEvent(new InputEvent('input', { inputType: 'insertText' })); 'ok'",
        );
        ok(&c, "globalThis.log.length === 3");
        ok(&c, "globalThis.log.map(r => r.join(':')).join('|') === 'Enter:true:true|3:true:true|insertText:true:true'");
    }

    #[test]
    fn the_new_getters_refuse_a_foreign_receiver() {
        let c = ctx();
        for (proto, name) in [
            ("KeyboardEvent", "key"),
            ("WheelEvent", "deltaY"),
            ("FocusEvent", "relatedTarget"),
            ("InputEvent", "inputType"),
        ] {
            let msg = throws(
                &c,
                &format!("Object.getOwnPropertyDescriptor({proto}.prototype,'{name}').get.call({{}})"),
            );
            assert!(msg.contains("non-Event"), "{proto}.{name}: {msg}");
            assert!(msg.contains(proto), "{proto}.{name}: {msg}");
        }
        let msg = throws(&c, "KeyboardEvent.prototype.getModifierState.call({}, 'Shift')");
        assert!(msg.contains("KeyboardEvent.getModifierState"), "{msg}");
    }
}
