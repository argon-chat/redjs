//! Safe-ish Rust API over the redjs ABI: `Context` + `Value` + host-function
//! registration. This is the "full control from Rust" layer — everything the
//! C ABI exposes, with RAII and Results. The C# side gets the same features
//! through `ffi.rs`.
//!
//! Values are plain `Value(u64)` copies — cheap, but only GC-safe while on
//! the stack of a thread that's inside the VM, or while protected. Use
//! [`Context::protect`] to hold one long-term.

use core::ffi::c_void;
use std::sync::OnceLock;

pub use crate::bytecode::Bytecode;

use crate::ffi;
use crate::glue::{self, RjsAbInfo, RjsHeapStats, RjsLayout, RjsStr, RjsTaInfo};
use crate::trampoline;

static LAYOUT: OnceLock<RjsLayout> = OnceLock::new();

/// The ABI layout (CallFrame offsets + NaN-boxing constants), fetched once.
pub fn layout() -> &'static RjsLayout {
    LAYOUT.get_or_init(|| unsafe {
        let mut l = RjsLayout::default();
        glue::rjs_layout(&mut l);
        l
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Value(pub u64);

impl Value {
    pub fn undefined() -> Value {
        Value(layout().value_undefined)
    }
    pub fn null() -> Value {
        Value(layout().value_null)
    }
    pub fn boolean(b: bool) -> Value {
        Value(if b { layout().value_true } else { layout().value_false })
    }
    pub fn int32(i: i32) -> Value {
        Value(layout().tag_number | (i as u32 as u64))
    }
    pub fn double(d: f64) -> Value {
        let i = d as i32;
        if i as f64 == d && !(i == 0 && d.is_sign_negative()) {
            return Value::int32(i);
        }
        Value((d.to_bits() as i64).wrapping_add(layout().double_encode_offset as i64) as u64)
    }
    pub fn number(d: f64) -> Value {
        Value::double(d)
    }

    pub fn is_undefined(&self) -> bool {
        self.0 == layout().value_undefined
    }
    pub fn is_null(&self) -> bool {
        self.0 == layout().value_null
    }
    pub fn is_boolean(&self) -> bool {
        self.0 == layout().value_true || self.0 == layout().value_false
    }
    pub fn is_int32(&self) -> bool {
        (self.0 & layout().tag_number) == layout().tag_number
    }
    pub fn is_number(&self) -> bool {
        (self.0 & layout().tag_number) != 0
    }
    pub fn is_cell(&self) -> bool {
        self.0 != 0 && (self.0 & layout().not_cell_mask) == 0
    }

    /// Decodes without coercion; `None` when not a number.
    pub fn as_number(&self) -> Option<f64> {
        if self.is_int32() {
            return Some(self.0 as u32 as i32 as f64);
        }
        if self.is_number() {
            return Some(f64::from_bits(
                (self.0 as i64).wrapping_sub(layout().double_encode_offset as i64) as u64,
            ));
        }
        None
    }
    pub fn as_bool(&self) -> Option<bool> {
        if self.0 == layout().value_true {
            Some(true)
        } else if self.0 == layout().value_false {
            Some(false)
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub enum Error {
    /// A JS exception; holds the exception value (alive only per the GC
    /// contract — extract info immediately).
    Exception(Value),
    BadArg,
    Exhausted,
    Engine(i32),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Exception(v) => write!(f, "JS exception (value {:#x})", v.0),
            Error::BadArg => write!(f, "invalid argument"),
            Error::Exhausted => write!(f, "resource exhausted"),
            Error::Engine(c) => write!(f, "engine error {c}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn check(status: i32, out: u64) -> Result<Value> {
    match status {
        0 => Ok(Value(out)),
        1 => Err(Error::Exception(Value(out))),
        2 => Err(Error::BadArg),
        3 => Err(Error::Exhausted),
        c => Err(Error::Engine(c)),
    }
}

/// Interned property name. Create once, use for every get/set. Holds the
/// owning context for the drop path (atom-table removal needs the VM lock).
pub struct Atom {
    ptr: *mut c_void,
    g: *mut c_void,
}

impl Drop for Atom {
    fn drop(&mut self) {
        unsafe { glue::rjs_atom_free(self.g, self.ptr) }
    }
}

/// Typed-array element kinds (stable RjsTaKind ids).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum TaKind {
    I8 = 1,
    U8 = 2,
    U8Clamped = 3,
    I16 = 4,
    U16 = 5,
    I32 = 6,
    U32 = 7,
    F32 = 8,
    F64 = 9,
    BigI64 = 10,
    BigU64 = 11,
}

/// One VM + global object. Not `Send`: use from one thread (or hand-manage
/// the API lock).
pub struct Context {
    g: *mut c_void,
    _not_send: core::marker::PhantomData<*mut ()>,
}

impl Context {
    /// Initializes JSC (idempotent) and creates a context. Contexts should
    /// live for the process lifetime (teardown crashes in this prebuilt).
    pub fn new() -> Context {
        Self::with_options("")
    }

    pub fn with_options(options: &str) -> Context {
        let c = std::ffi::CString::new(options).unwrap();
        unsafe {
            glue::rjs_init(if options.is_empty() { core::ptr::null() } else { c.as_ptr() });
            let g = ffi::redjs_context_create();
            assert!(!g.is_null(), "JSGlobalContextCreate failed");
            Context { g, _not_send: core::marker::PhantomData }
        }
    }

    pub fn raw(&self) -> *mut c_void {
        self.g
    }

    pub fn global(&self) -> Value {
        Value(unsafe { glue::rjs_global(self.g) })
    }

    // -- eval ---------------------------------------------------------------

    pub fn eval(&self, src: &str) -> Result<Value> {
        self.eval_named(src, "eval")
    }

    pub fn eval_named(&self, src: &str, url: &str) -> Result<Value> {
        let _profile_context = crate::profiling::ContextScope::enter(self.g as usize);
        let _profile = crate::profiling::span("js", "Evaluate JavaScript");
        let mut out = 0u64;
        let status = unsafe {
            glue::rjs_eval8(self.g, src.as_ptr(), src.len(), url.as_ptr(), url.len(), &mut out)
        };
        check(status, out)
    }

    // -- values / strings ---------------------------------------------------

    pub fn kind(&self, v: Value) -> u32 {
        unsafe { glue::rjs_kind(self.g, v.0) }
    }

    pub fn to_number(&self, v: Value) -> Result<f64> {
        let mut d = 0.0f64;
        let status = unsafe { glue::rjs_to_number(self.g, v.0, &mut d) };
        check(status, 0).map(|_| d)
    }

    /// ToString + copy out to a Rust `String`.
    pub fn to_rust_string(&self, v: Value) -> Result<String> {
        let mut sv = 0u64;
        let status = unsafe { glue::rjs_to_string(self.g, v.0, &mut sv) };
        check(status, sv)?;
        self.read_string(Value(sv))
    }

    /// Reads a JS string value's characters (must already be a string).
    pub fn read_string(&self, v: Value) -> Result<String> {
        let mut info = RjsStr::default();
        let status = unsafe { glue::rjs_string_info(self.g, v.0, &mut info) };
        check(status, 0)?;
        if info.len == 0 {
            return Ok(String::new());
        }
        unsafe {
            if info.is8bit != 0 {
                let bytes = core::slice::from_raw_parts(info.ptr as *const u8, info.len as usize);
                Ok(bytes.iter().map(|&b| b as char).collect())
            } else {
                let units = core::slice::from_raw_parts(info.ptr as *const u16, info.len as usize);
                Ok(String::from_utf16_lossy(units))
            }
        }
    }

    pub fn string(&self, s: &str) -> Value {
        let mut out = 0u64;
        let status = unsafe { glue::rjs_string8(self.g, s.as_ptr(), s.len(), &mut out) };
        check(status, out).expect("string creation failed")
    }

    pub fn protect(&self, v: Value) {
        unsafe { ffi::redjs_protect(self.g, v.0) }
    }

    pub fn unprotect(&self, v: Value) {
        unsafe { ffi::redjs_unprotect(self.g, v.0) }
    }

    // -- atoms / properties ---------------------------------------------------

    pub fn atom(&self, name: &str) -> Atom {
        let ptr = unsafe { glue::rjs_atom8(self.g, name.as_ptr(), name.len()) };
        assert!(!ptr.is_null(), "invalid atom name");
        Atom { ptr, g: self.g }
    }

    pub fn get(&self, base: Value, atom: &Atom) -> Result<Value> {
        let mut out = 0u64;
        let status = unsafe { glue::rjs_get(self.g, base.0, atom.ptr, &mut out) };
        check(status, out)
    }

    pub fn set(&self, obj: Value, atom: &Atom, v: Value) -> Result<()> {
        let status = unsafe { glue::rjs_set(self.g, obj.0, atom.ptr, v.0) };
        check(status, 0).map(|_| ())
    }

    pub fn define(&self, obj: Value, atom: &Atom, v: Value, attrs: u32) -> Result<()> {
        let status = unsafe { glue::rjs_define(self.g, obj.0, atom.ptr, v.0, attrs) };
        check(status, 0).map(|_| ())
    }

    pub fn get_index(&self, base: Value, i: u32) -> Result<Value> {
        let mut out = 0u64;
        let status = unsafe { glue::rjs_get_index(self.g, base.0, i, &mut out) };
        check(status, out)
    }

    pub fn set_index(&self, obj: Value, i: u32, v: Value) -> Result<()> {
        let status = unsafe { glue::rjs_set_index(self.g, obj.0, i, v.0) };
        check(status, 0).map(|_| ())
    }

    /// Convenience: `get` by name on the global object.
    pub fn global_get(&self, name: &str) -> Result<Value> {
        self.get(self.global(), &self.atom(name))
    }

    /// Convenience: define on the global object.
    pub fn global_set(&self, name: &str, v: Value) -> Result<()> {
        self.define(self.global(), &self.atom(name), v, 0)
    }

    // -- objects / arrays / functions ----------------------------------------

    pub fn object(&self) -> Value {
        let mut out = 0u64;
        let status = unsafe { glue::rjs_object_create(self.g, Value::undefined().0, &mut out) };
        check(status, out).expect("object creation failed")
    }

    pub fn array(&self, len: u32) -> Result<Value> {
        let mut out = 0u64;
        let status = unsafe { glue::rjs_array_create(self.g, len, &mut out) };
        check(status, out)
    }

    pub fn array_length(&self, v: Value) -> Result<u32> {
        let mut out = 0u32;
        let status = unsafe { glue::rjs_array_length(self.g, v.0, &mut out) };
        check(status, 0).map(|_| out)
    }

    /// Registers a Rust closure as a JS host function. The closure receives
    /// `(global, callframe)`; use [`HostArgs`] to decode arguments.
    /// Recursive entry into the same mutable Rust closure throws a TypeError;
    /// calling a different host function remains supported.
    pub fn function<F>(&self, name: &str, arity: u32, f: F) -> Result<Value>
    where
        F: FnMut(*mut c_void, *mut c_void) -> u64 + 'static,
    {
        let entry = trampoline::acquire_closure(Box::new(f)).ok_or(Error::Exhausted)?;
        let name16: Vec<u16> = name.encode_utf16().collect();
        let mut out = 0u64;
        let status = unsafe {
            glue::rjs_function_create(self.g, name16.as_ptr(), name16.len(), arity, entry, &mut out)
        };
        check(status, out)
    }

    /// Registers a raw `extern "sysv64"` host function (zero-overhead path).
    ///
    /// # Safety
    /// `f` must follow the JSC host convention for this build:
    /// `extern "sysv64" fn(*mut c_void, *mut c_void) -> u64`.
    pub unsafe fn function_sysv(&self, name: &str, arity: u32, f: *const c_void) -> Result<Value> {
        let name16: Vec<u16> = name.encode_utf16().collect();
        let mut out = 0u64;
        let status = glue::rjs_function_create(self.g, name16.as_ptr(), name16.len(), arity, f, &mut out);
        check(status, out)
    }

    pub fn call(&self, f: Value, this: Value, args: &[Value]) -> Result<Value> {
        let _profile = unsafe { crate::profiling::js_span(self.g, f.0, "JavaScript callback") };
        let mut out = 0u64;
        let status = unsafe {
            glue::rjs_call(self.g, f.0, this.0, args.as_ptr() as *const u64, args.len() as u32, &mut out)
        };
        check(status, out)
    }

    pub fn construct(&self, ctor: Value, args: &[Value]) -> Result<Value> {
        let mut out = 0u64;
        let status = unsafe {
            glue::rjs_construct(self.g, ctor.0, args.as_ptr() as *const u64, args.len() as u32, &mut out)
        };
        check(status, out)
    }

    // -- errors ---------------------------------------------------------------

    pub fn error(&self, kind: u32, msg: &str) -> Value {
        let msg16: Vec<u16> = msg.encode_utf16().collect();
        let mut out = 0u64;
        let status = unsafe { glue::rjs_error_create(self.g, kind, msg16.as_ptr(), msg16.len(), &mut out) };
        check(status, out).expect("error creation failed")
    }

    pub fn throw(&self, err: Value) {
        unsafe { glue::rjs_throw(self.g, err.0) }
    }

    /// (message, stack) of an exception value.
    pub fn error_info(&self, err: Value) -> (String, String) {
        let mut msg = 0u64;
        let mut stack = 0u64;
        unsafe { glue::rjs_error_info(self.g, err.0, &mut msg, &mut stack) };
        (
            self.read_string(Value(msg)).unwrap_or_default(),
            self.read_string(Value(stack)).unwrap_or_default(),
        )
    }

    // -- buffers ---------------------------------------------------------------

    /// Zero-copy ArrayBuffer over host memory. Caller keeps ownership when
    /// `dealloc` is `None` — detach before freeing the memory.
    ///
    /// # Safety
    /// `bytes..bytes+len` must stay valid until GC-with-deallocator or detach.
    pub unsafe fn arraybuffer_external(
        &self,
        bytes: *mut c_void,
        len: usize,
        dealloc: crate::capi::JSTypedArrayBytesDeallocator,
        dealloc_ctx: *mut c_void,
    ) -> Result<Value> {
        let mut out = 0u64;
        let status = ffi::redjs_arraybuffer_external(self.g, bytes, len, core::mem::transmute(dealloc), dealloc_ctx, &mut out);
        check(status, out)
    }

    pub fn arraybuffer_info(&self, v: Value) -> Result<RjsAbInfo> {
        let mut info = RjsAbInfo::default();
        let status = unsafe { glue::rjs_arraybuffer_info(self.g, v.0, &mut info) };
        check(status, 0).map(|_| info)
    }

    pub fn arraybuffer_detach(&self, v: Value) -> Result<()> {
        let status = unsafe { glue::rjs_arraybuffer_detach(self.g, v.0) };
        check(status, 0).map(|_| ())
    }

    pub fn typedarray(&self, kind: TaKind, len: usize) -> Result<Value> {
        let mut out = 0u64;
        let status = unsafe { ffi::redjs_typedarray_create(self.g, kind as u32, len, &mut out) };
        check(status, out)
    }

    /// # Safety — see [`Context::arraybuffer_external`].
    pub unsafe fn typedarray_external(
        &self,
        kind: TaKind,
        bytes: *mut c_void,
        byte_len: usize,
        dealloc: crate::capi::JSTypedArrayBytesDeallocator,
        dealloc_ctx: *mut c_void,
    ) -> Result<Value> {
        let mut out = 0u64;
        let status = ffi::redjs_typedarray_external(self.g, kind as u32, bytes, byte_len, core::mem::transmute(dealloc), dealloc_ctx, &mut out);
        check(status, out)
    }

    pub fn typedarray_info(&self, v: Value) -> Result<RjsTaInfo> {
        let mut info = RjsTaInfo::default();
        let status = unsafe { glue::rjs_typedarray_info(self.g, v.0, &mut info) };
        check(status, 0).map(|_| info)
    }

    // -- promises ---------------------------------------------------------------

    /// Returns (promise, resolveFn, rejectFn).
    pub fn promise(&self) -> Result<(Value, Value, Value)> {
        let mut resolve = 0u64;
        let mut reject = 0u64;
        let mut out = 0u64;
        let status = unsafe { ffi::redjs_promise_create(self.g, &mut resolve, &mut reject, &mut out) };
        check(status, out).map(|p| (p, Value(resolve), Value(reject)))
    }

    /// (status, result): 0 pending, 1 fulfilled, 2 rejected.
    pub fn promise_info(&self, v: Value) -> Result<(u32, Value)> {
        let mut st = 0u32;
        let mut res = 0u64;
        let status = unsafe { glue::rjs_promise_info(self.g, v.0, &mut st, &mut res) };
        check(status, 0).map(|_| (st, Value(res)))
    }

    // -- vm ---------------------------------------------------------------------

    /// A microtask checkpoint ([`crate::web::checkpoint`]): drains the queue,
    /// then runs the post-checkpoint hooks when no JS is on the stack.
    pub fn drain_microtasks(&self) {
        unsafe {
            crate::web::checkpoint(self.g);
        }
    }

    /// This context's process-unique id ([`crate::context::ContextId`]) —
    /// unlike [`Context::raw`], never shared with a later context.
    pub fn id(&self) -> crate::context::ContextId {
        crate::context::id_of(self.g).expect("a redjs Context is always registered")
    }

    pub fn gc(&self) {
        unsafe { glue::rjs_gc(self.g, 0) }
    }

    pub fn heap_stats(&self) -> RjsHeapStats {
        let mut s = RjsHeapStats::default();
        unsafe { glue::rjs_heap_stats(self.g, &mut s) };
        s
    }
}

/// Zero-FFI access to host-call arguments via the exported CallFrame layout —
/// the same technique the generated C# uses.
pub struct HostArgs {
    cf: *mut c_void,
}

impl HostArgs {
    /// # Safety: `cf` must be the CallFrame passed to a host function, used
    /// only during that call.
    pub unsafe fn new(cf: *mut c_void) -> HostArgs {
        HostArgs { cf }
    }

    pub fn argc(&self) -> u32 {
        let l = layout();
        let n = unsafe { *((self.cf as usize + l.cf_argc_offset as usize) as *const u32) };
        n.saturating_sub(1) // excludes `this`
    }

    pub fn this(&self) -> Value {
        let l = layout();
        Value(unsafe { *((self.cf as usize + l.cf_this_offset as usize) as *const u64) })
    }

    /// `undefined` when out of range (JS semantics).
    pub fn arg(&self, i: u32) -> Value {
        if i >= self.argc() {
            return Value::undefined();
        }
        let l = layout();
        Value(unsafe {
            *((self.cf as usize + l.cf_args_offset as usize + (i as usize) * 8) as *const u64)
        })
    }
}
