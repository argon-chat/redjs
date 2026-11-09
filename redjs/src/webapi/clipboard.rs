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
//! # `ClipboardItem`, `read()` and `write()`
//!
//! The item-based half takes the three representations every browser writes —
//! `text/plain`, `text/html` and `image/png` — when the backend does
//! ([`ClipboardHost::item_types`]). As in Chrome:
//!
//! * one item per `write()` (more is a `NotAllowedError`, "Support for
//!   multiple ClipboardItems is not implemented.");
//! * a type the backend does not write — `image/svg+xml`, a `web ` custom
//!   format — is a `NotAllowedError` naming it, so an app's fallback chain
//!   (try the rich item, then a smaller one) takes the next step;
//! * a value may be a Blob, a string or a promise of either, and every one
//!   settles before anything is written: the clipboard changes all at once or
//!   not at all;
//! * `image/png` bytes that are not a PNG are a `DataError`.
//!
//! `read()` answers with one item holding what the backend found, each
//! representation a Blob of its type; an empty clipboard is a `DataError`
//! ("No valid data on clipboard."). A text-only backend still answers `read()`
//! with its text.
//!
//! What the platform writes for an image (Chrome writes the PNG and a bitmap
//! for the apps that only read bitmaps) is the backend's business.
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

    /// The MIME types [`Self::write_items`] takes, out of [`ITEM_TYPES`]. The
    /// default is none — a text-only backend — and `write()` then refuses
    /// every type the way a browser refuses one it does not support.
    fn item_types(&self) -> &'static [&'static str] {
        &[]
    }

    /// Replaces the clipboard's contents with these representations of ONE
    /// item, all at once: `(MIME type, bytes)`, UTF-8 for the text types and
    /// the encoded file for an image. Called only with types out of
    /// [`Self::item_types`].
    fn write_items(&self, items: &[(String, Vec<u8>)]) -> Result<(), String> {
        let _ = items;
        Err("this clipboard backend writes text only".into())
    }

    /// What the clipboard holds, as representations in [`ITEM_TYPES`]:
    /// `(MIME type, bytes)`. `Ok(vec![])` is an empty clipboard. The default
    /// reads the text.
    fn read_items(&self) -> Result<Vec<(String, Vec<u8>)>, String> {
        let text = self.read_text()?;
        Ok(if text.is_empty() { Vec::new() } else { vec![("text/plain".to_string(), text.into_bytes())] })
    }
}

/// The representations `write()` and `read()` know: the three every browser
/// writes. What a backend actually takes is [`ClipboardHost::item_types`].
pub const ITEM_TYPES: &[&str] = &["text/plain", "text/html", "image/png"];

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

#[crate::host_fn]
unsafe fn clipboard_read_text(g: *mut c_void, _cf: *mut c_void) -> u64 {
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

#[crate::host_fn]
unsafe fn clipboard_write_text(g: *mut c_void, cf: *mut c_void) -> u64 {
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

// ---------------------------------------------------------------------------
// ClipboardItem
// ---------------------------------------------------------------------------

/// A `ClipboardItem`'s internal slots, which are private names: made once per
/// context, as a private name has to be to name the same slot twice.
#[derive(Clone, Copy)]
struct Slots {
    /// `ClipboardItem.prototype` — `read()` makes items without `new`.
    proto: u64,
    /// The representations as given: type → Blob | string | promise.
    items: *mut c_void,
    /// The types, a frozen array in the record's order.
    types: *mut c_void,
    /// `presentationStyle`.
    style: *mut c_void,
}

unsafe impl Send for Slots {}

fn registry() -> &'static std::sync::Mutex<std::collections::HashMap<usize, Slots>> {
    static R: OnceLock<std::sync::Mutex<std::collections::HashMap<usize, Slots>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

fn slots(g: *mut c_void) -> Option<Slots> {
    registry().lock().unwrap().get(&(g as usize)).copied()
}

/// The protos were protected in a heap that no longer exists.
pub(crate) fn forget(gone: crate::context::Gone) {
    registry().lock().unwrap().remove(&gone.key);
}

#[derive(Clone, Copy)]
enum Slot {
    Items,
    Types,
    Style,
}

unsafe fn slot_get(g: *mut c_void, obj: u64, slot: Slot) -> u64 {
    let Some(s) = slots(g) else { return UNDEF };
    if !is_cell(obj) {
        return UNDEF;
    }
    get_a(g, obj, match slot {
        Slot::Items => s.items,
        Slot::Types => s.types,
        Slot::Style => s.style,
    })
}

unsafe fn slot_set(g: *mut c_void, obj: u64, slot: Slot, v: u64) {
    let Some(s) = slots(g) else { return };
    let a = match slot {
        Slot::Items => s.items,
        Slot::Types => s.types,
        Slot::Style => s.style,
    };
    glue::rjs_define(g, obj, a, v, 0);
}

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// `global[a][b]`, called with `this` = `global[a]`.
unsafe fn call_static(g: *mut c_void, a: &str, b: &str, args: &[u64]) -> Result<u64, u64> {
    let owner = get(g, glue::rjs_global(g), a);
    call(g, get(g, owner, b), owner, args)
}

/// `f.bind(undefined, arg)`: a native reaction that needs to know which type
/// (or types) it is for.
unsafe fn bind1(g: *mut c_void, f: u64, arg: u64) -> u64 {
    call(g, get(g, f, "bind"), f, &[UNDEF, arg]).unwrap_or(f)
}

/// `p.then(f)`; a throw from `then` itself becomes the rejection.
unsafe fn then(g: *mut c_void, p: u64, f: u64) -> u64 {
    match call(g, get(g, p, "then"), p, &[f]) {
        Ok(v) => v,
        Err(e) => super::domexception::rejected_with(g, e),
    }
}

/// The string in `v`, through ToString; `Err` holds what ToString threw.
unsafe fn dom_string(g: *mut c_void, v: u64) -> Result<String, u64> {
    let mut sv = v;
    if glue::rjs_kind(g, v) != 5 && glue::rjs_to_string(g, v, &mut sv) != 0 {
        return Err(sv);
    }
    Ok(read_string(g, sv).unwrap_or_default())
}

/// Builds an item over `record` / `types` (already a frozen array).
unsafe fn make_item(g: *mut c_void, proto: u64, record: u64, types: u64, style: &str) -> u64 {
    let item = new_object(g, proto);
    slot_set(g, item, Slot::Items, record);
    slot_set(g, item, Slot::Types, types);
    slot_set(g, item, Slot::Style, js_string(g, style));
    item
}

/// The types of an item, as Rust strings, in order.
unsafe fn item_type_list(g: *mut c_void, types: u64) -> Vec<String> {
    (0..array_len(g, types)).filter_map(|i| read_string(g, get_i(g, types, i))).collect()
}

/// `new ClipboardItem(items, options)` — WebIDL's record conversion: the keys
/// are read once, in order, and each value once.
#[crate::host_fn]
unsafe fn item_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    const WHO: &str = "Failed to construct 'ClipboardItem'";
    let items = cf_arg(cf, 0);
    if !super::blob::is_object(g, items) {
        return throw_type_error(
            g,
            &format!("{WHO}: The provided value is not of type '(record<DOMString, (Blob or DOMString or Promise<(Blob or DOMString)>)>)'."),
        );
    }
    let keys = match call_static(g, "Object", "keys", &[items]) {
        Ok(k) => k,
        Err(e) => {
            glue::rjs_throw(g, e);
            return UNDEF;
        }
    };
    let n = array_len(g, keys);
    if n == 0 {
        return throw_type_error(g, &format!("{WHO}: Empty dictionary argument"));
    }
    let record = new_object(g, UNDEF);
    let types = new_array(g);
    for i in 0..n {
        let key = get_i(g, keys, i);
        let Some(name) = read_string(g, key) else { continue };
        let a = atom(g, &name);
        let mut v = UNDEF;
        let rc = glue::rjs_get(g, items, a, &mut v);
        glue::rjs_atom_free(g, a);
        if rc != 0 {
            glue::rjs_throw(g, v);
            return UNDEF;
        }
        define(g, record, &name, v, 0);
        set_i(g, types, i, key);
    }
    let _ = call_static(g, "Object", "freeze", &[types]);
    // `options.presentationStyle`: an enum, "unspecified" by default.
    let mut style = "unspecified".to_string();
    let options = cf_arg(cf, 1);
    if super::blob::is_object(g, options) {
        let v = get(g, options, "presentationStyle");
        if v != UNDEF {
            match dom_string(g, v) {
                Ok(s) if matches!(s.as_str(), "unspecified" | "inline" | "attachment") => style = s,
                Ok(s) => {
                    return throw_type_error(
                        g,
                        &format!("{WHO}: Failed to read the 'presentationStyle' property from 'ClipboardItemOptions': The provided value '{s}' is not a valid enum value of type PresentationStyle."),
                    )
                }
                Err(e) => {
                    glue::rjs_throw(g, e);
                    return UNDEF;
                }
            }
        }
    }
    let proto = slots(g).map_or(UNDEF, |s| s.proto);
    let this = ordinary_create(g, cf_new_target(cf), proto);
    slot_set(g, this, Slot::Items, record);
    slot_set(g, this, Slot::Types, types);
    slot_set(g, this, Slot::Style, js_string(g, &style));
    this
}

#[crate::host_fn]
unsafe fn item_types(g: *mut c_void, cf: *mut c_void) -> u64 {
    let types = slot_get(g, cf_this(cf), Slot::Types);
    if !is_cell(types) {
        return throw_type_error(g, "Illegal invocation");
    }
    types
}

#[crate::host_fn]
unsafe fn item_style(g: *mut c_void, cf: *mut c_void) -> u64 {
    let style = slot_get(g, cf_this(cf), Slot::Style);
    if style == UNDEF {
        return throw_type_error(g, "Illegal invocation");
    }
    style
}

/// The reaction that turns a settled representation into its Blob: bound to
/// the type, called with the value. A Blob is itself; a string becomes a Blob
/// of the type.
#[crate::host_fn]
unsafe fn to_blob(g: *mut c_void, cf: *mut c_void) -> u64 {
    let ty = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let v = cf_arg(cf, 1);
    if super::blob::data_of(g, v).is_some() {
        return v;
    }
    match dom_string(g, v) {
        Ok(s) => super::blob::create(g, UNDEF, s.into_bytes(), ty),
        Err(e) => {
            glue::rjs_throw(g, e);
            UNDEF
        }
    }
}

/// `item.getType(type)` — a promise of the representation's Blob.
#[crate::host_fn]
unsafe fn item_get_type(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let record = slot_get(g, this, Slot::Items);
    if !is_cell(record) {
        return rejected_promise(g, 1, "Illegal invocation");
    }
    let ty = match dom_string(g, cf_arg(cf, 0)) {
        Ok(t) => t,
        Err(e) => return super::domexception::rejected_with(g, e),
    };
    if !item_type_list(g, slot_get(g, this, Slot::Types)).contains(&ty) {
        return rejected_named(
            g,
            "NotFoundError",
            "Failed to execute 'getType' on 'ClipboardItem': The type was not found",
        );
    }
    let v = get(g, record, &ty);
    let settled = match call_static(g, "Promise", "resolve", &[v]) {
        Ok(p) => p,
        Err(e) => return super::domexception::rejected_with(g, e),
    };
    let reaction = bind1(g, make_fn(g, "", 2, to_blob), js_string(g, &ty));
    then(g, settled, reaction)
}

/// `ClipboardItem.supports(type)` — what `write()` would take.
#[crate::host_fn]
unsafe fn item_supports(g: *mut c_void, cf: *mut c_void) -> u64 {
    let ty = match dom_string(g, cf_arg(cf, 0)) {
        Ok(t) => t,
        Err(e) => {
            glue::rjs_throw(g, e);
            return UNDEF;
        }
    };
    let takes = host().map(|h| h.item_types()).unwrap_or(&[]);
    enc_bool(takes.contains(&ty.as_str()))
}

// ---------------------------------------------------------------------------
// read() / write()
// ---------------------------------------------------------------------------

#[crate::host_fn]
unsafe fn clipboard_read(g: *mut c_void, _cf: *mut c_void) -> u64 {
    const WHO: &str = "Failed to execute 'read' on 'Clipboard'";
    let Some(h) = host() else {
        advise_no_host();
        return rejected_named(g, "NotAllowedError", &format!("{WHO}: no clipboard backend is installed in this runtime."));
    };
    let items = match h.read_items() {
        Ok(items) => items,
        Err(why) => return rejected_named(g, "NotAllowedError", &format!("{WHO}: {why}")),
    };
    if items.is_empty() {
        return rejected_named(g, "DataError", "No valid data on clipboard.");
    }
    let record = new_object(g, UNDEF);
    let types = new_array(g);
    for (i, (ty, bytes)) in items.into_iter().enumerate() {
        let blob = super::blob::create(g, UNDEF, bytes, ty.clone());
        define(g, record, &ty, blob, 0);
        set_i(g, types, i as u32, js_string(g, &ty));
    }
    let _ = call_static(g, "Object", "freeze", &[types]);
    let proto = slots(g).map_or(UNDEF, |s| s.proto);
    let list = new_array(g);
    set_i(g, list, 0, make_item(g, proto, record, types, "unspecified"));
    resolved_promise(g, list)
}

/// The reaction that writes once every representation has settled: bound to
/// the types, called with the values in the same order.
#[crate::host_fn]
unsafe fn write_settled(g: *mut c_void, cf: *mut c_void) -> u64 {
    const WHO: &str = "Failed to execute 'write' on 'Clipboard'";
    let types = item_type_list(g, cf_arg(cf, 0));
    let values = cf_arg(cf, 1);
    let mut items = Vec::with_capacity(types.len());
    for (i, ty) in types.into_iter().enumerate() {
        let v = get_i(g, values, i as u32);
        let bytes = match super::blob::bytes_of(g, v, "navigator.clipboard.write()") {
            Some(Ok(bytes)) => bytes,
            Some(Err(_)) => {
                return super::domexception::throw(g, "DataError", &format!("Failed to read or decode Blob for clipboard item type {ty}."))
            }
            None => match dom_string(g, v) {
                Ok(s) => s.into_bytes(),
                Err(e) => {
                    glue::rjs_throw(g, e);
                    return UNDEF;
                }
            },
        };
        if ty == "image/png" && !bytes.starts_with(PNG_SIGNATURE) {
            return super::domexception::throw(g, "DataError", "Failed to read or decode Blob for clipboard item type image/png.");
        }
        items.push((ty, bytes));
    }
    let Some(h) = host() else {
        return super::domexception::throw(g, "NotAllowedError", &format!("{WHO}: no clipboard backend is installed in this runtime."));
    };
    match h.write_items(&items) {
        Ok(()) => UNDEF,
        Err(why) => super::domexception::throw(g, "NotAllowedError", &format!("{WHO}: {why}")),
    }
}

/// `navigator.clipboard.write(data)` — one `ClipboardItem`, every
/// representation settled first, then written at once.
#[crate::host_fn]
unsafe fn clipboard_write(g: *mut c_void, cf: *mut c_void) -> u64 {
    const WHO: &str = "Failed to execute 'write' on 'Clipboard'";
    let list = match call_static(g, "Array", "from", &[cf_arg(cf, 0)]) {
        Ok(l) => l,
        Err(e) => return super::domexception::rejected_with(g, e),
    };
    let n = array_len(g, list);
    if n == 0 {
        return resolved_promise(g, UNDEF);
    }
    if n > 1 {
        return rejected_named(g, "NotAllowedError", "Support for multiple ClipboardItems is not implemented.");
    }
    let item = get_i(g, list, 0);
    let record = slot_get(g, item, Slot::Items);
    let types = slot_get(g, item, Slot::Types);
    if !is_cell(record) || !is_cell(types) {
        return rejected_promise(g, 1, &format!("{WHO}: The provided value is not of type 'ClipboardItem'."));
    }
    let Some(h) = host() else {
        advise_no_host();
        return rejected_named(g, "NotAllowedError", &format!("{WHO}: no clipboard backend is installed in this runtime."));
    };
    let names = item_type_list(g, types);
    if let Some(ty) = names.iter().find(|t| !h.item_types().contains(&t.as_str())) {
        return rejected_named(g, "NotAllowedError", &format!("{WHO}: Type {ty} not supported on write."));
    }
    let values = new_array(g);
    for (i, ty) in names.iter().enumerate() {
        set_i(g, values, i as u32, get(g, record, ty));
    }
    let all = match call_static(g, "Promise", "all", &[values]) {
        Ok(p) => p,
        Err(e) => return super::domexception::rejected_with(g, e),
    };
    let reaction = bind1(g, make_fn(g, "", 2, write_settled), types);
    then(g, all, reaction)
}

// ---------------------------------------------------------------------------
// document.execCommand
// ---------------------------------------------------------------------------

#[crate::host_fn]
unsafe fn exec_command(g: *mut c_void, cf: *mut c_void) -> u64 {
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
#[crate::host_fn]
unsafe fn query_command_false(_g: *mut c_void, _cf: *mut c_void) -> u64 {
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

    let item_class = define_class(g, &ClassSpec {
        name: "ClipboardItem",
        arity: 1,
        construct: Some(item_construct),
        parent_proto: 0,
        methods: &[("getType", 1, item_get_type)],
        getters: &[("types", item_types), ("presentationStyle", item_style)],
        accessors: &[],
    });
    define(g, item_class.ctor, "supports", make_fn(g, "supports", 1, item_supports), 2);
    registry().lock().unwrap().insert(
        g as usize,
        Slots {
            proto: item_class.proto,
            items: atom_priv(g, "redjs#clipboardItems"),
            types: atom_priv(g, "redjs#clipboardTypes"),
            style: atom_priv(g, "redjs#clipboardStyle"),
        },
    );

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

    /// A clipboard that is a `Mutex<String>` — the OS, without an OS — plus
    /// the representations of the last item written.
    struct FakeClipboard;
    static TEXT: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
    static ITEMS: std::sync::Mutex<Vec<(String, Vec<u8>)>> = std::sync::Mutex::new(Vec::new());
    static FAIL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    /// The backend writes text only (`item_types` is empty), as v1 did.
    static TEXT_ONLY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
            ITEMS.lock().unwrap().clear();
            Ok(())
        }
        fn item_types(&self) -> &'static [&'static str] {
            if TEXT_ONLY.load(std::sync::atomic::Ordering::Relaxed) { &[] } else { ITEM_TYPES }
        }
        fn write_items(&self, items: &[(String, Vec<u8>)]) -> Result<(), String> {
            if FAIL.load(std::sync::atomic::Ordering::Relaxed) {
                return Err("the clipboard is held by another process".into());
            }
            *ITEMS.lock().unwrap() = items.to_vec();
            TEXT.lock().unwrap().clear();
            Ok(())
        }
        fn read_items(&self) -> Result<Vec<(String, Vec<u8>)>, String> {
            let items = ITEMS.lock().unwrap().clone();
            if !items.is_empty() {
                return Ok(items);
            }
            let text = self.read_text()?;
            Ok(if text.is_empty() { Vec::new() } else { vec![("text/plain".into(), text.into_bytes())] })
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
        TEXT_ONLY.store(false, std::sync::atomic::Ordering::Relaxed);
        TEXT.lock().unwrap().clear();
        ITEMS.lock().unwrap().clear();
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

    /// A PNG's first bytes: all `write()` checks of an `image/png`.
    const PNG: &str = "new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3])";

    #[test]
    fn a_clipboard_item_is_its_record_read_once() {
        let _g = install_fake();
        let c = ctx();
        ok(&c, "typeof ClipboardItem === 'function'");
        ok(&c, "Object.prototype.toString.call(new ClipboardItem({'text/plain': 'x'})) === '[object ClipboardItem]'");
        ok(&c, "JSON.stringify(new ClipboardItem({'text/plain': 'x', 'image/png': new Blob([])}).types) === '[\"text/plain\",\"image/png\"]'");
        ok(&c, "Object.isFrozen(new ClipboardItem({'text/plain': 'x'}).types)");
        ok(&c, "new ClipboardItem({'text/plain': 'x'}).presentationStyle === 'unspecified'");
        ok(&c, "new ClipboardItem({'text/plain': 'x'}, {presentationStyle: 'attachment'}).presentationStyle === 'attachment'");
        assert!(throws(&c, "new ClipboardItem({})").contains("Empty dictionary argument"));
        assert!(throws(&c, "new ClipboardItem('text/plain')").contains("record"));
        assert!(throws(&c, "ClipboardItem({'text/plain': 'x'})").contains("requires 'new'"));
        assert!(throws(&c, "new ClipboardItem({'text/plain': 'x'}, {presentationStyle: 'nope'})").contains("PresentationStyle"));
        ok(&c, "ClipboardItem.supports('image/png') && ClipboardItem.supports('text/html')");
        ok(&c, "!ClipboardItem.supports('image/svg+xml') && !ClipboardItem.supports('web text/x-mine')");
        // getType: a Blob of the type, from a string, a Blob or a promise.
        assert_eq!(
            run(&c, "const b = await new ClipboardItem({'text/plain': 'hi'}).getType('text/plain');
                     return (b instanceof Blob) + ':' + b.type + ':' + await b.text();"),
            "true:text/plain:hi"
        );
        assert_eq!(
            run(&c, "const b = await new ClipboardItem({'text/html': Promise.resolve('<b>x</b>')}).getType('text/html');
                     return b.type + ':' + await b.text();"),
            "text/html:<b>x</b>"
        );
        let got = run(&c, "return await new ClipboardItem({'text/plain': 'hi'}).getType('image/png');");
        assert!(got.starts_with("ERROR: NotFoundError"), "{got}");
    }

    #[test]
    fn write_then_read_round_trips_an_image_and_its_text() {
        let _g = install_fake();
        let c = ctx();
        assert_eq!(
            run(
                &c,
                &format!(
                    "const png = new Blob([{PNG}], {{type: 'image/png'}});
                     await navigator.clipboard.write([new ClipboardItem({{
                       'image/png': Promise.resolve(png), 'text/plain': 'caption'
                     }})]);
                     const [item] = await navigator.clipboard.read();
                     const back = new Uint8Array(await (await item.getType('image/png')).arrayBuffer());
                     return item.types.join(',') + ':' + back.length + ':' + back[1] + ':' +
                       await (await item.getType('text/plain')).text();"
                )
            ),
            "image/png,text/plain:11:80:caption"
        );
        let items = ITEMS.lock().unwrap().clone();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].0, "image/png");
        assert_eq!(&items[0].1[..4], b"\x89PNG");
        assert_eq!(items[1], ("text/plain".to_string(), b"caption".to_vec()));
        // readText still reads a text clipboard after an item write of text.
        assert_eq!(
            run(&c, "await navigator.clipboard.write([new ClipboardItem({'text/plain': 'only'})]);
                     return (await (await navigator.clipboard.read())[0].getType('text/plain')).text();"),
            "only"
        );
    }

    #[test]
    fn write_refuses_what_a_browser_refuses_and_writes_nothing() {
        let _g = install_fake();
        let c = ctx();
        *TEXT.lock().unwrap() = "untouched".into();
        // A type the backend does not take: the app's fallback chain moves on.
        let got = run(&c, "return await navigator.clipboard.write([new ClipboardItem({'image/png': new Blob([]), 'web text/x-mine': 'x'})]);");
        assert!(got.starts_with("ERROR: NotAllowedError"), "{got}");
        assert!(got.contains("web text/x-mine"), "{got}");
        // More than one item.
        let got = run(&c, "const i = () => new ClipboardItem({'text/plain': 'x'}); return await navigator.clipboard.write([i(), i()]);");
        assert!(got.contains("multiple ClipboardItems"), "{got}");
        // Not a PNG under image/png.
        let got = run(&c, "return await navigator.clipboard.write([new ClipboardItem({'image/png': new Blob(['GIF89a'])})]);");
        assert!(got.starts_with("ERROR: DataError"), "{got}");
        // A representation that never resolves to anything writes nothing.
        let got = run(&c, "return await navigator.clipboard.write([new ClipboardItem({'text/plain': Promise.reject(new Error('gone'))})]);");
        assert!(got.contains("gone"), "{got}");
        // Not an item at all.
        let got = run(&c, "return await navigator.clipboard.write([{}]);");
        assert!(got.starts_with("ERROR: plain TypeError"), "{got}");
        assert_eq!(*TEXT.lock().unwrap(), "untouched");
        assert!(ITEMS.lock().unwrap().is_empty());
        // An empty clipboard has nothing to read.
        TEXT.lock().unwrap().clear();
        let got = run(&c, "return await navigator.clipboard.read();");
        assert!(got.starts_with("ERROR: DataError"), "{got}");
    }

    #[test]
    fn a_text_only_backend_refuses_every_item_type_but_still_reads_its_text() {
        let _g = install_fake();
        TEXT_ONLY.store(true, std::sync::atomic::Ordering::Relaxed);
        let c = ctx();
        ok(&c, "!ClipboardItem.supports('text/plain')");
        let got = run(&c, "return await navigator.clipboard.write([new ClipboardItem({'text/plain': 'x'})]);");
        assert!(got.starts_with("ERROR: NotAllowedError"), "{got}");
        *TEXT.lock().unwrap() = "from the OS".into();
        assert_eq!(
            run(&c, "const [i] = await navigator.clipboard.read(); return (await i.getType('text/plain')).text();"),
            "from the OS"
        );
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
