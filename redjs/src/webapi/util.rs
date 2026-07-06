//! Shared machinery for the native Web-API classes: value codecs, CallFrame
//! access, string/byte bridges, the class builder (prototype + constructable
//! native function + accessors), typed host-object data, private-name
//! internal slots and per-context state.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::capi;
use crate::glue;

pub const UNDEF: u64 = 0xa;
pub const NULL: u64 = 0x2;
pub const TRUE: u64 = 0x7;
pub const FALSE: u64 = 0x6;
pub const TAG_NUMBER: u64 = 0xfffe_0000_0000_0000;

#[inline]
pub fn enc_i32(i: i32) -> u64 {
    TAG_NUMBER | (i as u32 as u64)
}

#[inline]
pub fn enc_bool(b: bool) -> u64 {
    if b { TRUE } else { FALSE }
}

#[inline]
pub fn enc_f64(d: f64) -> u64 {
    let i = d as i32;
    if i as f64 == d && !(i == 0 && d.is_sign_negative()) {
        return enc_i32(i);
    }
    (d.to_bits() as i64).wrapping_add(1i64 << 49) as u64
}

#[inline]
pub fn dec_f64(v: u64) -> Option<f64> {
    if (v & TAG_NUMBER) == TAG_NUMBER {
        return Some(v as u32 as i32 as f64);
    }
    if (v & TAG_NUMBER) != 0 {
        return Some(f64::from_bits((v as i64).wrapping_sub(1i64 << 49) as u64));
    }
    None
}

#[inline]
pub fn is_cell(v: u64) -> bool {
    v != 0 && (v & (TAG_NUMBER | 0x2)) == 0
}

#[inline]
pub fn truthy_flag(v: u64) -> bool {
    v == TRUE
}

// ---- CallFrame --------------------------------------------------------------

#[inline]
pub unsafe fn cf_argc(cf: *mut c_void) -> u32 {
    (*((cf as usize + 32) as *const u32)).saturating_sub(1)
}

#[inline]
pub unsafe fn cf_this(cf: *mut c_void) -> u64 {
    *((cf as usize + 40) as *const u64)
}

#[inline]
pub unsafe fn cf_arg(cf: *mut c_void, i: u32) -> u64 {
    if i < cf_argc(cf) {
        *((cf as usize + 48 + (i as usize) * 8) as *const u64)
    } else {
        UNDEF
    }
}

/// new.target inside a construct callback (JSC keeps it in the this slot).
#[inline]
pub unsafe fn cf_new_target(cf: *mut c_void) -> u64 {
    cf_this(cf)
}

// ---- strings / bytes --------------------------------------------------------

pub unsafe fn js_string(g: *mut c_void, s: &str) -> u64 {
    let mut out = UNDEF;
    glue::rjs_string8(g, s.as_ptr(), s.len(), &mut out);
    out
}

/// ToString-coercing read (spec argument conversion).
pub unsafe fn read_string(g: *mut c_void, v: u64) -> Option<String> {
    let mut sv = v;
    if glue::rjs_kind(g, v) != 5 {
        let mut out = 0u64;
        if glue::rjs_to_string(g, v, &mut out) != 0 {
            return None;
        }
        sv = out;
    }
    let mut info = crate::ffi::RjsStr::default();
    if glue::rjs_string_info(g, sv, &mut info) != 0 {
        return None;
    }
    if info.len == 0 {
        return Some(String::new());
    }
    if info.is8bit != 0 {
        let bytes = core::slice::from_raw_parts(info.ptr as *const u8, info.len as usize);
        Some(bytes.iter().map(|&b| b as char).collect())
    } else {
        let units = core::slice::from_raw_parts(info.ptr as *const u16, info.len as usize);
        Some(String::from_utf16_lossy(units))
    }
}

pub unsafe fn make_u8(g: *mut c_void, bytes: &[u8]) -> u64 {
    let mut out = UNDEF;
    if crate::ffi::redjs_typedarray_create(g, 2, bytes.len(), &mut out) != 0 {
        return UNDEF;
    }
    if !bytes.is_empty() {
        let mut ta = crate::ffi::RjsTaInfo::default();
        if glue::rjs_typedarray_info(g, out, &mut ta) == 0 && !ta.data.is_null() {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), ta.data as *mut u8, bytes.len());
        }
    }
    out
}

/// Bytes of an ArrayBuffer or any typed-array view (pointer into JS memory —
/// copy before any allocation/JS call).
pub unsafe fn bufferish<'a>(g: *mut c_void, v: u64) -> Option<&'a [u8]> {
    let mut ta = crate::ffi::RjsTaInfo::default();
    if glue::rjs_typedarray_info(g, v, &mut ta) == 0 {
        if ta.detached != 0 || ta.data.is_null() {
            return Some(&[]);
        }
        return Some(core::slice::from_raw_parts(ta.data as *const u8, ta.byte_length as usize));
    }
    let mut ab = crate::ffi::RjsAbInfo::default();
    if glue::rjs_arraybuffer_info(g, v, &mut ab) == 0 {
        if ab.detached != 0 || ab.data.is_null() {
            return Some(&[]);
        }
        return Some(core::slice::from_raw_parts(ab.data as *const u8, ab.byte_length as usize));
    }
    None
}

// ---- properties ---------------------------------------------------------------

pub unsafe fn atom(g: *mut c_void, name: &str) -> *mut c_void {
    glue::rjs_atom8(g, name.as_ptr(), name.len())
}

pub unsafe fn atom_priv(g: *mut c_void, name: &str) -> *mut c_void {
    glue::rjs_atom_private(g, name.as_ptr(), name.len())
}

/// One-shot define by name (cold paths: prototype building etc).
pub unsafe fn define(g: *mut c_void, obj: u64, name: &str, v: u64, attrs: u32) {
    let a = atom(g, name);
    glue::rjs_define(g, obj, a, v, attrs);
    glue::rjs_atom_free(g, a);
}

pub unsafe fn get_a(g: *mut c_void, obj: u64, a: *mut c_void) -> u64 {
    let mut out = UNDEF;
    if glue::rjs_get(g, obj, a, &mut out) != 0 {
        return UNDEF;
    }
    out
}

pub unsafe fn get(g: *mut c_void, obj: u64, name: &str) -> u64 {
    let a = atom(g, name);
    let v = get_a(g, obj, a);
    glue::rjs_atom_free(g, a);
    v
}

pub unsafe fn new_object(g: *mut c_void, proto: u64) -> u64 {
    let mut out = UNDEF;
    glue::rjs_object_create(g, proto, &mut out);
    out
}

pub unsafe fn new_array(g: *mut c_void) -> u64 {
    let mut out = UNDEF;
    glue::rjs_array_create(g, 0, &mut out);
    out
}

pub unsafe fn array_len(g: *mut c_void, arr: u64) -> u32 {
    let mut n = 0u32;
    glue::rjs_array_length(g, arr, &mut n);
    n
}

pub unsafe fn get_i(g: *mut c_void, arr: u64, i: u32) -> u64 {
    let mut out = UNDEF;
    glue::rjs_get_index(g, arr, i, &mut out);
    out
}

pub unsafe fn set_i(g: *mut c_void, arr: u64, i: u32, v: u64) {
    glue::rjs_set_index(g, arr, i, v);
}

pub unsafe fn call(g: *mut c_void, f: u64, this: u64, args: &[u64]) -> Result<u64, u64> {
    let mut out = UNDEF;
    let rc = glue::rjs_call(g, f, this, args.as_ptr(), args.len() as u32, &mut out);
    if rc == 0 { Ok(out) } else { Err(out) }
}

/// Throws a TypeError into the VM; returns undefined (host-fn return value).
pub unsafe fn throw_type_error(g: *mut c_void, msg: &str) -> u64 {
    let m: Vec<u16> = msg.encode_utf16().collect();
    let mut err = UNDEF;
    glue::rjs_error_create(g, 1, m.as_ptr(), m.len(), &mut err);
    glue::rjs_throw(g, err);
    UNDEF
}

pub unsafe fn throw_error(g: *mut c_void, kind: u32, msg: &str) -> u64 {
    let m: Vec<u16> = msg.encode_utf16().collect();
    let mut err = UNDEF;
    glue::rjs_error_create(g, kind, m.as_ptr(), m.len(), &mut err);
    glue::rjs_throw(g, err);
    UNDEF
}

// ---- promises -------------------------------------------------------------------

pub unsafe fn resolved_promise(g: *mut c_void, v: u64) -> u64 {
    let (mut res, mut rej, mut p) = (0u64, 0u64, 0u64);
    if crate::ffi::redjs_promise_create(g, &mut res, &mut rej, &mut p) != 0 {
        return UNDEF;
    }
    let _ = call(g, res, UNDEF, &[v]);
    p
}

pub unsafe fn rejected_promise(g: *mut c_void, kind: u32, msg: &str) -> u64 {
    let (mut res, mut rej, mut p) = (0u64, 0u64, 0u64);
    if crate::ffi::redjs_promise_create(g, &mut res, &mut rej, &mut p) != 0 {
        return UNDEF;
    }
    let m: Vec<u16> = msg.encode_utf16().collect();
    let mut err = UNDEF;
    glue::rjs_error_create(g, kind, m.as_ptr(), m.len(), &mut err);
    let _ = call(g, rej, UNDEF, &[err]);
    p
}

// ---- native class builder ----------------------------------------------------------

pub type HostFn = unsafe extern "sysv64" fn(*mut c_void, *mut c_void) -> u64;

/// The generic "constructor called without new" / callable-only thrower.
pub unsafe extern "sysv64" fn ctor_requires_new(g: *mut c_void, _cf: *mut c_void) -> u64 {
    throw_type_error(g, "Constructor requires 'new'")
}

pub unsafe extern "sysv64" fn illegal_constructor(g: *mut c_void, _cf: *mut c_void) -> u64 {
    throw_type_error(g, "Illegal constructor")
}

pub struct ClassSpec<'a> {
    pub name: &'a str,
    pub arity: u32,
    /// None → `new X()` throws "Illegal constructor" (spec: ImageBitmap etc).
    pub construct: Option<HostFn>,
    /// 0 → Object.prototype.
    pub parent_proto: u64,
    pub methods: &'a [(&'a str, u32, HostFn)],
    pub getters: &'a [(&'a str, HostFn)],
    pub accessors: &'a [(&'a str, HostFn, HostFn)],
}

pub struct Class {
    pub ctor: u64,
    pub proto: u64,
}

unsafe fn make_fn(g: *mut c_void, name: &str, arity: u32, f: HostFn) -> u64 {
    let n16: Vec<u16> = name.encode_utf16().collect();
    let mut out = UNDEF;
    glue::rjs_function_create(g, n16.as_ptr(), n16.len(), arity, f as *const c_void, &mut out);
    out
}

/// Builds a standards-shaped class: prototype object (methods + accessors,
/// all non-enumerable per spec), a constructable native function, `.prototype`
/// (RO/DontEnum/DontDelete) and `prototype.constructor` (DontEnum) wiring,
/// and defines it on the global object.
pub unsafe fn define_class(g: *mut c_void, spec: &ClassSpec) -> Class {
    let proto = new_object(g, if spec.parent_proto != 0 { spec.parent_proto } else { UNDEF });

    for (name, arity, f) in spec.methods {
        define(g, proto, name, make_fn(g, name, *arity, *f), 2 /* DontEnum */);
    }
    for (name, f) in spec.getters {
        let getter = make_fn(g, &format!("get {name}"), 0, *f);
        let a = atom(g, name);
        glue::rjs_define_accessor(g, proto, a, getter, UNDEF, 2);
        glue::rjs_atom_free(g, a);
    }
    for (name, getf, setf) in spec.accessors {
        let getter = make_fn(g, &format!("get {name}"), 0, *getf);
        let setter = make_fn(g, &format!("set {name}"), 1, *setf);
        let a = atom(g, name);
        glue::rjs_define_accessor(g, proto, a, getter, setter, 2);
        glue::rjs_atom_free(g, a);
    }

    let n16: Vec<u16> = spec.name.encode_utf16().collect();
    let mut ctor = UNDEF;
    let (call_fn, construct_fn): (HostFn, HostFn) = match spec.construct {
        Some(c) => (ctor_requires_new, c),
        None => (illegal_constructor, illegal_constructor),
    };
    glue::rjs_function_create_full(
        g,
        n16.as_ptr(),
        n16.len(),
        spec.arity,
        call_fn as *const c_void,
        construct_fn as *const c_void,
        &mut ctor,
    );

    define(g, ctor, "prototype", proto, 1 | 2 | 4);
    define(g, proto, "constructor", ctor, 2);
    define(g, glue::rjs_global(g), spec.name, ctor, 2);

    crate::ffi::redjs_protect(g, proto);
    crate::ffi::redjs_protect(g, ctor);
    Class { ctor, proto }
}

/// Spec OrdinaryCreateFromConstructor: instance prototype comes from
/// new.target.prototype (subclassing works), with a class default fallback.
pub unsafe fn ordinary_create(g: *mut c_void, new_target: u64, fallback_proto: u64) -> u64 {
    let mut proto = if is_cell(new_target) { get(g, new_target, "prototype") } else { UNDEF };
    if !is_cell(proto) {
        proto = fallback_proto;
    }
    new_object(g, proto)
}

// ---- typed host objects (Rust data + finalizer) ----------------------------------------

/// Per-T finalizer for host objects carrying `Box<T>`. Runs on GC threads —
/// only drops the box, never touches JS.
pub unsafe extern "C" fn finalize_box<T>(obj: capi::JSObjectRef) {
    let data = capi::JSObjectGetPrivate(obj);
    if !data.is_null() {
        drop(Box::from_raw(data as *mut T));
    }
}

pub unsafe fn make_host_class<T>(name: &str) -> capi::JSClassRef {
    let cname = std::ffi::CString::new(name).unwrap();
    let mut def = capi::JSClassDefinition::empty();
    def.class_name = cname.as_ptr();
    def.finalize = Some(finalize_box::<T>);
    capi::JSClassCreate(&def)
}

/// Creates an instance of a host class with `data`, prototype from
/// new.target (or the class default).
pub unsafe fn host_create<T>(g: *mut c_void, cls: capi::JSClassRef, proto: u64, data: T) -> u64 {
    let obj = capi::JSObjectMake(g, cls, Box::into_raw(Box::new(data)) as *mut c_void);
    if is_cell(proto) {
        capi::JSObjectSetPrototype(g, obj, proto as usize as capi::JSValueRef);
    }
    obj as usize as u64
}

/// Brand-checked data access (spec: wrong-brand `this` → None → TypeError).
pub unsafe fn host_data<'a, T>(g: *mut c_void, cls: capi::JSClassRef, v: u64) -> Option<&'a mut T> {
    if !is_cell(v) || !capi::JSValueIsObjectOfClass(g, v as usize as capi::JSValueRef, cls) {
        return None;
    }
    let p = capi::JSObjectGetPrivate(v as usize as capi::JSObjectRef);
    if p.is_null() {
        return None;
    }
    Some(&mut *(p as *mut T))
}

// ---- per-context state --------------------------------------------------------------------

/// Everything the native web classes need per context: cached atoms (public
/// and private), protected prototype/constructor values, host class refs.
/// Created once at install, never freed (contexts are process-lifetime).
pub struct WebState {
    pub g: *mut c_void,
    /// The globalThis proxy (what JS sees as `globalThis`/`window`) — NOT the
    /// JSGlobalObject itself; EventTarget slots always live on the latter.
    pub global_proxy: u64,

    // Private-name internal slots.
    pub s_listeners: *mut c_void, // EventTarget [[listeners]] → JS Array
    pub s_event: *mut c_void,     // Event [[state]] → plain JS object
    pub s_image: *mut c_void,     // HTMLImageElement [[bitmap]] → ImageBitmap host obj
    pub s_usp: *mut c_void,       // URL [[searchParams]] cache

    // Common public atoms (hot in dispatch).
    pub a_type: *mut c_void,
    pub a_cb: *mut c_void,
    pub a_once: *mut c_void,
    pub a_handle_event: *mut c_void,

    // Protected classes.
    pub event: Class,
    pub custom_event: Class,
    pub event_target: Class,
    pub blob_class: capi::JSClassRef,
    pub blob: Class,
    pub url_class: capi::JSClassRef,
    pub url: Class,
    pub usp_class: capi::JSClassRef,
    pub usp: Class,
    pub bitmap_class: capi::JSClassRef,
    pub bitmap: Class,
    pub image: Class,
    pub response_class: capi::JSClassRef,
    pub response: Class,

    // DOM.
    pub document: u64,
    pub elements: u64, // plain object: id → element
    pub json_parse: u64,
}

struct SendPtr(*mut WebState);
unsafe impl Send for SendPtr {}

fn states() -> &'static Mutex<HashMap<usize, SendPtr>> {
    static S: OnceLock<Mutex<HashMap<usize, SendPtr>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn put_state(g: *mut c_void, st: WebState) -> &'static WebState {
    let leaked: &'static WebState = Box::leak(Box::new(st));
    states().lock().unwrap().insert(g as usize, SendPtr(leaked as *const WebState as *mut WebState));
    leaked
}

/// Panics if the web API was not installed for this context. Shared ref —
/// WebState is immutable after install.
pub fn state(g: *mut c_void) -> &'static WebState {
    let p = states().lock().unwrap().get(&(g as usize)).map(|s| s.0).expect("redjs webapi not installed");
    unsafe { &*p }
}

pub fn has_state(g: *mut c_void) -> bool {
    states().lock().unwrap().contains_key(&(g as usize))
}
