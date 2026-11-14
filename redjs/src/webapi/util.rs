//! Shared machinery for the native Web-API classes: value codecs, CallFrame
//! access, string/byte bridges, the class builder (prototype + constructable
//! native function + accessors), typed host-object data, private-name
//! internal slots and per-context state.

use core::ffi::c_void;
use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
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

/// ECMAScript ToBoolean, without an engine call for booleans, undefined, null and numbers.
pub unsafe fn to_bool(g: *mut c_void, v: u64) -> bool {
    match v {
        TRUE => true,
        FALSE | UNDEF | NULL => false,
        _ => match dec_f64(v) {
            Some(d) => d != 0.0 && !d.is_nan(),
            None => glue::rjs_to_boolean(g, v) != 0,
        },
    }
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

/// ToNumber-coercing read (spec argument conversion). `undefined` → NaN, like
/// the language: callers that want a default must test for `UNDEF` first.
pub unsafe fn read_number(g: *mut c_void, v: u64) -> f64 {
    if let Some(d) = dec_f64(v) {
        return d;
    }
    let mut out = 0.0f64;
    if glue::rjs_to_number(g, v, &mut out) != 0 {
        return f64::NAN;
    }
    out
}

// ---- conversions that PROPAGATE ------------------------------------------------
//
// `Err(())` from any of these means: the conversion threw, and that exception
// is now pending in the VM again — the host function returns `UNDEF` and JS
// sees exactly what a getter / `valueOf` / `toString` threw. The swallowing
// helpers above stay for the many call sites whose spec text has no `?`.

/// ToNumber, rethrowing what it threw.
pub unsafe fn to_number(g: *mut c_void, v: u64) -> Result<f64, ()> {
    if let Some(d) = dec_f64(v) {
        return Ok(d);
    }
    let (mut d, mut exc) = (0.0f64, UNDEF);
    if glue::rjs_to_number_x(g, v, &mut d, &mut exc) != 0 {
        glue::rjs_throw(g, exc);
        return Err(());
    }
    Ok(d)
}

/// ToString as a JS string value, rethrowing what it threw.
pub unsafe fn to_js_string(g: *mut c_void, v: u64) -> Result<u64, ()> {
    if glue::rjs_kind(g, v) == 5 {
        return Ok(v);
    }
    let mut out = UNDEF;
    if glue::rjs_to_string(g, v, &mut out) != 0 {
        glue::rjs_throw(g, out);
        return Err(());
    }
    Ok(out)
}

/// `? Get(obj, name)`: getters run, and what they throw is rethrown.
pub unsafe fn try_get(g: *mut c_void, obj: u64, name: &str) -> Result<u64, ()> {
    let a = atom(g, name);
    let mut out = UNDEF;
    let rc = glue::rjs_get(g, obj, a, &mut out);
    glue::rjs_atom_free(g, a);
    if rc != 0 {
        glue::rjs_throw(g, out);
        return Err(());
    }
    Ok(out)
}

/// A JS string's code units, copied out (lone surrogates survive — the
/// lossy `read_string` would turn them into U+FFFD). `None` for a non-string.
pub unsafe fn read_string16(g: *mut c_void, v: u64) -> Option<Vec<u16>> {
    let mut info = crate::ffi::RjsStr::default();
    if glue::rjs_string_info(g, v, &mut info) != 0 {
        return None;
    }
    if info.len == 0 {
        return Some(Vec::new());
    }
    Some(if info.is8bit != 0 {
        core::slice::from_raw_parts(info.ptr as *const u8, info.len as usize).iter().map(|&b| b as u16).collect()
    } else {
        core::slice::from_raw_parts(info.ptr as *const u16, info.len as usize).to_vec()
    })
}

/// A JS string from UTF-16 code units (no validation: lone surrogates allowed).
pub unsafe fn js_string16(g: *mut c_void, units: &[u16]) -> u64 {
    let mut out = UNDEF;
    glue::rjs_string16(g, units.as_ptr(), units.len(), &mut out);
    out
}

/// WebIDL `[EnforceRange] unsigned long long`: ToNumber (propagating), then
/// a TypeError for NaN/±Infinity or anything outside 0..=2^53-1, else the
/// value truncated toward zero.
pub unsafe fn enforce_range_u64(g: *mut c_void, v: u64, what: &str) -> Result<u64, ()> {
    let d = to_number(g, v)?;
    if !d.is_finite() {
        throw_type_error(g, &format!("{what}: Value is not a finite number."));
        return Err(());
    }
    let t = d.trunc();
    if !(0.0..=9_007_199_254_740_991.0).contains(&t) {
        throw_type_error(g, &format!("{what}: Value is outside the 'unsigned long long' value range."));
        return Err(());
    }
    Ok(t as u64)
}

/// A numeric argument with a default for `undefined` — the shape almost every
/// geometry/path method wants (`unrestricted double x = 0`).
pub unsafe fn arg_num(g: *mut c_void, cf: *mut c_void, i: u32, default: f64) -> f64 {
    let v = cf_arg(cf, i);
    if v == UNDEF {
        return default;
    }
    read_number(g, v)
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

/// Bumped whenever a context's state is installed or forgotten: a thread's caches of keys and
/// states are only trusted for the generation they were filled in.
static GENERATION: AtomicU64 = AtomicU64::new(1);

fn bump_generation() {
    GENERATION.fetch_add(1, Ordering::AcqRel);
}

/// Property names are short; SipHash costs more than the lookup it guards.
#[derive(Default)]
struct FastHasher(u64);
impl Hasher for FastHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.write_u64(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut buf = [0u8; 8];
            buf[..rest.len()].copy_from_slice(rest);
            self.write_u64(u64::from_le_bytes(buf) ^ ((rest.len() as u64) << 56));
        }
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }
}
type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<FastHasher>>;
/// A context's kept atoms, by name.
type Names = FastMap<Box<str>, usize>;
/// `(context, list address, list length)`.
type ListKey = (usize, usize, usize);

/// Names built at run time (ids, header names) are kept too, up to this many per context; past
/// it a name gets a one-off atom.
const MAX_NAMES: usize = 4096;

struct SendList(&'static [*mut c_void]);
unsafe impl Send for SendList {}

#[derive(Default)]
struct Shared {
    names: FastMap<usize, Names>,
    lists: FastMap<ListKey, SendList>,
}

fn shared() -> &'static Mutex<Shared> {
    static K: OnceLock<Mutex<Shared>> = OnceLock::new();
    K.get_or_init(Default::default)
}

#[derive(Default)]
struct Local {
    generation: u64,
    names: FastMap<usize, Names>,
    lists: FastMap<ListKey, &'static [*mut c_void]>,
    state: Option<(usize, &'static WebState)>,
}

thread_local! {
    static LOCAL: RefCell<Local> = RefCell::new(Local::default());
}

fn local<R>(f: impl FnOnce(&mut Local) -> R) -> R {
    LOCAL.with(|l| {
        let mut l = l.borrow_mut();
        let generation = GENERATION.load(Ordering::Acquire);
        if l.generation != generation {
            l.names.clear();
            l.lists.clear();
            l.state = None;
            l.generation = generation;
        }
        f(&mut l)
    })
}

/// The context's kept atom for `name`, made on first use. `None` when the context already keeps
/// [`MAX_NAMES`] and `always` is false.
unsafe fn kept_atom(g: *mut c_void, name: &str, always: bool) -> Option<*mut c_void> {
    let ctx = g as usize;
    if let Some(a) = local(|l| l.names.get(&ctx).and_then(|m| m.get(name)).copied()) {
        return Some(a as *mut c_void);
    }
    let found = {
        let sh = shared().lock().unwrap();
        let m = sh.names.get(&ctx);
        match m.and_then(|m| m.get(name)) {
            Some(&a) => Some(a),
            None if !always && m.is_some_and(|m| m.len() >= MAX_NAMES) => return None,
            None => None,
        }
    };
    let a = match found {
        Some(a) => a,
        None => {
            let made = atom(g, name) as usize;
            let mut sh = shared().lock().unwrap();
            let m = sh.names.entry(ctx).or_default();
            match m.get(name) {
                // Made concurrently by another thread of this context's VM.
                Some(&a) => {
                    glue::rjs_atom_free(g, made as *mut c_void);
                    a
                }
                None => {
                    m.insert(name.into(), made);
                    made
                }
            }
        }
    };
    local(|l| l.names.entry(ctx).or_default().insert(name.into(), a));
    Some(a as *mut c_void)
}

/// The atom for `name`, made once per context and kept for its lifetime: a by-name `atom` +
/// `rjs_atom_free` costs more than the property access it names.
pub unsafe fn key(g: *mut c_void, name: &str) -> *mut c_void {
    kept_atom(g, name, true).unwrap_or(core::ptr::null_mut())
}

/// [`key`] for a whole list of names, with one lookup.
pub unsafe fn keys(g: *mut c_void, names: &'static [&'static str]) -> &'static [*mut c_void] {
    let k = (g as usize, names.as_ptr() as usize, names.len());
    if let Some(list) = local(|l| l.lists.get(&k).copied()) {
        return list;
    }
    let cached = shared().lock().unwrap().lists.get(&k).map(|l| l.0);
    let list = match cached {
        Some(list) => list,
        None => {
            let list: &'static [*mut c_void] = Box::leak(names.iter().map(|n| key(g, n)).collect());
            shared().lock().unwrap().lists.insert(k, SendList(list));
            list
        }
    };
    local(|l| l.lists.insert(k, list));
    list
}

/// `obj[names[i]]` into `out[i]`, under one engine lock. A non-object reads all undefined, and so
/// does a read that throws.
pub unsafe fn get_many(g: *mut c_void, obj: u64, names: &'static [&'static str], out: &mut [u64]) {
    debug_assert_eq!(names.len(), out.len());
    out.fill(UNDEF);
    if is_cell(obj) {
        glue::rjs_get_many(g, obj, keys(g, names).as_ptr(), names.len(), out.as_mut_ptr());
    }
}

/// [`get_many`] with the atoms in hand.
pub unsafe fn get_many_atoms(g: *mut c_void, obj: u64, atoms: &[*mut c_void], out: &mut [u64]) {
    debug_assert_eq!(atoms.len(), out.len());
    out.fill(UNDEF);
    if is_cell(obj) {
        glue::rjs_get_many(g, obj, atoms.as_ptr(), atoms.len(), out.as_mut_ptr());
    }
}

/// `obj[slot][name]` under one engine lock — a field of an internal-slot bag. `None` when `obj`
/// or the bag is not an object.
pub unsafe fn get_in(g: *mut c_void, obj: u64, slot: *mut c_void, name: &str) -> Option<u64> {
    if !is_cell(obj) {
        return None;
    }
    let atoms = [slot, key(g, name)];
    let mut out = UNDEF;
    match glue::rjs_get_path(g, obj, atoms.as_ptr(), 2, &mut out) {
        0 => Some(out),
        _ => None,
    }
}

/// Own data properties `obj[names[i]] = vals[i]`, under one engine lock.
pub unsafe fn define_many(g: *mut c_void, obj: u64, names: &'static [&'static str], vals: &[u64], attrs: u32) {
    debug_assert_eq!(names.len(), vals.len());
    glue::rjs_define_many(g, obj, keys(g, names).as_ptr(), vals.as_ptr(), names.len(), attrs);
}

pub unsafe fn define(g: *mut c_void, obj: u64, name: &str, v: u64, attrs: u32) {
    if let Some(a) = kept_atom(g, name, false) {
        glue::rjs_define(g, obj, a, v, attrs);
        return;
    }
    let a = atom(g, name);
    glue::rjs_define(g, obj, a, v, attrs);
    glue::rjs_atom_free(g, a);
}

/// Is `proto` on `v`'s prototype chain? Read off the stored [[Prototype]]
/// slots — no trap, no getter — so a brand check built on it is unobservable.
pub unsafe fn proto_chain_has(g: *mut c_void, v: u64, proto: u64) -> bool {
    if !is_cell(v) || !is_cell(proto) {
        return false;
    }
    let mut o = v;
    for _ in 0..4096 {
        let p = capi::JSObjectGetPrototype(g, o as usize as capi::JSObjectRef) as usize as u64;
        if !is_cell(p) {
            return false;
        }
        if p == proto {
            return true;
        }
        o = p;
    }
    false
}

pub unsafe fn get_a(g: *mut c_void, obj: u64, a: *mut c_void) -> u64 {
    let mut out = UNDEF;
    if glue::rjs_get(g, obj, a, &mut out) != 0 {
        return UNDEF;
    }
    out
}

pub unsafe fn get(g: *mut c_void, obj: u64, name: &str) -> u64 {
    if let Some(a) = kept_atom(g, name, false) {
        return get_a(g, obj, a);
    }
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

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub type HostFn = unsafe extern "sysv64" fn(*mut c_void, *mut c_void) -> u64;
#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
pub type HostFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u64;

/// The generic "constructor called without new" / callable-only thrower.
#[crate::host_fn]
pub unsafe fn ctor_requires_new(g: *mut c_void, _cf: *mut c_void) -> u64 {
    throw_type_error(g, "Constructor requires 'new'")
}

#[crate::host_fn]
pub unsafe fn illegal_constructor(g: *mut c_void, _cf: *mut c_void) -> u64 {
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

pub unsafe fn make_fn(g: *mut c_void, name: &str, arity: u32, f: HostFn) -> u64 {
    let n16: Vec<u16> = name.encode_utf16().collect();
    let mut out = UNDEF;
    glue::rjs_function_create(g, n16.as_ptr(), n16.len(), arity, f as *const c_void, &mut out);
    out
}

/// `proto[Symbol.toStringTag] = tag`, with WebIDL's attributes: not
/// writable, not enumerable, configurable.
pub unsafe fn to_string_tag(g: *mut c_void, proto: u64, tag: &str) {
    let a = glue::rjs_atom_wellknown(g, 1);
    if !a.is_null() {
        glue::rjs_define(g, proto, a, js_string(g, tag), 1 | 2);
        glue::rjs_atom_free(g, a);
    }
}

/// Builds a standards-shaped class: prototype object (methods + accessors,
/// all non-enumerable per spec), a constructable native function, `.prototype`
/// (RO/DontEnum/DontDelete) and `prototype.constructor` (DontEnum) wiring,
/// and defines it on the global object.
///
/// The prototype carries `Symbol.toStringTag` = the class name, as every
/// WebIDL interface prototype does (WebIDL §3.7.3 "interface prototype
/// object"). Without it `Object.prototype.toString` says `[object Object]`,
/// and code that classifies objects that way treats a platform object as a
/// plain one: Vue's `reactive()` wrapped a `MediaDeviceInfo` in a Proxy, and
/// its native getters then refused the Proxy as a foreign `this`
/// (research/reports/m26/client-sweep-1.md, finding 6).
pub unsafe fn define_class(g: *mut c_void, spec: &ClassSpec) -> Class {
    let proto = new_object(g, if spec.parent_proto != 0 { spec.parent_proto } else { UNDEF });
    to_string_tag(g, proto, spec.name);

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

/// Named properties answered from Rust, as WebIDL's named and indexed properties are (`style.color`,
/// `style[0]`, `localStorage.key`). A hook that answers `None`/`false` leaves the name to the ordinary
/// lookup (own properties, then the prototype), so prototype methods stay reachable. `Err` is the value to
/// throw. Symbols never reach the hooks.
pub trait NamedProps: 'static {
    unsafe fn get(&mut self, g: *mut c_void, this: u64, name: &str) -> Result<Option<u64>, u64>;
    unsafe fn set(&mut self, _g: *mut c_void, _this: u64, _name: &str, _value: u64) -> Result<bool, u64> {
        Ok(false)
    }
    unsafe fn delete(&mut self, _g: *mut c_void, _this: u64, _name: &str) -> Result<bool, u64> {
        Ok(false)
    }
    /// The names `Object.keys` and `for…in` list ahead of the own properties.
    unsafe fn names(&mut self, _g: *mut c_void, _this: u64) -> Vec<String> {
        Vec::new()
    }
}

thread_local! {
    static IN_SLOT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn in_slot<R>(f: impl FnOnce() -> R) -> R {
    let was = IN_SLOT.replace(true);
    let r = f();
    IN_SLOT.set(was);
    r
}

/// Reads a private-atom slot of a [`NamedProps`] instance. JSC asks the property callbacks about a
/// private name too (by its description), so a plain [`get_a`] from inside a hook would recurse.
pub unsafe fn named_slot_get(g: *mut c_void, obj: u64, atom: *mut c_void) -> u64 {
    in_slot(|| get_a(g, obj, atom))
}

/// Defines a private-atom slot on a [`NamedProps`] instance; see [`named_slot_get`].
pub unsafe fn named_slot_define(g: *mut c_void, obj: u64, atom: *mut c_void, v: u64) {
    in_slot(|| {
        glue::rjs_define(g, obj, atom, v, 0);
    })
}

unsafe fn prop_name(name: capi::JSStringRef) -> String {
    let len = capi::JSStringGetLength(name);
    let ptr = capi::JSStringGetCharactersPtr(name);
    if len == 0 || ptr.is_null() {
        return String::new();
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
}

unsafe fn named_data<'a, T>(obj: capi::JSObjectRef) -> Option<&'a mut T> {
    let p = capi::JSObjectGetPrivate(obj);
    (!p.is_null()).then(|| &mut *(p as *mut T))
}

unsafe extern "C" fn named_get<T: NamedProps>(
    ctx: capi::JSContextRef,
    obj: capi::JSObjectRef,
    name: capi::JSStringRef,
    exception: *mut capi::JSValueRef,
) -> capi::JSValueRef {
    if IN_SLOT.get() {
        return std::ptr::null();
    }
    let Some(data) = named_data::<T>(obj) else { return std::ptr::null() };
    match data.get(ctx, obj as usize as u64, &prop_name(name)) {
        Ok(Some(v)) => v as usize as capi::JSValueRef,
        Ok(None) => std::ptr::null(),
        Err(e) => {
            if !exception.is_null() {
                *exception = e as usize as capi::JSValueRef;
            }
            std::ptr::null()
        }
    }
}

unsafe extern "C" fn named_set<T: NamedProps>(
    ctx: capi::JSContextRef,
    obj: capi::JSObjectRef,
    name: capi::JSStringRef,
    value: capi::JSValueRef,
    exception: *mut capi::JSValueRef,
) -> bool {
    if IN_SLOT.get() {
        return false;
    }
    let Some(data) = named_data::<T>(obj) else { return false };
    match data.set(ctx, obj as usize as u64, &prop_name(name), value as usize as u64) {
        Ok(handled) => handled,
        Err(e) => {
            if !exception.is_null() {
                *exception = e as usize as capi::JSValueRef;
            }
            true
        }
    }
}

unsafe extern "C" fn named_delete<T: NamedProps>(
    ctx: capi::JSContextRef,
    obj: capi::JSObjectRef,
    name: capi::JSStringRef,
    exception: *mut capi::JSValueRef,
) -> bool {
    let Some(data) = named_data::<T>(obj) else { return false };
    match data.delete(ctx, obj as usize as u64, &prop_name(name)) {
        Ok(handled) => handled,
        Err(e) => {
            if !exception.is_null() {
                *exception = e as usize as capi::JSValueRef;
            }
            true
        }
    }
}

unsafe extern "C" fn named_names<T: NamedProps>(
    ctx: capi::JSContextRef,
    obj: capi::JSObjectRef,
    acc: capi::JSPropertyNameAccumulatorRef,
) {
    let Some(data) = named_data::<T>(obj) else { return };
    for n in data.names(ctx, obj as usize as u64) {
        let units: Vec<u16> = n.encode_utf16().collect();
        let s = capi::JSStringCreateWithCharacters(units.as_ptr(), units.len());
        capi::JSPropertyNameAccumulatorAddName(acc, s);
        capi::JSStringRelease(s);
    }
}

/// [`make_host_class`] whose instances answer named properties through `T`'s [`NamedProps`].
pub unsafe fn make_named_host_class<T: NamedProps>(name: &str) -> capi::JSClassRef {
    let cname = std::ffi::CString::new(name).unwrap();
    let mut def = capi::JSClassDefinition::empty();
    def.class_name = cname.as_ptr();
    def.finalize = Some(finalize_box::<T>);
    def.get_property = named_get::<T> as *const c_void;
    def.set_property = named_set::<T> as *const c_void;
    def.delete_property = named_delete::<T> as *const c_void;
    def.get_property_names = named_names::<T> as *const c_void;
    capi::JSClassCreate(&def)
}

/// An Error of `kind` (as [`throw_error`]) for a [`NamedProps`] hook to return, not thrown.
pub unsafe fn error_value(g: *mut c_void, kind: u32, msg: &str) -> u64 {
    let m: Vec<u16> = msg.encode_utf16().collect();
    let mut err = UNDEF;
    glue::rjs_error_create(g, kind, m.as_ptr(), m.len(), &mut err);
    err
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
    pub s_performance: *mut c_void,
    pub s_crypto: *mut c_void,
    pub s_subtle_crypto: *mut c_void,
    pub s_listeners: *mut c_void, // EventTarget [[listeners]] → JS Array
    pub s_event: *mut c_void,     // Event [[state]] → plain JS object
    pub s_image: *mut c_void,     // HTMLImageElement [[bitmap]] → ImageBitmap host obj
    pub s_image_request: *mut c_void, // current load generation; fences canceled/replaced loads
    pub s_usp: *mut c_void,       // URL [[searchParams]] cache
    pub s_iter_target: *mut c_void, // iterator [[target]] → iterated host obj

    // Common public atoms (hot in dispatch).
    pub a_type: *mut c_void,
    pub a_cb: *mut c_void,
    pub a_flags: *mut c_void, // listener entry: once/capture/passive/removed bits
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
    pub usp_iter_class: capi::JSClassRef,
    pub usp_iter_proto: u64,
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
    bump_generation();
    leaked
}

/// Panics if the web API was not installed for this context. Shared ref —
/// WebState is immutable after install.
pub fn state(g: *mut c_void) -> &'static WebState {
    if let Some(st) = local(|l| l.state.and_then(|(k, st)| (k == g as usize).then_some(st))) {
        return st;
    }
    let p = states().lock().unwrap().get(&(g as usize)).map(|s| s.0).expect("redjs webapi not installed");
    let st: &'static WebState = unsafe { &*p };
    local(|l| l.state = Some((g as usize, st)));
    st
}

pub fn has_state(g: *mut c_void) -> bool {
    states().lock().unwrap().contains_key(&(g as usize))
}

/// `crate::context` fan-out. The leaked `WebState` stays leaked (its values
/// were protected in a heap that no longer exists); only the key is freed so
/// a new context at the same address installs from nothing.
pub(crate) fn forget_state(gone: crate::context::Gone) {
    states().lock().unwrap().remove(&gone.key);
    // The atoms are left alone: their VM may already be gone.
    let mut sh = shared().lock().unwrap();
    sh.names.remove(&gone.key);
    sh.lists.retain(|k, _| k.0 != gone.key);
    drop(sh);
    bump_generation();
}

#[cfg(test)]
mod key_tests {
    use super::*;
    use crate::webapi::testing::ctx;

    #[test]
    fn names_past_the_kept_limit_still_read_and_write() {
        let c = ctx();
        let g = c.raw();
        unsafe {
            let o = new_object(g, UNDEF);
            crate::ffi::redjs_protect(g, o);
            let n = MAX_NAMES + 50;
            for i in 0..n {
                define(g, o, &format!("k{i}"), enc_f64(i as f64), 0);
            }
            for i in 0..n {
                assert_eq!(dec_f64(get(g, o, &format!("k{i}"))), Some(i as f64), "k{i}");
            }
            assert_eq!(get(g, o, "missing"), UNDEF);
            crate::ffi::redjs_unprotect(g, o);
        }
    }

    #[test]
    fn a_new_context_does_not_reuse_another_contexts_atoms() {
        for _ in 0..8 {
            let c = ctx();
            let g = c.raw();
            unsafe {
                let o = new_object(g, UNDEF);
                define(g, o, "x", TRUE, 0);
                assert_eq!(get(g, o, "x"), TRUE);
            }
        }
    }
}

#[cfg(test)]
mod named_tests {
    use super::*;
    use crate::webapi::testing::{ctx, ok, s, throws};
    use std::collections::BTreeMap;

    /// Answers every `x…` name (unset ones read `""`) and index 0; refuses `xbad`.
    struct Bag(BTreeMap<String, String>);

    impl NamedProps for Bag {
        unsafe fn get(&mut self, g: *mut c_void, _this: u64, name: &str) -> Result<Option<u64>, u64> {
            Ok(match name {
                "0" => Some(js_string(g, "zero")),
                n if n.starts_with('x') => Some(js_string(g, self.0.get(n).map_or("", String::as_str))),
                _ => None,
            })
        }
        unsafe fn set(&mut self, g: *mut c_void, _this: u64, name: &str, value: u64) -> Result<bool, u64> {
            if name == "xbad" {
                return Err(error_value(g, 1, "xbad is read-only"));
            }
            if !name.starts_with('x') {
                return Ok(false);
            }
            let v = to_js_string(g, value).ok().and_then(|v| read_string(g, v)).unwrap_or_default();
            self.0.insert(name.to_string(), v);
            Ok(true)
        }
        unsafe fn delete(&mut self, _g: *mut c_void, _this: u64, name: &str) -> Result<bool, u64> {
            Ok(self.0.remove(name).is_some())
        }
        unsafe fn names(&mut self, _g: *mut c_void, _this: u64) -> Vec<String> {
            self.0.keys().cloned().collect()
        }
    }

    #[test]
    fn named_properties_come_from_rust_and_the_rest_is_ordinary() {
        let c = ctx();
        unsafe {
            let g = c.raw();
            let cls = make_named_host_class::<Bag>("Bag");
            let proto = get(g, get(g, glue::rjs_global(g), "Object"), "prototype");
            let bag = host_create(g, cls, proto, Bag(BTreeMap::new()));
            define(g, glue::rjs_global(g), "bag", bag, 0);
            let slot = atom_priv(g, "x-slot");
            named_slot_define(g, bag, slot, js_string(g, "kept"));
            assert_eq!(read_string(g, named_slot_get(g, bag, slot)).as_deref(), Some("kept"));
        }
        ok(&c, "bag.xa === '' && bag[0] === 'zero' && bag.toString === Object.prototype.toString");
        ok(&c, "(bag.xa = 7, bag.xa === '7') && 'xa' in bag && 'xz' in bag && !('nope' in bag)");
        ok(&c, "(bag.other = 5, bag.other === 5 && Object.getOwnPropertyDescriptor(bag, 'other').value === 5)");
        assert_eq!(s(&c, "Object.keys(bag).join()"), "xa,other");
        ok(&c, "delete bag.xa && bag.xa === '' && Object.keys(bag).join() === 'other'");
        assert_eq!(throws(&c, "bag.xbad = 1"), "xbad is read-only");
        ok(&c, "(() => { try { bag.xbad = 1 } catch (e) { return e instanceof TypeError } })()");
    }
}
