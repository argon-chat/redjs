//! Native web-scheduling core: `performance.now()`, requestAnimationFrame and
//! setTimeout/setInterval implemented INSIDE redjs, pumped by the host once
//! per frame with a single FFI call.
//!
//! Why native: these are the hottest engine globals (`performance.now()` is
//! called constantly by game/profiling code, rAF once per frame per system).
//! As `extern "sysv64"` Rust host functions they cost ~2 ns from JS — no
//! trampoline slot, no reverse-P/Invoke. And everything shares ONE monotonic
//! clock: rAF timestamps, timer deadlines, `performance.now()` and the value
//! the host reads via `redjs_now` are the same time base (the current player
//! has three different clocks for these).
//!
//! Semantics mirror the player's TimerScheduler/AnimationFrameScheduler
//! (which mirror the browser where it matters):
//! - a pump snapshots due timers first; callbacks registered during the drain
//!   fire on the NEXT pump (no same-tick reschedule loops);
//! - intervals reschedule from `now + interval` (min 1 ms), one-shots die;
//! - a timer cleared mid-drain by an earlier callback does not fire;
//! - rAF callbacks are moved out before invoking; re-registration lands in
//!   the next frame's batch; each gets the pump's `now` as DOMHighResTimeStamp;
//! - callback exceptions go to the error funnel (`errors::report`, kind
//!   `PumpCallback`) and never abort the pump.
//!
//! Threading: schedule/cancel host fns run on the JS thread (inside the VM);
//! `redjs_pump` must be called from that same thread. The registry mutex is
//! never held across a JS callback.

use core::ffi::c_void;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::Instant;

use crate::capi;
use crate::glue;

// ---------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------

struct Clock {
    origin: Instant,
    /// Unix epoch ms at origin (performance.timeOrigin).
    epoch_origin_ms: f64,
}

fn clock() -> &'static Clock {
    static CLOCK: OnceLock<Clock> = OnceLock::new();
    CLOCK.get_or_init(|| Clock {
        origin: Instant::now(),
        epoch_origin_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0),
    })
}

pub fn now_ms() -> f64 {
    clock().origin.elapsed().as_secs_f64() * 1000.0
}

// ---------------------------------------------------------------------------
// Per-context scheduler state
// ---------------------------------------------------------------------------

struct Timer {
    cb: u64,
    /// Extra `setTimeout(cb, ms, ...args)` arguments (each protected).
    args: Vec<u64>,
    next_fire: f64,
    interval: f64,
    repeating: bool,
}

type NativeTask = Box<dyn FnOnce(*mut c_void) + Send>;

#[derive(Default)]
struct Scheduler {
    raf: Vec<(u32, u64)>, // (id, protected callback) in registration order
    raf_next_id: u32,
    timers: HashMap<u32, Timer>,
    timer_next_id: u32,
    /// requestIdleCallback queue (fired after the rAF batch each pump).
    idle: Vec<(u32, u64)>,
    idle_next_id: u32,
    /// Native one-shot tasks (async semantics for native APIs: image loads…).
    native: Vec<(f64, NativeTask)>,
}

/// Schedules a native task on this context's pump (spec-async operations of
/// the native web API: Image.src loading etc). Runs on the pump thread.
pub(crate) fn schedule_native(g: *mut c_void, delay_ms: f64, task: NativeTask) {
    let deadline = now_ms() + delay_ms.max(0.0);
    let mut reg = registry().lock().unwrap();
    reg.entry(g as usize).or_default().native.push((deadline, task));
}

fn registry() -> &'static Mutex<HashMap<usize, Scheduler>> {
    static REG: OnceLock<Mutex<HashMap<usize, Scheduler>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `crate::context` fan-out. The timers, rAF/idle callbacks and native tasks
/// of a context that is gone are DROPPED, never run: their callbacks are
/// protected values in a heap that no longer exists.
pub(crate) fn forget(gone: crate::context::Gone) {
    let dropped = registry().lock().unwrap().remove(&gone.key);
    drop(dropped); // outside the lock: native tasks may own Arcs with Drop impls
}

#[cfg(test)]
pub(crate) fn has_scheduler(g: *mut c_void) -> bool {
    registry().lock().unwrap().contains_key(&(g as usize))
}

// ---------------------------------------------------------------------------
// Host functions (SysV — direct JSC host calling convention)
// ---------------------------------------------------------------------------

#[inline]
unsafe fn cf_argc(cf: *mut c_void) -> u32 {
    (*((cf as usize + 32) as *const u32)).saturating_sub(1)
}

#[inline]
unsafe fn cf_arg(cf: *mut c_void, i: u32) -> u64 {
    if i < cf_argc(cf) {
        *((cf as usize + 48 + (i as usize) * 8) as *const u64)
    } else {
        ENC_UNDEFINED
    }
}

const ENC_UNDEFINED: u64 = 0xa;
const TAG_NUMBER: u64 = 0xfffe_0000_0000_0000;

#[inline]
fn enc_int32(i: i32) -> u64 {
    TAG_NUMBER | (i as u32 as u64)
}

#[inline]
fn enc_double(d: f64) -> u64 {
    let i = d as i32;
    if i as f64 == d && !(i == 0 && d.is_sign_negative()) {
        return enc_int32(i);
    }
    (d.to_bits() as i64).wrapping_add((1i64) << 49) as u64
}

#[inline]
fn dec_number(v: u64) -> Option<f64> {
    if (v & TAG_NUMBER) == TAG_NUMBER {
        return Some(v as u32 as i32 as f64);
    }
    if (v & TAG_NUMBER) != 0 {
        return Some(f64::from_bits((v as i64).wrapping_sub((1i64) << 49) as u64));
    }
    None
}

#[inline]
fn is_cell(v: u64) -> bool {
    v != 0 && (v & (TAG_NUMBER | 0x2)) == 0
}

unsafe extern "sysv64" fn perf_now(_g: *mut c_void, _cf: *mut c_void) -> u64 {
    enc_double(now_ms())
}

unsafe extern "sysv64" fn raf_request(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cb = cf_arg(cf, 0);
    if !is_cell(cb) {
        return enc_int32(0);
    }
    capi::JSValueProtect(g, cb as usize as capi::JSValueRef);
    let mut reg = registry().lock().unwrap();
    let s = reg.entry(g as usize).or_default();
    s.raf_next_id += 1;
    let id = s.raf_next_id;
    s.raf.push((id, cb));
    enc_int32(id as i32)
}

unsafe extern "sysv64" fn raf_cancel(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Some(id) = dec_number(cf_arg(cf, 0)) {
        let removed = {
            let mut reg = registry().lock().unwrap();
            let s = reg.entry(g as usize).or_default();
            match s.raf.iter().position(|(i, _)| *i == id as u32) {
                Some(pos) => Some(s.raf.remove(pos).1),
                None => None,
            }
        };
        if let Some(cb) = removed {
            capi::JSValueUnprotect(g, cb as usize as capi::JSValueRef);
        }
    }
    ENC_UNDEFINED
}

unsafe fn timer_set(g: *mut c_void, cf: *mut c_void, repeating: bool) -> u64 {
    timer_set_inner(g, cf, repeating, None, 2)
}

/// `delay_override`: fixed delay ignoring arg 1 (setImmediate). `args_from`:
/// first callback-argument index (2 for setTimeout, 1 for setImmediate).
unsafe fn timer_set_inner(g: *mut c_void, cf: *mut c_void, repeating: bool, delay_override: Option<f64>, args_from: u32) -> u64 {
    let cb = cf_arg(cf, 0);
    if !is_cell(cb) {
        return enc_int32(0);
    }
    let delay = delay_override.unwrap_or_else(|| dec_number(cf_arg(cf, 1)).unwrap_or(0.0).max(0.0));
    let interval = if repeating { delay.max(1.0) } else { delay };
    // Spec: trailing arguments are passed to the callback on every fire.
    let args: Vec<u64> = (args_from..cf_argc(cf)).map(|i| cf_arg(cf, i)).collect();
    capi::JSValueProtect(g, cb as usize as capi::JSValueRef);
    for &a in &args {
        if is_cell(a) {
            capi::JSValueProtect(g, a as usize as capi::JSValueRef);
        }
    }
    let now = now_ms();
    let mut reg = registry().lock().unwrap();
    let s = reg.entry(g as usize).or_default();
    s.timer_next_id += 1;
    let id = s.timer_next_id;
    s.timers.insert(id, Timer { cb, args, next_fire: now + delay, interval, repeating });
    enc_int32(id as i32)
}

unsafe extern "sysv64" fn timer_set_timeout(g: *mut c_void, cf: *mut c_void) -> u64 {
    timer_set(g, cf, false)
}

unsafe extern "sysv64" fn timer_set_interval(g: *mut c_void, cf: *mut c_void) -> u64 {
    timer_set(g, cf, true)
}

/// `setImmediate(cb, ...args)` — a 0-delay macrotask (fires on the next pump,
/// before that pump's rAF batch). Shares the timer registry, so
/// `clearImmediate` is just `clearTimeout`.
unsafe extern "sysv64" fn immediate_set(g: *mut c_void, cf: *mut c_void) -> u64 {
    timer_set_inner(g, cf, false, Some(0.0), 1)
}

/// `queueMicrotask(cb)` — the VM's real microtask queue.
unsafe extern "sysv64" fn queue_microtask(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cb = cf_arg(cf, 0);
    if glue::rjs_queue_microtask(g, cb) != 0 {
        let m: Vec<u16> = "queueMicrotask: argument must be callable".encode_utf16().collect();
        let mut err = ENC_UNDEFINED;
        glue::rjs_error_create(g, 1, m.as_ptr(), m.len(), &mut err);
        glue::rjs_throw(g, err);
    }
    ENC_UNDEFINED
}

unsafe extern "sysv64" fn idle_request(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cb = cf_arg(cf, 0);
    if !is_cell(cb) {
        return enc_int32(0);
    }
    capi::JSValueProtect(g, cb as usize as capi::JSValueRef);
    let mut reg = registry().lock().unwrap();
    let s = reg.entry(g as usize).or_default();
    s.idle_next_id += 1;
    let id = s.idle_next_id;
    s.idle.push((id, cb));
    enc_int32(id as i32)
}

unsafe extern "sysv64" fn idle_cancel(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Some(id) = dec_number(cf_arg(cf, 0)) {
        let removed = {
            let mut reg = registry().lock().unwrap();
            let s = reg.entry(g as usize).or_default();
            match s.idle.iter().position(|(i, _)| *i == id as u32) {
                Some(pos) => Some(s.idle.remove(pos).1),
                None => None,
            }
        };
        if let Some(cb) = removed {
            capi::JSValueUnprotect(g, cb as usize as capi::JSValueRef);
        }
    }
    ENC_UNDEFINED
}

/// IdleDeadline.timeRemaining(): a real decreasing budget so hydrate-style
/// `while (deadline.timeRemaining() > 0)` loops make progress AND terminate.
unsafe extern "sysv64" fn idle_time_remaining(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = *((cf as usize + 40) as *const u64);
    let a = glue::rjs_atom8(g, "__t0".as_ptr(), 4);
    let mut t0 = ENC_UNDEFINED;
    glue::rjs_get(g, this, a, &mut t0);
    glue::rjs_atom_free(g, a);
    let t0 = dec_number(t0).unwrap_or(0.0);
    enc_double((4.0 - (now_ms() - t0)).max(0.0))
}

unsafe extern "sysv64" fn timer_clear(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Some(id) = dec_number(cf_arg(cf, 0)) {
        let removed = {
            let mut reg = registry().lock().unwrap();
            let s = reg.entry(g as usize).or_default();
            s.timers.remove(&(id as u32))
        };
        if let Some(t) = removed {
            unprotect_timer(g, &t);
        }
    }
    ENC_UNDEFINED
}

unsafe fn unprotect_timer(g: *mut c_void, t: &Timer) {
    capi::JSValueUnprotect(g, t.cb as usize as capi::JSValueRef);
    for &a in &t.args {
        if is_cell(a) {
            capi::JSValueUnprotect(g, a as usize as capi::JSValueRef);
        }
    }
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

unsafe fn define_native(g: *mut c_void, target: u64, name: &str, arity: u32, f: *const c_void) -> i32 {
    let name16: Vec<u16> = name.encode_utf16().collect();
    let mut func = 0u64;
    let rc = glue::rjs_function_create(g, name16.as_ptr(), name16.len(), arity, f, &mut func);
    if rc != 0 {
        return rc;
    }
    let atom = glue::rjs_atom8(g, name.as_ptr(), name.len());
    let rc = glue::rjs_define(g, target, atom, func, 0);
    glue::rjs_atom_free(g, atom);
    rc
}

/// Installs `performance` (now/timeOrigin), requestAnimationFrame /
/// cancelAnimationFrame, setTimeout/clearTimeout/setInterval/clearInterval
/// as NATIVE globals sharing one clock. Idempotent per context.
pub unsafe fn install(g: *mut c_void) -> i32 {
    // First: if this address last belonged to another context, its timers are
    // purged here instead of being fired into this one (crate::context).
    crate::context::ensure(g);
    let _ = clock(); // pin the time origin before anything reads it
    registry().lock().unwrap().entry(g as usize).or_default();

    let global = glue::rjs_global(g);

    // performance = { now(), timeOrigin }
    let mut perf = 0u64;
    let rc = glue::rjs_object_create(g, ENC_UNDEFINED, &mut perf);
    if rc != 0 {
        return rc;
    }
    let rc = define_native(g, perf, "now", 0, perf_now as *const c_void);
    if rc != 0 {
        return rc;
    }
    let atom = glue::rjs_atom8(g, "timeOrigin".as_ptr(), "timeOrigin".len());
    glue::rjs_define(g, perf, atom, enc_double(clock().epoch_origin_ms), 0);
    glue::rjs_atom_free(g, atom);
    let atom = glue::rjs_atom8(g, "performance".as_ptr(), "performance".len());
    let rc = glue::rjs_define(g, global, atom, perf, 0);
    glue::rjs_atom_free(g, atom);
    if rc != 0 {
        return rc;
    }

    let fns: [(&str, u32, *const c_void); 11] = [
        ("requestAnimationFrame", 1, raf_request as *const c_void),
        ("cancelAnimationFrame", 1, raf_cancel as *const c_void),
        ("setTimeout", 2, timer_set_timeout as *const c_void),
        ("setInterval", 2, timer_set_interval as *const c_void),
        ("clearTimeout", 1, timer_clear as *const c_void),
        ("clearInterval", 1, timer_clear as *const c_void),
        ("setImmediate", 1, immediate_set as *const c_void),
        ("clearImmediate", 1, timer_clear as *const c_void),
        ("queueMicrotask", 1, queue_microtask as *const c_void),
        ("requestIdleCallback", 1, idle_request as *const c_void),
        ("cancelIdleCallback", 1, idle_cancel as *const c_void),
    ];
    for (name, arity, f) in fns {
        let rc = define_native(g, global, name, arity, f);
        if rc != 0 {
            return rc;
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Pump
// ---------------------------------------------------------------------------

pub const PUMP_TIMERS: u32 = 1;
pub const PUMP_RAF: u32 = 2;
pub const PUMP_MICROTASKS: u32 = 4;
/// A microtask checkpoint after EVERY task the pump runs (each timer, rAF and
/// idle callback, each native task, each module pump's delivery batch), the
/// way the HTML event loop does it — instead of one drain at the end of the
/// pump. OFF for vue-native's window today: turning it on there changes Vue's
/// flush timing runtime-wide, which is its own decision (m25 indexeddb-design
/// §4.1 IDB-D1, workers-design §3.2). A worker agent's loop always sets it.
pub const PUMP_CHECKPOINT_PER_TASK: u32 = 8;

pub use crate::ffi::RjsPumpStats;

unsafe fn fire(g: *mut c_void, cb: u64, args: &[u64], flags: u32) {
    let mut out = 0u64;
    let rc = glue::rjs_call(g, cb, ENC_UNDEFINED, args.as_ptr(), args.len() as u32, &mut out);
    if rc == 1 {
        crate::errors::report(g, out, crate::errors::ErrKind::PumpCallback);
    }
    if flags & PUMP_CHECKPOINT_PER_TASK != 0 {
        checkpoint(g);
    }
}

// ---------------------------------------------------------------------------
// The microtask checkpoint
// ---------------------------------------------------------------------------

/// Runs after every checkpoint that drained with no JS on the stack, with the
/// context it ran for. The seam IndexedDB's "cleanup Indexed Database
/// transactions" step hangs on (HTML §8.1.7.3 step 6). Must not run user JS:
/// it runs at the END of a checkpoint, and a microtask it queued would wait
/// for the next one.
pub type CheckpointHook = unsafe fn(*mut c_void);

static ANY_CHECKPOINT_HOOK: AtomicBool = AtomicBool::new(false);

fn checkpoint_hooks() -> &'static RwLock<Vec<CheckpointHook>> {
    static H: OnceLock<RwLock<Vec<CheckpointHook>>> = OnceLock::new();
    H.get_or_init(|| RwLock::new(Vec::new()))
}

/// Registers a post-checkpoint hook, process-wide and for good (modules
/// register at install; registering the same fn twice is a no-op).
pub fn on_checkpoint(f: CheckpointHook) {
    let mut hooks = checkpoint_hooks().write().unwrap();
    if !hooks.iter().any(|h| *h as usize == f as usize) {
        hooks.push(f);
    }
    ANY_CHECKPOINT_HOOK.store(true, Ordering::Release);
}

thread_local! {
    /// HTML's per-event-loop "performing a microtask checkpoint" flag, kept per
    /// context: the contexts this thread is checkpointing right now.
    static CHECKPOINTING: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// What one [`checkpoint`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checkpoint {
    /// Microtasks drained and the post-checkpoint hooks ran.
    Full,
    /// Microtasks drained, hooks NOT run: JavaScript is on the stack (a host
    /// function asked for a drain mid-script). Running them there would, for
    /// example, deactivate an IndexedDB transaction the calling script created a
    /// moment ago — stricter than any browser.
    DrainedInJs,
    /// Nothing at all: this context is already performing a checkpoint further
    /// up the stack (HTML: "if performing a microtask checkpoint is true,
    /// return"). The outer drain runs whatever is still queued.
    Nested,
}

/// THE microtask checkpoint — the single entry point every drain in redjs goes
/// through: the end of [`pump`], [`crate::api::Context::drain_microtasks`], the
/// `redjs_drain_microtasks` export (so vue-native's own drains and a C# host's
/// count), and an event dispatcher's between-listeners step. Nothing calls
/// `rjs_drain_microtasks` directly any more.
///
/// = drain the VM's microtask queue (and JSC's deferred engine work), then the
/// [`on_checkpoint`] hooks — the hooks only when no JS is on the stack.
///
/// One drain this function does not see: JSC drains by itself whenever the
/// OUTERMOST API lock is released (`JSLock::willReleaseLock`), i.e. after any
/// top-level call made without the lock held. Those drains run no hooks. An
/// embedder that holds the lock across its frame — vue-native's pump holds
/// `redjs_lock`, a worker agent's loop will too — routes every drain here.
///
/// # Safety
/// `g` must be a live context, on its VM thread.
pub unsafe fn checkpoint(g: *mut c_void) -> Checkpoint {
    let key = g as usize;
    let nested = CHECKPOINTING.with(|c| {
        let mut c = c.borrow_mut();
        if c.contains(&key) {
            true
        } else {
            c.push(key);
            false
        }
    });
    if nested {
        return Checkpoint::Nested;
    }
    glue::rjs_drain_microtasks(g);
    let full = glue::rjs_in_js(g) == 0;
    if full && ANY_CHECKPOINT_HOOK.load(Ordering::Acquire) {
        let hooks: Vec<CheckpointHook> = checkpoint_hooks().read().unwrap().clone();
        for h in hooks {
            h(g);
        }
    }
    CHECKPOINTING.with(|c| c.borrow_mut().retain(|&k| k != key));
    if full { Checkpoint::Full } else { Checkpoint::DrainedInJs }
}

pub unsafe fn pump(g: *mut c_void, flags: u32, stats: *mut RjsPumpStats) {
    let now = now_ms();
    let key = g as usize;
    let mut timers_fired = 0u32;
    let mut raf_fired = 0u32;
    let per_task = flags & PUMP_CHECKPOINT_PER_TASK != 0;

    if flags & PUMP_TIMERS != 0 {
        // Native one-shots first (image loads etc). Taken under the lock, run
        // without it; tasks scheduled by tasks land on the next pump.
        let due_native: Vec<NativeTask> = {
            let mut reg = registry().lock().unwrap();
            let s = reg.entry(key).or_default();
            let mut due = Vec::new();
            let mut i = 0;
            while i < s.native.len() {
                if now >= s.native[i].0 {
                    due.push(s.native.swap_remove(i).1);
                } else {
                    i += 1;
                }
            }
            due
        };
        for task in due_native {
            task(g);
            if per_task {
                checkpoint(g);
            }
        }

        // Snapshot due timers (entries stay in the map so a mid-drain clear
        // by an earlier callback is honored), then fire without the lock.
        let due: Vec<(u32, u64, Vec<u64>, f64, bool, f64)> = {
            let mut reg = registry().lock().unwrap();
            let s = reg.entry(key).or_default();
            let mut due: Vec<_> = s
                .timers
                .iter()
                .filter(|(_, t)| now >= t.next_fire)
                .map(|(id, t)| (*id, t.cb, t.args.clone(), t.interval, t.repeating, t.next_fire))
                .collect();
            // Browser ordering: earliest deadline first, registration order ties.
            due.sort_by(|a, b| a.5.total_cmp(&b.5).then(a.0.cmp(&b.0)));
            due
        };

        for (id, cb, args, interval, repeating, _deadline) in due {
            // Skip if a callback earlier in this drain cleared it.
            let still_there = registry().lock().unwrap().entry(key).or_default().timers.contains_key(&id);
            if !still_there {
                continue;
            }
            fire(g, cb, &args, flags);
            timers_fired += 1;

            let removed = {
                let mut reg = registry().lock().unwrap();
                let s = reg.entry(key).or_default();
                if repeating {
                    // Don't resurrect an interval its own callback cleared.
                    if let Some(t) = s.timers.get_mut(&id) {
                        t.next_fire = now + interval;
                    }
                    None
                } else {
                    s.timers.remove(&id)
                }
            };
            if let Some(t) = removed {
                unprotect_timer(g, &t);
            }
        }
    }

    if flags & PUMP_RAF != 0 {
        // Swap the batch out; re-registrations during callbacks land in the
        // fresh vec (next frame).
        let batch: Vec<(u32, u64)> = {
            let mut reg = registry().lock().unwrap();
            core::mem::take(&mut reg.entry(key).or_default().raf)
        };
        let ts = [enc_double(now)];
        for (_, cb) in &batch {
            fire(g, *cb, &ts, flags);
            raf_fired += 1;
        }
        for (_, cb) in &batch {
            capi::JSValueUnprotect(g, *cb as usize as capi::JSValueRef);
        }
    }

    if flags & PUMP_RAF != 0 {
        // Idle callbacks after the frame work (requestIdleCallback). Each gets
        // an IdleDeadline with a real decreasing ~4ms budget.
        let idle_batch: Vec<(u32, u64)> = {
            let mut reg = registry().lock().unwrap();
            core::mem::take(&mut reg.entry(key).or_default().idle)
        };
        if !idle_batch.is_empty() {
            let mut deadline = ENC_UNDEFINED;
            glue::rjs_object_create(g, ENC_UNDEFINED, &mut deadline);
            let a = glue::rjs_atom8(g, "didTimeout".as_ptr(), 10);
            glue::rjs_define(g, deadline, a, 0x6 /* false */, 0);
            glue::rjs_atom_free(g, a);
            let a = glue::rjs_atom8(g, "__t0".as_ptr(), 4);
            glue::rjs_define(g, deadline, a, enc_double(now_ms()), 2);
            glue::rjs_atom_free(g, a);
            let name: Vec<u16> = "timeRemaining".encode_utf16().collect();
            let mut trf = ENC_UNDEFINED;
            glue::rjs_function_create(g, name.as_ptr(), name.len(), 0, idle_time_remaining as *const c_void, &mut trf);
            let a = glue::rjs_atom8(g, "timeRemaining".as_ptr(), 13);
            glue::rjs_define(g, deadline, a, trf, 2);
            glue::rjs_atom_free(g, a);
            for (_, cb) in &idle_batch {
                fire(g, *cb, &[deadline], flags);
            }
            for (_, cb) in &idle_batch {
                capi::JSValueUnprotect(g, *cb as usize as capi::JSValueRef);
            }
        }
    }

    if flags & PUMP_TIMERS != 0 {
        // Permission `change` delivery. One `drain_changes()` call, and nothing
        // at all unless a host is installed and reports a subscription.
        crate::webapi::permissions::pump(g);
        if per_task {
            checkpoint(g);
        }
        // WebSocket delivery: open/message/error/close events parsed off-thread
        // and dispatched here, on the thread that owns the VM. One relaxed
        // atomic read when no socket is connected.
        crate::webapi::websocket::pump(g);
        if per_task {
            checkpoint(g);
        }
    }

    #[cfg(feature = "audio")]
    if flags & PUMP_TIMERS != 0 {
        // Web Audio upkeep: firewheel graph update, AudioParam timelines,
        // scheduled starts/stops, `onended` dispatch.
        crate::webapi::audio::pump(g);
        // Device hot-plug polling. Costs two property reads unless something
        // is actually listening for `devicechange`.
        crate::webapi::mediadevices::pump(g);
        if per_task {
            checkpoint(g);
        }
    }

    if flags & PUMP_MICROTASKS != 0 {
        checkpoint(g);
    }

    if !stats.is_null() {
        let (raf_pending, timers_active, next_deadline) = {
            let mut reg = registry().lock().unwrap();
            let s = reg.entry(key).or_default();
            let next = s
                .timers
                .values()
                .map(|t| t.next_fire)
                .fold(f64::INFINITY, f64::min);
            (s.raf.len() as u32, s.timers.len() as u32, if next.is_finite() { next } else { -1.0 })
        };
        *stats = RjsPumpStats {
            now,
            timers_fired,
            raf_fired,
            raf_pending,
            timers_active,
            next_timer_deadline: next_deadline,
        };
    }
}

pub fn set_uncaught(handler: *const c_void) {
    crate::errors::set_handler(handler)
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;
    use crate::api::Context;
    use crate::webapi::testing::{ok, s};
    use crate::webapi::util::{define, make_fn};

    /// Hooks are process-wide and tests run in parallel, so every observation
    /// is filed under the context it was made for.
    fn hook_runs() -> &'static Mutex<HashMap<usize, u32>> {
        static R: OnceLock<Mutex<HashMap<usize, u32>>> = OnceLock::new();
        R.get_or_init(|| Mutex::new(HashMap::new()))
    }
    fn seen() -> &'static Mutex<HashMap<usize, Vec<Checkpoint>>> {
        static S: OnceLock<Mutex<HashMap<usize, Vec<Checkpoint>>>> = OnceLock::new();
        S.get_or_init(|| Mutex::new(HashMap::new()))
    }
    unsafe fn count_hook(g: *mut c_void) {
        *hook_runs().lock().unwrap().entry(g as usize).or_default() += 1;
    }
    fn runs(c: &Context) -> u32 {
        hook_runs().lock().unwrap().get(&(c.raw() as usize)).copied().unwrap_or(0)
    }
    fn outcomes(c: &Context) -> Vec<Checkpoint> {
        seen().lock().unwrap().get(&(c.raw() as usize)).cloned().unwrap_or_default()
    }

    /// `__cp()` — a host function that asks for a checkpoint from inside JS.
    unsafe extern "sysv64" fn cp_from_js(g: *mut c_void, _cf: *mut c_void) -> u64 {
        let r = checkpoint(g);
        seen().lock().unwrap().entry(g as usize).or_default().push(r);
        ENC_UNDEFINED
    }

    fn ctx() -> Context {
        on_checkpoint(count_hook);
        let c = crate::webapi::testing::ctx();
        unsafe {
            install(c.raw());
            let g = c.raw();
            define(g, glue::rjs_global(g), "__cp", make_fn(g, "__cp", 0, cp_from_js), 2);
        }
        c
    }

    #[test]
    fn a_checkpoint_from_rust_drains_and_runs_the_hooks() {
        let c = ctx();
        s(&c, "globalThis.x = 0; Promise.resolve().then(() => { x = 1; }); 0");
        let before = runs(&c);
        assert_eq!(unsafe { checkpoint(c.raw()) }, Checkpoint::Full);
        ok(&c, "x === 1");
        assert_eq!(runs(&c), before + 1, "hooks run once per full checkpoint");
        // `Context::drain_microtasks` IS a checkpoint.
        c.drain_microtasks();
        assert_eq!(runs(&c), before + 2);
    }

    #[test]
    fn a_checkpoint_asked_for_mid_script_drains_but_never_runs_the_hooks() {
        // HTML only cleans up (IDB transactions…) when the JS stack is empty: a
        // hook here would deactivate a transaction the calling script just made.
        let c = ctx();
        let before = runs(&c);
        ok(&c, "globalThis.y = 0; Promise.resolve().then(() => { y = 1; }); __cp(); y === 1");
        assert_eq!(outcomes(&c), vec![Checkpoint::DrainedInJs]);
        assert_eq!(runs(&c), before, "no hook while JS is on the stack");
    }

    fn probes() -> &'static Mutex<Vec<usize>> {
        static P: Mutex<Vec<usize>> = Mutex::new(Vec::new());
        &P
    }
    /// A hook that asks for another checkpoint of the context it runs for —
    /// only for contexts that opted in, since hooks are process-wide.
    unsafe fn reentering_hook(g: *mut c_void) {
        if probes().lock().unwrap().contains(&(g as usize)) {
            let r = checkpoint(g);
            seen().lock().unwrap().entry(g as usize).or_default().push(r);
        }
    }

    #[test]
    fn a_checkpoint_inside_a_checkpoint_is_a_no_op() {
        // "If the event loop's performing a microtask checkpoint is true, return."
        on_checkpoint(reentering_hook);
        let c = ctx();
        probes().lock().unwrap().push(c.raw() as usize);
        let before = runs(&c);
        assert_eq!(unsafe { checkpoint(c.raw()) }, Checkpoint::Full);
        probes().lock().unwrap().retain(|&k| k != c.raw() as usize);
        assert_eq!(outcomes(&c), vec![Checkpoint::Nested]);
        assert_eq!(runs(&c), before + 1, "the nested call ran no hooks of its own");
        // The flag is per context and cleared on the way out.
        assert_eq!(unsafe { checkpoint(c.raw()) }, Checkpoint::Full);
    }

    #[test]
    fn per_task_checkpoints_run_microtasks_between_timers() {
        // HTML: the microtask a timer queues runs before the NEXT timer.
        // Without the flag the pump keeps its one-drain-at-the-end shape.
        for (flags, expect) in [
            (PUMP_TIMERS | PUMP_MICROTASKS | PUMP_CHECKPOINT_PER_TASK, "a,micro,b"),
            (PUMP_TIMERS | PUMP_MICROTASKS, "a,b,micro"),
        ] {
            let c = ctx();
            s(
                &c,
                "globalThis.log = [];
                 setTimeout(() => { log.push('a'); Promise.resolve().then(() => log.push('micro')); }, 0);
                 setTimeout(() => log.push('b'), 0); 0",
            );
            // Held across the pump, as an embedder's frame does (vue-native's
            // pump holds `redjs_lock`): JSC drains microtasks by itself whenever
            // the OUTERMOST API lock is released, so an unlocked pump would get
            // a drain after every call regardless of the flag.
            unsafe {
                glue::rjs_lock(c.raw());
                pump(c.raw(), flags, core::ptr::null_mut());
                glue::rjs_unlock(c.raw());
            }
            assert_eq!(s(&c, "log.join()"), expect, "flags {flags}");
        }
    }
}
