//! IOPowerSources uses capacity ratios and minutes for per-source charge ETA;
//! IOPSGetTimeRemainingEstimate supplies the unified discharge ETA in seconds.
//! https://github.com/apple-oss-distributions/IOKitUser/tree/main/ps.subproj

use super::BatteryState;

#[derive(Clone, Copy, Debug, PartialEq)]
enum SourceKind {
    Internal,
    Ups,
    Other,
}
#[derive(Clone, Copy, Debug, PartialEq)]
enum Power {
    Ac,
    Battery,
    Offline,
    Unknown,
}

#[derive(Clone, Copy, Debug)]
struct PowerSource {
    kind: SourceKind,
    present: bool,
    power: Power,
    charging: bool,
    charged: bool,
    current: Option<i64>,
    maximum: Option<i64>,
    minutes_to_full: Option<i64>,
}

fn aggregate(sources: &[PowerSource], remaining_seconds: f64) -> BatteryState {
    let available = |s: &&PowerSource| s.present && s.power != Power::Offline;
    // An external UPS and an internal pack may report different capacity
    // units. Prefer internal packs; use UPS devices when there is no pack.
    let kind = if sources.iter().filter(available).any(|s| s.kind == SourceKind::Internal) {
        SourceKind::Internal
    } else {
        SourceKind::Ups
    };
    let batteries: Vec<_> = sources.iter().filter(available).filter(|s| s.kind == kind).collect();
    if batteries.is_empty() {
        return BatteryState::default();
    }
    let full = |s: &PowerSource| s.charged || matches!((s.current, s.maximum), (Some(c), Some(m)) if m > 0 && c >= m);
    let all_full = batteries.iter().copied().all(full);
    let all_empty = batteries.iter().all(|s| s.current == Some(0) && s.maximum.is_some_and(|m| m > 0));
    let charging = batteries.iter().any(|s| s.charging || s.power != Power::Battery);
    let capacities: Option<Vec<_>> = batteries
        .iter()
        .map(|s| match (s.current, s.maximum) {
            (Some(c), Some(m)) if c >= 0 && m > 0 => Some((c.min(m) as f64, m as f64)),
            _ => None,
        })
        .collect();
    let level = if all_full {
        1.0
    } else if let Some(capacities) = capacities {
        let current: f64 = capacities.iter().map(|s| s.0).sum();
        let maximum: f64 = capacities.iter().map(|s| s.1).sum();
        ((current / maximum).clamp(0.0, 1.0) * 100.0).round() / 100.0
    } else {
        1.0
    };
    let charging_time = if all_full {
        0.0
    } else if charging {
        batteries
            .iter()
            .copied()
            .map(|s| {
                if full(s) {
                    0.0
                } else if s.charging {
                    s.minutes_to_full.filter(|t| *t >= 0).map(|t| t as f64 * 60.0).unwrap_or(f64::INFINITY)
                } else {
                    f64::INFINITY
                }
            })
            .fold(0.0, f64::max)
    } else {
        f64::INFINITY
    };
    let discharging_time = if charging {
        f64::INFINITY
    } else if all_empty {
        0.0
    } else if remaining_seconds.is_finite() && remaining_seconds >= 0.0 {
        remaining_seconds
    } else {
        f64::INFINITY
    };
    BatteryState { charging, charging_time, discharging_time, level }
}

#[cfg(target_os = "macos")]
pub(super) mod native {
    use super::super::publish;
    use super::*;
    use crate::context::ContextId;
    use std::{
        ffi::{c_char, c_void, CStr},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
    };

    type Cf = *const c_void;
    type RunLoop = *mut c_void;
    type Source = *mut c_void;
    const UTF8: u32 = 0x0800_0100;
    const SINT64: i32 = 4;

    #[repr(C)]
    struct SourceContext {
        version: isize,
        info: *mut c_void,
        retain: Option<unsafe extern "C" fn(*const c_void) -> *const c_void>,
        release: Option<unsafe extern "C" fn(*const c_void)>,
        copy_description: Option<unsafe extern "C" fn(*const c_void) -> Cf>,
        equal: Option<unsafe extern "C" fn(*const c_void, *const c_void) -> u8>,
        hash: Option<unsafe extern "C" fn(*const c_void) -> usize>,
        schedule: Option<unsafe extern "C" fn(*mut c_void, RunLoop, Cf)>,
        cancel: Option<unsafe extern "C" fn(*mut c_void, RunLoop, Cf)>,
        perform: Option<unsafe extern "C" fn(*mut c_void)>,
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(value: Cf);
        fn CFGetTypeID(value: Cf) -> usize;
        fn CFEqual(a: Cf, b: Cf) -> u8;
        fn CFArrayGetCount(array: Cf) -> isize;
        fn CFArrayGetValueAtIndex(array: Cf, index: isize) -> Cf;
        fn CFDictionaryGetValue(dict: Cf, key: Cf) -> Cf;
        fn CFStringCreateWithCString(allocator: Cf, bytes: *const c_char, encoding: u32) -> Cf;
        fn CFStringGetTypeID() -> usize;
        fn CFNumberGetTypeID() -> usize;
        fn CFNumberGetValue(number: Cf, kind: i32, value: *mut c_void) -> u8;
        fn CFBooleanGetTypeID() -> usize;
        fn CFBooleanGetValue(value: Cf) -> u8;
        fn CFRunLoopGetCurrent() -> RunLoop;
        fn CFRunLoopAddSource(run_loop: RunLoop, source: Source, mode: Cf);
        fn CFRunLoopRemoveSource(run_loop: RunLoop, source: Source, mode: Cf);
        fn CFRunLoopSourceCreate(allocator: Cf, order: isize, context: *mut SourceContext) -> Source;
        fn CFRunLoopSourceSignal(source: Source);
        fn CFRunLoopSourceInvalidate(source: Source);
        fn CFRunLoopWakeUp(run_loop: RunLoop);
        fn CFRunLoopStop(run_loop: RunLoop);
        fn CFRunLoopRun();
        static kCFRunLoopDefaultMode: Cf;
    }
    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPSCopyPowerSourcesInfo() -> Cf;
        fn IOPSCopyPowerSourcesList(info: Cf) -> Cf;
        fn IOPSGetPowerSourceDescription(info: Cf, source: Cf) -> Cf;
        fn IOPSGetTimeRemainingEstimate() -> f64;
        fn IOPSNotificationCreateRunLoopSource(callback: unsafe extern "C" fn(*mut c_void), context: *mut c_void) -> Source;
    }

    struct Owned(Cf);
    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe {
                if !self.0.is_null() {
                    CFRelease(self.0);
                }
            }
        }
    }
    unsafe fn string(bytes: &CStr) -> Owned {
        Owned(CFStringCreateWithCString(core::ptr::null(), bytes.as_ptr(), UTF8))
    }
    unsafe fn value(dict: Cf, key: &CStr) -> Cf {
        let key = string(key);
        if key.0.is_null() {
            core::ptr::null()
        } else {
            CFDictionaryGetValue(dict, key.0)
        }
    }
    unsafe fn boolean(dict: Cf, key: &CStr) -> bool {
        let v = value(dict, key);
        !v.is_null() && CFGetTypeID(v) == CFBooleanGetTypeID() && CFBooleanGetValue(v) != 0
    }
    unsafe fn number(dict: Cf, key: &CStr) -> Option<i64> {
        let v = value(dict, key);
        if v.is_null() || CFGetTypeID(v) != CFNumberGetTypeID() {
            return None;
        }
        let mut result = 0i64;
        (CFNumberGetValue(v, SINT64, &mut result as *mut _ as *mut c_void) != 0).then_some(result)
    }
    unsafe fn string_is(dict: Cf, key: &CStr, expected: &CStr) -> bool {
        let v = value(dict, key);
        if v.is_null() || CFGetTypeID(v) != CFStringGetTypeID() {
            return false;
        }
        let expected = string(expected);
        !expected.0.is_null() && CFEqual(v, expected.0) != 0
    }

    pub(crate) const ASYNC_INITIAL: bool = true;
    pub(crate) fn read() -> BatteryState {
        BatteryState::default()
    }
    unsafe fn snapshot() -> BatteryState {
        let info = Owned(IOPSCopyPowerSourcesInfo());
        if info.0.is_null() {
            return BatteryState::default();
        }
        let list = Owned(IOPSCopyPowerSourcesList(info.0));
        if list.0.is_null() {
            return BatteryState::default();
        }
        let mut sources = Vec::new();
        for index in 0..CFArrayGetCount(list.0) {
            let dict = IOPSGetPowerSourceDescription(info.0, CFArrayGetValueAtIndex(list.0, index));
            if dict.is_null() {
                continue;
            }
            let kind = if string_is(dict, c"Type", c"InternalBattery") {
                SourceKind::Internal
            } else if string_is(dict, c"Type", c"UPS") {
                SourceKind::Ups
            } else {
                SourceKind::Other
            };
            let power = if string_is(dict, c"Power Source State", c"AC Power") {
                Power::Ac
            } else if string_is(dict, c"Power Source State", c"Battery Power") {
                Power::Battery
            } else if string_is(dict, c"Power Source State", c"Off Line") {
                Power::Offline
            } else {
                Power::Unknown
            };
            sources.push(PowerSource {
                kind,
                power,
                present: boolean(dict, c"Is Present"),
                charging: boolean(dict, c"Is Charging"),
                charged: boolean(dict, c"Is Charged"),
                current: number(dict, c"Current Capacity"),
                maximum: number(dict, c"Max Capacity"),
                minutes_to_full: number(dict, c"Time to Full Charge"),
            });
        }
        aggregate(&sources, IOPSGetTimeRemainingEstimate())
    }

    #[derive(Clone, Copy)]
    struct Handles {
        run_loop: usize,
        stop_source: usize,
    }
    #[derive(Default)]
    struct Shared {
        stopped: AtomicBool,
        handles: Mutex<Option<Handles>>,
    }
    pub(crate) struct Subscription {
        shared: Arc<Shared>,
    }
    impl Drop for Subscription {
        fn drop(&mut self) {
            self.shared.stopped.store(true, Ordering::Release);
            // Signal rather than only CFRunLoopStop: a signal queued before
            // CFRunLoopRun starts is retained, so startup cancellation is safe.
            // Worker removal / release uses this same lock to prevent stale refs.
            if let Some(h) = *self.shared.handles.lock().unwrap() {
                unsafe {
                    CFRunLoopSourceSignal(h.stop_source as Source);
                    CFRunLoopWakeUp(h.run_loop as RunLoop);
                }
            }
            // No join: source setup / teardown must never block the JS thread.
        }
    }
    unsafe extern "C" fn changed(context: *mut c_void) {
        publish(ContextId(context as usize as u64), snapshot());
    }
    unsafe extern "C" fn stop(context: *mut c_void) {
        CFRunLoopStop(context as RunLoop);
    }

    unsafe fn monitor(id: ContextId, shared: &Shared) {
        let run_loop = CFRunLoopGetCurrent();
        let changes = Owned(IOPSNotificationCreateRunLoopSource(changed, id.0 as usize as *mut c_void));
        let mut context = SourceContext {
            version: 0,
            info: run_loop,
            retain: None,
            release: None,
            copy_description: None,
            equal: None,
            hash: None,
            schedule: None,
            cancel: None,
            perform: Some(stop),
        };
        let shutdown = Owned(CFRunLoopSourceCreate(core::ptr::null(), 0, &mut context));
        if changes.0.is_null() || shutdown.0.is_null() {
            eprintln!("[redjs] battery: IOPowerSources notification source unavailable");
            publish(id, snapshot());
            return;
        }
        CFRunLoopAddSource(run_loop, changes.0 as Source, kCFRunLoopDefaultMode);
        CFRunLoopAddSource(run_loop, shutdown.0 as Source, kCFRunLoopDefaultMode);
        let cancelled = {
            let mut handles = shared.handles.lock().unwrap();
            if shared.stopped.load(Ordering::Acquire) {
                true
            } else {
                *handles = Some(Handles { run_loop: run_loop as usize, stop_source: shutdown.0 as usize });
                false
            }
        };
        if cancelled {
            // Invalidation can enter native cleanup. Release the handles mutex
            // first so Subscription::drop never waits for this work.
            CFRunLoopSourceInvalidate(changes.0 as Source);
            CFRunLoopSourceInvalidate(shutdown.0 as Source);
            return;
        }
        publish(id, snapshot());
        CFRunLoopRun();
        // Clear under the lock before invalidating / releasing native objects.
        shared.handles.lock().unwrap().take();
        CFRunLoopSourceInvalidate(changes.0 as Source);
        CFRunLoopSourceInvalidate(shutdown.0 as Source);
        CFRunLoopRemoveSource(run_loop, changes.0 as Source, kCFRunLoopDefaultMode);
        CFRunLoopRemoveSource(run_loop, shutdown.0 as Source, kCFRunLoopDefaultMode);
    }

    pub(crate) fn subscribe(id: ContextId) -> Subscription {
        let shared = Arc::new(Shared::default());
        let worker = shared.clone();
        if let Err(error) =
            std::thread::Builder::new().name("redjs-battery-iokit".into()).spawn(move || unsafe { monitor(id, &worker) })
        {
            eprintln!("[redjs] battery: could not start IOPowerSources subscription ({error})");
            publish(id, BatteryState::default());
        }
        Subscription { shared }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source() -> PowerSource {
        PowerSource {
            kind: SourceKind::Internal,
            present: true,
            power: Power::Battery,
            charging: false,
            charged: false,
            current: Some(30),
            maximum: Some(100),
            minutes_to_full: Some(-1),
        }
    }
    #[test]
    fn iokit_capacity_and_time_units() {
        assert_eq!(
            aggregate(&[source()], 3600.0),
            BatteryState { charging: false, charging_time: f64::INFINITY, discharging_time: 3600.0, level: 0.3 }
        );
        let ac = PowerSource { power: Power::Ac, charging: true, minutes_to_full: Some(25), ..source() };
        assert_eq!(
            aggregate(&[ac], -2.0),
            BatteryState { charging: true, charging_time: 1500.0, discharging_time: f64::INFINITY, level: 0.3 }
        );
        assert!(aggregate(&[PowerSource { charging: false, ..ac }], -2.0).charging_time.is_infinite());
        assert_eq!(aggregate(&[PowerSource { charged: true, ..ac }], -2.0), BatteryState::default());
    }
    #[test]
    fn iokit_multiple_packs_and_ups_selection() {
        let a = PowerSource { current: Some(20), ..source() };
        let b = PowerSource { current: Some(80), ..source() };
        assert_eq!(aggregate(&[a, b], 7200.0).level, 0.5);
        let ups = PowerSource { kind: SourceKind::Ups, current: Some(99), ..source() };
        assert_eq!(aggregate(&[a, ups], 7200.0).level, 0.2);
        assert_eq!(aggregate(&[ups], 7200.0).level, 0.99);
        let ac = PowerSource { power: Power::Ac, charging: true, minutes_to_full: Some(10), ..a };
        let bc = PowerSource { power: Power::Ac, charging: true, minutes_to_full: Some(20), ..b };
        assert_eq!(aggregate(&[ac, bc], -2.0).charging_time, 1200.0);
    }
    #[test]
    fn iokit_missing_sources_and_unknown_values() {
        assert_eq!(aggregate(&[], -1.0), BatteryState::default());
        for s in [
            PowerSource { present: false, ..source() },
            PowerSource { kind: SourceKind::Other, ..source() },
            PowerSource { power: Power::Offline, ..source() },
        ] {
            assert_eq!(aggregate(&[s], -1.0), BatteryState::default());
        }
        let unknown = aggregate(&[PowerSource { current: None, maximum: Some(0), ..source() }], -1.0);
        assert!(unknown.level == 1.0 && unknown.discharging_time.is_infinite());
        assert_eq!(aggregate(&[PowerSource { current: Some(0), ..source() }], -1.0).discharging_time, 0.0);
        assert!(aggregate(&[PowerSource { current: Some(1), maximum: Some(1000), ..source() }], -1.0)
            .discharging_time
            .is_infinite());
        let s = PowerSource { power: Power::Unknown, charging: true, minutes_to_full: Some(-1), ..source() };
        assert!(aggregate(&[s], f64::NAN).charging && aggregate(&[s], f64::NAN).charging_time.is_infinite());
    }
}
