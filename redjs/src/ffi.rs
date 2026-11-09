//! The exported `redjs_*` C ABI — the single surface the C# (NativeAOT)
//! bindings are generated from (csbindgen reads exactly this file).
//!
//! Conventions:
//! - `ctx` is the value returned by [`redjs_context_create`] (a
//!   `JSC::JSGlobalObject*`; also valid as a C-API `JSContextRef`).
//! - JS values are `u64` = JSC `EncodedJSValue` (JSVALUE64 NaN-boxing).
//!   Numbers, bools, undefined and null can be encoded/decoded on the C# side
//!   with the constants from [`crate::glue::RjsLayout`] — no FFI call.
//! - Fallible calls return `i32`: `0` ok, `1` JS exception (the out param
//!   receives the exception value), `2` invalid argument, `3` resource
//!   exhaustion (trampoline pool).
//! - Strings in: UTF-16 (`*const u16` + len, matches C# `char*`) or UTF-8.
//!   Strings out: [`redjs_string_info`] exposes the engine's internal bytes
//!   (Latin-1 or UTF-16) for a single-copy read on the C# side.
//!
//! GC contract: a `u64` value only held on the current native stack is safe
//! while this thread is inside the VM (conservative scan). Values stored in
//! heap memory must be pinned via [`redjs_protect`] and released via
//! [`redjs_unprotect`]. Values received as host-call arguments are rooted by
//! the CallFrame for the duration of the call.

use core::ffi::{c_char, c_void};
use core::ptr::{null_mut, NonNull};

use crate::capi;
use crate::glue;
use crate::trampoline;

pub const REDJS_OK: i32 = 0;
pub const REDJS_EXCEPTION: i32 = 1;
pub const REDJS_BAD_ARG: i32 = 2;
pub const REDJS_EXHAUSTED: i32 = 3;

// ---------------------------------------------------------------------------
// ABI structs (repr C — mirrored verbatim into C# by csbindgen)
// ---------------------------------------------------------------------------

/// Everything the C# side needs for zero-FFI value decode + host-call
/// argument access. Filled by `redjs_layout` from the real WebKit headers at
/// build time; `abi_version` bumps when anything moves.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct RjsLayout {
    pub abi_version: u32,
    /// Byte offsets from the CallFrame pointer passed to host functions.
    pub cf_argc_offset: u32,
    pub cf_this_offset: u32,
    pub cf_args_offset: u32,
    pub cf_callee_offset: u32,
    pub reserved0: u32,
    /// JSVALUE64 NaN-boxing constants.
    pub tag_number: u64,
    pub double_encode_offset: u64,
    pub not_cell_mask: u64,
    pub value_undefined: u64,
    pub value_null: u64,
    pub value_true: u64,
    pub value_false: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct RjsHeapStats {
    pub heap_size: u64,
    pub heap_capacity: u64,
    pub extra_memory: u64,
    pub object_count: u64,
    pub protected_object_count: u64,
    pub global_object_count: u64,
}

/// A view of a JS string's internal characters: Latin-1 bytes (`is8bit=1`)
/// or UTF-16 code units. Valid while the string value stays alive.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RjsStr {
    pub ptr: *const c_void,
    pub len: u32,
    pub is8bit: u32,
}

impl Default for RjsStr {
    fn default() -> Self {
        Self { ptr: core::ptr::null(), len: 0, is8bit: 1 }
    }
}

/// One-call introspection of a typed-array view. `kind`: 1 i8, 2 u8,
/// 3 u8clamped, 4 i16, 5 u16, 6 i32, 7 u32, 8 f32, 9 f64, 10 bigi64,
/// 11 bigu64, 12 dataview, 13 f16.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RjsTaInfo {
    pub data: *mut c_void,
    pub length: u32,
    pub byte_length: u32,
    pub byte_offset: u32,
    pub kind: u32,
    pub detached: u32,
    pub shared: u32,
}

impl Default for RjsTaInfo {
    fn default() -> Self {
        Self { data: core::ptr::null_mut(), length: 0, byte_length: 0, byte_offset: 0, kind: 0, detached: 0, shared: 0 }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RjsAbInfo {
    pub data: *mut c_void,
    pub byte_length: u64,
    pub detached: u32,
    pub shared: u32,
}

impl Default for RjsAbInfo {
    fn default() -> Self {
        Self { data: core::ptr::null_mut(), byte_length: 0, detached: 0, shared: 0 }
    }
}

/// Result of one `redjs_pump`: what fired and what is still scheduled.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct RjsPumpStats {
    /// The pump's snapshot time (ms, same clock as performance.now()).
    pub now: f64,
    pub timers_fired: u32,
    pub raf_fired: u32,
    pub raf_pending: u32,
    pub timers_active: u32,
    /// -1 when no timers are scheduled.
    pub next_timer_deadline: f64,
}

// ---------------------------------------------------------------------------
// Init / context / VM
// ---------------------------------------------------------------------------

/// Process-wide init. `options` is an optional JSC options list
/// (`"useJIT=true,jitPolicyScale=0.5"` style, see `JSC_dumpOptions=1`).
/// Idempotent; must be called before the first context on options use.
#[no_mangle]
pub unsafe extern "C" fn redjs_init(options: *const c_char) -> i32 {
    // ── AOT ICU anchor ──────────────────────────────────────────────────
    // The bun-webkit ICU is repacked: brkitr / display-name items are zstd
    // frames, and the patched udata.cpp calls our `bun_icu_maybe_decompress`
    // hook (icu.rs) only through a WEAK reference. In the cdylib (redjs.dll,
    // JIT path) every object is kept so the strong hook wins. But the thin
    // `-bundle` staticlib pulls an archive member only on a STRONG reference,
    // and nothing strongly references the hook → icu.o is dropped from the
    // NativeAOT image → the weak-null default leaves compressed items raw →
    // text layout (BreakIterator) reads garbage and JSC fastfails. Touching
    // the symbol here (redjs_init is DirectPInvoke'd, always in the image)
    // forces icu.o into the static-link closure. See packages/jscore/README.
    core::hint::black_box(crate::icu::bun_icu_maybe_decompress as *const ());
    glue::rjs_init(options)
}

/// Adjusts JSC options after init. Options already consumed by compiled code
/// may not take effect; prefer `redjs_init`.
#[no_mangle]
pub unsafe extern "C" fn redjs_set_options(options: *const c_char) -> i32 {
    glue::rjs_set_options(options)
}

/// Creates a VM + global object and returns the context handle
/// (`JSGlobalObject*`). Contexts are expected to live for the process
/// lifetime in the player.
///
/// Also arms JSC's unhandled-promise-rejection hook (`crate::errors`) — here
/// rather than behind an opt-in call, because the hook existing and nobody
/// arming it is precisely how a rejection became an empty screen and no log
/// line. Rejections surface through the same handler as pumped-callback
/// throws (`redjs_set_uncaught_handler`); until one is registered they go to
/// stderr.
#[no_mangle]
pub unsafe extern "C" fn redjs_context_create() -> *mut c_void {
    glue::rjs_init(core::ptr::null());
    #[cfg(feature = "native-aot")]
    let aot_global = crate::aot::create_context_if_active();
    #[cfg(not(feature = "native-aot"))]
    let aot_global: *mut c_void = null_mut();
    let g = if aot_global.is_null() { capi::JSGlobalContextCreate(null_mut()) } else { aot_global };
    if !g.is_null() {
        // Before anything is keyed by this address: if it belonged to a context
        // released earlier, that context's registrations are purged here, so
        // nothing below mistakes them for this one's (crate::context).
        crate::context::register(g);
        crate::errors::install_rejection(g);
    }
    g
}

/// Releases the context. ⚠ Known issue: this Bun WebKit prebuilt fastfail-
/// crashes in VM teardown (`JSGlobalContextRelease`). Prefer leaking contexts
/// (create once, keep for process lifetime). Kept for API completeness.
///
/// Every redjs registry keyed by this context is cleared first, so a context
/// created later at the same address starts from nothing.
#[no_mangle]
pub unsafe extern "C" fn redjs_context_release(ctx: *mut c_void) {
    #[cfg(feature = "native-aot")]
    crate::aot::forget_context(ctx);
    crate::context::forget(ctx);
    capi::JSGlobalContextRelease(ctx);
}

/// This context's process-unique id: minted at creation from one counter,
/// never 0, NEVER reused — unlike the `ctx` address, which a later context can
/// receive. 0 for an address redjs has never seen. Key anything that can be
/// delivered to a context *later* (from another thread, after a wait) by this,
/// not by the address.
#[no_mangle]
pub unsafe extern "C" fn redjs_context_id(ctx: *mut c_void) -> u64 {
    crate::context::id_of(ctx).map(|id| id.0).unwrap_or(0)
}

/// The global object of this context as a JS value.
#[no_mangle]
pub unsafe extern "C" fn redjs_global(ctx: *mut c_void) -> u64 {
    glue::rjs_global(ctx)
}

/// Fills the ABI layout: CallFrame offsets for zero-FFI argument access in
/// host functions + JSValue NaN-boxing constants. `abi_version` is 1.
#[no_mangle]
pub unsafe extern "C" fn redjs_layout(out: *mut RjsLayout) {
    glue::rjs_layout(out)
}

/// Takes the VM API lock (recursive). Batch many redjs calls (e.g. one frame)
/// under one lock to skip per-call locking; host callbacks already run locked.
#[no_mangle]
pub unsafe extern "C" fn redjs_lock(ctx: *mut c_void) {
    glue::rjs_lock(ctx)
}

#[no_mangle]
pub unsafe extern "C" fn redjs_unlock(ctx: *mut c_void) {
    glue::rjs_unlock(ctx)
}

/// A microtask checkpoint: runs the microtask queue (promise reactions, await
/// continuations), then — when no JS is on the stack — the post-checkpoint
/// steps redjs's web APIs hang on it. Call after eval/call batches — e.g. once
/// per frame after the rAF callback. Same entry point `redjs_pump` uses.
#[no_mangle]
pub unsafe extern "C" fn redjs_drain_microtasks(ctx: *mut c_void) {
    crate::web::checkpoint(ctx);
}

/// mode: 0 = full sync, 1 = eden (young gen) sync, 2 = async full request.
#[no_mangle]
pub unsafe extern "C" fn redjs_gc(ctx: *mut c_void, mode: i32) {
    glue::rjs_gc(ctx, mode)
}

#[no_mangle]
pub unsafe extern "C" fn redjs_heap_stats(ctx: *mut c_void, out: *mut RjsHeapStats) {
    glue::rjs_heap_stats(ctx, out)
}

/// Tells the GC about external memory owned by JS objects (e.g. big external
/// ArrayBuffers) so collection pressure scales with reality.
#[no_mangle]
pub unsafe extern "C" fn redjs_report_extra_memory(ctx: *mut c_void, bytes: u64) {
    glue::rjs_report_extra_memory(ctx, bytes)
}

// ---------------------------------------------------------------------------
// Prepared scripts and native images
// ---------------------------------------------------------------------------

/// Generates trusted classic-script bytecode without executing it. The handle
/// owns `bytes` until `redjs_bytecode_free`; depth=UINT32_MAX compiles all nested
/// functions. Keep the exact source/URL and use only with this engine/options.
#[no_mangle]
pub unsafe extern "C" fn redjs_bytecode_generate(
    ctx: *mut c_void, src: *const u8, len: usize, url: *const u8, url_len: usize,
    depth: u32, handle: *mut *mut c_void, bytes: *mut *const u8,
    byte_length: *mut usize, out: *mut u64,
) -> i32 {
    glue::rjs_bytecode_generate(ctx, src, len, url, url_len, depth, handle, bytes, byte_length, out)
}

/// Frees a compiler-produced bytecode handle; null is accepted.
#[no_mangle]
pub unsafe extern "C" fn redjs_bytecode_free(handle: *mut c_void) {
    glue::rjs_bytecode_free(handle)
}

/// Evaluates trusted compiler-produced bytecode with its exact source/URL.
/// `used` reports acceptance; a miss falls back to source before execution.
/// Arbitrary bytes are unsafe even if their checksum is correct. JSC retains
/// its own byte copy, so the compiler handle can be freed after this call.
#[no_mangle]
pub unsafe extern "C" fn redjs_eval8_cached(
    ctx: *mut c_void, src: *const u8, len: usize, url: *const u8, url_len: usize,
    bytes: *const u8, byte_length: usize, used: *mut u32, out: *mut u64,
) -> i32 {
    glue::rjs_eval8_cached(ctx, src, len, url, url_len, bytes, byte_length, used, out)
}

/// Copies native-AOT build provenance as UTF-8 JSON, without initializing JSC.
/// Returns the required byte length (no NUL), or zero when native-aot is off.
#[no_mangle]
pub unsafe extern "C" fn redjs_aot_build_info(destination: *mut u8, capacity: usize) -> usize {
    #[cfg(feature = "native-aot")]
    {
        let info = format!(
            "{{\"frontend\":\"{}\",\"engine\":\"{}\",\"tableId\":\"{}\",\"compileOptions\":\"{}\"}}",
            crate::aot::FRONTEND_REVISION, crate::aot::ENGINE_VERSION,
            crate::aot::BUILD_TABLE_ID, crate::aot::COMPILE_OPTIONS,
        );
        if capacity >= info.len() && !destination.is_null() {
            core::ptr::copy_nonoverlapping(info.as_ptr(), destination, info.len());
        }
        info.len()
    }
    #[cfg(not(feature = "native-aot"))]
    { let _ = (destination, capacity); 0 }
}

/// Loads an already verified native image before JSC initialization. `file`
/// must have read+execute access and deny write sharing. Hash/check the same
/// file against trusted engine/frontend/table/options metadata first. The
/// loader retains its own handle; returns 1 for acceptance, otherwise rejects.
/// Profile 0 uses engine defaults; 1 is compact. Native-AOT-disabled builds reject.
#[no_mangle]
pub unsafe extern "C" fn redjs_aot_load_handle(file: isize, identity: *const u8, profile: u32) -> u32 {
    #[cfg(feature = "native-aot")]
    { glue::rjs_aot_load_handle_with_profile(file, identity, profile) }
    #[cfg(not(feature = "native-aot"))]
    { let _ = (file, identity, profile); 0 }
}

/// Starts a one-based native entry module. Status 3 means pending top-level
/// await (not completed success); otherwise normal redjs eval statuses apply.
#[no_mangle]
pub unsafe extern "C" fn redjs_aot_run(ctx: *mut c_void, module: u32, out: *mut u64) -> i32 {
    if ctx.is_null() || out.is_null() { return REDJS_BAD_ARG; }
    #[cfg(feature = "native-aot")]
    { glue::rjs_aot_run(ctx, module, out) }
    #[cfg(not(feature = "native-aot"))]
    { let _ = module; REDJS_BAD_ARG }
}

/// Returns 1 after a native image has been selected on this VM thread.
#[no_mangle]
pub unsafe extern "C" fn redjs_aot_is_active() -> i32 {
    #[cfg(feature = "native-aot")]
    { glue::rjs_aot_is_active() }
    #[cfg(not(feature = "native-aot"))]
    { 0 }
}

// ---------------------------------------------------------------------------
// Eval
// ---------------------------------------------------------------------------

/// Evaluates UTF-16 source. `url` (may be empty) names the script in stack
/// traces. Runs as a classic script (not a module).
#[no_mangle]
pub unsafe extern "C" fn redjs_eval16(
    ctx: *mut c_void,
    src: *const u16,
    len: usize,
    url: *const u16,
    url_len: usize,
    out: *mut u64,
) -> i32 {
    glue::rjs_eval16(ctx, src, len, url, url_len, out)
}

/// Evaluates UTF-8 source (for file-loaded scripts).
#[no_mangle]
pub unsafe extern "C" fn redjs_eval8(
    ctx: *mut c_void,
    src: *const u8,
    len: usize,
    url: *const u8,
    url_len: usize,
    out: *mut u64,
) -> i32 {
    glue::rjs_eval8(ctx, src, len, url, url_len, out)
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// Classifies a value: 0 undefined, 1 null, 2 bool, 3 int32, 4 double,
/// 5 string, 6 symbol, 7 bigint, 8 object, 9 function, 10 array,
/// 11 typed-array view, 12 arraybuffer, 13 promise.
#[no_mangle]
pub unsafe extern "C" fn redjs_kind(ctx: *mut c_void, v: u64) -> u32 {
    glue::rjs_kind(ctx, v)
}

/// ToNumber with JS coercion (may run valueOf → may throw).
#[no_mangle]
pub unsafe extern "C" fn redjs_to_number(ctx: *mut c_void, v: u64, out: *mut f64) -> i32 {
    glue::rjs_to_number(ctx, v, out)
}

/// ToString with JS coercion; returns a JS string value (read it with
/// `redjs_string_info`).
#[no_mangle]
pub unsafe extern "C" fn redjs_to_string(ctx: *mut c_void, v: u64, out: *mut u64) -> i32 {
    glue::rjs_to_string(ctx, v, out)
}

#[no_mangle]
pub unsafe extern "C" fn redjs_to_boolean(ctx: *mut c_void, v: u64) -> i32 {
    glue::rjs_to_boolean(ctx, v)
}

#[no_mangle]
pub unsafe extern "C" fn redjs_strict_equals(ctx: *mut c_void, a: u64, b: u64) -> i32 {
    glue::rjs_strict_equals(ctx, a, b)
}

/// Pins a value against GC (refcounted). Required before storing a value in
/// heap memory (C# fields).
#[no_mangle]
pub unsafe extern "C" fn redjs_protect(ctx: *mut c_void, v: u64) {
    capi::JSValueProtect(ctx, v as usize as capi::JSValueRef);
}

#[no_mangle]
pub unsafe extern "C" fn redjs_unprotect(ctx: *mut c_void, v: u64) {
    capi::JSValueUnprotect(ctx, v as usize as capi::JSValueRef);
}

// ---------------------------------------------------------------------------
// Strings
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn redjs_string16(ctx: *mut c_void, p: *const u16, len: usize, out: *mut u64) -> i32 {
    glue::rjs_string16(ctx, p, len, out)
}

#[no_mangle]
pub unsafe extern "C" fn redjs_string8(ctx: *mut c_void, p: *const u8, len: usize, out: *mut u64) -> i32 {
    glue::rjs_string8(ctx, p, len, out)
}

/// Exposes the engine-internal character data of a JS string: Latin-1 bytes
/// (`is8bit=1`) or UTF-16 code units. The pointer stays valid while the
/// string value is alive (keep it on the stack / protected during the read).
#[no_mangle]
pub unsafe extern "C" fn redjs_string_info(ctx: *mut c_void, v: u64, out: *mut RjsStr) -> i32 {
    glue::rjs_string_info(ctx, v, out)
}

// ---------------------------------------------------------------------------
// Atoms (interned property names — create once, reuse every frame)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn redjs_atom16(ctx: *mut c_void, p: *const u16, len: usize) -> *mut c_void {
    glue::rjs_atom16(ctx, p, len)
}

#[no_mangle]
pub unsafe extern "C" fn redjs_atom8(ctx: *mut c_void, p: *const u8, len: usize) -> *mut c_void {
    glue::rjs_atom8(ctx, p, len)
}

/// A PRIVATE-name atom: properties keyed by it are invisible to all of JS
/// (not even getOwnPropertySymbols) — internal slots for host classes.
/// Each call mints a DISTINCT symbol even for the same name: create once,
/// cache on the host side, reuse.
#[no_mangle]
pub unsafe extern "C" fn redjs_atom_private(ctx: *mut c_void, p: *const u8, len: usize) -> *mut c_void {
    glue::rjs_atom_private(ctx, p, len)
}

/// Frees an atom of this context's VM. Must not outlive the VM (typically
/// atoms just live forever). Requires the context: the atom-table removal
/// must run under the VM's lock.
#[no_mangle]
pub unsafe extern "C" fn redjs_atom_free(ctx: *mut c_void, atom: *mut c_void) {
    glue::rjs_atom_free(ctx, atom)
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

/// `base[atom]` with full JS semantics (prototype chain, getters).
#[no_mangle]
pub unsafe extern "C" fn redjs_get(ctx: *mut c_void, base: u64, atom: *mut c_void, out: *mut u64) -> i32 {
    glue::rjs_get(ctx, base, atom, out)
}

/// `obj[atom] = v` with full JS semantics (setters, proto chain).
#[no_mangle]
pub unsafe extern "C" fn redjs_set(ctx: *mut c_void, obj: u64, atom: *mut c_void, v: u64) -> i32 {
    glue::rjs_set(ctx, obj, atom, v)
}

/// Defines an own property directly (no setters). attrs bits: 1 readonly,
/// 2 dontenum, 4 dontdelete. Use for building namespaces/prototypes.
#[no_mangle]
pub unsafe extern "C" fn redjs_define(ctx: *mut c_void, obj: u64, atom: *mut c_void, v: u64, attrs: u32) -> i32 {
    glue::rjs_define(ctx, obj, atom, v, attrs)
}

#[no_mangle]
pub unsafe extern "C" fn redjs_get_index(ctx: *mut c_void, base: u64, index: u32, out: *mut u64) -> i32 {
    glue::rjs_get_index(ctx, base, index, out)
}

#[no_mangle]
pub unsafe extern "C" fn redjs_set_index(ctx: *mut c_void, obj: u64, index: u32, v: u64) -> i32 {
    glue::rjs_set_index(ctx, obj, index, v)
}

/// 1 = has, 0 = doesn't, -1 = exception, -2 = not an object.
#[no_mangle]
pub unsafe extern "C" fn redjs_has(ctx: *mut c_void, obj: u64, atom: *mut c_void) -> i32 {
    glue::rjs_has(ctx, obj, atom)
}

/// 1 = deleted, 0 = refused (non-configurable), -1 = exception, -2 = not an object.
#[no_mangle]
pub unsafe extern "C" fn redjs_delete(ctx: *mut c_void, obj: u64, atom: *mut c_void) -> i32 {
    glue::rjs_delete(ctx, obj, atom)
}

// ---------------------------------------------------------------------------
// Objects / arrays / functions / calls
// ---------------------------------------------------------------------------

/// Creates `{}`. `proto`: object → used as prototype; null value → null
/// prototype; anything else → Object.prototype.
#[no_mangle]
pub unsafe extern "C" fn redjs_object_create(ctx: *mut c_void, proto: u64, out: *mut u64) -> i32 {
    glue::rjs_object_create(ctx, proto, out)
}

#[no_mangle]
pub unsafe extern "C" fn redjs_array_create(ctx: *mut c_void, len: u32, out: *mut u64) -> i32 {
    glue::rjs_array_create(ctx, len, out)
}

#[no_mangle]
pub unsafe extern "C" fn redjs_array_length(ctx: *mut c_void, v: u64, out: *mut u32) -> i32 {
    glue::rjs_array_length(ctx, v, out)
}

/// Registers an MS-ABI host function `EncodedJSValue f(JSGlobalObject*,
/// CallFrame*)` (C# `[UnmanagedCallersOnly]` static) as a JS function.
/// Decode arguments on the C# side via the RjsLayout CallFrame offsets.
/// Consumes one trampoline slot for the process lifetime.
#[no_mangle]
pub unsafe extern "C" fn redjs_function_create(
    ctx: *mut c_void,
    name: *const u16,
    name_len: usize,
    arity: u32,
    ms_fn: *const c_void,
    out: *mut u64,
) -> i32 {
    let Some(entry) = trampoline::acquire(trampoline::KIND_BARE, ms_fn, null_mut()) else {
        return REDJS_EXHAUSTED;
    };
    glue::rjs_function_create(ctx, name, name_len, arity, entry, out)
}

/// Like `redjs_function_create` but the callback also receives an opaque
/// `data` pointer: `EncodedJSValue f(JSGlobalObject*, CallFrame*, void*)`.
#[no_mangle]
pub unsafe extern "C" fn redjs_function_create_with_data(
    ctx: *mut c_void,
    name: *const u16,
    name_len: usize,
    arity: u32,
    ms_fn: *const c_void,
    data: *mut c_void,
    out: *mut u64,
) -> i32 {
    let Some(entry) = trampoline::acquire(trampoline::KIND_DATA, ms_fn, data) else {
        return REDJS_EXHAUSTED;
    };
    glue::rjs_function_create(ctx, name, name_len, arity, entry, out)
}

/// Registers a raw JSC host function — the zero-overhead path, no trampoline
/// slot. Use SysV on Windows x64 and the platform C ABI on Unix. The exported
/// symbol's historical name is retained for existing embedders.
#[no_mangle]
pub unsafe extern "C" fn redjs_function_create_sysv(
    ctx: *mut c_void,
    name: *const u16,
    name_len: usize,
    arity: u32,
    sysv_fn: *const c_void,
    out: *mut u64,
) -> i32 {
    glue::rjs_function_create(ctx, name, name_len, arity, sysv_fn, out)
}

/// Trampoline pool usage: `used` out of `total` slots.
#[no_mangle]
pub unsafe extern "C" fn redjs_host_slots(used: *mut u32, total: *mut u32) {
    if !used.is_null() {
        *used = trampoline::slots_used();
    }
    if !total.is_null() {
        *total = trampoline::slots_total();
    }
}

/// Calls a JS function value. `this_v` = undefined gives global-this
/// semantics in sloppy mode.
#[no_mangle]
pub unsafe extern "C" fn redjs_call(
    ctx: *mut c_void,
    f: u64,
    this_v: u64,
    args: *const u64,
    argc: u32,
    out: *mut u64,
) -> i32 {
    glue::rjs_call(ctx, f, this_v, args, argc, out)
}

/// `new ctor(...args)`.
#[no_mangle]
pub unsafe extern "C" fn redjs_construct(
    ctx: *mut c_void,
    ctor: u64,
    args: *const u64,
    argc: u32,
    out: *mut u64,
) -> i32 {
    glue::rjs_construct(ctx, ctor, args, argc, out)
}

// ---------------------------------------------------------------------------
// Errors / exceptions
// ---------------------------------------------------------------------------

/// Throws `err` into the VM. Call from inside a host function right before
/// returning; the returned value is then ignored by JSC.
#[no_mangle]
pub unsafe extern "C" fn redjs_throw(ctx: *mut c_void, err: u64) {
    glue::rjs_throw(ctx, err)
}

/// kind: 0 Error, 1 TypeError, 2 RangeError, 3 SyntaxError.
#[no_mangle]
pub unsafe extern "C" fn redjs_error_create(
    ctx: *mut c_void,
    kind: u32,
    msg: *const u16,
    len: usize,
    out: *mut u64,
) -> i32 {
    glue::rjs_error_create(ctx, kind, msg, len, out)
}

/// Extracts `message` and `stack` as JS strings from an exception value
/// (never throws; falls back to ToString / empty).
#[no_mangle]
pub unsafe extern "C" fn redjs_error_info(ctx: *mut c_void, err: u64, msg_out: *mut u64, stack_out: *mut u64) -> i32 {
    glue::rjs_error_info(ctx, err, msg_out, stack_out)
}

/// Checks for (and clears) a pending VM exception: 1 + value if present.
#[no_mangle]
pub unsafe extern "C" fn redjs_exception_pending(ctx: *mut c_void, out: *mut u64) -> i32 {
    glue::rjs_exception_pending(ctx, out)
}

// ---------------------------------------------------------------------------
// ArrayBuffers / typed arrays
// ---------------------------------------------------------------------------

/// Creates an ArrayBuffer VIEWING host memory (zero copy). `dealloc(bytes,
/// dealloc_ctx)` fires when the buffer is GC'd or detached; pass null to keep
/// ownership on the host (the JS side must then be detached before the memory
/// dies — see `redjs_arraybuffer_detach`).
#[no_mangle]
pub unsafe extern "C" fn redjs_arraybuffer_external(
    ctx: *mut c_void,
    bytes: *mut c_void,
    byte_len: usize,
    dealloc: *const c_void, // Option<unsafe extern "C" fn(bytes, ctx)> — null = host keeps ownership
    dealloc_ctx: *mut c_void,
    out: *mut u64,
) -> i32 {
    let mut exc: capi::JSValueRef = core::ptr::null();
    let dealloc: capi::JSTypedArrayBytesDeallocator = core::mem::transmute(dealloc);
    let obj = capi::JSObjectMakeArrayBufferWithBytesNoCopy(ctx, bytes, byte_len, dealloc, dealloc_ctx, &mut exc);
    finish_capi_make(obj, exc, out)
}

#[no_mangle]
pub unsafe extern "C" fn redjs_arraybuffer_info(ctx: *mut c_void, v: u64, out: *mut RjsAbInfo) -> i32 {
    glue::rjs_arraybuffer_info(ctx, v, out)
}

/// Detaches the buffer (spec `transfer`): JS views become zero-length, the
/// deallocator (if any) fires. Use on `unmap()` for mapped-GPU-memory views.
#[no_mangle]
pub unsafe extern "C" fn redjs_arraybuffer_detach(ctx: *mut c_void, v: u64) -> i32 {
    glue::rjs_arraybuffer_detach(ctx, v)
}

/// Creates a typed array of `kind` (RjsTaKind: 1 i8, 2 u8, 3 u8clamped,
/// 4 i16, 5 u16, 6 i32, 7 u32, 8 f32, 9 f64, 10 bigi64, 11 bigu64) with a
/// fresh buffer of `length` elements.
#[no_mangle]
pub unsafe extern "C" fn redjs_typedarray_create(ctx: *mut c_void, kind: u32, length: usize, out: *mut u64) -> i32 {
    let capi_kind = capi::ta_kind_to_capi(kind);
    if capi_kind == capi::K_JS_TYPED_ARRAY_TYPE_NONE {
        return REDJS_BAD_ARG;
    }
    let mut exc: capi::JSValueRef = core::ptr::null();
    let obj = capi::JSObjectMakeTypedArray(ctx, capi_kind, length, &mut exc);
    finish_capi_make(obj, exc, out)
}

/// Typed array VIEWING host memory (zero copy), e.g. a wgpu mapped range.
#[no_mangle]
pub unsafe extern "C" fn redjs_typedarray_external(
    ctx: *mut c_void,
    kind: u32,
    bytes: *mut c_void,
    byte_len: usize,
    dealloc: *const c_void, // Option<unsafe extern "C" fn(bytes, ctx)> — null = host keeps ownership
    dealloc_ctx: *mut c_void,
    out: *mut u64,
) -> i32 {
    let capi_kind = capi::ta_kind_to_capi(kind);
    if capi_kind == capi::K_JS_TYPED_ARRAY_TYPE_NONE {
        return REDJS_BAD_ARG;
    }
    let mut exc: capi::JSValueRef = core::ptr::null();
    let dealloc: capi::JSTypedArrayBytesDeallocator = core::mem::transmute(dealloc);
    let obj = capi::JSObjectMakeTypedArrayWithBytesNoCopy(ctx, capi_kind, bytes, byte_len, dealloc, dealloc_ctx, &mut exc);
    finish_capi_make(obj, exc, out)
}

/// Typed array over an existing ArrayBuffer value (`ab`), like
/// `new Float32Array(buffer, byteOffset, length)`.
#[no_mangle]
pub unsafe extern "C" fn redjs_typedarray_wrap(
    ctx: *mut c_void,
    kind: u32,
    ab: u64,
    byte_offset: usize,
    length: usize,
    out: *mut u64,
) -> i32 {
    let capi_kind = capi::ta_kind_to_capi(kind);
    if capi_kind == capi::K_JS_TYPED_ARRAY_TYPE_NONE {
        return REDJS_BAD_ARG;
    }
    let mut exc: capi::JSValueRef = core::ptr::null();
    let obj = capi::JSObjectMakeTypedArrayWithArrayBufferAndOffset(
        ctx,
        capi_kind,
        ab as usize as capi::JSObjectRef,
        byte_offset,
        length,
        &mut exc,
    );
    finish_capi_make(obj, exc, out)
}

/// One-call introspection of any typed-array view (kind, data pointer,
/// element count, byte length/offset, detached/shared flags). THE fast path
/// for `writeBuffer(queue, ..., Float32Array)` — read JS memory directly.
#[no_mangle]
pub unsafe extern "C" fn redjs_typedarray_info(ctx: *mut c_void, v: u64, out: *mut RjsTaInfo) -> i32 {
    glue::rjs_typedarray_info(ctx, v, out)
}

unsafe fn finish_capi_make(obj: capi::JSObjectRef, exc: capi::JSValueRef, out: *mut u64) -> i32 {
    if obj.is_null() {
        if !exc.is_null() && !out.is_null() {
            *out = exc as usize as u64;
            return REDJS_EXCEPTION;
        }
        return REDJS_BAD_ARG;
    }
    if !out.is_null() {
        *out = obj as usize as u64;
    }
    REDJS_OK
}

// ---------------------------------------------------------------------------
// Promises
// ---------------------------------------------------------------------------

/// Creates a deferred promise. `resolve_out`/`reject_out` receive plain JS
/// functions — invoke them with `redjs_call(f, undefined, &value, 1, ...)`,
/// then `redjs_drain_microtasks` to run reactions.
#[no_mangle]
pub unsafe extern "C" fn redjs_promise_create(
    ctx: *mut c_void,
    resolve_out: *mut u64,
    reject_out: *mut u64,
    out: *mut u64,
) -> i32 {
    let mut resolve: capi::JSObjectRef = null_mut();
    let mut reject: capi::JSObjectRef = null_mut();
    let mut exc: capi::JSValueRef = core::ptr::null();
    let promise = capi::JSObjectMakeDeferredPromise(ctx, &mut resolve, &mut reject, &mut exc);
    if promise.is_null() {
        if !exc.is_null() && !out.is_null() {
            *out = exc as usize as u64;
            return REDJS_EXCEPTION;
        }
        return REDJS_BAD_ARG;
    }
    *resolve_out = resolve as usize as u64;
    *reject_out = reject as usize as u64;
    *out = promise as usize as u64;
    REDJS_OK
}

/// status: 0 pending, 1 fulfilled, 2 rejected; `result_out` = value/reason
/// (undefined while pending).
#[no_mangle]
pub unsafe extern "C" fn redjs_promise_info(ctx: *mut c_void, v: u64, status_out: *mut u32, result_out: *mut u64) -> i32 {
    glue::rjs_promise_info(ctx, v, status_out, result_out)
}

// ---------------------------------------------------------------------------
// Host objects (identity + private data + finalizer)
// ---------------------------------------------------------------------------

static HOST_FINALIZER: core::sync::atomic::AtomicPtr<c_void> =
    core::sync::atomic::AtomicPtr::new(null_mut());

unsafe extern "C" fn host_finalize(obj: capi::JSObjectRef) {
    let f = HOST_FINALIZER.load(core::sync::atomic::Ordering::Acquire);
    if let Some(f) = NonNull::new(f) {
        let data = capi::JSObjectGetPrivate(obj);
        if !data.is_null() {
            let cb: unsafe extern "C" fn(*mut c_void) = core::mem::transmute(f.as_ptr());
            cb(data);
        }
    }
}

/// Sets the process-global host-object finalizer `void f(void* data)`.
/// ⚠ Runs on GC/sweeper threads — only enqueue, never call back into JS.
#[no_mangle]
pub unsafe extern "C" fn redjs_set_host_finalizer(f: *const c_void) {
    HOST_FINALIZER.store(f as *mut c_void, core::sync::atomic::Ordering::Release);
}

/// Creates a host-object class (one per wrapper family, e.g. "GPUBuffer").
/// `name` is a NUL-terminated UTF-8 string; may be null.
#[no_mangle]
pub unsafe extern "C" fn redjs_hostclass_create(name: *const c_char) -> *mut c_void {
    let mut def = capi::JSClassDefinition::empty();
    def.class_name = name;
    def.finalize = Some(host_finalize);
    capi::JSClassCreate(&def)
}

/// Creates a JS object of `cls` carrying an opaque `data` pointer (C# handle:
/// index+generation packed, or a GCHandle). `proto`: 0 → class default.
#[no_mangle]
pub unsafe extern "C" fn redjs_hostobject_create(
    ctx: *mut c_void,
    cls: *mut c_void,
    data: *mut c_void,
    proto: u64,
    out: *mut u64,
) -> i32 {
    let obj = capi::JSObjectMake(ctx, cls, data);
    if obj.is_null() {
        return REDJS_BAD_ARG;
    }
    if proto != 0 {
        capi::JSObjectSetPrototype(ctx, obj, proto as usize as capi::JSValueRef);
    }
    if !out.is_null() {
        *out = obj as usize as u64;
    }
    REDJS_OK
}

/// Reads the private data pointer back from a host object (null if `v` is
/// not a host object). The single FFI call in a wrapper method dispatch.
#[no_mangle]
pub unsafe extern "C" fn redjs_hostobject_data(v: u64) -> *mut c_void {
    // Immediates (null/undefined/numbers) must never reach JSObjectGetPrivate
    // — it derefs the value as a cell pointer.
    if v == 0 || (v & (0xfffe_0000_0000_0000u64 | 0x2)) != 0 {
        return core::ptr::null_mut();
    }
    capi::JSObjectGetPrivate(v as usize as capi::JSObjectRef)
}

/// Replaces the private data pointer (0 allowed). Returns 1 on success.
#[no_mangle]
pub unsafe extern "C" fn redjs_hostobject_set_data(v: u64, data: *mut c_void) -> i32 {
    if v == 0 || (v & (0xfffe_0000_0000_0000u64 | 0x2)) != 0 {
        return 0;
    }
    capi::JSObjectSetPrivate(v as usize as capi::JSObjectRef, data) as i32
}

// ---------------------------------------------------------------------------
// Web scheduling core (performance / rAF / timers), pumped by the host
// ---------------------------------------------------------------------------

/// Installs NATIVE `performance` (now/timeOrigin), requestAnimationFrame /
/// cancelAnimationFrame and setTimeout/setInterval/clearTimeout/clearInterval
/// globals. All of them and `redjs_now` share one monotonic clock, so rAF
/// timestamps, timer deadlines and performance.now() are a single time base.
/// Idempotent per context (re-install replaces the globals, state persists).
#[no_mangle]
pub unsafe extern "C" fn redjs_web_install(ctx: *mut c_void) -> i32 {
    crate::web::install(ctx)
}

/// Runs one frame of the web scheduler: due timers (flag 1), the rAF batch
/// with the snapshot time as timestamp (flag 2), then the microtask queue
/// (flag 4). Call once per render frame with flags=7. Callback exceptions go
/// to the uncaught handler and never abort the pump. `stats` may be null.
#[no_mangle]
pub unsafe extern "C" fn redjs_pump(ctx: *mut c_void, flags: u32, stats: *mut RjsPumpStats) {
    crate::web::pump(ctx, flags, stats)
}

/// Milliseconds on the scheduler clock — the exact value JS sees from
/// performance.now(). Use this on the host for unified frame timing.
#[no_mangle]
pub unsafe extern "C" fn redjs_now() -> f64 {
    crate::web::now_ms()
}

/// Process-global handler for exceptions no JS code will ever see:
/// `void f(ctx, exception_value)` (MS ABI). Called with the VM lock held —
/// extract info (redjs_error_info), don't re-enter JS.
///
/// Two sources today: a throw from a pumped callback (timer / rAF / idle), and
/// an **unhandled promise rejection** (armed for every context at creation —
/// `crate::errors`), where the value is the rejection reason. With no handler
/// registered both go to stderr instead of vanishing.
///
/// ⚠ Print the message, not just the stack: JSC's `Error.prototype.stack` has
/// no `Name: message` header line, so a `stack || message` formatter silently
/// drops what went wrong. `redjs_error_info` returns the two separately.
#[no_mangle]
pub unsafe extern "C" fn redjs_set_uncaught_handler(handler: *const c_void) {
    crate::web::set_uncaught(handler)
}

/// Installs the NATIVE Web-API layer (standards-shaped, defined through
/// JSC's object model): Event/CustomEvent/EventTarget, TextEncoder/Decoder,
/// Blob, URL/URLSearchParams, Headers/Response/fetch (local files + data:
/// URIs), ImageBitmap/HTMLImageElement/Image/createImageBitmap,
/// document/window ' plus a small JS remainder for dumb stubs (console fmt,
/// atob/btoa, OffscreenCanvas-2D, silent Web Audio).
///
/// `scripts_root`/`assets_root` (UTF-16) are the fetch/Image URL resolution
/// bases (bundle dir, assets dir). Call AFTER `redjs_web_install` (the
/// scheduler drives async image loads). Idempotent per context.
#[no_mangle]
pub unsafe extern "C" fn redjs_webshim_install(
    ctx: *mut c_void,
    scripts_root: *const u16,
    scripts_len: usize,
    assets_root: *const u16,
    assets_len: usize,
) -> i32 {
    let scripts = if scripts_len == 0 {
        String::new()
    } else {
        String::from_utf16_lossy(core::slice::from_raw_parts(scripts_root, scripts_len))
    };
    let assets = if assets_len == 0 {
        String::new()
    } else {
        String::from_utf16_lossy(core::slice::from_raw_parts(assets_root, assets_len))
    };
    crate::webapi::install(ctx, &scripts, &assets)
}

/// Sets what `navigator.userAgent` reports (UTF-16), for every context in the
/// process, from the next read on — before or after install. `appVersion` and
/// `platform` follow it. A null `ua` restores the OS-specific
/// [`crate::webapi::navigator::DEFAULT_USER_AGENT`].
#[no_mangle]
pub unsafe extern "C" fn redjs_navigator_set_user_agent(ua: *const u16, len: usize) {
    if ua.is_null() {
        crate::webapi::navigator::set_user_agent(None);
        return;
    }
    let s = String::from_utf16_lossy(core::slice::from_raw_parts(ua, len));
    crate::webapi::navigator::set_user_agent(Some(&s));
}

/// Sets what `navigator.onLine` reports, for every context in the process,
/// from any thread: `1` online, `0` offline, negative = no provider (reads
/// `true`, Chromium's answer when it cannot tell). A change queues an
/// `online` / `offline` event for every context's next pump; repeating the
/// current state is not a change. See `webapi::navigator`.
#[no_mangle]
pub extern "C" fn redjs_navigator_set_on_line(state: i32) {
    crate::webapi::navigator::set_on_line(match state {
        s if s < 0 => None,
        0 => Some(false),
        _ => Some(true),
    });
}

/// The per-pump delivery budget, process-wide, in microseconds: how long one
/// `redjs_pump` may keep delivering — and waiting for — results produced on
/// other threads (IndexedDB) while any are still in flight. Default 4000.
/// 0 = deliver what is already queued and never wait. Present in every build
/// (the budget is the pump's, not IndexedDB's).
#[no_mangle]
pub extern "C" fn redjs_idb_set_budget_us(us: u32) {
    crate::web::set_pump_budget_us(us);
}

/// Ends IndexedDB's work before exit: transactions that never committed are
/// lost (as when a browser is killed) and every database is closed. Returns 1
/// if the server finished within `timeout_ms` (or never started), 0 if not.
/// A build without the `indexeddb` feature always returns 1.
#[no_mangle]
pub extern "C" fn redjs_idb_shutdown(timeout_ms: u32) -> i32 {
    #[cfg(feature = "indexeddb")]
    {
        crate::webapi::indexeddb::shutdown(std::time::Duration::from_millis(timeout_ms as u64)) as i32
    }
    #[cfg(not(feature = "indexeddb"))]
    {
        let _ = timeout_ms;
        1
    }
}

/// Puts IndexedDB ON DISK for an embedder with no Rust host (PEWPEW's C#):
/// `folder_utf8[..len]` holds one SQLite file per database, encrypted at rest
/// under `key32` (32 raw bytes the embedder keeps safe, e.g. with .NET
/// `ProtectedData`) or plaintext when it is null; `quota_bytes` 0 = 1 GiB.
/// Process-wide, BEFORE the first script touches `indexedDB`: returns 0 on
/// success, 1 once IndexedDB is already in use (the configuration is latched
/// at the first use), 2 for a bad argument, 3 in a build without
/// `indexeddb-sqlite`. Call `redjs_idb_shutdown` at exit.
///
/// # Safety
/// `folder_utf8` must point at `len` readable bytes; `key32` at 32 or be null.
#[no_mangle]
pub unsafe extern "C" fn redjs_idb_configure(folder_utf8: *const u8, len: usize, key32: *const u8, quota_bytes: u64) -> i32 {
    #[cfg(feature = "indexeddb-sqlite")]
    {
        if folder_utf8.is_null() || len == 0 {
            return 2;
        }
        let Ok(folder) = std::str::from_utf8(std::slice::from_raw_parts(folder_utf8, len)) else { return 2 };
        let key = if key32.is_null() {
            None
        } else {
            let mut k = [0u8; 32];
            k.copy_from_slice(std::slice::from_raw_parts(key32, 32));
            Some(k)
        };
        match crate::webapi::indexeddb::configure(std::path::PathBuf::from(folder), key, quota_bytes) {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }
    #[cfg(not(feature = "indexeddb-sqlite"))]
    {
        let _ = (folder_utf8, len, key32, quota_bytes);
        3
    }
}

/// Registers (or clears, with null) the host CONTENT PROVIDER for fetch/Image
/// local reads (W-HIVE). `read_fn` is
/// `int32_t (const uint8_t* path_utf8, size_t path_len, void (*sink)(void* ctx, const uint8_t* ptr, size_t len), void* sink_ctx)`
/// — on a hit it returns 0 after calling `sink` exactly once with the full
/// decoded bytes; non-zero = miss, resolution then falls back to the on-disk
/// roots from `redjs_webshim_install`. Called by Rust with the standard C ABI
/// (never by JSC — no SysV trampoline). Reads may come from any JS thread —
/// the provider must be thread-safe.
#[no_mangle]
pub unsafe extern "C" fn redjs_content_provider_set(ctx: *mut c_void, read_fn: *const c_void) {
    let read = if read_fn.is_null() {
        None
    } else {
        Some(core::mem::transmute::<*const c_void, crate::webapi::fsutil::ContentReadFn>(read_fn))
    };
    // Registered first, so a recycled address is purged BEFORE this hook lands
    // rather than by the install that follows it.
    crate::context::ensure(ctx);
    crate::webapi::fsutil::set_content_provider(ctx, read);
}

// ---------------------------------------------------------------------------
// DOM host hooks + image pixel access (native web API)
// ---------------------------------------------------------------------------

/// Updates window.innerWidth/innerHeight/devicePixelRatio and dispatches a
/// trusted `resize` event on window. `dpr` <= 0 keeps the current value.
#[no_mangle]
pub unsafe extern "C" fn redjs_dom_resize(ctx: *mut c_void, width: i32, height: i32, dpr: f64) {
    crate::webapi::dom::resize(ctx, width, height, dpr)
}

/// Updates document.hidden/visibilityState and dispatches `visibilitychange`.
#[no_mangle]
pub unsafe extern "C" fn redjs_dom_set_visibility(ctx: *mut c_void, hidden: i32) {
    crate::webapi::dom::set_visibility(ctx, hidden != 0)
}

/// Fires DOMContentLoaded on document (readyState interactive→complete) and
/// `load` on window. Call once after the entry script has been evaluated.
#[no_mangle]
pub unsafe extern "C" fn redjs_dom_content_loaded(ctx: *mut c_void) {
    crate::webapi::dom::dom_content_loaded(ctx)
}

/// Registers a value under `document.getElementById(id)` (the host canvas
/// wrapper, etc). UTF-16 id.
#[no_mangle]
pub unsafe extern "C" fn redjs_dom_register_element(ctx: *mut c_void, id: *const u16, id_len: usize, value: u64) {
    let id = String::from_utf16_lossy(core::slice::from_raw_parts(id, id_len));
    crate::webapi::dom::register_element(ctx, &id, value)
}

/// RGBA8 pixels of an ImageBitmap / loaded HTMLImageElement / ImageData —
/// direct pointer into engine memory for zero-copy GPU upload
/// (`copyExternalImageToTexture`). The pointer stays valid while the value is
/// alive (protect during upload if it leaves the stack). Returns 2 when the
/// value is not an image source.
#[no_mangle]
pub unsafe extern "C" fn redjs_image_pixels(
    ctx: *mut c_void,
    v: u64,
    out_ptr: *mut *const u8,
    out_byte_len: *mut usize,
    out_width: *mut u32,
    out_height: *mut u32,
) -> i32 {
    let st = crate::webapi::util::state(ctx);
    // ImageBitmap directly, else an HTMLImageElement's loaded bitmap.
    let bmp = if let Some(b) = crate::webapi::util::host_data::<crate::webapi::image::BitmapData>(ctx, st.bitmap_class, v) {
        Some(&*b)
    } else {
        let slot = crate::webapi::util::get_a(ctx, v, st.s_image);
        crate::webapi::util::host_data::<crate::webapi::image::BitmapData>(ctx, st.bitmap_class, slot).map(|b| &*b)
    };
    match bmp {
        Some(b) => {
            *out_ptr = b.rgba.as_ptr();
            *out_byte_len = b.rgba.len();
            *out_width = b.width;
            *out_height = b.height;
            REDJS_OK
        }
        None => REDJS_BAD_ARG,
    }
}

/// Defines a spec accessor property on `obj`. `getter`/`setter` are function
/// values (or undefined for one-sided accessors). attrs as in redjs_define.
#[no_mangle]
pub unsafe extern "C" fn redjs_define_accessor(
    ctx: *mut c_void,
    obj: u64,
    atom: *mut c_void,
    getter: u64,
    setter: u64,
    attrs: u32,
) -> i32 {
    glue::rjs_define_accessor(ctx, obj, atom, getter, setter, attrs)
}

/// Aggressive memory release for level transitions / minimized window:
/// two full sync GCs (second pass sweeps finalizer-freed externals) followed
/// by returning free heap pages to the OS. Expect a frame-time spike — call
/// on loading screens, not mid-gameplay.
#[no_mangle]
pub unsafe extern "C" fn redjs_release_memory(ctx: *mut c_void) {
    glue::rjs_release_memory(ctx)
}

/// [`redjs_release_memory`], after deleting every piece of compiled code
/// (bytecode, JIT) that is not running, as JSC's own `shrinkFootprintWhenIdle`
/// does; each function compiles again from its source when next called. For
/// measuring how much of a heap is code rather than data, and for a host that
/// wants the deepest release (a long-minimised window).
#[no_mangle]
pub unsafe extern "C" fn redjs_release_memory_and_code(ctx: *mut c_void) {
    glue::rjs_release_memory_and_code(ctx)
}

// ---------------------------------------------------------------------------
// Simple dialogs (alert / confirm / prompt) — host hook
// ---------------------------------------------------------------------------

/// Registers (or clears, with null) the host DIALOG handler backing `alert`,
/// `confirm` and `prompt`. `handler` is
/// `int32_t (int32_t kind, const uint16_t* msg, size_t msg_len, const uint16_t* def, size_t def_len, void (*sink)(void* ctx, const uint16_t* ptr, size_t len), void* sink_ctx)`
/// — `kind` is 0 alert / 1 confirm / 2 prompt; the return value is ignored for
/// alert, and is 0 = false/cancelled, non-zero = true/accepted otherwise. A
/// prompt returning non-zero must call `sink` exactly once with the entered
/// text. Called by Rust with the standard C ABI (never by JSC — no SysV
/// trampoline), on the JS thread with the caller blocked: a handler that pumps
/// a modal loop must not re-enter JS.
///
/// With NO handler registered the three globals THROW rather than returning a
/// default — see `webapi::dialogs` for why that is the safer failure.
#[no_mangle]
pub unsafe extern "C" fn redjs_dialog_handler_set(ctx: *mut c_void, handler: *const c_void) {
    let f = if handler.is_null() {
        None
    } else {
        Some(core::mem::transmute::<*const c_void, crate::webapi::dialogs::DialogFn>(handler))
    };
    crate::context::ensure(ctx); // see redjs_content_provider_set
    crate::webapi::dialogs::set_handler(ctx, f);
}
