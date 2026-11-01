//! `navigator.mediaDevices` — audio device enumeration without a DOM.
//!
//! A device picker in a desktop application is built from
//! `navigator.mediaDevices.enumerateDevices()`. The chosen output is applied
//! with `AudioContext.setSinkId()` (see [`super::audio`]), the chosen input is
//! opened with `getUserMedia({ audio: { deviceId } })` (see
//! [`super::mediastream`], whose script replaces the `getUserMedia` stub here).
//!
//! ## What this module reports, and what it deliberately does not
//!
//! `audiooutput` and `audioinput`. Both lists come from the CPAL host that
//! [`super::audio`] plays through and [`super::mediastream`] captures through,
//! so every id in them is one `setSinkId` or `getUserMedia` can select: the
//! enumeration and the selection cannot disagree.
//!
//! Inputs are **named only when they could be opened without asking**, i.e.
//! when the capture gate is open (see "The capture gate" in mediastream.rs).
//! Otherwise (capture refused, or the host's prompt not yet answered) the list
//! carries one `audioinput` with an empty `deviceId` and `label`, the shape a
//! browser reports before the user has granted the microphone: the page can
//! tell a microphone exists, and `getUserMedia` then asks or says
//! `NotAllowedError`. A picker never offers a device that cannot be opened.
//! `jsc_tests::the_input_boundary_is_pinned` pins that. A fake capture
//! (`mediastream::set_fake_capture`) lists its one synthetic input instead of
//! the machine's.
//!
//! `videoinput` is omitted: this runtime captures no video.
//!
//! Every `audioinput` entry is an `InputDeviceInfo` (a `MediaDeviceInfo` with
//! `getCapabilities()`), as in a browser; `audiooutput` entries are plain
//! `MediaDeviceInfo`s.
//!
//! ## Shape notes (deviations, all deliberate)
//!
//! - A synthetic `deviceId: "default"` entry always leads the list, mirroring
//!   what Chrome reports on Windows. It is what the application persists when
//!   the user has not pinned a specific endpoint, and it is the reason the list
//!   is never empty even on a machine with no audio hardware at all.
//! - An output's `label` is always populated. In a browser it is empty until a
//!   capture permission is granted; outputs have no gate here, and hiding
//!   their names would only break the picker. Inputs follow the browser (above).
//! - `groupId` is `""`. CPAL exposes no grouping of an input and an output that
//!   belong to the same physical device, and inventing one would be a guess.
//! - `devicechange` is polled (CPAL has no hot-plug callback), and only while
//!   something is actually listening — see [`pump`]. An embedder that hears
//!   the OS's own notifications reports them instead
//!   ([`set_host_notifications`]), and then nothing polls: the devices are
//!   read again only after a change.
//!
//! ## Following the system default
//!
//! An `AudioContext` whose sink is the default, and a microphone opened on
//! the default, move to the new endpoint when the user changes the system
//! default, as Chrome's do. Both need the embedder's notifications
//! ([`notify_default_changed`]); without them they stay where they opened.

use core::ffi::c_void;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// With host notifications: how long a burst of them must go quiet before
/// the devices are read. Plugging in a headset reports each endpoint and
/// each default role on its own, and one read covers them all.
const SETTLE: Duration = Duration::from_millis(150);

/// With host notifications: how often the pump checks whether anything
/// listens for `devicechange`. The check reads no device.
const LISTEN_CHECK: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// Change notifications from the embedder
// ---------------------------------------------------------------------------

/// Which side of the audio path a default-device change is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Output,
    Input,
}

/// The embedder reports device changes ([`set_host_notifications`]).
static HOST_NOTIFIES: AtomicBool = AtomicBool::new(false);
/// Bumped on every reported change.
static CHANGES: AtomicU64 = AtomicU64::new(0);
/// Bumped each time the system default output moves.
static DEFAULT_OUTPUT: AtomicU64 = AtomicU64::new(0);
/// Bumped each time the system default input moves.
static DEFAULT_INPUT: AtomicU64 = AtomicU64::new(0);

/// From now on the embedder reports device changes itself, from the OS's own
/// notifications (vue-native: Windows' `IMMNotificationClient`, in vn-voice).
/// Nothing polls any more.
pub fn set_host_notifications() {
    HOST_NOTIFIES.store(true, Ordering::Release);
}

/// An endpoint was added, removed, enabled or disabled. Lock-free, so it is
/// safe from any thread, the OS's notification thread included.
pub fn notify_devices_changed() {
    CHANGES.fetch_add(1, Ordering::AcqRel);
}

/// The system default endpoint for `flow` moved (on Windows, the "console"
/// role's, which is the one CPAL reports as default). `AudioContext`s on the
/// default sink and microphones opened on the default follow it, and
/// `devicechange` fires as for any other change. Lock-free, like
/// [`notify_devices_changed`].
pub fn notify_default_changed(flow: Flow) {
    match flow {
        Flow::Output => DEFAULT_OUTPUT.fetch_add(1, Ordering::AcqRel),
        Flow::Input => DEFAULT_INPUT.fetch_add(1, Ordering::AcqRel),
    };
    notify_devices_changed();
}

/// How many times the default for `flow` has moved. A follower keeps the
/// value it last acted on and compares.
pub fn default_generation(flow: Flow) -> u64 {
    match flow {
        Flow::Output => DEFAULT_OUTPUT.load(Ordering::Acquire),
        Flow::Input => DEFAULT_INPUT.load(Ordering::Acquire),
    }
}

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

/// The system's audio capture endpoints, as CPAL lists them.
pub fn input_devices() -> Vec<AudioDevice> {
    CpalEnumerator
        .default_host()
        .input_devices()
        .into_iter()
        .map(|d| AudioDevice {
            id: format!("{}", d.id),
            label: d.name.unwrap_or_else(|| String::from("Unknown input device")),
            is_default: d.is_default,
        })
        .collect()
}

/// What a fake capture lists (webapi/mediastream.rs, `set_fake_capture`).
fn fake_inputs() -> Vec<AudioDevice> {
    use super::mediastream::fake::{DEVICE_ID, LABEL};
    vec![AudioDevice { id: DEVICE_ID.to_string(), label: LABEL.to_string(), is_default: true }]
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

/// Outputs and inputs together: plugging in a headset's microphone is a
/// `devicechange` as much as its speakers are.
fn snapshot_now() -> Vec<(String, bool)> {
    let mut s = snapshot_of(&output_devices());
    let inputs = if super::mediastream::fake_capture() { fake_inputs() } else { input_devices() };
    s.extend(inputs.into_iter().map(|d| (format!("in:{}", d.id), d.is_default)));
    s
}

// ---------------------------------------------------------------------------
// Per-context state
// ---------------------------------------------------------------------------

struct MdClasses {
    info: Class,
    /// `InputDeviceInfo`: the class of every `audioinput` entry.
    input: Class,
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
    /// Host notifications: `CHANGES` as last seen.
    seen: u64,
    /// Host notifications: when `CHANGES` last moved, while not yet read.
    moved_at: Option<Instant>,
}

impl PollState {
    fn new(obj: u64) -> PollState {
        PollState {
            obj,
            snapshot: Vec::new(),
            armed: false,
            last_poll: Instant::now(),
            seen: CHANGES.load(Ordering::Acquire),
            moved_at: None,
        }
    }

    /// One pump's work at `now`. `host`: the embedder reports changes, the
    /// last one numbered `changes`. `listening` asks whether anything listens
    /// for `devicechange`; `read` enumerates. Each is called only when due.
    /// Returns whether `devicechange` fires.
    fn step(
        &mut self,
        now: Instant,
        host: bool,
        changes: u64,
        listening: impl FnOnce() -> bool,
        read: impl FnOnce() -> Vec<(String, bool)>,
    ) -> bool {
        if !host {
            if now.duration_since(self.last_poll) < POLL_INTERVAL {
                return false;
            }
            self.last_poll = now;
            if !listening() {
                // Disarm: the snapshot goes stale while unobserved, so the
                // next listener must take a fresh baseline instead of being
                // told that everything changed the moment it subscribed.
                self.armed = false;
                self.snapshot.clear();
                return false;
            }
            let next = read();
            let fire = self.armed && devices_changed(&self.snapshot, &next);
            self.snapshot = next;
            self.armed = true;
            return fire;
        }

        if changes != self.seen {
            self.seen = changes;
            self.moved_at = Some(now);
        }
        if let Some(at) = self.moved_at {
            if now.duration_since(at) >= SETTLE {
                self.moved_at = None;
                // Without a baseline there is nothing to compare with; one is
                // taken when a listener arrives.
                if self.armed {
                    let next = read();
                    let fire = devices_changed(&self.snapshot, &next);
                    self.snapshot = next;
                    return fire;
                }
            }
        }
        if now.duration_since(self.last_poll) >= LISTEN_CHECK {
            self.last_poll = now;
            match (listening(), self.armed) {
                (true, false) => {
                    self.snapshot = read();
                    self.armed = true;
                }
                (false, true) => {
                    self.armed = false;
                    self.snapshot.clear();
                }
                _ => {}
            }
        }
        false
    }
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

unsafe fn new_device_info(g: *mut c_void, kind: &str, device_id: &str, label: &str) -> u64 {
    let c = classes(g);
    // mediacapture-main §9.2.1: an input device is an InputDeviceInfo.
    let proto = if kind == "audioinput" || kind == "videoinput" { c.input.proto } else { c.info.proto };
    let obj = new_object(g, proto);
    let slots = new_object(g, UNDEF);
    define(g, slots, "deviceId", js_string(g, device_id), 0);
    define(g, slots, "kind", js_string(g, kind), 0);
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
// InputDeviceInfo
// ---------------------------------------------------------------------------

/// `getCapabilities()` (mediacapture-main §9.2.2): what the audio track of
/// `getUserMedia({ audio: { deviceId } })` would report from its own
/// `getCapabilities()`, in the shape and order mediastream.js builds there,
/// from the format a capture of this device runs at. `{}` for the anonymous
/// entry listed while capture is not allowed (no id: nothing to describe), and
/// only the ids when the device is gone.
unsafe extern "sysv64" fn input_get_capabilities(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let slots = info_slots(g, this);
    if !is_cell(slots) || !proto_chain_has(g, this, classes(g).input.proto) {
        return throw_type_error(g, "InputDeviceInfo.getCapabilities on wrong this");
    }
    let out = new_object(g, UNDEF);
    let id = read_string(g, get(g, slots, "deviceId")).unwrap_or_default();
    if id.is_empty() {
        return out;
    }
    define(g, out, "deviceId", js_string(g, &id), 0);
    define(g, out, "groupId", get(g, slots, "groupId"), 0);
    let Some((rate, channels)) = super::mediastream::input_format(g as usize, &id) else {
        return out;
    };
    let range = |name: &str, min: f64, max: f64| {
        let r = new_object(g, UNDEF);
        define(g, r, "min", enc_f64(min), 0);
        define(g, r, "max", enc_f64(max), 0);
        define(g, out, name, r, 0);
    };
    range("sampleRate", rate as f64, rate as f64);
    range("sampleSize", 16.0, 16.0);
    range("channelCount", 1.0, channels as f64);
    range("latency", 0.01, 0.01);
    // Each switch can be on only when the embedder installed processing
    // (mediastream.rs, "Processing"); Chrome lists `[true, false]`.
    let can = super::mediastream::capture_processing_available();
    for name in ["echoCancellation", "noiseSuppression", "autoGainControl"] {
        let a = new_array(g);
        if can {
            set_i(g, a, 0, TRUE);
            set_i(g, a, 1, FALSE);
        } else {
            set_i(g, a, 0, FALSE);
        }
        define(g, out, name, a, 0);
    }
    out
}

// ---------------------------------------------------------------------------
// MediaDevices
// ---------------------------------------------------------------------------

/// One kind's entries: the synthetic default first, then every endpoint.
unsafe fn push_kind(g: *mut c_void, arr: u64, i: &mut u32, kind: &str, devices: &[AudioDevice]) {
    let default_label = devices
        .iter()
        .find(|d| d.is_default)
        .map(|d| format!("Default - {}", d.label))
        .unwrap_or_else(|| String::from("Default"));
    set_i(g, arr, *i, new_device_info(g, kind, DEFAULT_SINK, &default_label));
    *i += 1;
    for d in devices {
        // A backend that ever hands back the literal id "default" would produce
        // two entries the picker cannot tell apart.
        if d.id == DEFAULT_SINK {
            continue;
        }
        set_i(g, arr, *i, new_device_info(g, kind, &d.id, &d.label));
        *i += 1;
    }
}

unsafe extern "sysv64" fn md_enumerate_devices(g: *mut c_void, _cf: *mut c_void) -> u64 {
    let arr = new_array(g);
    let mut i = 0u32;

    // Inputs first, as Chrome orders them. Named only when `getUserMedia`
    // could open them without asking (webapi/mediastream.rs, "The capture
    // gate"); otherwise one entry with no id and no label, which is what a
    // browser lists before the user has granted the microphone. A fake
    // capture lists its one synthetic input and nothing of the machine's.
    let inputs = if super::mediastream::fake::enabled(g as usize) {
        fake_inputs()
    } else {
        input_devices()
    };
    if !inputs.is_empty() {
        if super::mediastream::inputs_visible(g as usize) {
            push_kind(g, arr, &mut i, "audioinput", &inputs);
        } else {
            set_i(g, arr, i, new_device_info(g, "audioinput", "", ""));
            i += 1;
        }
    }

    push_kind(g, arr, &mut i, "audiooutput", &output_devices());
    resolved_promise(g, arr)
}

/// Replaced by mediastream.js. Left as the answer if that script ever fails to
/// install, so the failure is named rather than `undefined is not a function`.
unsafe extern "sysv64" fn md_get_user_media(g: *mut c_void, _cf: *mut c_void) -> u64 {
    super::domexception::rejected(
        g,
        "NotSupportedError",
        "navigator.mediaDevices.getUserMedia is unavailable: the MediaStream surface \
         (redjs/mediastream.js) failed to install. See the log for its error.",
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
/// With host notifications, cheap when nothing changed, too.
pub unsafe fn pump(g: *mut c_void) {
    let host = HOST_NOTIFIES.load(Ordering::Acquire);
    let changes = CHANGES.load(Ordering::Acquire);
    // Taken out of the registry for the step, which may read JS properties
    // and enumerate; no lock is held meanwhile.
    let Some(mut p) = polls().lock().unwrap().remove(&(g as usize)) else {
        return;
    };
    let obj = p.obj;
    let fire = p.step(Instant::now(), host, changes, || has_device_change_listener(g, obj), snapshot_now);
    polls().lock().unwrap().insert(g as usize, p);
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

    let input = define_class(g, &ClassSpec {
        name: "InputDeviceInfo",
        arity: 0,
        construct: None,
        parent_proto: info.proto,
        methods: &[("getCapabilities", 0, input_get_capabilities)],
        getters: &[],
        accessors: &[],
    });
    // WebIDL: an interface object's [[Prototype]] is its parent's.
    crate::capi::JSObjectSetPrototype(g, input.ctor as usize as crate::capi::JSObjectRef, info.ctor as usize as crate::capi::JSValueRef);

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

    let boxed: &'static MdClasses = Box::leak(Box::new(MdClasses { info, input, s_md }));
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

    polls().lock().unwrap().insert(g as usize, PollState::new(md));
}

/// The capabilities of the fake microphone (48 kHz mono), as its track
/// reports them: mediastream.rs pins the track to the same text.
#[cfg(test)]
pub(crate) const FAKE_CAPABILITIES: &str = concat!(
    r#"{"deviceId":"default","groupId":"","sampleRate":{"min":48000,"max":48000},"#,
    r#""sampleSize":{"min":16,"max":16},"channelCount":{"min":1,"max":1},"latency":{"min":0.01,"max":0.01},"#,
    r#""echoCancellation":[false],"noiseSuppression":[false],"autoGainControl":[false]}"#
);

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

    fn list(ids: &[(&str, bool)]) -> Vec<(String, bool)> {
        ids.iter().map(|(id, d)| (id.to_string(), *d)).collect()
    }

    /// Drives `step` the way the pump does, with a device list and a count
    /// of the reads it caused.
    struct Rig {
        p: PollState,
        t: Instant,
        host: bool,
        changes: u64,
        listening: bool,
        devices: Vec<(String, bool)>,
        reads: usize,
    }

    impl Rig {
        fn new(host: bool) -> Rig {
            let t = Instant::now();
            let mut p = PollState::new(0);
            p.seen = 0;
            p.last_poll = t;
            Rig { p, t, host, changes: 0, listening: true, devices: list(&[("a", true)]), reads: 0 }
        }

        /// Advances the clock by `ms` and runs one step.
        fn at(&mut self, ms: u64) -> bool {
            self.t += Duration::from_millis(ms);
            let reads = &mut self.reads;
            let devices = self.devices.clone();
            let listening = self.listening;
            self.p.step(self.t, self.host, self.changes, || listening, || {
                *reads += 1;
                devices
            })
        }

        /// Steps every 16 ms for `ms`, and counts the fires.
        fn run(&mut self, ms: u64) -> usize {
            (0..ms / 16).filter(|_| self.at(16)).count()
        }
    }

    #[test]
    fn with_host_notifications_the_devices_are_read_only_after_a_change() {
        let mut r = Rig::new(true);
        // A listener arrives: one baseline read, and then nothing for as long
        // as nothing changes, however long that is.
        assert_eq!(r.run(300), 0);
        assert_eq!(r.reads, 1, "the baseline");
        assert_eq!(r.run(10_000), 0);
        assert_eq!(r.reads, 1, "no change, no read");

        // A headset: a burst of five notifications inside the settle time is
        // one read, and one event.
        r.devices = list(&[("a", false), ("h", true)]);
        for _ in 0..5 {
            r.changes += 1;
            assert!(!r.at(20));
        }
        assert_eq!(r.reads, 1, "nothing is read while the burst goes on");
        assert_eq!(r.run(300), 1);
        assert_eq!(r.reads, 2);
    }

    #[test]
    fn a_notified_change_that_leaves_the_list_as_it_was_fires_nothing() {
        // A role this runtime does not list (Windows' "communications"
        // default) moved, or a disabled endpoint changed state.
        let mut r = Rig::new(true);
        r.run(300);
        r.changes += 1;
        assert_eq!(r.run(300), 0);
        assert_eq!(r.reads, 2, "read once, and found the same list");
    }

    #[test]
    fn without_a_listener_a_change_is_not_read() {
        let mut r = Rig::new(true);
        r.listening = false;
        r.changes += 3;
        r.devices = list(&[("b", true)]);
        assert_eq!(r.run(1_000), 0);
        assert_eq!(r.reads, 0);
        // The listener that arrives later takes the list as it is now: what
        // changed before it subscribed is not reported to it.
        r.listening = true;
        assert_eq!(r.run(300), 0);
        assert_eq!(r.reads, 1);
    }

    #[test]
    fn without_host_notifications_the_poll_runs_every_two_seconds() {
        let mut r = Rig::new(false);
        assert_eq!(r.run(1_900), 0);
        assert_eq!(r.reads, 0);
        assert_eq!(r.run(200), 0);
        assert_eq!(r.reads, 1, "the first poll is the baseline");
        r.devices = list(&[("a", true), ("b", false)]);
        assert_eq!(r.run(2_100), 1);
        assert_eq!(r.reads, 2);
    }

    #[test]
    fn a_default_change_moves_its_flows_generation_and_counts_as_a_change() {
        let (out0, in0, ch0) =
            (default_generation(Flow::Output), default_generation(Flow::Input), CHANGES.load(Ordering::Acquire));
        notify_default_changed(Flow::Input);
        assert_eq!(default_generation(Flow::Input), in0 + 1);
        assert_eq!(default_generation(Flow::Output), out0, "only the input moved");
        assert!(CHANGES.load(Ordering::Acquire) > ch0, "devicechange fires for it too");
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
    fn the_outputs_are_listed_with_a_default_first() {
        let c = ctx();
        assert_eq!(
            run(
                &c,
                "const ds = (await navigator.mediaDevices.enumerateDevices()).filter(d => d.kind === 'audiooutput');
                 if (ds.length < 1) return 'empty list';
                 if (ds[0].deviceId !== 'default') return 'first is ' + ds[0].deviceId;
                 const ids = new Set();
                 for (const d of ds) {
                   if (!(d instanceof MediaDeviceInfo)) return 'not a MediaDeviceInfo';
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
        // The gate is closed here (no test enables capture for this context):
        // at most one anonymous microphone, never a named device the page
        // could offer and then fail to open, and only audio kinds at all.
        let c = ctx();
        assert_eq!(
            run(
                &c,
                "const ds = await navigator.mediaDevices.enumerateDevices();
                 const kinds = new Set(ds.map(d => d.kind));
                 for (const k of kinds) if (k !== 'audioinput' && k !== 'audiooutput') return 'kind ' + k;
                 const ins = ds.filter(d => d.kind === 'audioinput');
                 if (ins.length > 1) return 'named inputs while closed: ' + ins.length;
                 if (ins.length && (ins[0].deviceId !== '' || ins[0].label !== '')) return 'an input is named';
                 if (ins.length && ds.indexOf(ins[0]) !== 0) return 'inputs are not first';
                 return 'ok';"
            ),
            "ok"
        );
        let got = run(&c, "await navigator.mediaDevices.getUserMedia({ audio: true }); return 'resolved';");
        assert!(got.contains("DOMException NotAllowedError"), "{got}");
        assert!(got.contains("has not enabled microphone capture"), "getUserMedia must say why: {got}");
    }

    #[test]
    fn every_enumerated_input_id_is_unique_and_non_empty() {
        let devices = super::input_devices();
        let mut ids: Vec<&str> = devices.iter().map(|d| d.id.as_str()).collect();
        assert!(ids.iter().all(|i| !i.is_empty()), "{devices:?}");
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate input ids in {devices:?}");
    }

    /// An `audioinput` entry is an InputDeviceInfo, as in a browser (the
    /// client's device picker classifies entries that way). Entries are made
    /// directly: what enumerateDevices lists depends on the machine.
    #[test]
    fn an_input_entry_is_an_input_device_info_with_capabilities() {
        let c = ctx();
        ok(&c, "Object.getPrototypeOf(InputDeviceInfo) === MediaDeviceInfo && InputDeviceInfo.prototype instanceof MediaDeviceInfo");
        ok(&c, "Object.prototype.toString.call(InputDeviceInfo.prototype) === '[object InputDeviceInfo]'");
        assert!(throws(&c, "new InputDeviceInfo()").contains("Illegal constructor"));
        unsafe {
            let g = c.raw();
            crate::glue::rjs_lock(g);
            for (name, kind, id) in [("anon", "audioinput", ""), ("mic", "audioinput", "default"), ("spk", "audiooutput", "default")] {
                let d = super::new_device_info(g, kind, id, id);
                crate::webapi::util::define(g, crate::glue::rjs_global(g), name, d, 0);
            }
            crate::glue::rjs_unlock(g);
        }
        ok(&c, "anon instanceof InputDeviceInfo && mic instanceof InputDeviceInfo && !(spk instanceof InputDeviceInfo) && spk instanceof MediaDeviceInfo");
        ok(&c, "Object.prototype.toString.call(mic) === '[object InputDeviceInfo]' && Object.prototype.toString.call(spk) === '[object MediaDeviceInfo]'");
        ok(&c, "mic.kind === 'audioinput' && mic.toJSON().deviceId === 'default'");
        // The anonymous entry has nothing to describe.
        assert_eq!(s(&c, "JSON.stringify(anon.getCapabilities())"), "{}");
        // Capture not allowed: no format to read, the ids only.
        assert_eq!(s(&c, "JSON.stringify(mic.getCapabilities())"), r#"{"deviceId":"default","groupId":""}"#);
        assert!(throws(&c, "InputDeviceInfo.prototype.getCapabilities.call(spk)").contains("wrong this"));
        assert!(throws(&c, "InputDeviceInfo.prototype.getCapabilities.call({})").contains("wrong this"));
        crate::webapi::mediastream::fake::enable(c.raw());
        assert_eq!(s(&c, "JSON.stringify(mic.getCapabilities())"), super::FAKE_CAPABILITIES);
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
