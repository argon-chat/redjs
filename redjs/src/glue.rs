//! Raw externs into `cpp/glue.cpp` (JSC internal API layer). See that file
//! for semantics. All fallible functions return an `i32` status:
//! `0` ok, `1` JS exception (out param receives the exception value),
//! `2` invalid argument.

use core::ffi::{c_char, c_void};

// The repr(C) ABI structs are defined in ffi.rs (the csbindgen input file);
// re-exported here so internal callers can keep using `glue::RjsLayout` etc.
pub use crate::ffi::{RjsAbInfo, RjsHeapStats, RjsLayout, RjsStr, RjsTaInfo};

pub const OK: i32 = 0;
pub const EXCEPTION: i32 = 1;
pub const BAD_ARG: i32 = 2;

extern "C" {
    pub fn rjs_layout(out: *mut RjsLayout);
    pub fn rjs_init(options: *const c_char) -> i32;
    pub fn rjs_set_options(options: *const c_char) -> i32;
    pub fn rjs_global(g: *mut c_void) -> u64;
    pub fn rjs_lock(g: *mut c_void);
    pub fn rjs_unlock(g: *mut c_void);
    pub fn rjs_drain_microtasks(g: *mut c_void);
    pub fn rjs_gc(g: *mut c_void, mode: i32);
    pub fn rjs_heap_stats(g: *mut c_void, out: *mut RjsHeapStats);
    pub fn rjs_report_extra_memory(g: *mut c_void, bytes: u64);

    pub fn rjs_eval16(g: *mut c_void, src: *const u16, len: usize, url: *const u16, url_len: usize, out: *mut u64) -> i32;
    pub fn rjs_eval8(g: *mut c_void, src: *const u8, len: usize, url: *const u8, url_len: usize, out: *mut u64) -> i32;

    pub fn rjs_kind(g: *mut c_void, v: u64) -> u32;
    pub fn rjs_to_number(g: *mut c_void, v: u64, out: *mut f64) -> i32;
    pub fn rjs_to_string(g: *mut c_void, v: u64, out: *mut u64) -> i32;
    pub fn rjs_to_boolean(g: *mut c_void, v: u64) -> i32;
    pub fn rjs_strict_equals(g: *mut c_void, a: u64, b: u64) -> i32;

    pub fn rjs_string16(g: *mut c_void, p: *const u16, len: usize, out: *mut u64) -> i32;
    pub fn rjs_string8(g: *mut c_void, p: *const u8, len: usize, out: *mut u64) -> i32;
    pub fn rjs_string_info(g: *mut c_void, v: u64, out: *mut RjsStr) -> i32;

    pub fn rjs_atom16(g: *mut c_void, p: *const u16, len: usize) -> *mut c_void;
    pub fn rjs_atom8(g: *mut c_void, p: *const u8, len: usize) -> *mut c_void;
    /// Private-name atom: properties keyed by it are invisible to all JS —
    /// internal slots for native classes.
    pub fn rjs_atom_private(g: *mut c_void, p: *const u8, len: usize) -> *mut c_void;
    pub fn rjs_atom_free(g: *mut c_void, atom: *mut c_void);

    pub fn rjs_get(g: *mut c_void, base: u64, atom: *mut c_void, out: *mut u64) -> i32;
    pub fn rjs_set(g: *mut c_void, obj: u64, atom: *mut c_void, val: u64) -> i32;
    pub fn rjs_define(g: *mut c_void, obj: u64, atom: *mut c_void, val: u64, attrs: u32) -> i32;
    pub fn rjs_get_index(g: *mut c_void, base: u64, index: u32, out: *mut u64) -> i32;
    pub fn rjs_set_index(g: *mut c_void, obj: u64, index: u32, val: u64) -> i32;
    pub fn rjs_has(g: *mut c_void, obj: u64, atom: *mut c_void) -> i32;
    pub fn rjs_delete(g: *mut c_void, obj: u64, atom: *mut c_void) -> i32;

    pub fn rjs_object_create(g: *mut c_void, proto: u64, out: *mut u64) -> i32;
    pub fn rjs_array_create(g: *mut c_void, len: u32, out: *mut u64) -> i32;
    pub fn rjs_array_length(g: *mut c_void, v: u64, out: *mut u32) -> i32;
    /// Own enumerable string keys as a JS array of strings.
    pub fn rjs_own_keys(g: *mut c_void, obj: u64, out: *mut u64) -> i32;
    pub fn rjs_function_create(g: *mut c_void, name: *const u16, name_len: usize, arity: u32, sysv_fn: *const c_void, out: *mut u64) -> i32;
    /// Constructable function: `ctor_fn` runs for `new F()`. Inside it the
    /// CallFrame's THIS slot holds new.target (JSC C++-constructor convention).
    pub fn rjs_function_create_full(g: *mut c_void, name: *const u16, name_len: usize, arity: u32, call_fn: *const c_void, ctor_fn: *const c_void, out: *mut u64) -> i32;
    /// Spec accessor property; getter/setter are function values or undefined.
    pub fn rjs_define_accessor(g: *mut c_void, obj: u64, atom: *mut c_void, getter: u64, setter: u64, attrs: u32) -> i32;
    pub fn rjs_call(g: *mut c_void, f: u64, this_v: u64, args: *const u64, argc: u32, out: *mut u64) -> i32;
    pub fn rjs_construct(g: *mut c_void, ctor: u64, args: *const u64, argc: u32, out: *mut u64) -> i32;

    pub fn rjs_throw(g: *mut c_void, err: u64);
    pub fn rjs_error_create(g: *mut c_void, kind: u32, msg: *const u16, len: usize, out: *mut u64) -> i32;
    pub fn rjs_error_info(g: *mut c_void, err: u64, msg_out: *mut u64, stack_out: *mut u64) -> i32;
    pub fn rjs_exception_pending(g: *mut c_void, out: *mut u64) -> i32;

    pub fn rjs_typedarray_info(g: *mut c_void, v: u64, out: *mut RjsTaInfo) -> i32;
    pub fn rjs_arraybuffer_info(g: *mut c_void, v: u64, out: *mut RjsAbInfo) -> i32;
    pub fn rjs_arraybuffer_detach(g: *mut c_void, v: u64) -> i32;

    pub fn rjs_promise_info(g: *mut c_void, v: u64, status_out: *mut u32, result_out: *mut u64) -> i32;
}
