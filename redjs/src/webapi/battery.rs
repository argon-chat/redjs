//! Optional Battery Status API (https://www.w3.org/TR/battery-status/).
//!
//! `navigator.getBattery()` caches one Promise and one EventTarget per context.
//! Windows uses power-setting notifications; Linux uses UPower's system D-Bus
//! display device; macOS uses IOPowerSources run-loop notifications. Their
//! callbacks touch no JS: they coalesce the latest snapshot into this context's inbox and wake its existing pump.
//! Delivery updates each changed attribute and fires its trusted event as one
//! task, with a microtask checkpoint after it. There is no polling thread.
//!
//! Acquisition on Linux / macOS runs off the JS thread. The first Promise is
//! fulfilled by the pump only after the initial native snapshot is available.
//! Unknown status, no battery, and platforms without a provider use the spec's
//! fully charged / plugged-in defaults. Windows has no charging ETA in this
//! API, so chargingTime is Infinity until full; unknown discharge ETA is also
//! Infinity. Remaining-time estimates refresh with power/capacity changes.
//! Native embedders have no Document permissions policy or insecure browsing
//! context: opting into the Cargo feature enables this application capability.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use super::events;
use super::util::*;
use crate::{context::ContextId, glue};

#[cfg(any(target_os = "linux", test))]
mod linux;
#[cfg(any(target_os = "macos", test))]
mod macos;
#[cfg(target_os = "linux")]
use linux::native as platform;
#[cfg(target_os = "macos")]
use macos::native as platform;

#[derive(Clone, Copy, Debug, PartialEq)]
struct BatteryState {
    charging: bool,
    charging_time: f64,
    discharging_time: f64,
    level: f64,
}

impl Default for BatteryState {
    fn default() -> Self {
        Self { charging: true, charging_time: 0.0, discharging_time: f64::INFINITY, level: 1.0 }
    }
}

// Matches SYSTEM_POWER_STATUS. Kept outside the Windows module so conversion
// tests exercise actual OS sentinels on every build platform.
#[cfg(any(windows, test))]
#[repr(C)]
#[derive(Default)]
struct PowerStatus {
    ac_line_status: u8,
    battery_flag: u8,
    battery_life_percent: u8,
    system_status_flag: u8,
    battery_life_time: u32,
    battery_full_life_time: u32,
}

#[cfg(any(windows, test))]
impl From<PowerStatus> for BatteryState {
    fn from(s: PowerStatus) -> Self {
        if s.battery_flag == 255 || s.battery_flag & 128 != 0 {
            return Self::default();
        }
        let charging = s.ac_line_status != 0 || s.battery_flag & 8 != 0;
        Self {
            charging,
            charging_time: if s.battery_life_percent == 100 { 0.0 } else { f64::INFINITY },
            discharging_time: if !charging && s.battery_life_time != u32::MAX {
                s.battery_life_time as f64
            } else {
                f64::INFINITY
            },
            level: if s.battery_life_percent <= 100 { s.battery_life_percent as f64 / 100.0 } else { 1.0 },
        }
    }
}

struct BatteryContext {
    class: Class,
    navigator: u64,
    // Private-name atoms belong to this heap; no public Symbol can forge them.
    s_battery: usize,
    s_handlers: usize,
    manager: u64,
    promise: u64,
    // Protected until an asynchronous provider's first snapshot is pumped.
    resolve: u64,
    current: BatteryState,
    pending: Option<BatteryState>,
    delivering: bool,
    subscription: Option<platform::Subscription>,
    #[cfg(test)]
    initial: Option<BatteryState>,
    #[cfg(test)]
    defer_initial: bool,
}

fn registry() -> &'static Mutex<HashMap<u64, BatteryContext>> {
    static R: OnceLock<Mutex<HashMap<u64, BatteryContext>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

// An idle pump with no update anywhere costs just this load.
static PENDING: AtomicUsize = AtomicUsize::new(0);
static ACQUIRING: AtomicUsize = AtomicUsize::new(0);

/// Removes the id before unregistering native callbacks. A callback already in
/// flight can only address this dead id, never a context reusing its address.
/// Protected values / atoms belong to the old heap and are not touched here.
pub(crate) fn forget(gone: crate::context::Gone) {
    let old = {
        let mut reg = registry().lock().unwrap();
        let old = reg.remove(&gone.id.0);
        if old.as_ref().is_some_and(|c| c.pending.is_some()) {
            PENDING.fetch_sub(1, Ordering::AcqRel);
        }
        if old.as_ref().is_some_and(|c| is_cell(c.resolve)) {
            ACQUIRING.fetch_sub(1, Ordering::AcqRel);
        }
        old
    };
    // Unregistration can wait for a callback; never hold the registry lock.
    drop(old);
}

fn queue(c: &mut BatteryContext, state: BatteryState) {
    let was_pending = c.pending.is_some();
    c.pending = (is_cell(c.resolve) || c.delivering || state != c.current).then_some(state);
    match (was_pending, c.pending.is_some()) {
        (false, true) => {
            PENDING.fetch_add(1, Ordering::AcqRel);
        }
        (true, false) => {
            PENDING.fetch_sub(1, Ordering::AcqRel);
        }
        _ => {}
    }
}

/// Background providers publish Rust snapshots only. A removed context id is
/// never recreated; late acquisition / notification callbacks are harmless.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn publish(id: ContextId, state: BatteryState) {
    let changed = {
        let mut reg = registry().lock().unwrap();
        let Some(c) = reg.get_mut(&id.0) else { return };
        queue(c, state);
        c.pending.is_some()
    };
    if changed {
        crate::web::wake(id);
    }
}

#[cfg(windows)]
fn refresh(id: ContextId) {
    let changed = {
        let mut reg = registry().lock().unwrap();
        let Some(c) = reg.get_mut(&id.0) else { return };
        // Sampling under the lock also orders callbacks from both native
        // subscriptions: an older sample cannot overwrite a newer one.
        queue(c, platform::read());
        c.pending.is_some()
    };
    if changed {
        crate::web::wake(id);
    }
}

unsafe fn branded(g: *mut c_void, value: u64) -> Option<(ContextId, usize, BatteryState)> {
    let id = crate::context::id_of(g)?;
    let (manager, brand, handlers, current) = {
        let reg = registry().lock().unwrap();
        let c = reg.get(&id.0)?;
        (c.manager, c.s_battery, c.s_handlers, c.current)
    };
    // Identity first: Object.create(manager) and proxies cannot inherit a brand.
    if !is_cell(value) || value != manager || get_a(g, value, brand as *mut c_void) != TRUE {
        return None;
    }
    Some((id, handlers, current))
}

macro_rules! battery_getter {
    ($name:ident, $field:ident, $encode:ident) => {
        #[crate::host_fn]
        unsafe fn $name(g: *mut c_void, cf: *mut c_void) -> u64 {
            match branded(g, cf_this(cf)) {
                Some((_, _, state)) => $encode(state.$field),
                None => throw_type_error(g, "Illegal invocation"),
            }
        }
    };
}

battery_getter!(get_charging, charging, enc_bool);
battery_getter!(get_charging_time, charging_time, enc_f64);
battery_getter!(get_discharging_time, discharging_time, enc_f64);
battery_getter!(get_level, level, enc_f64);

unsafe fn get_handler(g: *mut c_void, cf: *mut c_void, name: &str) -> u64 {
    let value = cf_this(cf);
    let Some((_, slot, _)) = branded(g, value) else { return throw_type_error(g, "Illegal invocation") };
    get(g, get_a(g, value, slot as *mut c_void), name)
}

unsafe fn set_handler(g: *mut c_void, cf: *mut c_void, name: &str) -> u64 {
    let value = cf_this(cf);
    let Some((_, slot, _)) = branded(g, value) else { return throw_type_error(g, "Illegal invocation") };
    let handler = cf_arg(cf, 0);
    define(g, get_a(g, value, slot as *mut c_void), name, if glue::rjs_kind(g, handler) == 9 { handler } else { NULL }, 0);
    UNDEF
}

macro_rules! battery_handler {
    ($get:ident, $set:ident, $name:literal) => {
        #[crate::host_fn]
        unsafe fn $get(g: *mut c_void, cf: *mut c_void) -> u64 {
            get_handler(g, cf, $name)
        }
        #[crate::host_fn]
        unsafe fn $set(g: *mut c_void, cf: *mut c_void) -> u64 {
            set_handler(g, cf, $name)
        }
    };
}

battery_handler!(get_on_charging, set_on_charging, "onchargingchange");
battery_handler!(get_on_charging_time, set_on_charging_time, "onchargingtimechange");
battery_handler!(get_on_discharging_time, set_on_discharging_time, "ondischargingtimechange");
battery_handler!(get_on_level, set_on_level, "onlevelchange");

#[crate::host_fn]
pub(super) unsafe fn get_battery(g: *mut c_void, cf: *mut c_void) -> u64 {
    let id = crate::context::ensure(g);
    let navigator = registry().lock().unwrap().get(&id.0).expect("battery installed after navigator").navigator;
    if cf_this(cf) != navigator {
        // Promise-returning WebIDL operations reject for an invalid receiver.
        // Identity also rejects proxies and objects inheriting from navigator.
        return rejected_promise(g, 1, "Illegal invocation");
    }
    let (proto, brand, handler_slot) = {
        let reg = registry().lock().unwrap();
        let c = reg.get(&id.0).expect("battery installed after navigator");
        if is_cell(c.promise) {
            return c.promise;
        }
        (c.class.proto, c.s_battery, c.s_handlers)
    };
    let manager = new_object(g, proto);
    // Root before any further JSC allocation, including the private slot bag.
    crate::ffi::redjs_protect(g, manager);
    glue::rjs_define(g, manager, brand as *mut c_void, TRUE, 1 | 2 | 4);
    let handlers = new_object(g, UNDEF);
    glue::rjs_define(g, manager, handler_slot as *mut c_void, handlers, 1 | 2 | 4);
    for name in ["onchargingchange", "onchargingtimechange", "ondischargingtimechange", "onlevelchange"] {
        define(g, handlers, name, NULL, 0);
    }
    // Install the cache before resolving: resolution can synchronously read
    // an inherited `then` getter, and that getter may call getBattery again.
    let (mut resolve, mut reject, mut promise) = (UNDEF, UNDEF, UNDEF);
    if crate::ffi::redjs_promise_create(g, &mut resolve, &mut reject, &mut promise) != 0 {
        crate::ffi::redjs_unprotect(g, manager);
        return UNDEF;
    }
    crate::ffi::redjs_protect(g, promise);
    crate::ffi::redjs_protect(g, resolve);
    #[cfg(test)]
    let initial = registry().lock().unwrap().get(&id.0).unwrap().initial;
    #[cfg(not(test))]
    let initial: Option<BatteryState> = None;
    #[cfg(test)]
    let defer_injected = registry().lock().unwrap().get(&id.0).unwrap().defer_initial;
    #[cfg(not(test))]
    let defer_injected = false;
    let deferred = defer_injected || (initial.is_none() && platform::ASYNC_INITIAL);
    {
        let mut reg = registry().lock().unwrap();
        let c = reg.get_mut(&id.0).unwrap();
        c.manager = manager;
        c.promise = promise;
        c.resolve = if deferred { resolve } else { UNDEF };
        if deferred {
            ACQUIRING.fetch_add(1, Ordering::AcqRel);
        }
        c.current = initial.unwrap_or_else(platform::read);
    }
    // Windows can call back immediately: the entry must already be complete,
    // and registration must run without its lock. Tests inject context-local
    // state and skip the real subscription, avoiding machine-dependent races.
    if initial.is_none() && !defer_injected {
        let subscription = platform::subscribe(id);
        registry().lock().unwrap().get_mut(&id.0).unwrap().subscription = Some(subscription);
    }
    // Native Promise machinery remains independent of a replaced global.
    // Even an already fulfilled Promise invokes .then callbacks asynchronously.
    if !deferred {
        let _ = call(g, resolve, UNDEF, &[manager]);
        crate::ffi::redjs_unprotect(g, resolve);
    }
    promise
}

unsafe fn inflight(g: *mut c_void) -> bool {
    if ACQUIRING.load(Ordering::Acquire) == 0 {
        return false;
    }
    let Some(id) = crate::context::id_of(g) else { return false };
    registry().lock().unwrap().get(&id.0).is_some_and(|c| is_cell(c.resolve))
}

unsafe fn ready(g: *mut c_void) -> bool {
    if PENDING.load(Ordering::Acquire) == 0 {
        return false;
    }
    let Some(id) = crate::context::id_of(g) else { return false };
    registry().lock().unwrap().get(&id.0).is_some_and(|c| c.pending.is_some())
}

unsafe fn drain(g: *mut c_void) -> usize {
    if PENDING.load(Ordering::Acquire) == 0 {
        return 0;
    }
    let Some(id) = crate::context::id_of(g) else { return 0 };
    let (manager, old, next, resolve) = {
        let mut reg = registry().lock().unwrap();
        let Some(c) = reg.get_mut(&id.0) else { return 0 };
        let Some(next) = c.pending.take() else { return 0 };
        PENDING.fetch_sub(1, Ordering::AcqRel);
        let resolve = c.resolve;
        c.resolve = UNDEF;
        if is_cell(resolve) {
            ACQUIRING.fetch_sub(1, Ordering::AcqRel);
            // Initial acquisition is one atomic snapshot, visible in full to
            // Promise handlers. Startup is not a battery change notification.
            c.current = next;
        } else {
            c.delivering = true;
        }
        (c.manager, c.current, next, resolve)
    };
    crate::web::checkpoint(g);
    if is_cell(resolve) {
        let _ = call(g, resolve, UNDEF, &[manager]);
        crate::ffi::redjs_unprotect(g, resolve);
        crate::web::checkpoint(g);
        return 1;
    }
    let mut count = 0;
    macro_rules! update {
        ($field:ident, $event:literal) => {
            if old.$field != next.$field {
                {
                    let mut reg = registry().lock().unwrap();
                    let Some(c) = reg.get_mut(&id.0) else { return count };
                    c.current.$field = next.$field;
                }
                events::dispatch(g, manager, events::new_trusted_event(g, $event));
                crate::web::checkpoint(g);
                count += 1;
            }
        };
    }
    update!(charging, "chargingchange");
    update!(charging_time, "chargingtimechange");
    update!(discharging_time, "dischargingtimechange");
    update!(level, "levelchange");
    {
        let mut reg = registry().lock().unwrap();
        if let Some(c) = reg.get_mut(&id.0) {
            c.delivering = false;
            // A callback during delivery may repeat this same snapshot.
            if c.pending == Some(c.current) {
                c.pending = None;
                PENDING.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }
    count
}

pub(super) unsafe fn install(g: *mut c_void, event_target_proto: u64) {
    let class = define_class(
        g,
        &ClassSpec {
            name: "BatteryManager",
            arity: 0,
            construct: None,
            parent_proto: event_target_proto,
            methods: &[],
            getters: &[
                ("charging", get_charging),
                ("chargingTime", get_charging_time),
                ("dischargingTime", get_discharging_time),
                ("level", get_level),
            ],
            accessors: &[
                ("onchargingchange", get_on_charging, set_on_charging),
                ("onchargingtimechange", get_on_charging_time, set_on_charging_time),
                ("ondischargingtimechange", get_on_discharging_time, set_on_discharging_time),
                ("onlevelchange", get_on_level, set_on_level),
            ],
        },
    );
    let id = crate::context::ensure(g);
    let navigator = get(g, glue::rjs_global(g), "navigator");
    crate::ffi::redjs_protect(g, navigator);
    let s_battery = atom_priv(g, "redjs#battery") as usize;
    let s_handlers = atom_priv(g, "redjs#batteryHandlers") as usize;
    registry().lock().unwrap().insert(
        id.0,
        BatteryContext {
            class,
            navigator,
            s_battery,
            s_handlers,
            manager: UNDEF,
            promise: UNDEF,
            resolve: UNDEF,
            current: BatteryState::default(),
            pending: None,
            delivering: false,
            subscription: None,
            #[cfg(test)]
            initial: None,
            #[cfg(test)]
            defer_initial: false,
        },
    );
    crate::web::register_pump_source(crate::web::PumpSource { inflight, ready, drain });
}

#[cfg(windows)]
mod platform {
    use super::*;
    pub(super) const ASYNC_INITIAL: bool = false;

    #[repr(C)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }
    #[repr(C)]
    struct SubscribeParameters {
        callback: unsafe extern "system" fn(*const c_void, u32, *const c_void) -> u32,
        context: *const c_void,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetSystemPowerStatus(status: *mut PowerStatus) -> i32;
    }
    #[link(name = "powrprof")]
    extern "system" {
        fn PowerSettingRegisterNotification(
            guid: *const Guid,
            flags: u32,
            recipient: *mut c_void,
            registration: *mut *mut c_void,
        ) -> u32;
        fn PowerSettingUnregisterNotification(registration: *mut c_void) -> u32;
    }

    pub(super) fn read() -> BatteryState {
        let mut status = PowerStatus::default();
        if unsafe { GetSystemPowerStatus(&mut status) } != 0 {
            status.into()
        } else {
            BatteryState::default()
        }
    }

    // Callback userdata is the monotonic id encoded as a pointer, never a
    // dereferenced allocation: a late callback has no freed memory to touch.
    unsafe extern "system" fn changed(context: *const c_void, _kind: u32, _setting: *const c_void) -> u32 {
        refresh(ContextId(context as usize as u64));
        0
    }

    pub(super) struct Subscription {
        handles: Vec<usize>,
        // Windows receives this stable address for both registrations. Keep it
        // alive until Drop has unregistered every handle; do not assume the
        // notification API copies its input structure.
        recipient: Box<SubscribeParameters>,
    }

    // Handles may be unregistered on another native thread. The boxed
    // parameters remain at a stable address and are immutable after creation;
    // their only data pointer is an opaque ContextId, never dereferenced.
    // No JS value or VM pointer crosses threads through this subscription.
    unsafe impl Send for Subscription {}

    #[cfg(test)]
    impl Subscription {
        pub(super) fn registration_count(&self) -> usize {
            self.handles.len()
        }
    }

    impl Drop for Subscription {
        fn drop(&mut self) {
            for handle in &self.handles {
                unsafe {
                    PowerSettingUnregisterNotification(*handle as *mut c_void);
                }
            }
        }
    }

    pub(super) fn subscribe(id: ContextId) -> Subscription {
        const SETTINGS: [Guid; 2] = [
            // GUID_ACDC_POWER_SOURCE; GUID_BATTERY_PERCENTAGE_REMAINING.
            Guid { data1: 0x5d3e9a59, data2: 0xe9d5, data3: 0x4b00, data4: [0xa6, 0xbd, 0xff, 0x34, 0xff, 0x51, 0x65, 0x48] },
            Guid { data1: 0xa7ad8041, data2: 0xb45a, data3: 0x4cae, data4: [0x87, 0xa3, 0xee, 0xcb, 0xb4, 0x68, 0xa9, 0xe1] },
        ];
        let mut subscription = Subscription {
            handles: Vec::new(),
            recipient: Box::new(SubscribeParameters { callback: changed, context: id.0 as usize as *const c_void }),
        };
        let recipient = subscription.recipient.as_ref() as *const SubscribeParameters as *mut c_void;
        for guid in &SETTINGS {
            let mut handle = core::ptr::null_mut();
            // DEVICE_NOTIFY_CALLBACK = 2. Recipient is an input-only pointer.
            let error = unsafe { PowerSettingRegisterNotification(guid, 2, recipient, &mut handle) };
            if error == 0 {
                subscription.handles.push(handle as usize);
            } else {
                eprintln!("[redjs] battery: Windows power notification registration failed ({error}); this setting will retain its last reported value");
            }
        }
        subscription
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod platform {
    use super::*;
    pub(super) struct Subscription;
    pub(super) const ASYNC_INITIAL: bool = false;
    pub(super) fn read() -> BatteryState {
        BatteryState::default()
    }
    pub(super) fn subscribe(_id: ContextId) -> Subscription {
        Subscription
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webapi::testing::{ctx, ok, s};

    fn initial(c: &crate::api::Context, state: BatteryState) {
        let id = crate::context::id_of(c.raw()).unwrap();
        registry().lock().unwrap().get_mut(&id.0).unwrap().initial = Some(state);
    }

    fn publish(c: &crate::api::Context, state: BatteryState) {
        let id = crate::context::id_of(c.raw()).unwrap();
        super::publish(id, state);
    }

    fn pump(c: &crate::api::Context) {
        unsafe {
            // Match a host frame: only explicit checkpoints drain microtasks.
            glue::rjs_lock(c.raw());
            crate::web::pump(c.raw(), crate::web::PUMP_TIMERS | crate::web::PUMP_MICROTASKS, core::ptr::null_mut());
            glue::rjs_unlock(c.raw());
        }
    }

    fn battery(c: &crate::api::Context) {
        s(c, "navigator.getBattery().then(b => globalThis.battery = b); 0");
        pump(c);
    }

    #[test]
    fn windows_values_and_unknown_sentinels() {
        fn raw(ac: u8, flags: u8, level: u8, seconds: u32) -> PowerStatus {
            PowerStatus {
                ac_line_status: ac,
                battery_flag: flags,
                battery_life_percent: level,
                battery_life_time: seconds,
                ..PowerStatus::default()
            }
        }
        assert_eq!(BatteryState::from(raw(1, 128, 255, u32::MAX)), BatteryState::default());
        assert_eq!(BatteryState::from(raw(255, 255, 255, u32::MAX)), BatteryState::default());
        let dc = BatteryState::from(raw(0, 2, 25, 3600));
        assert_eq!(dc, BatteryState { charging: false, charging_time: f64::INFINITY, discharging_time: 3600.0, level: 0.25 });
        let ac = BatteryState::from(raw(1, 8, 50, 3600));
        assert!(ac.charging && ac.charging_time.is_infinite() && ac.discharging_time.is_infinite());
        assert_eq!(BatteryState::from(raw(1, 1, 100, u32::MAX)), BatteryState::default());
        let unknown = BatteryState::from(raw(0, 0, 255, u32::MAX));
        assert!(
            !unknown.charging
                && unknown.level == 1.0
                && unknown.charging_time.is_infinite()
                && unknown.discharging_time.is_infinite()
        );
        assert_eq!(BatteryState::from(raw(0, 4, 0, 0)).discharging_time, 0.0);
    }

    #[test]
    fn promise_singleton_branding_and_default_values() {
        let c = ctx();
        initial(&c, BatteryState::default());
        ok(&c, "navigator.getBattery.name === 'getBattery' && navigator.getBattery.length === 0 && !Object.hasOwn(navigator, 'getBattery')");
        ok(&c, "(() => { globalThis.sync = true; globalThis.ranAsync = false; const p = navigator.getBattery(); globalThis.first = p; p.then(b => { globalThis.battery = b; globalThis.ranAsync = !sync; }); sync = false; return p instanceof Promise && p === navigator.getBattery() && !ranAsync; })()");
        pump(&c);
        ok(&c, "ranAsync && battery instanceof BatteryManager && battery instanceof EventTarget && Object.prototype.toString.call(battery) === '[object BatteryManager]'");
        ok(&c, "battery.charging === true && battery.chargingTime === 0 && battery.dischargingTime === Infinity && battery.level === 1");
        ok(&c, "['onchargingchange','onchargingtimechange','ondischargingtimechange','onlevelchange'].every(k => battery[k] === null)");
        ok(&c, "(() => { try { new BatteryManager(); return false; } catch (e) { return e instanceof TypeError; } })()");
        ok(&c, "['charging','chargingTime','dischargingTime','level','onlevelchange'].every(k => { const get = Object.getOwnPropertyDescriptor(BatteryManager.prototype,k).get; return [{}, BatteryManager.prototype, Object.create(battery), new Proxy(battery,{})].every(x => { try { get.call(x); return false; } catch(e) { return e instanceof TypeError; } }); })");
        ok(&c, "(() => { battery.level = 0; battery.charging = false; battery.onlevelchange = 42; return battery.level === 1 && battery.charging && battery.onlevelchange === null; })()");
        ok(&c, "(() => { try { structuredClone(battery); return false; } catch(e) { return e.name === 'DataCloneError'; } })()");
        s(&c, "globalThis.badReceiver = 0; for (const value of [{}, Object.create(navigator), new Proxy(navigator, {})]) Navigator.prototype.getBattery.call(value).catch(e => { if (e instanceof TypeError) badReceiver++; }); 0");
        pump(&c);
        ok(&c, "badReceiver === 3 && first === navigator.getBattery()");
    }

    #[test]
    fn promise_resolution_can_reenter_get_battery() {
        let c = ctx();
        initial(&c, BatteryState::default());
        ok(&c, "(() => { Object.defineProperty(BatteryManager.prototype, 'then', { configurable: true, get() { globalThis.reentered = navigator.getBattery(); return undefined; } }); const first = navigator.getBattery(); delete BatteryManager.prototype.then; return first === reentered && first === navigator.getBattery(); })()");
        battery(&c);
        ok(&c, "battery.level === 1");
    }

    #[test]
    fn updates_are_context_local_trusted_tasks_and_only_fire_on_change() {
        let a = ctx();
        let b = ctx();
        initial(&a, BatteryState::default());
        initial(&b, BatteryState::default());
        battery(&a);
        battery(&b);
        s(&a, "globalThis.log = []; for (const name of ['chargingchange','chargingtimechange','dischargingtimechange','levelchange']) battery.addEventListener(name, function(e) { log.push(e.type + ':' + e.isTrusted + ':' + (e.target === battery && e.currentTarget === battery && this === battery) + ':' + e.bubbles + ':' + e.cancelable); Promise.resolve().then(() => log.push('micro:' + e.type)); }); battery.onlevelchange = function(e) { log.push('handler:' + (this === battery) + ':' + battery.level); }; 0");
        let dc = BatteryState { charging: false, charging_time: f64::INFINITY, discharging_time: 7200.0, level: 0.75 };
        publish(&a, dc);
        ok(&a, "log.length === 0 && battery.charging && battery.level === 1");
        pump(&a);
        assert_eq!(s(&a, "log.join('|')"), "chargingchange:true:true:false:false|micro:chargingchange|chargingtimechange:true:true:false:false|micro:chargingtimechange|dischargingtimechange:true:true:false:false|micro:dischargingtimechange|levelchange:true:true:false:false|micro:levelchange|handler:true:0.75");
        ok(&a, "battery.charging === false && battery.chargingTime === Infinity && battery.dischargingTime === 7200 && battery.level === 0.75");
        ok(&b, "battery.charging && battery.level === 1 && battery.chargingTime === 0");
        publish(&a, dc);
        pump(&a);
        ok(&a, "log.length === 9");
        // A stalled context gets a bounded latest snapshot; a round trip back
        // to its current state has no visible change and no event.
        publish(&a, BatteryState::default());
        publish(&a, dc);
        pump(&a);
        ok(&a, "log.length === 9");
        s(&a, "globalThis.onceCount = 0; battery.onlevelchange = null; battery.addEventListener('levelchange', () => onceCount++, { once: true }); 0");
        publish(&a, BatteryState { level: 0.5, ..dc });
        pump(&a);
        publish(&a, BatteryState { level: 0.4, ..dc });
        pump(&a);
        ok(&a, "onceCount === 1 && battery.level === 0.4");
    }

    #[test]
    fn asynchronous_initial_snapshot_is_coalesced_rooted_and_fulfilled_atomically() {
        let c = ctx();
        let id = crate::context::id_of(c.raw()).unwrap();
        registry().lock().unwrap().get_mut(&id.0).unwrap().defer_initial = true;
        s(&c, "globalThis.initialLog = []; globalThis.first = navigator.getBattery(); first.then(b => { initialLog.push([b.charging, b.chargingTime, b.dischargingTime, b.level]); }); 0");
        ok(&c, "initialLog.length === 0 && first === navigator.getBattery()");
        let manager = registry().lock().unwrap().get(&id.0).unwrap().manager;
        unsafe {
            define(c.raw(), glue::rjs_global(c.raw()), "pendingBattery", manager, 0);
            glue::rjs_gc(c.raw(), 0);
        }
        s(&c, "globalThis.initialEvents = 0; for (const type of ['chargingchange','chargingtimechange','dischargingtimechange','levelchange']) pendingBattery.addEventListener(type, () => initialEvents++); Object.defineProperty(BatteryManager.prototype, 'then', { configurable: true, get() { globalThis.reentered = navigator.getBattery(); return undefined; } }); 0");
        super::publish(id, BatteryState { charging: false, charging_time: f64::INFINITY, discharging_time: 3600.0, level: 0.2 });
        super::publish(id, BatteryState { charging: false, charging_time: f64::INFINITY, discharging_time: 7200.0, level: 0.5 });
        pump(&c);
        ok(&c, "initialEvents === 0 && initialLog.length === 1 && initialLog[0][0] === false && initialLog[0][1] === Infinity && initialLog[0][2] === 7200 && initialLog[0][3] === 0.5 && reentered === first");
        s(&c, "delete BatteryManager.prototype.then; 0");
        super::publish(id, BatteryState::default());
        pump(&c);
        ok(&c, "initialEvents === 4 && pendingBattery.level === 1");
    }

    #[test]
    fn asynchronous_default_snapshot_still_completes_initial_promise() {
        let c = ctx();
        let id = crate::context::id_of(c.raw()).unwrap();
        registry().lock().unwrap().get_mut(&id.0).unwrap().defer_initial = true;
        s(&c, "globalThis.initialDone = false; navigator.getBattery().then(b => { initialDone = b.charging && b.level === 1 && b.chargingTime === 0 && b.dischargingTime === Infinity; }); 0");
        super::publish(id, BatteryState { level: 0.2, ..BatteryState::default() });
        // Returning to construction defaults must retain initial completion.
        super::publish(id, BatteryState::default());
        pump(&c);
        ok(&c, "initialDone");
    }

    #[test]
    fn closing_during_acquisition_discards_late_completion() {
        let c = ctx();
        let id = crate::context::id_of(c.raw()).unwrap();
        registry().lock().unwrap().get_mut(&id.0).unwrap().defer_initial = true;
        s(&c, "navigator.getBattery(); 0");
        assert!(is_cell(registry().lock().unwrap().get(&id.0).unwrap().resolve));
        crate::context::forget(c.raw());
        super::publish(id, BatteryState { level: 0.1, ..BatteryState::default() });
        assert!(!registry().lock().unwrap().contains_key(&id.0));
    }

    #[cfg(windows)]
    #[test]
    fn windows_native_provider_snapshot_and_cleanup() {
        let c = ctx();
        // Deliberately no injected initial value: exercise real Windows calls.
        // These invariants also hold on desktops with no physical battery.
        battery(&c);
        ok(&c, "typeof battery.charging === 'boolean' && battery.level >= 0 && battery.level <= 1 && (battery.chargingTime === 0 || battery.chargingTime === Infinity) && battery.dischargingTime >= 0");
        let id = crate::context::id_of(c.raw()).unwrap();
        let registrations = registry()
            .lock()
            .unwrap()
            .get(&id.0)
            .unwrap()
            .subscription
            .as_ref()
            .expect("native subscription initialized")
            .registration_count();
        eprintln!("[redjs battery smoke] registered {registrations}/2 Windows power settings");
        crate::context::forget(c.raw());
        assert!(!registry().lock().unwrap().contains_key(&id.0));
        refresh(id);
        assert!(!registry().lock().unwrap().contains_key(&id.0));
    }

    #[test]
    fn cleanup_discards_pending_state_and_old_context_id() {
        let c = ctx();
        initial(&c, BatteryState::default());
        battery(&c);
        let id = crate::context::id_of(c.raw()).unwrap();
        publish(&c, BatteryState { level: 0.2, ..BatteryState::default() });
        crate::context::forget(c.raw());
        assert!(!registry().lock().unwrap().contains_key(&id.0));
        #[cfg(windows)]
        refresh(id); // A late native callback does not resurrect the entry.
        assert!(!registry().lock().unwrap().contains_key(&id.0));
    }
}
