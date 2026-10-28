//! `navigator.mediaDevices` — audio device enumeration without a DOM.
//!
//! A device picker in a desktop application is built from
//! `navigator.mediaDevices.enumerateDevices()`, and the chosen output is then
//! applied with `AudioContext.setSinkId()` (see [`super::audio`]). Neither call
//! needs an `<audio>` element, a `MediaStream`, or a media-capture permission —
//! which is exactly why the pair is implementable here while capture is not.
//!
//! ## What this module reports, and what it deliberately does not
//!
//! Only `audiooutput`. The list comes from the same CPAL host enumerator that
//! [`super::audio`] opens its output stream through, so every id in it is an id
//! `setSinkId` can actually select — the enumeration and the selection cannot
//! disagree.
//!
//! `audioinput` and `videoinput` are **omitted on purpose**. This runtime has no
//! `getUserMedia` and no `MediaStream`, so every microphone or camera listed
//! here would be a device the application could offer the user and then never
//! open: a picker where every choice is silent. Input enumeration belongs with
//! capture, in the media/WebRTC layer, and arrives with it — not before.
//! `jsc_tests::the_input_boundary_is_pinned` fails if that ever drifts silently.
//!
//! ## Shape notes (deviations, all deliberate)
//!
//! - A synthetic `deviceId: "default"` entry always leads the list, mirroring
//!   what Chrome reports on Windows. It is what the application persists when
//!   the user has not pinned a specific endpoint, and it is the reason the list
//!   is never empty even on a machine with no audio hardware at all.
//! - `label` is always populated. In a browser it is empty until a capture
//!   permission is granted; there is no such gate here, and hiding the names
//!   would only break the picker.
//! - `groupId` is `""`. CPAL exposes no grouping of an input and an output that
//!   belong to the same physical device, and inventing one would be a guess.
//! - `devicechange` is polled (CPAL has no hot-plug callback), and only while
//!   something is actually listening — see [`pump`].

use core::ffi::c_void;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use firewheel::cpal::cpal::DeviceId;
use firewheel::cpal::CpalEnumerator;

use super::events;
use super::util::*;
use crate::glue;

/// The sink id that means "whatever the system default is". Web Audio spells
/// this `""`; `enumerateDevices` (and every browser device picker built on it)
/// also has a literal `"default"` entry. Both are accepted everywhere.
pub const DEFAULT_SINK: &str = "default";

/// How often the `devicechange` poll re-enumerates, while listening.
const POLL_INTERVAL: Duration = Duration::from_millis(2000);

/// One system audio endpoint, in the terms the JS side needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioDevice {
    /// `cpal::DeviceId` rendered through `Display`. Stable across restarts and
    /// parseable back with `FromStr` — which is what makes it safe for the
    /// application to persist the user's choice.
    pub id: String,
    pub label: String,
    pub is_default: bool,
}

/// The system's audio output endpoints, default first.
///
/// Does not include the synthetic `"default"` entry — that is added by the JS
/// surface only. Callers resolving a sink id want the real endpoints.
pub fn output_devices() -> Vec<AudioDevice> {
    let enumerator = CpalEnumerator;
    let host = enumerator.default_host();
    host.output_devices()
        .into_iter()
        .map(|d| AudioDevice {
            id: format!("{}", d.id),
            label: d.name.unwrap_or_else(|| String::from("Unknown output device")),
            is_default: d.is_default,
        })
        .collect()
}

/// Resolve a Web-facing sink id to a CPAL device selection.
///
/// `Ok(None)` = the system default device; `Ok(Some(id))` = that endpoint;
/// `Err(())` = no such device, which is what `setSinkId` turns into a
/// `NotFoundError`. Validating here rather than letting CPAL fall back to the
/// default is the whole point: a silent fallback would resolve the promise
/// while playing out of the wrong speakers.
pub fn resolve_output(sink_id: &str) -> Result<Option<DeviceId>, ()> {
    if sink_id.is_empty() || sink_id == DEFAULT_SINK {
        return Ok(None);
    }
    if !output_devices().iter().any(|d| d.id == sink_id) {
        return Err(());
    }
    DeviceId::from_str(sink_id).map(Some).map_err(|_| ())
}

/// Has the device set changed in a way worth reporting?
///
/// Compares identity *and* which endpoint is default: on Windows the user
/// switching the default playback endpoint changes nothing about the list but
/// changes where a context with the default sink is heard, and an application
/// that tracks the default wants to know.
fn devices_changed(prev: &[(String, bool)], next: &[(String, bool)]) -> bool {
    prev != next
}

fn snapshot_of(devices: &[AudioDevice]) -> Vec<(String, bool)> {
    devices.iter().map(|d| (d.id.clone(), d.is_default)).collect()
}

// ---------------------------------------------------------------------------
// Per-context state
// ---------------------------------------------------------------------------

struct MdClasses {
    info: Class,
    /// Private slot bag on a MediaDeviceInfo ({deviceId, kind, label, groupId}).
    s_md: *mut c_void,
}

unsafe impl Send for MdClasses {}
unsafe impl Sync for MdClasses {}

fn classes_reg() -> &'static Mutex<HashMap<usize, &'static MdClasses>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static MdClasses>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> &'static MdClasses {
    classes_reg().lock().unwrap().get(&(g as usize)).copied().expect("webapi::mediadevices not installed")
}

/// The `devicechange` poll's state. `armed` is false while nobody is listening,
/// so a game that never touches `mediaDevices` pays nothing: the gate check is
/// two property reads and the enumeration never runs.
struct PollState {
    /// The protected `navigator.mediaDevices` singleton.
    obj: u64,
    snapshot: Vec<(String, bool)>,
    armed: bool,
    last_poll: Instant,
}

fn polls() -> &'static Mutex<HashMap<usize, PollState>> {
    static P: OnceLock<Mutex<HashMap<usize, PollState>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn forget(gone: crate::context::Gone) {
    classes_reg().lock().unwrap().remove(&gone.key);
    polls().lock().unwrap().remove(&gone.key);
}

// ---------------------------------------------------------------------------
// MediaDeviceInfo
// ---------------------------------------------------------------------------

unsafe fn info_slots(g: *mut c_void, obj: u64) -> u64 {
    get_a(g, obj, classes(g).s_md)
}

unsafe fn new_device_info(g: *mut c_void, device_id: &str, label: &str) -> u64 {
    let c = classes(g);
    let obj = new_object(g, c.info.proto);
    let slots = new_object(g, UNDEF);
    define(g, slots, "deviceId", js_string(g, device_id), 0);
    define(g, slots, "kind", js_string(g, "audiooutput"), 0);
    define(g, slots, "label", js_string(g, label), 0);
    define(g, slots, "groupId", js_string(g, ""), 0);
    glue::rjs_define(g, obj, c.s_md, slots, 0);
    obj
}

macro_rules! info_getter {
    ($fn_name:ident, $slot:literal) => {
        unsafe extern "sysv64" fn $fn_name(g: *mut c_void, cf: *mut c_void) -> u64 {
            let slots = info_slots(g, cf_this(cf));
            if !is_cell(slots) {
                return throw_type_error(g, concat!("MediaDeviceInfo.", $slot, " on wrong this"));
            }
            get(g, slots, $slot)
        }
    };
}

info_getter!(info_get_device_id, "deviceId");
info_getter!(info_get_kind, "kind");
info_getter!(info_get_label, "label");
info_getter!(info_get_group_id, "groupId");

/// Spec: `toJSON()` returns a plain object with the same four members — how the
/// application logs and persists a device without dragging the class along.
unsafe extern "sysv64" fn info_to_json(g: *mut c_void, cf: *mut c_void) -> u64 {
    let slots = info_slots(g, cf_this(cf));
    if !is_cell(slots) {
        return throw_type_error(g, "MediaDeviceInfo.toJSON on wrong this");
    }
    let out = new_object(g, UNDEF);
    for key in ["deviceId", "kind", "label", "groupId"] {
        define(g, out, key, get(g, slots, key), 0);
    }
    out
}

// ---------------------------------------------------------------------------
// MediaDevices
// ---------------------------------------------------------------------------

unsafe extern "sysv64" fn md_enumerate_devices(g: *mut c_void, _cf: *mut c_void) -> u64 {
    let devices = output_devices();
    let arr = new_array(g);
    let mut i = 0u32;

    // The synthetic default, first — see the module header.
    let default_label = devices
        .iter()
        .find(|d| d.is_default)
        .map(|d| format!("Default - {}", d.label))
        .unwrap_or_else(|| String::from("Default"));
    set_i(g, arr, i, new_device_info(g, DEFAULT_SINK, &default_label));
    i += 1;

    for d in &devices {
        // A backend that ever hands back the literal id "default" would produce
        // two entries the picker cannot tell apart.
        if d.id == DEFAULT_SINK {
            continue;
        }
        set_i(g, arr, i, new_device_info(g, &d.id, &d.label));
        i += 1;
    }
    resolved_promise(g, arr)
}

/// `getUserMedia` is not here, and saying so by name beats `undefined is not a
/// function` from inside a call stack five frames deep.
unsafe extern "sysv64" fn md_get_user_media(g: *mut c_void, _cf: *mut c_void) -> u64 {
    super::domexception::rejected(
        g,
        "NotSupportedError",
        "navigator.mediaDevices.getUserMedia is not implemented — this runtime has \
         no media capture and no MediaStream. Output selection (enumerateDevices + \
         AudioContext.setSinkId) works without it; capture arrives with the media/WebRTC layer.",
    )
}

// ---------------------------------------------------------------------------
// devicechange polling
// ---------------------------------------------------------------------------

/// Is anything actually listening for `devicechange` on this object?
unsafe fn has_device_change_listener(g: *mut c_void, obj: u64) -> bool {
    let st = state(g);
    let list = get_a(g, obj, st.s_listeners);
    if is_cell(list) {
        let n = array_len(g, list);
        for i in 0..n {
            let e = get_i(g, list, i);
            if read_string(g, get_a(g, e, st.a_type)).as_deref() == Some("devicechange") {
                return true;
            }
        }
    }
    let handler = get(g, obj, "ondevicechange");
    is_cell(handler) && glue::rjs_kind(g, handler) == 9
}

/// Called once per pump. Cheap when nobody listens: no enumeration at all.
pub unsafe fn pump(g: *mut c_void) {
    let (obj, prev, armed) = {
        let mut reg = polls().lock().unwrap();
        let Some(p) = reg.get_mut(&(g as usize)) else {
            return;
        };
        if p.last_poll.elapsed() < POLL_INTERVAL {
            return;
        }
        p.last_poll = Instant::now();
        (p.obj, p.snapshot.clone(), p.armed)
    };

    if !has_device_change_listener(g, obj) {
        // Disarm: the snapshot goes stale while unobserved, so the next
        // listener must take a fresh baseline instead of being told that
        // everything changed the moment it subscribed.
        if let Some(p) = polls().lock().unwrap().get_mut(&(g as usize)) {
            p.armed = false;
            p.snapshot.clear();
        }
        return;
    }

    let next = snapshot_of(&output_devices());
    let fire = armed && devices_changed(&prev, &next);
    if let Some(p) = polls().lock().unwrap().get_mut(&(g as usize)) {
        p.snapshot = next;
        p.armed = true;
    }
    if fire {
        let ev = events::new_trusted_event(g, "devicechange");
        events::dispatch(g, obj, ev);
    }
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

pub unsafe fn install(g: *mut c_void, event_target_proto: u64) {
    let s_md = atom_priv(g, "redjs#mediaDevices");

    let info = define_class(g, &ClassSpec {
        name: "MediaDeviceInfo",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[("toJSON", 0, info_to_json)],
        getters: &[
            ("deviceId", info_get_device_id),
            ("kind", info_get_kind),
            ("label", info_get_label),
            ("groupId", info_get_group_id),
        ],
        accessors: &[],
    });

    let md_class = define_class(g, &ClassSpec {
        name: "MediaDevices",
        arity: 0,
        construct: None,
        parent_proto: event_target_proto,
        methods: &[
            ("enumerateDevices", 0, md_enumerate_devices),
            ("getUserMedia", 1, md_get_user_media),
        ],
        getters: &[],
        accessors: &[],
    });

    let boxed: &'static MdClasses = Box::leak(Box::new(MdClasses { info, s_md }));
    classes_reg().lock().unwrap().insert(g as usize, boxed);

    // The singleton. `navigator` may already exist (the host is free to define
    // its own); only `mediaDevices` is ours, and it is defined onto whatever
    // object is there.
    let md = new_object(g, md_class.proto);
    crate::ffi::redjs_protect(g, md);

    let global = glue::rjs_global(g);
    let existing = get(g, global, "navigator");
    let navigator = if is_cell(existing) {
        existing
    } else {
        let n = new_object(g, UNDEF);
        define(g, global, "navigator", n, 2 /* DontEnum */);
        n
    };
    define(g, navigator, "mediaDevices", md, 2);

    polls().lock().unwrap().insert(
        g as usize,
        PollState { obj: md, snapshot: Vec::new(), armed: false, last_poll: Instant::now() },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_sentinels_resolve_to_the_system_default() {
        // Both spellings, because the application persists "default" while Web
        // Audio's own default is "". A regression in either one sends a user
        // who never picked a device to a NotFoundError.
        assert_eq!(resolve_output(""), Ok(None));
        assert_eq!(resolve_output(DEFAULT_SINK), Ok(None));
    }

    #[test]
    fn an_unknown_id_is_refused_rather_than_silently_defaulted() {
        assert_eq!(resolve_output("not-a-device"), Err(()));
        assert_eq!(resolve_output("wasapi:{00000000-0000-0000-0000-000000000000}"), Err(()));
    }

    #[test]
    fn every_enumerated_id_can_be_selected_again() {
        // The invariant that keeps the picker and the sink from disagreeing:
        // an id this module hands to JS must parse back into a device CPAL can
        // open. Vacuous on a machine with no audio hardware — see the report.
        for d in output_devices() {
            assert!(!d.id.is_empty(), "an enumerated device has an empty id: {d:?}");
            assert!(
                matches!(resolve_output(&d.id), Ok(Some(_))),
                "enumerated id does not resolve back: {d:?}",
            );
        }
    }

    #[test]
    fn enumerated_ids_are_unique() {
        let devices = output_devices();
        let mut ids: Vec<&str> = devices.iter().map(|d| d.id.as_str()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate device ids in {devices:?}");
    }

    #[test]
    fn the_change_detector_sees_adds_removals_and_a_new_default() {
        let a = vec![("a".to_string(), true), ("b".to_string(), false)];
        assert!(!devices_changed(&a, &a.clone()), "an unchanged list must not fire");

        let added = vec![("a".to_string(), true), ("b".to_string(), false), ("c".to_string(), false)];
        assert!(devices_changed(&a, &added), "a plugged-in device must fire");

        let removed = vec![("a".to_string(), true)];
        assert!(devices_changed(&a, &removed), "an unplugged device must fire");

        // Same devices, the OS default moved. The list is identical by id; only
        // the flag moved, and an implementation comparing ids alone misses it.
        let redefaulted = vec![("a".to_string(), false), ("b".to_string(), true)];
        assert!(devices_changed(&a, &redefaulted), "a new system default must fire");
    }
}

#[cfg(test)]
mod jsc_tests {
    use crate::api::Context;
    use crate::webapi::testing::*;

    fn run(c: &Context, body: &str) -> String {
        s(
            c,
            &format!(
                "globalThis.__out = '<pending>'; (async () => {{ {body} }})().then(\
                   v => {{ globalThis.__out = String(v); }}, \
                   e => {{ globalThis.__out = 'ERROR: ' + \
                     (e instanceof DOMException ? 'DOMException ' + e.name + ': ' : '') + \
                     ((e && e.message) || e); }}); 'started'"
            ),
        );
        for _ in 0..64 {
            c.drain_microtasks();
        }
        s(c, "globalThis.__out")
    }

    #[test]
    fn media_devices_is_a_singleton_event_target() {
        let c = ctx();
        ok(&c, "navigator.mediaDevices === navigator.mediaDevices");
        ok(&c, "navigator.mediaDevices instanceof MediaDevices");
        // The application registers a `devicechange` listener in its
        // constructor, unconditionally. Without the EventTarget prototype that
        // call throws and the audio package never finishes constructing.
        ok(&c, "typeof navigator.mediaDevices.addEventListener === 'function'");
        ok(&c, "typeof navigator.mediaDevices.removeEventListener === 'function'");
    }

    #[test]
    fn both_listener_shapes_receive_a_device_change() {
        // Hot-plug itself cannot be forced from a test; what is pinned here is
        // the delivery path a poll tick uses — the `addEventListener` form the
        // application actually uses, and the `ondevicechange` attribute.
        let c = ctx();
        ok(
            &c,
            "let n = 0;
             navigator.mediaDevices.addEventListener('devicechange', () => { n++; });
             navigator.mediaDevices.ondevicechange = () => { n += 10; };
             navigator.mediaDevices.dispatchEvent(new Event('devicechange'));
             n === 11",
        );
    }

    #[test]
    fn the_list_is_outputs_only_with_a_default_first() {
        let c = ctx();
        assert_eq!(
            run(
                &c,
                "const ds = await navigator.mediaDevices.enumerateDevices();
                 if (!Array.isArray(ds)) return 'not an array';
                 if (ds.length < 1) return 'empty list';
                 if (ds[0].deviceId !== 'default') return 'first is ' + ds[0].deviceId;
                 const ids = new Set();
                 for (const d of ds) {
                   if (!(d instanceof MediaDeviceInfo)) return 'not a MediaDeviceInfo';
                   if (d.kind !== 'audiooutput') return 'kind: ' + d.kind;
                   if (typeof d.deviceId !== 'string' || !d.deviceId) return 'bad deviceId';
                   if (typeof d.label !== 'string' || !d.label) return 'bad label';
                   if (typeof d.groupId !== 'string') return 'bad groupId';
                   if (d.toJSON().deviceId !== d.deviceId) return 'toJSON mismatch';
                   if (ids.has(d.deviceId)) return 'duplicate id ' + d.deviceId;
                   ids.add(d.deviceId);
                 }
                 return 'ok';"
            ),
            "ok"
        );
    }

    #[test]
    fn the_input_boundary_is_pinned() {
        // Capture is the media/WebRTC layer's, not this module's. Listing a
        // microphone here would put a device in the picker that nothing can
        // open; this test fails the day inputs start appearing, so the change
        // has to be a decision rather than a drift.
        let c = ctx();
        assert_eq!(
            run(
                &c,
                "const ds = await navigator.mediaDevices.enumerateDevices();
                 const other = ds.filter(d => d.kind !== 'audiooutput').map(d => d.kind);
                 return other.length ? other.join(',') : 'outputs-only';"
            ),
            "outputs-only"
        );
        let got = run(&c, "await navigator.mediaDevices.getUserMedia({ audio: true }); return 'resolved';");
        assert!(got.contains("DOMException NotSupportedError"), "{got}");
        assert!(got.contains("MediaStream"), "getUserMedia must say why: {got}");
    }

    #[test]
    fn media_device_info_is_not_constructible() {
        let c = ctx();
        let msg = throws(&c, "new MediaDeviceInfo()");
        assert!(msg.contains("Illegal constructor"), "{msg}");
        let msg = throws(&c, "new MediaDevices()");
        assert!(msg.contains("Illegal constructor"), "{msg}");
    }

    #[test]
    fn the_getters_refuse_a_foreign_this() {
        let c = ctx();
        let msg = throws(
            &c,
            "Object.getOwnPropertyDescriptor(MediaDeviceInfo.prototype, 'deviceId').get.call({})",
        );
        assert!(msg.contains("wrong this"), "{msg}");
    }
}
