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
//! - `DataTransfer` is the HTML drag data store: string and file items,
//!   `items` (a `DataTransferItemList` of `DataTransferItem`s), `files` (a
//!   `FileList`), and the four modes — read/write for a script-made one,
//!   protected for `dragenter` / `dragover` / `dragleave` (types visible,
//!   data and files not), read-only for `drop`, and disabled after. The HOST
//!   makes the one an OS drop carries ([`create_drag_data`]) and moves it
//!   between modes ([`set_drag_data_mode`]). `webkitGetAsEntry` is absent
//!   (there is no FileSystemEntry); `getAsFileSystemHandle` exists only while
//!   the host has installed it ([`set_file_system_handle_hook`]).
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

/// HTML §"drag data store mode", plus the state after the event: the object
/// "no longer associated with a drag data store".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragDataMode {
    /// `dragstart`, and every `new DataTransfer()`: readable and writable.
    ReadWrite,
    /// `drop`: everything readable, nothing writable.
    ReadOnly,
    /// `dragenter` / `dragover` / `dragleave`: the kinds and types of the
    /// items are visible; their data and files are not.
    Protected,
    /// The event is over: no items, no data, nothing writable.
    Disabled,
}

impl DragDataMode {
    fn readable(self) -> bool {
        matches!(self, DragDataMode::ReadWrite | DragDataMode::ReadOnly)
    }
}

/// One item of the drag data store's item list.
#[derive(Clone, Debug, PartialEq)]
pub enum ItemKind {
    /// A string item; the `String` is its data.
    Text(String),
    /// A file item. The `File` itself lives in the DataTransfer's JS-side
    /// slot under the item's id; `path` is set for a host drop (what
    /// `getAsFileSystemHandle` is handed), `None` for `items.add(file)`.
    File { path: Option<std::path::PathBuf>, is_dir: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// Never reused within a DataTransfer: a `DataTransferItem` whose item
    /// was removed must not come back to life as a later one.
    pub id: u64,
    pub kind: ItemKind,
    /// The item's type string (a format for text, the File's type for a file).
    pub mime: String,
}

pub struct DataTransferData {
    /// The item list, in order (`types` and `items` are order-sensitive).
    pub items: Vec<Item>,
    pub drop_effect: String,
    pub effect_allowed: String,
    pub mode: DragDataMode,
    next_id: u64,
}

impl Default for DataTransferData {
    /// What `new DataTransfer()` makes (HTML §"DataTransfer()").
    fn default() -> Self {
        DataTransferData { items: Vec::new(), drop_effect: "none".into(), effect_allowed: "none".into(), mode: DragDataMode::ReadWrite, next_id: 1 }
    }
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
    fn text_index(&self, f: &str) -> Option<usize> {
        self.items.iter().position(|i| matches!(i.kind, ItemKind::Text(_)) && i.mime == f)
    }

    /// `getData` over the item list (the mode is the caller's business).
    /// `"url"` reads `text/uri-list` and answers its first URL: the first
    /// line that is not a `#` comment.
    pub fn get(&self, format: &str) -> String {
        let f = normalize_format(format);
        let Some(i) = self.text_index(&f) else { return String::new() };
        let ItemKind::Text(data) = &self.items[i].kind else { return String::new() };
        if format.eq_ignore_ascii_case("url") {
            return data.split(['\r', '\n']).find(|l| !l.is_empty() && !l.starts_with('#')).unwrap_or("").to_string();
        }
        data.clone()
    }

    /// `setData`: the old item of that format goes and the new one is
    /// appended — so it moves to the end of `types`, as the spec says.
    pub fn set(&mut self, format: &str, data: &str) {
        let f = normalize_format(format);
        if let Some(i) = self.text_index(&f) {
            self.items.remove(i);
        }
        self.push(ItemKind::Text(data.to_string()), f);
    }

    /// `clearData`: string items only — "does not affect whether any files
    /// were included in the drag".
    pub fn clear(&mut self, format: Option<&str>) {
        match format {
            Some(f) => {
                let f = normalize_format(f);
                if let Some(i) = self.text_index(&f) {
                    self.items.remove(i);
                }
            }
            None => self.items.retain(|i| !matches!(i.kind, ItemKind::Text(_))),
        }
    }

    /// `types`: the string items' formats, then `"Files"` if there is a file.
    pub fn types(&self) -> Vec<String> {
        if self.mode == DragDataMode::Disabled {
            return Vec::new();
        }
        let mut t: Vec<String> = self.items.iter().filter(|i| matches!(i.kind, ItemKind::Text(_))).map(|i| i.mime.clone()).collect();
        if self.items.iter().any(|i| matches!(i.kind, ItemKind::File { .. })) {
            t.push("Files".into());
        }
        t
    }

    fn push(&mut self, kind: ItemKind, mime: String) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(Item { id, kind, mime });
        id
    }

    /// The items as `items` shows them: none once the event is over.
    fn visible(&self) -> &[Item] {
        if self.mode == DragDataMode::Disabled { &[] } else { &self.items }
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
    list_cls: capi::JSClassRef,
    list: Class,
    item_cls: capi::JSClassRef,
    item: Class,
    /// DataTransfer → its slot bag: `list` (the one DataTransferItemList),
    /// `files` and `objs` (id → File / DataTransferItem, null-prototype).
    s_dt: *mut c_void,
    /// DataTransferItemList / DataTransferItem → their DataTransfer.
    s_owner: *mut c_void,
    /// `Object.defineProperty` / `Object.freeze`, as installed.
    define_property: u64,
    freeze: u64,
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

/// The host's `getAsFileSystemHandle` (File System Access): given a dropped
/// item's path and whether it is a directory, a `FileSystemHandle` value
/// (`Ok`) or the exception the promise rejects with (`Err`). Called on the JS
/// thread with the item's drag data store still readable.
pub type HandleHook = unsafe fn(g: *mut c_void, path: &std::path::Path, is_dir: bool) -> Result<u64, u64>;

fn handle_hooks() -> &'static Mutex<HashMap<usize, HandleHook>> {
    static H: OnceLock<Mutex<HashMap<usize, HandleHook>>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn forget(gone: crate::context::Gone) {
    registry().lock().unwrap().remove(&gone.key);
    handle_hooks().lock().unwrap().remove(&gone.key);
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

unsafe fn dt_of<'a>(g: *mut c_void, v: u64) -> Option<&'a mut DataTransferData> {
    host_data::<DataTransferData>(g, classes(g).dt_cls, v)
}

unsafe fn this_dt<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut DataTransferData> {
    dt_of(g, cf_this(cf))
}

/// The DataTransfer's slot bag (see `UiClasses::s_dt`).
unsafe fn bag(g: *mut c_void, dt: u64) -> u64 {
    get_a(g, dt, classes(g).s_dt)
}

/// A DataTransfer with its bag and its one DataTransferItemList.
unsafe fn make_dt(g: *mut c_void, proto: u64, data: DataTransferData) -> u64 {
    let c = classes(g);
    let dt = host_create(g, c.dt_cls, proto, data);
    let b = new_object(g, NULL);
    define(g, b, "files", new_object(g, NULL), 0);
    define(g, b, "objs", new_object(g, NULL), 0);
    let list = host_create(g, c.list_cls, c.list.proto, DtListData { shown: 0 });
    glue::rjs_define(g, list, c.s_owner, dt, 1 | 2 | 4);
    define(g, b, "list", list, 0);
    glue::rjs_define(g, dt, c.s_dt, b, 1 | 2 | 4);
    dt
}

unsafe extern "sysv64" fn dt_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let proto = ordinary_create_proto(g, cf_new_target(cf), c.dt.proto);
    make_dt(g, proto, DataTransferData::default())
}

unsafe fn ordinary_create_proto(g: *mut c_void, new_target: u64, fallback: u64) -> u64 {
    let p = if is_cell(new_target) { get(g, new_target, "prototype") } else { UNDEF };
    if is_cell(p) { p } else { fallback }
}

/// The cached `DataTransferItem` for item `id` (the same object every time).
unsafe fn item_obj(g: *mut c_void, dt: u64, id: u64) -> u64 {
    let c = classes(g);
    let objs = get(g, bag(g, dt), "objs");
    let key = id.to_string();
    let v = get(g, objs, &key);
    if is_cell(v) {
        return v;
    }
    let o = host_create(g, c.item_cls, c.item.proto, DtItemData { id });
    glue::rjs_define(g, o, c.s_owner, dt, 1 | 2 | 4);
    define(g, objs, &key, o, 0);
    o
}

/// Makes the list's own index properties match the item list again, and
/// lets go of the Files and item objects of items that are gone. Runs after
/// every change to the item list or the mode.
unsafe fn resync(g: *mut c_void, dt: u64) {
    let c = classes(g);
    let Some(d) = dt_of(g, dt) else { return };
    let ids: Vec<u64> = d.visible().iter().map(|i| i.id).collect();
    let all: Vec<u64> = d.items.iter().map(|i| i.id).collect();
    let b = bag(g, dt);
    let list = get(g, b, "list");
    let Some(ld) = host_data::<DtListData>(g, c.list_cls, list) else { return };
    let shown = ld.shown;
    ld.shown = ids.len() as u32;
    for i in ids.len() as u32..shown {
        let a = atom(g, &i.to_string());
        glue::rjs_delete(g, list, a);
        glue::rjs_atom_free(g, a);
    }
    for (i, id) in ids.iter().enumerate() {
        let o = item_obj(g, dt, *id);
        // A legacy platform object's index: read-only, enumerable,
        // configurable (WebIDL) — configurable is also what lets the next
        // resync take it away.
        let desc = new_object(g, NULL);
        define(g, desc, "value", o, 0);
        define(g, desc, "enumerable", TRUE, 0);
        define(g, desc, "configurable", TRUE, 0);
        let _ = call(g, c.define_property, UNDEF, &[list, enc_i32(i as i32), desc]);
    }
    for map in ["files", "objs"] {
        let m = get(g, b, map);
        let mut keys = UNDEF;
        if glue::rjs_own_keys(g, m, &mut keys) != 0 {
            continue;
        }
        for k in 0..array_len(g, keys) {
            let Some(name) = read_string(g, get_i(g, keys, k)) else { continue };
            if name.parse::<u64>().is_ok_and(|id| !all.contains(&id)) {
                let a = atom(g, &name);
                glue::rjs_delete(g, m, a);
                glue::rjs_atom_free(g, a);
            }
        }
    }
}

/// `DOMString` argument `i` of a method that requires `n`, converted with
/// ToString (rethrowing); `Err` = the exception is pending.
unsafe fn string_arg(g: *mut c_void, cf: *mut c_void, i: u32, n: u32, who: &str) -> Result<String, ()> {
    let argc = cf_argc(cf);
    if argc < n {
        throw_type_error(g, &format!("Failed to execute {who}: {n} argument{} required, but only {argc} present.", if n == 1 { "" } else { "s" }));
        return Err(());
    }
    let s = to_js_string(g, cf_arg(cf, i))?;
    Ok(read_string(g, s).unwrap_or_default())
}

unsafe extern "sysv64" fn dt_get_data(g: *mut c_void, cf: *mut c_void) -> u64 {
    if this_dt(g, cf).is_none() {
        return throw_type_error(g, "DataTransfer.getData called on a non-DataTransfer");
    }
    let Ok(format) = string_arg(g, cf, 0, 1, "'getData' on 'DataTransfer'") else { return UNDEF };
    let Some(d) = this_dt(g, cf) else { return UNDEF };
    // Protected (dragenter/over/leave): types are visible, data is not.
    let v = if d.mode.readable() { d.get(&format) } else { String::new() };
    js_string(g, &v)
}

unsafe extern "sysv64" fn dt_set_data(g: *mut c_void, cf: *mut c_void) -> u64 {
    if this_dt(g, cf).is_none() {
        return throw_type_error(g, "DataTransfer.setData called on a non-DataTransfer");
    }
    let Ok(format) = string_arg(g, cf, 0, 2, "'setData' on 'DataTransfer'") else { return UNDEF };
    let Ok(data) = string_arg(g, cf, 1, 2, "'setData' on 'DataTransfer'") else { return UNDEF };
    let Some(d) = this_dt(g, cf) else { return UNDEF };
    if d.mode == DragDataMode::ReadWrite {
        d.set(&format, &data);
        resync(g, cf_this(cf));
    }
    UNDEF
}

unsafe extern "sysv64" fn dt_clear_data(g: *mut c_void, cf: *mut c_void) -> u64 {
    if this_dt(g, cf).is_none() {
        return throw_type_error(g, "DataTransfer.clearData called on a non-DataTransfer");
    }
    let format = if cf_arg(cf, 0) == UNDEF {
        None
    } else {
        let Ok(f) = string_arg(g, cf, 0, 1, "'clearData' on 'DataTransfer'") else { return UNDEF };
        Some(f)
    };
    let Some(d) = this_dt(g, cf) else { return UNDEF };
    if d.mode == DragDataMode::ReadWrite {
        d.clear(format.as_deref());
        resync(g, cf_this(cf));
    }
    UNDEF
}

/// `types`: a frozen array (a new one per read).
unsafe extern "sysv64" fn dt_types(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(d) = this_dt(g, cf) else {
        return throw_type_error(g, "DataTransfer.types called on a non-DataTransfer");
    };
    let formats = d.types();
    let arr = new_array(g);
    for (i, f) in formats.iter().enumerate() {
        let v = js_string(g, f);
        set_i(g, arr, i as u32, v);
    }
    let _ = call(g, classes(g).freeze, UNDEF, &[arr]);
    arr
}

/// `files`: a new FileList per read over the SAME File objects, empty unless
/// the data is readable (HTML: protected mode shows no files).
unsafe extern "sysv64" fn dt_files(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let Some(d) = this_dt(g, cf) else {
        return throw_type_error(g, "DataTransfer.files called on a non-DataTransfer");
    };
    let ids: Vec<u64> = if d.mode.readable() {
        d.items.iter().filter(|i| matches!(i.kind, ItemKind::File { .. })).map(|i| i.id).collect()
    } else {
        Vec::new()
    };
    let files_map = get(g, bag(g, this), "files");
    let files: Vec<u64> = ids.iter().map(|id| get(g, files_map, &id.to_string())).filter(|v| is_cell(*v)).collect();
    super::file::create_file_list(g, &files)
}

unsafe extern "sysv64" fn dt_items(g: *mut c_void, cf: *mut c_void) -> u64 {
    if this_dt(g, cf).is_none() {
        return throw_type_error(g, "DataTransfer.items called on a non-DataTransfer");
    }
    get(g, bag(g, cf_this(cf)), "list")
}

unsafe extern "sysv64" fn dt_get_drop(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_dt(g, cf) {
        Some(d) => {
            let v = d.drop_effect.clone();
            js_string(g, &v)
        }
        None => throw_type_error(g, "DataTransfer.dropEffect called on a non-DataTransfer"),
    }
}

/// `dropEffect` takes one of its four values in any mode; anything else is
/// ignored (HTML).
unsafe extern "sysv64" fn dt_set_drop(g: *mut c_void, cf: *mut c_void) -> u64 {
    if this_dt(g, cf).is_none() {
        return throw_type_error(g, "DataTransfer.dropEffect called on a non-DataTransfer");
    }
    let Ok(s) = to_js_string(g, cf_arg(cf, 0)) else { return UNDEF };
    let v = read_string(g, s).unwrap_or_default();
    if let Some(d) = this_dt(g, cf) {
        if matches!(v.as_str(), "none" | "copy" | "link" | "move") {
            d.drop_effect = v;
        }
    }
    UNDEF
}

unsafe extern "sysv64" fn dt_get_effect(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_dt(g, cf) {
        Some(d) => {
            let v = d.effect_allowed.clone();
            js_string(g, &v)
        }
        None => throw_type_error(g, "DataTransfer.effectAllowed called on a non-DataTransfer"),
    }
}

/// `effectAllowed` changes only in read/write mode (dragstart), and only to
/// one of its nine values (HTML).
unsafe extern "sysv64" fn dt_set_effect(g: *mut c_void, cf: *mut c_void) -> u64 {
    if this_dt(g, cf).is_none() {
        return throw_type_error(g, "DataTransfer.effectAllowed called on a non-DataTransfer");
    }
    let Ok(s) = to_js_string(g, cf_arg(cf, 0)) else { return UNDEF };
    let v = read_string(g, s).unwrap_or_default();
    if let Some(d) = this_dt(g, cf) {
        let known = matches!(v.as_str(), "none" | "copy" | "copyLink" | "copyMove" | "link" | "linkMove" | "move" | "all" | "uninitialized");
        if known && d.mode == DragDataMode::ReadWrite {
            d.effect_allowed = v;
        }
    }
    UNDEF
}

unsafe extern "sysv64" fn dt_noop(_g: *mut c_void, _cf: *mut c_void) -> u64 {
    UNDEF
}

// ---- DataTransferItemList ---------------------------------------------------

/// What a DataTransferItemList carries natively: how many index properties
/// it shows (what the next resync has to take away). Its items are its
/// DataTransfer's.
struct DtListData {
    shown: u32,
}

/// The list (or item) `v`'s DataTransfer and its data.
unsafe fn owner<'a>(g: *mut c_void, v: u64) -> Option<(u64, &'a mut DataTransferData)> {
    let dt = get_a(g, v, classes(g).s_owner);
    dt_of(g, dt).map(|d| (dt, d))
}

unsafe fn this_list<'a>(g: *mut c_void, cf: *mut c_void, what: &str) -> Result<(u64, &'a mut DataTransferData), u64> {
    let c = classes(g);
    let this = cf_this(cf);
    if host_data::<DtListData>(g, c.list_cls, this).is_none() {
        return Err(throw_type_error(g, &format!("DataTransferItemList.{what} called on a non-DataTransferItemList")));
    }
    owner(g, this).ok_or_else(|| throw_type_error(g, "DataTransferItemList: lost its DataTransfer"))
}

unsafe extern "sysv64" fn list_length(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_list(g, cf, "length") {
        Ok((_, d)) => enc_f64(d.visible().len() as f64),
        Err(e) => e,
    }
}

/// `add(data, type)` / `add(file)`. `null` unless read/write; a second
/// string item of a type is a NotSupportedError.
unsafe extern "sysv64" fn list_add(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Err(e) = this_list(g, cf, "add") {
        return e;
    }
    let argc = cf_argc(cf);
    // Overload resolution on the argument count: two arguments are
    // (DOMString, DOMString), one must be a File.
    let (kind, mime, file) = if argc >= 2 {
        let Ok(data) = string_arg(g, cf, 0, 2, "'add' on 'DataTransferItemList'") else { return UNDEF };
        let Ok(ty) = string_arg(g, cf, 1, 2, "'add' on 'DataTransferItemList'") else { return UNDEF };
        (ItemKind::Text(data), ty.to_ascii_lowercase(), UNDEF)
    } else if argc == 1 {
        let f = cf_arg(cf, 0);
        if !super::file::is_file(g, f) {
            return throw_type_error(g, "Failed to execute 'add' on 'DataTransferItemList': parameter 1 is not of type 'File'.");
        }
        let mime = super::blob::data_of(g, f).map(|b| b.mime.to_ascii_lowercase()).unwrap_or_default();
        (ItemKind::File { path: None, is_dir: false }, mime, f)
    } else {
        return throw_type_error(g, "Failed to execute 'add' on 'DataTransferItemList': 1 argument required, but only 0 present.");
    };
    let Ok((dt, d)) = this_list(g, cf, "add") else { return UNDEF };
    if d.mode != DragDataMode::ReadWrite {
        return NULL;
    }
    if matches!(kind, ItemKind::Text(_)) && d.text_index(&mime).is_some() {
        return super::domexception::throw(
            g,
            "NotSupportedError",
            &format!("Failed to execute 'add' on 'DataTransferItemList': An item already exists for type '{mime}'."),
        );
    }
    let id = d.push(kind, mime);
    if is_cell(file) {
        define(g, get(g, bag(g, dt), "files"), &id.to_string(), file, 0);
    }
    resync(g, dt);
    item_obj(g, dt, id)
}

unsafe extern "sysv64" fn list_remove(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Err(e) = this_list(g, cf, "remove") {
        return e;
    }
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "Failed to execute 'remove' on 'DataTransferItemList': 1 argument required, but only 0 present.");
    }
    let Ok(n) = to_number(g, cf_arg(cf, 0)) else { return UNDEF };
    let Ok((dt, d)) = this_list(g, cf, "remove") else { return UNDEF };
    match d.mode {
        DragDataMode::Disabled => return UNDEF,
        DragDataMode::ReadWrite => {}
        _ => {
            return super::domexception::throw(
                g,
                "InvalidStateError",
                "Failed to execute 'remove' on 'DataTransferItemList': The list is not writable.",
            )
        }
    }
    // `unsigned long`: ToNumber, then modulo 2^32.
    let i = if n.is_finite() { n.trunc().rem_euclid(4_294_967_296.0) as usize } else { 0 };
    if i < d.items.len() {
        d.items.remove(i);
        resync(g, dt);
    }
    UNDEF
}

unsafe extern "sysv64" fn list_clear(g: *mut c_void, cf: *mut c_void) -> u64 {
    let (dt, d) = match this_list(g, cf, "clear") {
        Ok(v) => v,
        Err(e) => return e,
    };
    if d.mode == DragDataMode::ReadWrite {
        d.items.clear();
        resync(g, dt);
    }
    UNDEF
}

// ---- DataTransferItem ---------------------------------------------------------

struct DtItemData {
    id: u64,
}

/// The item behind a DataTransferItem: `None` once it is disabled (removed,
/// or its event is over) — every member then answers as for no item.
unsafe fn this_item(g: *mut c_void, cf: *mut c_void, what: &str) -> Result<Option<(u64, DragDataMode, Item)>, u64> {
    let c = classes(g);
    let this = cf_this(cf);
    let Some(id) = host_data::<DtItemData>(g, c.item_cls, this).map(|d| d.id) else {
        return Err(throw_type_error(g, &format!("DataTransferItem.{what} called on a non-DataTransferItem")));
    };
    let Some((dt, d)) = owner(g, this) else { return Ok(None) };
    Ok(d.visible().iter().find(|i| i.id == id).cloned().map(|i| (dt, d.mode, i)))
}

unsafe extern "sysv64" fn item_kind(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_item(g, cf, "kind") {
        Ok(Some((_, _, i))) => js_string(g, if matches!(i.kind, ItemKind::Text(_)) { "string" } else { "file" }),
        Ok(None) => js_string(g, ""),
        Err(e) => e,
    }
}

unsafe extern "sysv64" fn item_type(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_item(g, cf, "type") {
        Ok(Some((_, _, i))) => js_string(g, &i.mime),
        Ok(None) => js_string(g, ""),
        Err(e) => e,
    }
}

/// The callback and the string, carried to the task that delivers them.
struct AsString {
    callback: u64,
    data: String,
}
unsafe impl Send for AsString {}

/// `getAsString(callback)`: the data is delivered from a queued task, never
/// synchronously, and only for a readable string item.
unsafe extern "sysv64" fn item_get_as_string(g: *mut c_void, cf: *mut c_void) -> u64 {
    let found = match this_item(g, cf, "getAsString") {
        Ok(v) => v,
        Err(e) => return e,
    };
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "Failed to execute 'getAsString' on 'DataTransferItem': 1 argument required, but only 0 present.");
    }
    let cb = cf_arg(cf, 0);
    if cb == NULL {
        return UNDEF;
    }
    if glue::rjs_kind(g, cb) != 9 {
        return throw_type_error(g, "Failed to execute 'getAsString' on 'DataTransferItem': The callback provided as parameter 1 is not a function.");
    }
    let Some((_, mode, Item { kind: ItemKind::Text(data), .. })) = found else { return UNDEF };
    if !mode.readable() {
        return UNDEF;
    }
    crate::ffi::redjs_protect(g, cb);
    let task = AsString { callback: cb, data };
    let id = crate::context::id_of(g);
    // A task of its own, delivered within the pump's budget (blob's inbox).
    super::blob::queue_task(
        g,
        Box::new(move |g| unsafe {
            let task = task;
            if crate::context::id_of(g) != id {
                return; // a different context at this address: the value is not its
            }
            let s = js_string(g, &task.data);
            if let Err(exc) = call(g, task.callback, UNDEF, &[s]) {
                crate::errors::report(g, exc, crate::errors::ErrKind::Listener);
            }
            crate::ffi::redjs_unprotect(g, task.callback);
        }),
    );
    UNDEF
}

/// `getAsFile()`: the File of a readable file item, else `null`.
unsafe extern "sysv64" fn item_get_as_file(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_item(g, cf, "getAsFile") {
        Ok(Some((dt, mode, i))) if mode.readable() && matches!(i.kind, ItemKind::File { .. }) => {
            let f = get(g, get(g, bag(g, dt), "files"), &i.id.to_string());
            if is_cell(f) { f } else { NULL }
        }
        Ok(_) => NULL,
        Err(e) => e,
    }
}

/// `getAsFileSystemHandle()` (File System Access): defined only while the host
/// has a hook installed, so `'getAsFileSystemHandle' in item` tells the truth.
/// `null` for a string item, a script-added File, or outside drop/dragstart.
unsafe extern "sysv64" fn item_get_as_handle(g: *mut c_void, cf: *mut c_void) -> u64 {
    let found = match this_item(g, cf, "getAsFileSystemHandle") {
        Ok(v) => v,
        Err(e) => return super::domexception::rejected_with(g, e),
    };
    let hook = handle_hooks().lock().unwrap().get(&(g as usize)).copied();
    let (Some(hook), Some((_, mode, Item { kind: ItemKind::File { path: Some(path), is_dir }, .. }))) = (hook, found) else {
        return resolved_promise(g, NULL);
    };
    if !mode.readable() {
        return resolved_promise(g, NULL);
    }
    match hook(g, &path, is_dir) {
        Ok(h) => resolved_promise(g, h),
        Err(e) => super::domexception::rejected_with(g, e),
    }
}

// ---- the host's side ------------------------------------------------------------

/// One dropped path, as the host's drop handler has it.
#[derive(Clone, Debug)]
pub struct DropEntry {
    pub path: std::path::PathBuf,
    pub is_dir: bool,
    /// `(size, mtime)` if the host stat'ed it already; `None` stats it here.
    pub stat: Option<(u64, std::time::SystemTime)>,
}

/// A DataTransfer for an OS drop: one path-backed `File` per entry (nothing
/// is read), `effectAllowed: "all"`, `dropEffect: "copy"`, in `mode`. The
/// host dispatches `dragenter` / `dragover` / `dragleave` with it in
/// [`DragDataMode::Protected`], switches it to `ReadOnly` for `drop`
/// ([`set_drag_data_mode`]), and to `Disabled` when the event is over.
///
/// A directory becomes a 0-byte File named after it, as in Chromium; reading
/// it fails. Its contents are reachable through `getAsFileSystemHandle` when
/// the host installs that hook (advisory A-F3 says so, once).
///
/// # Safety
/// `g` must be a live context with the web API installed, on its thread.
pub unsafe fn create_drag_data(g: *mut c_void, entries: &[DropEntry], mode: DragDataMode) -> u64 {
    let c = classes(g);
    let data = DataTransferData { effect_allowed: "all".into(), drop_effect: "copy".into(), mode, ..DataTransferData::default() };
    let dt = make_dt(g, c.dt.proto, data);
    let files_map = get(g, bag(g, dt), "files");
    for e in entries {
        let name = e.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let (size, mtime) = match (e.is_dir, e.stat) {
            (false, Some((s, m))) => (s, Some(m)),
            (false, None) => super::file::snapshot(&e.path).unwrap_or((0, None)),
            (true, _) => (0, e.stat.map(|s| s.1).or_else(|| std::fs::metadata(&e.path).and_then(|m| m.modified()).ok())),
        };
        if e.is_dir {
            super::blob::advise(
                "A-F3",
                "File (A-F3): a directory was dropped. It appears in dataTransfer.files as a 0-byte File that cannot be \
                 read, as in Chromium; walk its contents with items[i].getAsFileSystemHandle() where the host provides it."
                    .to_string(),
            );
        }
        let f = super::file::create_file(g, &e.path, &name, if e.is_dir { Some("") } else { None }, size, mtime, "");
        let mime = super::blob::data_of(g, f).map(|b| b.mime.clone()).unwrap_or_default();
        let Some(d) = dt_of(g, dt) else { break };
        let id = d.push(ItemKind::File { path: Some(e.path.clone()), is_dir: e.is_dir }, mime);
        define(g, files_map, &id.to_string(), f, 0);
    }
    resync(g, dt);
    dt
}

/// Moves a DataTransfer to another mode (the host, between drag events).
/// `false` when `dt` is not a DataTransfer.
///
/// # Safety
/// As [`create_drag_data`].
pub unsafe fn set_drag_data_mode(g: *mut c_void, dt: u64, mode: DragDataMode) -> bool {
    let Some(d) = dt_of(g, dt) else { return false };
    d.mode = mode;
    resync(g, dt);
    true
}

/// Installs (or, with `None`, removes) the host's `getAsFileSystemHandle`.
/// The method appears on `DataTransferItem.prototype` only while a hook is
/// installed — feature detection stays honest.
///
/// # Safety
/// As [`create_drag_data`].
pub unsafe fn set_file_system_handle_hook(g: *mut c_void, hook: Option<HandleHook>) {
    match hook {
        Some(h) => {
            handle_hooks().lock().unwrap().insert(g as usize, h);
        }
        None => {
            handle_hooks().lock().unwrap().remove(&(g as usize));
        }
    }
    let Some(c) = registry().lock().unwrap().get(&(g as usize)).copied() else { return };
    define_handle_method(g, c, hook.is_some());
}

unsafe fn define_handle_method(g: *mut c_void, c: &UiClasses, present: bool) {
    let a = atom(g, "getAsFileSystemHandle");
    if present {
        glue::rjs_define(g, c.item.proto, a, make_fn(g, "getAsFileSystemHandle", 0, item_get_as_handle), 2);
    } else {
        glue::rjs_delete(g, c.item.proto, a);
    }
    glue::rjs_atom_free(g, a);
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
    to_string_tag(g, dt.proto, "DataTransfer");

    let list_cls = make_host_class::<DtListData>("DataTransferItemList");
    let list = define_class(g, &ClassSpec {
        name: "DataTransferItemList",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[("add", 1, list_add), ("remove", 1, list_remove), ("clear", 0, list_clear)],
        getters: &[("length", list_length)],
        accessors: &[],
    });
    to_string_tag(g, list.proto, "DataTransferItemList");
    // An indexed getter and `length`: it iterates as an array does (WebIDL).
    let global = glue::rjs_global(g);
    let values = get(g, get(g, get(g, global, "Array"), "prototype"), "values");
    let it = glue::rjs_atom_iterator(g);
    glue::rjs_define(g, list.proto, it, values, 2);
    glue::rjs_atom_free(g, it);

    let item_cls = make_host_class::<DtItemData>("DataTransferItem");
    let item = define_class(g, &ClassSpec {
        name: "DataTransferItem",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[("getAsString", 1, item_get_as_string), ("getAsFile", 0, item_get_as_file)],
        getters: &[("kind", item_kind), ("type", item_type)],
        accessors: &[],
    });
    to_string_tag(g, item.proto, "DataTransferItem");

    let object = get(g, global, "Object");
    let define_property = get(g, object, "defineProperty");
    let freeze = get(g, object, "freeze");
    crate::ffi::redjs_protect(g, define_property);
    crate::ffi::redjs_protect(g, freeze);

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
        list_cls,
        list,
        item_cls,
        item,
        s_dt: atom_priv(g, "redjs#dataTransfer"),
        s_owner: atom_priv(g, "redjs#dataTransferOwner"),
        define_property,
        freeze,
    }));
    registry().lock().unwrap().insert(g as usize, boxed);
    // A hook the host installed before this context had the classes.
    if handle_hooks().lock().unwrap().contains_key(&(g as usize)) {
        define_handle_method(g, boxed, true);
    }
}

unsafe fn to_string_tag(g: *mut c_void, proto: u64, tag: &str) {
    let a = glue::rjs_atom_wellknown(g, 1);
    if !a.is_null() {
        glue::rjs_define(g, proto, a, js_string(g, tag), 1 | 2);
        glue::rjs_atom_free(g, a);
    }
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
    fn a_script_made_data_transfer_is_read_write_with_items() {
        let c = ctx();
        ok(&c, "(() => { const dt = new DataTransfer(); return dt.items === dt.items && dt.items instanceof DataTransferItemList \
                 && dt.items.length === 0 && dt.files instanceof FileList && dt.files.length === 0 \
                 && dt.dropEffect === 'none' && dt.effectAllowed === 'none'; })()");
        ok(&c, "(() => { const dt = new DataTransfer(); dt.setData('text', 'a'); dt.setData('text/html', '<b>'); dt.setData('Text/Plain', 'b'); \
                 const f = new File(['x'], 'x.txt', { type: 'Text/Plain' }); const it = dt.items.add(f); \
                 return dt.types.join() === 'text/html,text/plain,Files' && Object.isFrozen(dt.types) && dt.getData('text') === 'b' \
                   && dt.items.length === 3 && it === dt.items[2] && it.kind === 'file' && it.type === 'text/plain' && it.getAsFile() === f \
                   && dt.files[0] === f && dt.files !== dt.files && [...dt.items].map(i => i.kind).join() === 'string,string,file'; })()");
        ok(&c, "(() => { const dt = new DataTransfer(); dt.setData('url', '#c\\r\\nhttps://a.example/\\r\\nhttps://b.example/'); \
                 return dt.getData('URL') === 'https://a.example/' && dt.getData('text/uri-list').startsWith('#c'); })()");
        // clearData() leaves files alone; items.clear() does not.
        ok(&c, "(() => { const dt = new DataTransfer(); dt.setData('a', '1'); dt.items.add(new File([], 'f')); dt.clearData(); \
                 const kept = dt.types.join() === 'Files' && dt.items.length === 1; dt.items.clear(); return kept && dt.items.length === 0 && dt.items[0] === undefined; })()");
        assert!(throws(&c, "(() => { const dt = new DataTransfer(); dt.items.add('x', 'a/b'); dt.items.add('y', 'A/B'); })()")
            .contains("already exists"));
        assert!(throws(&c, "new DataTransfer().items.add('only one')").contains("not of type 'File'"));
        ok(&c, "(() => { const dt = new DataTransfer(); const it = dt.items.add('s', 'a/b'); dt.items.remove(0); \
                 return it.kind === '' && it.type === '' && dt.items.length === 0; })()");
        ok(&c, "(() => { const dt = new DataTransfer(); dt.dropEffect = 'bogus'; dt.effectAllowed = 'bogus'; const a = dt.dropEffect + dt.effectAllowed; \
                 dt.dropEffect = 'move'; dt.effectAllowed = 'copyLink'; return a === 'nonenone' && dt.dropEffect === 'move' && dt.effectAllowed === 'copyLink'; })()");
        assert_eq!(
            throws(&c, "new DataTransfer().getData()"),
            "Failed to execute 'getData' on 'DataTransfer': 1 argument required, but only 0 present."
        );
        ok(&c, "'getAsFileSystemHandle' in DataTransferItem.prototype === false && !('webkitGetAsEntry' in DataTransferItem.prototype)");
        assert!(throws(&c, "new DataTransferItemList()").contains("Illegal constructor"));
        assert!(throws(&c, "new DataTransferItem()").contains("Illegal constructor"));
    }

    #[test]
    fn get_as_string_delivers_from_a_task() {
        let c = crate::webapi::file::testing::file_ctx();
        let r = crate::webapi::file::testing::run(
            &c,
            "const dt = new DataTransfer(); dt.setData('text', 'hi'); let sync = 'none'; \
             const got = new Promise(r => dt.items[0].getAsString(s => { r(s); })); dt.items[0].getAsString(s => { sync = s; }); \
             const before = sync; dt.items[0].getAsString(null); \
             let threw = false; try { dt.items[0].getAsString(5); } catch (e) { threw = e instanceof TypeError; } \
             return [before, await got, threw].join();",
        );
        assert_eq!(r, "none,hi,true");
    }

    #[test]
    fn an_os_drop_is_protected_then_read_only_then_disabled() {
        use super::{create_drag_data, set_drag_data_mode, DragDataMode, DropEntry};
        use crate::webapi::file::testing::{file_ctx, run, TempFile};
        let c = file_ctx();
        let a = TempFile::new("photo.png", b"PNGDATA");
        let dir = std::env::temp_dir().join(format!("redjs-drop-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let g = c.raw();
        unsafe {
            let entries =
                [DropEntry { path: a.0.clone(), is_dir: false, stat: None }, DropEntry { path: dir.clone(), is_dir: true, stat: None }];
            let dt = create_drag_data(g, &entries, DragDataMode::Protected);
            crate::webapi::util::define(g, crate::glue::rjs_global(g), "dt", dt, 0);
            // dragenter / dragover: kinds and types, no files, no data.
            ok(&c, "dt.types.join() === 'Files' && dt.items.length === 2 && dt.items[0].kind === 'file' && dt.items[0].type === 'image/png' \
                    && dt.files.length === 0 && dt.items[0].getAsFile() === null && dt.effectAllowed === 'all' && dt.dropEffect === 'copy'");
            ok(&c, "(() => { dt.setData('x', 'y'); dt.items.add('a', 'b'); return dt.types.join() === 'Files'; })()");
            assert!(throws(&c, "dt.items.remove(0)").contains("not writable"));
            // drop: everything readable, nothing writable.
            assert!(set_drag_data_mode(g, dt, DragDataMode::ReadOnly));
            ok(&c, "dt.files.length === 2 && dt.files[0].name === 'photo.png' && dt.items[0].getAsFile() === dt.files[0] \
                    && dt.files[1].size === 0 && dt.files[1].type === '' && dt.items.add('a', 'b') === null");
            assert_eq!(run(&c, "return await dt.files[0].text();"), "PNGDATA");
            assert!(run(&c, "return await dt.files[1].text();").starts_with("ERROR: NotReadableError"), "a directory cannot be read");
            ok(&c, "(() => { globalThis.kept = dt.items[0]; return true; })()");
            // After the event: nothing is left in it.
            set_drag_data_mode(g, dt, DragDataMode::Disabled);
            ok(&c, "dt.types.length === 0 && dt.items.length === 0 && dt.items[0] === undefined && dt.files.length === 0 \
                    && kept.kind === '' && kept.getAsFile() === null && dt.getData('text') === ''");
        }
        assert!(crate::webapi::blob::advisories().iter().any(|m| m.contains("A-F3")));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn the_file_system_handle_hook_is_called_for_a_dropped_path() {
        use super::{create_drag_data, set_drag_data_mode, set_file_system_handle_hook, DragDataMode, DropEntry};
        use crate::webapi::file::testing::{file_ctx, run, TempFile};
        unsafe fn hook(g: *mut core::ffi::c_void, path: &std::path::Path, is_dir: bool) -> Result<u64, u64> {
            let o = crate::webapi::util::new_object(g, crate::webapi::util::UNDEF);
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            crate::webapi::util::define(g, o, "name", crate::webapi::util::js_string(g, &name), 0);
            crate::webapi::util::define(g, o, "kind", crate::webapi::util::js_string(g, if is_dir { "directory" } else { "file" }), 0);
            Ok(o)
        }
        let c = file_ctx();
        let t = TempFile::new("h.txt", b"h");
        let g = c.raw();
        unsafe {
            set_file_system_handle_hook(g, Some(hook));
            ok(&c, "typeof DataTransferItem.prototype.getAsFileSystemHandle === 'function'");
            let dt = create_drag_data(g, &[DropEntry { path: t.0.clone(), is_dir: false, stat: None }], DragDataMode::Protected);
            crate::webapi::util::define(g, crate::glue::rjs_global(g), "dt", dt, 0);
            assert_eq!(run(&c, "return String(await dt.items[0].getAsFileSystemHandle());"), "null", "protected: null");
            set_drag_data_mode(g, dt, DragDataMode::ReadOnly);
            assert_eq!(run(&c, "const h = await dt.items[0].getAsFileSystemHandle(); return h.kind + ':' + h.name;"), "file:h.txt");
            assert_eq!(
                run(&c, "const d = new DataTransfer(); d.items.add(new File([], 'f')); d.items.add('s', 't'); \
                         return [await d.items[0].getAsFileSystemHandle(), await d.items[1].getAsFileSystemHandle()].join();"),
                ",",
                "a script-added File and a string item have no handle"
            );
            set_file_system_handle_hook(g, None);
        }
        ok(&c, "!('getAsFileSystemHandle' in DataTransferItem.prototype)");
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
