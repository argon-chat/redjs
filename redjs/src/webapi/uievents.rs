//! Native `UIEvent` / `MouseEvent` / `PointerEvent` / `DragEvent` and the
//! `DataTransfer` a drag event carries (UI Events, Pointer Events, HTML §Drag
//! and drop).
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

unsafe extern "sysv64" fn m_get_modifier_state(g: *mut c_void, cf: *mut c_void) -> u64 {
    let key = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let Some(s) = slots(g, cf_this(cf)) else {
        return throw_type_error(g, "MouseEvent.getModifierState called on a non-Event");
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

    let boxed: &'static UiClasses =
        Box::leak(Box::new(UiClasses { ui_event, mouse, pointer, drag, dt_cls, dt }));
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
}
