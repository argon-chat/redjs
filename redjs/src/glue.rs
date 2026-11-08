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

/// `rjs_brand` output. Internal (not part of the C ABI): only the serializer
/// reads it. Field meanings per brand are in glue.cpp.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RjsBrandInfo {
    pub brand: u32,
    pub flags: u32,
    pub ta_kind: u32,
    pub reserved: u32,
    pub byte_length: u64,
    pub byte_offset: u64,
    pub length: u64,
    pub max_byte_length: u64,
    pub number: f64,
    pub data: *mut c_void,
}

impl Default for RjsBrandInfo {
    fn default() -> Self {
        RjsBrandInfo {
            brand: 0,
            flags: 0,
            ta_kind: 0,
            reserved: 0,
            byte_length: 0,
            byte_offset: 0,
            length: 0,
            max_byte_length: 0,
            number: 0.0,
            data: core::ptr::null_mut(),
        }
    }
}

pub mod brand {
    pub const NONE: u32 = 0;
    pub const PLAIN: u32 = 1;
    pub const ARRAY: u32 = 2;
    pub const FUNCTION: u32 = 3;
    pub const PROXY: u32 = 4;
    pub const DATE: u32 = 5;
    pub const REGEXP: u32 = 6;
    pub const MAP: u32 = 7;
    pub const SET: u32 = 8;
    pub const ERROR: u32 = 9;
    pub const BOOLEAN_OBJECT: u32 = 10;
    pub const NUMBER_OBJECT: u32 = 11;
    pub const STRING_OBJECT: u32 = 12;
    pub const BIGINT_OBJECT: u32 = 13;
    pub const SYMBOL_OBJECT: u32 = 14;
    pub const ARRAY_BUFFER: u32 = 15;
    pub const SHARED_ARRAY_BUFFER: u32 = 16;
    pub const VIEW: u32 = 17;
    pub const PROMISE: u32 = 18;
    pub const GLOBAL: u32 = 19;
    pub const OTHER: u32 = 20;

    pub const F_DETACHED: u32 = 1;
    pub const F_RESIZABLE: u32 = 2;
    pub const F_LENGTH_TRACKING: u32 = 4;
    pub const F_REDJS_SLOT: u32 = 8;
}

extern "C" {
    pub fn rjs_layout(out: *mut RjsLayout);
    pub fn rjs_init(options: *const c_char) -> i32;
    pub fn rjs_set_options(options: *const c_char) -> i32;
    pub fn rjs_global(g: *mut c_void) -> u64;
    pub fn rjs_lock(g: *mut c_void);
    pub fn rjs_unlock(g: *mut c_void);
    pub fn rjs_drain_microtasks(g: *mut c_void);
    /// 1 while any JS frame of this VM is on the stack, else 0.
    pub fn rjs_in_js(g: *mut c_void) -> i32;

    pub fn rjs_bytecode_cache_version() -> u32;
    pub fn rjs_bytecode_cache_options(g: *mut c_void, destination: *mut u8, capacity: usize) -> usize;
    pub fn rjs_bytecode_generate(
        g: *mut c_void, src: *const u8, len: usize, url: *const u8, url_len: usize,
        function_depth: u32, handle: *mut *mut c_void, bytes: *mut *const u8,
        byte_length: *mut usize, out: *mut u64,
    ) -> i32;
    pub fn rjs_bytecode_free(handle: *mut c_void);
    #[cfg(feature = "native-aot")]
    pub fn rjs_aot_load_handle_with_profile(file: isize, identity: *const u8, profile: u32) -> u32;
    #[cfg(feature = "native-aot")]
    pub fn rjs_aot_run(context: *mut c_void, module: u32, result: *mut u64) -> i32;
    #[cfg(feature = "native-aot")]
    pub fn rjs_aot_is_active() -> i32;
    /// `bytes` must be a trusted JSC-produced payload, never adversarial input.
    pub fn rjs_eval8_cached(
        g: *mut c_void, src: *const u8, len: usize, url: *const u8, url_len: usize,
        bytes: *const u8, byte_length: usize, used: *mut u32, out: *mut u64,
    ) -> i32;

    // -- structured serialization support (glue.cpp, same section name) --------
    /// Well-known symbol atom: 0 iterator, 1 toStringTag, 2 asyncIterator.
    pub fn rjs_atom_wellknown(g: *mut c_void, which: u32) -> *mut c_void;
    /// The engine's own type of `v` (`BRAND_*`), plus what the serializer needs.
    pub fn rjs_brand(g: *mut c_void, v: u64, out: *mut RjsBrandInfo) -> u32;
    pub fn rjs_wrapper_value(g: *mut c_void, v: u64, out: *mut u64) -> i32;
    pub fn rjs_regexp_parts(g: *mut c_void, v: u64, pattern: *mut u64, flags: *mut u64) -> i32;
    pub fn rjs_get_v(g: *mut c_void, obj: u64, key: u64, out: *mut u64) -> i32;
    pub fn rjs_has_own_v(g: *mut c_void, obj: u64, key: u64, exc: *mut u64) -> i32;
    pub fn rjs_define_v(g: *mut c_void, obj: u64, key: u64, val: u64) -> i32;
    pub fn rjs_get_own_data(g: *mut c_void, obj: u64, atom: *mut c_void, out: *mut u64) -> i32;
    pub fn rjs_string_latin1(g: *mut c_void, p: *const u8, len: usize, out: *mut u64) -> i32;
    pub fn rjs_view_buffer(g: *mut c_void, v: u64, out: *mut u64) -> i32;
    pub fn rjs_array_dense(g: *mut c_void, v: u64, len: *mut u32) -> i32;
    /// HasOwnProperty + [[Get]] in one own lookup: 1 present, 0 absent, -1 exception.
    pub fn rjs_own_get_v(g: *mut c_void, obj: u64, key: u64, out: *mut u64) -> i32;
    pub fn rjs_own_get_index(g: *mut c_void, obj: u64, index: u32, out: *mut u64) -> i32;
    /// CreateDataProperty with an index key (no prototype-chain setter).
    pub fn rjs_define_index(g: *mut c_void, obj: u64, index: u32, val: u64) -> i32;
    pub fn rjs_stack_soft_limit(g: *mut c_void) -> usize;
    pub fn rjs_gc(g: *mut c_void, mode: i32);
    pub fn rjs_heap_stats(g: *mut c_void, out: *mut RjsHeapStats);
    pub fn rjs_report_extra_memory(g: *mut c_void, bytes: u64);
    /// Full GC ×2 + release free malloc pages to the OS (bmalloc scavenge).
    pub fn rjs_release_memory(g: *mut c_void);
    /// One non-blocking turn of this thread's WTF RunLoop: fires the due JSC
    /// timers (incremental sweeper, GC activity callbacks) — see glue.cpp.
    pub fn rjs_run_loop_cycle();
    /// rjs_release_memory after deleting all compiled code that is not running.
    pub fn rjs_release_memory_and_code(g: *mut c_void);

    pub fn rjs_eval16(g: *mut c_void, src: *const u16, len: usize, url: *const u16, url_len: usize, out: *mut u64) -> i32;
    pub fn rjs_eval8(g: *mut c_void, src: *const u8, len: usize, url: *const u8, url_len: usize, out: *mut u64) -> i32;

    pub fn rjs_kind(g: *mut c_void, v: u64) -> u32;
    pub fn rjs_to_number(g: *mut c_void, v: u64, out: *mut f64) -> i32;
    /// ToNumber whose exception comes back in `exc` (status 1).
    pub fn rjs_to_number_x(g: *mut c_void, v: u64, out: *mut f64, exc: *mut u64) -> i32;
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
    /// The well-known `Symbol.iterator` as an atom (iteration protocol).
    pub fn rjs_atom_iterator(g: *mut c_void) -> *mut c_void;
    /// Enqueues a no-arg call of `fn` on the VM's microtask queue.
    pub fn rjs_queue_microtask(g: *mut c_void, f: u64) -> i32;
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
