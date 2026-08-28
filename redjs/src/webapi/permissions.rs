//! `navigator.permissions` — the W3C Permissions API over a host-supplied
//! platform backend.
//!
//! # Why the backend is a trait and not code in this file
//!
//! Nothing about a permission is portable. macOS answers from TCC, Windows from
//! a per-user consent store, Linux from a portal or from nothing at all, and an
//! embedder with no OS-level notion of consent (a game shipping its own
//! settings screen) answers from its own preferences. What *is* portable is the
//! shape: four states, a fixed capability set, a status read, a request, and a
//! change signal. That shape is [`PermissionHost`]; the platform is somebody
//! else's crate.
//!
//! An embedder installs one with [`set_host`] before or after
//! [`crate::webapi::install`] — the hook is read at call time, not at install
//! time. With no host installed the API still exists and answers honestly:
//! every capability is [`PermissionState::Unsupported`], which the browser-shaped
//! surface reports as `"denied"` (see below). That is deliberately *not* a
//! missing `navigator.permissions`: feature-detecting code that branches on
//! `navigator.permissions &&` would otherwise take a silently different path
//! here than in a browser.
//!
//! # The four states and the three the web has words for
//!
//! W3C `PermissionState` is `"granted" | "denied" | "prompt"`. A real platform
//! layer needs a fourth — *this OS has no such concept* — and squeezing it into
//! one of the three loses the difference between "the user said no" and "there
//! was never anyone to ask". So [`PermissionState`] carries `Unsupported`, and
//! the **browser-facing** surface projects it onto `"denied"`
//! ([`PermissionState::web_state`]): the capability is not available, which is
//! what `"denied"` means to every consumer that checks it. Embedders that want
//! the distinction read it through their own extension API — that is what
//! vue-native's `appPermissions` does — not by widening a web enum.
//!
//! # Divergence: one `PermissionStatus` per name, not one per query
//!
//! The spec creates a fresh `PermissionStatus` for every `query()`. This module
//! returns the **same** object for the same name in the same context, forever.
//! Two reasons: a live `PermissionStatus` must stay reachable from native code
//! to receive `change`, and there is no weak reference here to let go of it, so
//! a fresh object per query is an unbounded leak in a long-lived app that polls.
//!
//! Observable difference: `(await q('microphone')) !== (await q('microphone'))`
//! is `false` here and `true` in a browser, and two components that each assign
//! `status.onchange` overwrite one another. `addEventListener('change', …)` —
//! the form the spec's own examples use — behaves identically. This is the one
//! deliberate shape divergence in the module and it is pinned by a test.
//!
//! # `request()` is not here
//!
//! The web has no standalone "ask for this permission" call: a browser asks as
//! a side effect of `getUserMedia`, `Notification.requestPermission`, and so on.
//! [`PermissionHost::request`] exists because a platform layer needs it, but no
//! part of `navigator.permissions` calls it. An embedder that wants a
//! standalone request exposes it under its own namespace and documents the
//! divergence.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, RwLock};

use super::events;
use super::util::*;
use crate::glue;

// ---------------------------------------------------------------------------
// The platform contract
// ---------------------------------------------------------------------------

/// The capabilities this runtime knows how to name.
///
/// Deliberately a closed set: a permission the surface cannot name is a
/// permission no application can portably ask about, and an open string would
/// let a typo silently become a capability that is always denied.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Capability {
    Microphone,
    Camera,
    /// Screen/window capture. W3C spells it `display-capture`.
    DisplayCapture,
    Notifications,
    ClipboardRead,
    ClipboardWrite,
    Geolocation,
}

impl Capability {
    /// Every capability, in the order a settings UI should list them.
    pub const ALL: &'static [Capability] = &[
        Capability::Microphone,
        Capability::Camera,
        Capability::DisplayCapture,
        Capability::Notifications,
        Capability::ClipboardRead,
        Capability::ClipboardWrite,
        Capability::Geolocation,
    ];

    /// The W3C `PermissionName` for this capability — the string `query()` takes.
    pub fn web_name(self) -> &'static str {
        match self {
            Capability::Microphone => "microphone",
            Capability::Camera => "camera",
            Capability::DisplayCapture => "display-capture",
            Capability::Notifications => "notifications",
            Capability::ClipboardRead => "clipboard-read",
            Capability::ClipboardWrite => "clipboard-write",
            Capability::Geolocation => "geolocation",
        }
    }

    /// Parse a `PermissionName`. `None` is a `TypeError` at the JS boundary,
    /// exactly as an unknown enum value is in a browser.
    pub fn from_web_name(name: &str) -> Option<Capability> {
        Capability::ALL.iter().copied().find(|c| c.web_name() == name)
    }
}

/// What a platform can tell us about one capability.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PermissionState {
    Granted,
    Denied,
    /// The OS will ask the user the first time the capability is exercised.
    Prompt,
    /// This platform has no notion of consent for this capability, or no way to
    /// read it. Projected to `"denied"` on the browser-shaped surface.
    Unsupported,
}

impl PermissionState {
    /// The W3C `PermissionState` spelling. `Unsupported` becomes `"denied"` —
    /// the honest projection, since the capability is not available either way.
    pub fn web_state(self) -> &'static str {
        match self {
            PermissionState::Granted => "granted",
            PermissionState::Prompt => "prompt",
            PermissionState::Denied | PermissionState::Unsupported => "denied",
        }
    }

    /// The four-state spelling, for an embedder's own extension surface.
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionState::Granted => "granted",
            PermissionState::Denied => "denied",
            PermissionState::Prompt => "prompt",
            PermissionState::Unsupported => "unsupported",
        }
    }
}

/// What starting a request did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RequestOutcome {
    /// The answer was available without asking anyone (already granted; or the
    /// platform cannot ask and the current state is final).
    Settled(PermissionState),
    /// A flow was started and the answer arrives later. The token is minted by
    /// the host and is whatever it needs to correlate the settlement.
    Pending(u32),
}

/// The platform backend behind `navigator.permissions`.
///
/// One implementation per OS. The trait is intentionally free of any
/// browser-shaped type so that a backend can be written and tested without a JS
/// context anywhere near it.
pub trait PermissionHost: Send + Sync {
    /// A human name for this platform, for advisories: `"Windows"`, `"macOS"`.
    fn platform(&self) -> &'static str;

    /// The current state. Must be cheap enough to call once per capability per
    /// frame — the surface reads it live rather than caching, so that a state
    /// changed behind the app's back is never reported stale.
    fn status(&self, cap: Capability) -> PermissionState;

    /// Start a request. Never blocks. Not called by `navigator.permissions` —
    /// see the module header.
    ///
    /// The default is the honest answer for a platform that cannot ask: report
    /// what is already true.
    fn request(&self, cap: Capability) -> RequestOutcome {
        RequestOutcome::Settled(self.status(cap))
    }

    /// Requests that have finished since the last call, as `(token, state)`,
    /// where the token is the one [`RequestOutcome::Pending`] handed back.
    ///
    /// Drained once per pump from the JS thread. Keeping settlement here rather
    /// than in the embedder is what lets each platform own its own policy — a
    /// completion handler on macOS, "watch the consent store and give up after
    /// a while" on Windows — without the JS side knowing the difference. A host
    /// that never returns `Pending` never implements this.
    fn drain_settled(&self) -> Vec<(u32, PermissionState)> {
        Vec::new()
    }

    /// Capabilities whose state changed since the last call.
    ///
    /// `None` means *this platform offers no change notification* — the surface
    /// then never fires `change`, rather than faking it with a poll the
    /// application did not ask to pay for. `Some(vec![])` means "subscribed,
    /// nothing changed".
    ///
    /// Called once per pump from the JS thread; implementations drain a queue
    /// filled by whatever OS mechanism they use.
    fn drain_changes(&self) -> Option<Vec<Capability>> {
        None
    }
}

static HOST: RwLock<Option<&'static dyn PermissionHost>> = RwLock::new(None);

/// Install the platform backend. Last writer wins; an embedder calls this once
/// at boot. Safe to call before or after [`crate::webapi::install`].
pub fn set_host(host: &'static dyn PermissionHost) {
    *HOST.write().unwrap() = Some(host);
}

/// The installed backend, if any.
pub fn host() -> Option<&'static dyn PermissionHost> {
    *HOST.read().unwrap()
}

/// The state to report with no host installed, plus a one-shot note on stderr
/// so a missing `set_host` is findable rather than mysterious.
fn status_of(cap: Capability) -> PermissionState {
    match host() {
        Some(h) => h.status(cap),
        None => {
            static ONCE: OnceLock<()> = OnceLock::new();
            ONCE.get_or_init(|| {
                eprintln!(
                    "[redjs] navigator.permissions: no PermissionHost installed — every \
                     capability reads as denied. Call redjs::webapi::permissions::set_host()."
                );
            });
            PermissionState::Unsupported
        }
    }
}

// ---------------------------------------------------------------------------
// Per-context state
// ---------------------------------------------------------------------------

struct PermClasses {
    status: Class,
    /// Private slot bag on a PermissionStatus ({ name }).
    s_perm: *mut c_void,
}

unsafe impl Send for PermClasses {}
unsafe impl Sync for PermClasses {}

fn classes_reg() -> &'static Mutex<HashMap<usize, &'static PermClasses>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static PermClasses>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> &'static PermClasses {
    classes_reg().lock().unwrap().get(&(g as usize)).copied().expect("webapi::permissions not installed")
}

/// The live `PermissionStatus` per capability, per context — protected, because
/// `pump` dispatches `change` onto them from native code. See the divergence
/// note in the module header for why there is exactly one of each.
fn live() -> &'static Mutex<HashMap<usize, HashMap<Capability, u64>>> {
    static L: OnceLock<Mutex<HashMap<usize, HashMap<Capability, u64>>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// PermissionStatus
// ---------------------------------------------------------------------------

unsafe fn status_slots(g: *mut c_void, obj: u64) -> u64 {
    get_a(g, obj, classes(g).s_perm)
}

unsafe fn status_cap(g: *mut c_void, obj: u64) -> Option<Capability> {
    let slots = status_slots(g, obj);
    if !is_cell(slots) {
        return None;
    }
    read_string(g, get(g, slots, "name")).as_deref().and_then(Capability::from_web_name)
}

unsafe extern "sysv64" fn status_get_name(g: *mut c_void, cf: *mut c_void) -> u64 {
    match status_cap(g, cf_this(cf)) {
        Some(cap) => js_string(g, cap.web_name()),
        None => throw_type_error(g, "PermissionStatus.name on wrong this"),
    }
}

/// Read live rather than from a cached slot: a state changed behind the app's
/// back (the user flipped a privacy toggle) is visible on the very next read,
/// with or without a working change subscription.
unsafe extern "sysv64" fn status_get_state(g: *mut c_void, cf: *mut c_void) -> u64 {
    match status_cap(g, cf_this(cf)) {
        Some(cap) => js_string(g, status_of(cap).web_state()),
        None => throw_type_error(g, "PermissionStatus.state on wrong this"),
    }
}

unsafe fn status_for(g: *mut c_void, cap: Capability) -> u64 {
    if let Some(obj) = live().lock().unwrap().get(&(g as usize)).and_then(|m| m.get(&cap)).copied() {
        return obj;
    }
    let c = classes(g);
    let obj = new_object(g, c.status.proto);
    let slots = new_object(g, UNDEF);
    define(g, slots, "name", js_string(g, cap.web_name()), 0);
    glue::rjs_define(g, obj, c.s_perm, slots, 0);
    // Held for the life of the context so `pump` can dispatch onto it.
    crate::ffi::redjs_protect(g, obj);
    live().lock().unwrap().entry(g as usize).or_default().insert(cap, obj);
    obj
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

/// `navigator.permissions.query(descriptor)`.
///
/// Browser behaviour, kept exactly: a non-object descriptor or a missing/unknown
/// `name` **rejects with a `TypeError`** rather than resolving to `"denied"`, so
/// a typo surfaces as a typo instead of as a permission problem.
unsafe extern "sysv64" fn perms_query(g: *mut c_void, cf: *mut c_void) -> u64 {
    const KIND_OBJECT: u32 = 8;
    let desc = cf_arg(cf, 0);
    if !is_cell(desc) || glue::rjs_kind(g, desc) != KIND_OBJECT {
        return rejected_promise(
            g,
            1, // TypeError
            "Failed to execute 'query' on 'Permissions': parameter 1 is not of type \
             'PermissionDescriptor'.",
        );
    }
    // `name` is a REQUIRED WebIDL member, so an absent one is its own error and
    // not "the enum value 'undefined' is invalid" — `read_string` stringifies,
    // so the raw value has to be inspected before it does.
    const KIND_UNDEFINED: u32 = 0;
    let raw = get(g, desc, "name");
    if glue::rjs_kind(g, raw) == KIND_UNDEFINED {
        return rejected_promise(
            g,
            1,
            "Failed to read the 'name' property from 'PermissionDescriptor': Required member is \
             undefined.",
        );
    }
    let Some(name) = read_string(g, raw) else {
        return rejected_promise(
            g,
            1,
            "Failed to read the 'name' property from 'PermissionDescriptor': Required member is \
             undefined.",
        );
    };
    let Some(cap) = Capability::from_web_name(&name) else {
        let known: Vec<&str> = Capability::ALL.iter().map(|c| c.web_name()).collect();
        return rejected_promise(
            g,
            1,
            &format!(
                "Failed to read the 'name' property from 'PermissionDescriptor': The provided \
                 value '{name}' is not a valid enum value of type PermissionName. Known names: {}.",
                known.join(", ")
            ),
        );
    };
    resolved_promise(g, status_for(g, cap))
}

// ---------------------------------------------------------------------------
// change delivery
// ---------------------------------------------------------------------------

/// Called once per pump. Costs one `drain_changes()` call — nothing at all on a
/// platform whose host returns `None`, and no work per capability unless
/// something actually changed.
pub unsafe fn pump(g: *mut c_void) {
    let Some(h) = host() else { return };
    let Some(changed) = h.drain_changes() else { return };
    if changed.is_empty() {
        return;
    }
    for cap in changed {
        // Only capabilities somebody has queried have a status object; a change
        // nobody subscribed to is not an event.
        let obj = live().lock().unwrap().get(&(g as usize)).and_then(|m| m.get(&cap)).copied();
        let Some(obj) = obj else { continue };
        let ev = events::new_trusted_event(g, "change");
        events::dispatch(g, obj, ev);
    }
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

pub unsafe fn install(g: *mut c_void, event_target_proto: u64) {
    let s_perm = atom_priv(g, "redjs#permissionStatus");

    let status = define_class(g, &ClassSpec {
        name: "PermissionStatus",
        arity: 0,
        construct: None,
        parent_proto: event_target_proto,
        methods: &[],
        getters: &[("name", status_get_name), ("state", status_get_state)],
        accessors: &[],
    });

    let perms_class = define_class(g, &ClassSpec {
        name: "Permissions",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[("query", 1, perms_query)],
        getters: &[],
        accessors: &[],
    });

    let boxed: &'static PermClasses = Box::leak(Box::new(PermClasses { status, s_perm }));
    classes_reg().lock().unwrap().insert(g as usize, boxed);

    let perms = new_object(g, perms_class.proto);
    crate::ffi::redjs_protect(g, perms);

    // `navigator` may already exist (mediadevices, or the host's own); only
    // `permissions` is ours, and it goes onto whatever object is there.
    let global = glue::rjs_global(g);
    let existing = get(g, global, "navigator");
    let navigator = if is_cell(existing) {
        existing
    } else {
        let n = new_object(g, UNDEF);
        define(g, global, "navigator", n, 2 /* DontEnum */);
        n
    };
    define(g, navigator, "permissions", perms, 2);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_capability_round_trips_through_its_web_name() {
        // The name table is the contract with `query()`; a capability whose
        // name does not parse back is one no application can ask about.
        for cap in Capability::ALL {
            assert_eq!(Capability::from_web_name(cap.web_name()), Some(*cap), "{cap:?}");
        }
        assert_eq!(Capability::ALL.len(), 7);
    }

    #[test]
    fn the_w3c_names_are_the_w3c_spellings() {
        // Pinned literally: `display-capture` and `clipboard-read` are hyphenated
        // in the spec, and a camelCase slip here is a silent divergence that
        // only shows up as a TypeError in somebody else's app.
        assert_eq!(Capability::Microphone.web_name(), "microphone");
        assert_eq!(Capability::Camera.web_name(), "camera");
        assert_eq!(Capability::DisplayCapture.web_name(), "display-capture");
        assert_eq!(Capability::Notifications.web_name(), "notifications");
        assert_eq!(Capability::ClipboardRead.web_name(), "clipboard-read");
        assert_eq!(Capability::ClipboardWrite.web_name(), "clipboard-write");
        assert_eq!(Capability::Geolocation.web_name(), "geolocation");
    }

    #[test]
    fn unknown_names_do_not_parse() {
        for bad in ["Microphone", "mic", "displayCapture", "clipboard", "", "midi"] {
            assert_eq!(Capability::from_web_name(bad), None, "{bad:?} must not parse");
        }
    }

    #[test]
    fn unsupported_projects_to_denied_on_the_web_surface_and_keeps_its_own_name() {
        // The whole point of the fourth state: a platform layer can tell the
        // two apart while the browser-shaped surface still only ever says one
        // of the three words the web knows.
        assert_eq!(PermissionState::Unsupported.web_state(), "denied");
        assert_eq!(PermissionState::Denied.web_state(), "denied");
        assert_eq!(PermissionState::Unsupported.as_str(), "unsupported");
        assert_eq!(PermissionState::Denied.as_str(), "denied");
        assert_eq!(PermissionState::Granted.web_state(), "granted");
        assert_eq!(PermissionState::Prompt.web_state(), "prompt");
    }

    /// A host that answers from a table — the shape every backend test uses.
    struct TableHost(&'static [(Capability, PermissionState)]);
    impl PermissionHost for TableHost {
        fn platform(&self) -> &'static str {
            "test"
        }
        fn status(&self, cap: Capability) -> PermissionState {
            self.0.iter().find(|(c, _)| *c == cap).map(|(_, s)| *s).unwrap_or(PermissionState::Unsupported)
        }
    }

    #[test]
    fn the_default_request_reports_what_is_already_true() {
        // A backend that cannot ask must not invent a Pending that never
        // settles; the default impl is the one every such platform inherits.
        let h = TableHost(&[
            (Capability::Microphone, PermissionState::Granted),
            (Capability::Camera, PermissionState::Denied),
        ]);
        assert_eq!(h.request(Capability::Microphone), RequestOutcome::Settled(PermissionState::Granted));
        assert_eq!(h.request(Capability::Camera), RequestOutcome::Settled(PermissionState::Denied));
        assert_eq!(
            h.request(Capability::Geolocation),
            RequestOutcome::Settled(PermissionState::Unsupported)
        );
    }

    #[test]
    fn no_subscription_is_distinguishable_from_no_changes() {
        // `None` must not be spelled `Some(vec![])`: the first means "never fire
        // `change` on this platform", the second "nothing happened this frame".
        let h = TableHost(&[]);
        assert!(h.drain_changes().is_none());
    }
}

#[cfg(test)]
mod jsc_tests {
    use super::*;
    use crate::api::Context;
    use crate::webapi::testing::*;

    fn run(c: &Context, body: &str) -> String {
        s(
            c,
            &format!(
                "globalThis.__out = '<pending>'; (async () => {{ {body} }})().then(\
                   v => {{ globalThis.__out = String(v); }}, \
                   e => {{ globalThis.__out = 'ERROR: ' + ((e && e.message) || e); }}); 'started'"
            ),
        );
        for _ in 0..64 {
            c.drain_microtasks();
        }
        s(c, "globalThis.__out")
    }

    /// Installed once for the whole test binary — `set_host` is process-global,
    /// which is what an embedder's single boot-time call looks like.
    fn install_scripted_host() {
        static H: ScriptedHost = ScriptedHost;
        set_host(&H);
    }

    struct ScriptedHost;
    impl PermissionHost for ScriptedHost {
        fn platform(&self) -> &'static str {
            "scripted"
        }
        fn status(&self, cap: Capability) -> PermissionState {
            match cap {
                Capability::Microphone => PermissionState::Granted,
                Capability::Camera => PermissionState::Denied,
                Capability::DisplayCapture => PermissionState::Prompt,
                _ => PermissionState::Unsupported,
            }
        }
    }

    #[test]
    fn permissions_is_a_singleton_on_navigator() {
        let c = ctx();
        ok(&c, "navigator.permissions === navigator.permissions");
        ok(&c, "navigator.permissions instanceof Permissions");
        ok(&c, "typeof navigator.permissions.query === 'function'");
        // mediadevices and permissions must share one `navigator`, whichever
        // installs first.
        ok(&c, "typeof navigator === 'object' && navigator !== null");
    }

    #[test]
    fn query_resolves_a_permission_status_that_is_an_event_target() {
        install_scripted_host();
        let c = ctx();
        assert_eq!(
            run(
                &c,
                "const st = await navigator.permissions.query({ name: 'microphone' });
                 if (!(st instanceof PermissionStatus)) return 'not a PermissionStatus';
                 if (st.name !== 'microphone') return 'name: ' + st.name;
                 if (typeof st.addEventListener !== 'function') return 'not an EventTarget';
                 return 'ok';"
            ),
            "ok"
        );
    }

    #[test]
    fn the_three_web_states_come_through_and_unsupported_reads_as_denied() {
        install_scripted_host();
        let c = ctx();
        assert_eq!(
            run(
                &c,
                "const q = n => navigator.permissions.query({ name: n }).then(s => s.state);
                 return [await q('microphone'), await q('camera'), await q('display-capture'),
                         await q('geolocation')].join(',');"
            ),
            "granted,denied,prompt,denied"
        );
    }

    #[test]
    fn an_unknown_name_is_a_type_error_not_a_denial() {
        // The failure mode this prevents: `query({name:'mic'})` quietly
        // resolving "denied" and the app disabling a feature that works.
        let c = ctx();
        let got = run(&c, "const s = await navigator.permissions.query({ name: 'mic' }); return s.state;");
        assert!(got.starts_with("ERROR:"), "{got}");
        assert!(got.contains("not a valid enum value"), "{got}");
        assert!(got.contains("microphone"), "the message must list the known names: {got}");

        let got = run(&c, "return (await navigator.permissions.query('microphone')).state;");
        assert!(got.contains("PermissionDescriptor"), "{got}");

        let got = run(&c, "return (await navigator.permissions.query({})).state;");
        assert!(got.contains("Required member is undefined"), "{got}");

        // …and an explicit `undefined` is the same error, not an invalid-enum one.
        let got = run(&c, "return (await navigator.permissions.query({ name: undefined })).state;");
        assert!(got.contains("Required member is undefined"), "{got}");
    }

    #[test]
    fn the_documented_divergence_is_exactly_one_status_per_name() {
        // Pinned so it stays a decision. If this ever flips to per-query
        // objects, `pump`'s dispatch target set has to change with it.
        install_scripted_host();
        let c = ctx();
        assert_eq!(
            run(
                &c,
                "const a = await navigator.permissions.query({ name: 'microphone' });
                 const b = await navigator.permissions.query({ name: 'microphone' });
                 const d = await navigator.permissions.query({ name: 'camera' });
                 return (a === b) + ',' + (a === d);"
            ),
            "true,false"
        );
    }

    #[test]
    fn both_change_listener_shapes_fire() {
        install_scripted_host();
        let c = ctx();
        assert_eq!(
            run(
                &c,
                "const st = await navigator.permissions.query({ name: 'microphone' });
                 let n = 0;
                 st.addEventListener('change', () => { n++; });
                 st.onchange = () => { n += 10; };
                 st.dispatchEvent(new Event('change'));
                 return n;"
            ),
            "11"
        );
    }

    #[test]
    fn the_classes_are_not_constructible_and_the_getters_refuse_a_foreign_this() {
        let c = ctx();
        assert!(throws(&c, "new PermissionStatus()").contains("Illegal constructor"));
        assert!(throws(&c, "new Permissions()").contains("Illegal constructor"));
        let msg = throws(
            &c,
            "Object.getOwnPropertyDescriptor(PermissionStatus.prototype, 'state').get.call({})",
        );
        assert!(msg.contains("wrong this"), "{msg}");
    }
}
