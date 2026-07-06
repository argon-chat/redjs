//! The exported `redjs_*` C ABI — the single surface the C# (NativeAOT)
//! bindings are generated from (csbindgen reads this file + src/glue.rs).
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

// ---------------------------------------------------------------------------
// Init / context / VM
// ---------------------------------------------------------------------------

/// Process-wide init. `options` is an optional JSC options list
/// (`"useJIT=true,jitPolicyScale=0.5"` style, see `JSC_dumpOptions=1`).
/// Idempotent; must be called before the first context on options use.
#[no_mangle]
pub unsafe extern "C" fn redjs_init(options: *const c_char) -> i32 {
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
#[no_mangle]
pub unsafe extern "C" fn redjs_context_create() -> *mut c_void {
    glue::rjs_init(core::ptr::null());
    capi::JSGlobalContextCreate(null_mut())
}

/// Releases the context. ⚠ Known issue: this Bun WebKit prebuilt fastfail-
/// crashes in VM teardown (`JSGlobalContextRelease`). Prefer leaking contexts
/// (create once, keep for process lifetime). Kept for API completeness.
#[no_mangle]
pub unsafe extern "C" fn redjs_context_release(ctx: *mut c_void) {
    capi::JSGlobalContextRelease(ctx);
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

/// Runs the microtask queue (promise reactions, await continuations). Call
/// after eval/call batches — e.g. once per frame after the rAF callback.
#[no_mangle]
pub unsafe extern "C" fn redjs_drain_microtasks(ctx: *mut c_void) {
    glue::rjs_drain_microtasks(ctx)
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

/// Registers a raw SysV host function (Rust `extern "sysv64"`, Zig
/// `callconv(.SysV)`) — the zero-overhead path, no trampoline slot.
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
    capi::JSObjectGetPrivate(v as usize as capi::JSObjectRef)
}

/// Replaces the private data pointer (0 allowed). Returns 1 on success.
#[no_mangle]
pub unsafe extern "C" fn redjs_hostobject_set_data(v: u64, data: *mut c_void) -> i32 {
    capi::JSObjectSetPrivate(v as usize as capi::JSObjectRef, data) as i32
}
