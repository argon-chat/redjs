//! Platform host-function trampoline pool.
//!
//! JSC host functions on Windows x64 (this Bun WebKit build) are called with
//! the System V AMD64 convention (`JSC_HOST_CALL_ATTRIBUTES = sysv_abi`).
//! Rust can produce such functions (`extern "sysv64"`); .NET cannot. Each C#
//! host function therefore occupies one pre-compiled trampoline slot: JSC
//! calls `t<i>` (SysV), which forwards to the stored MS-ABI pointer. rustc
//! emits the correct ABI transition (shadow space, xmm6-15/rsi/rdi are
//! callee-saved in MS and caller-saved in SysV, so this direction is cheap).
//! On Unix, both JSC and the stored callback use the platform C ABI; the
//! generated entries keep the same pool and closure lifetime contract.
//!
//! Slots are write-once (publication via `kind` with Release/Acquire) and are
//! never freed: host functions live as long as the process, which matches the
//! player's usage (a bounded set registered at startup).

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering};

include!(concat!(env!("OUT_DIR"), "/trampolines.rs"));

/// EncodedJSValue for `undefined` (JSVALUE64: OtherTag | UndefinedTag). Only
/// used for the impossible vacant-slot case.
const ENC_UNDEFINED: u64 = 0x0a;

pub const KIND_VACANT: u32 = 0;
/// `EncodedJSValue f(JSGlobalObject*, CallFrame*)` — platform C ABI (C# static).
pub const KIND_BARE: u32 = 1;
/// `EncodedJSValue f(JSGlobalObject*, CallFrame*, void* data)` — platform C ABI.
pub const KIND_DATA: u32 = 2;

type BareFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u64;
type DataFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> u64;

struct Slot {
    kind: AtomicU32,
    f: AtomicPtr<c_void>,
    data: AtomicPtr<c_void>,
}

#[allow(clippy::declare_interior_mutable_const)]
const SLOT_INIT: Slot = Slot {
    kind: AtomicU32::new(KIND_VACANT),
    f: AtomicPtr::new(core::ptr::null_mut()),
    data: AtomicPtr::new(core::ptr::null_mut()),
};

static SLOTS: [Slot; TRAMPOLINE_COUNT] = [SLOT_INIT; TRAMPOLINE_COUNT];
static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);

/// Reserves a slot and returns the native entry point JSC should call, or
/// `None` if the pool is exhausted.
pub(crate) fn acquire(kind: u32, f: *const c_void, data: *mut c_void) -> Option<*const c_void> {
    let i = NEXT_SLOT.fetch_add(1, Ordering::Relaxed);
    if i >= TRAMPOLINE_COUNT {
        NEXT_SLOT.store(TRAMPOLINE_COUNT, Ordering::Relaxed);
        return None;
    }
    let s = &SLOTS[i];
    s.f.store(f as *mut c_void, Ordering::Relaxed);
    s.data.store(data, Ordering::Relaxed);
    s.kind.store(kind, Ordering::Release); // publish
    Some(TRAMPOLINES[i] as *const c_void)
}

pub fn slots_used() -> u32 {
    NEXT_SLOT.load(Ordering::Relaxed).min(TRAMPOLINE_COUNT) as u32
}

pub fn slots_total() -> u32 {
    TRAMPOLINE_COUNT as u32
}

#[inline(always)]
pub(crate) unsafe fn dispatch(i: usize, g: *mut c_void, cf: *mut c_void) -> u64 {
    let s = SLOTS.get_unchecked(i);
    match s.kind.load(Ordering::Acquire) {
        KIND_BARE => {
            let f: BareFn = core::mem::transmute(s.f.load(Ordering::Relaxed));
            f(g, cf)
        }
        KIND_DATA => {
            let f: DataFn = core::mem::transmute(s.f.load(Ordering::Relaxed));
            f(g, cf, s.data.load(Ordering::Relaxed))
        }
        _ => ENC_UNDEFINED,
    }
}

// ---- Rust closures as host functions --------------------------------------

/// Boxed Rust host closure: `(global, callframe) -> EncodedJSValue`.
pub type HostClosure = Box<dyn FnMut(*mut c_void, *mut c_void) -> u64 + 'static>;

struct ClosureState {
    active: AtomicBool,
    callback: UnsafeCell<HostClosure>,
}
struct ClosureCall<'a>(&'a AtomicBool);
impl Drop for ClosureCall<'_> {
    fn drop(&mut self) { self.0.store(false, Ordering::Release); }
}

unsafe extern "C" fn closure_adapter(g: *mut c_void, cf: *mut c_void, data: *mut c_void) -> u64 {
    let state = &*(data as *const ClosureState);
    // JS getters/callbacks can recursively call this same native function.
    // Reject before creating any mutable reference to the FnMut callback.
    if state.active.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        let message: Vec<u16> = "a mutable native callback cannot be called recursively".encode_utf16().collect();
        let mut error = ENC_UNDEFINED;
        if crate::glue::rjs_error_create(g, 1, message.as_ptr(), message.len(), &mut error) == 0 {
            crate::glue::rjs_throw(g, error);
        }
        return ENC_UNDEFINED;
    }
    let _call = ClosureCall(&state.active);
    let callback = &mut *state.callback.get();
    callback(g, cf)
}

/// Registers a Rust closure in the pool (leaks the closure — host functions
/// are process-lifetime). Returns the native JSC entry point.
pub fn acquire_closure(f: HostClosure) -> Option<*const c_void> {
    let data = Box::into_raw(Box::new(ClosureState {
        active: AtomicBool::new(false),
        callback: UnsafeCell::new(f),
    }));
    let entry = acquire(KIND_DATA, closure_adapter as *const c_void, data.cast());
    if entry.is_none() { drop(unsafe { Box::from_raw(data) }); }
    entry
}
