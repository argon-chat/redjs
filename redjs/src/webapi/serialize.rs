//! The structured serializer: HTML §2.7 `StructuredSerializeInternal` /
//! `StructuredDeserialize` (+ the `WithTransfer` pair), writing BYTES.
//!
//! [`serialize`] turns a JS value into a [`Serialized`] — a `Vec<u8>` plus an
//! attachment table — that holds no JSC value and no pointer into any heap. It
//! is `Send`: bytes made on one thread decode in a context on another (the
//! Worker seam), and [`deserialize_storage`] reads bytes that came back from
//! disk (the IndexedDB seam). Nothing here keeps process-global JS state; the
//! per-context intrinsics live in a registry keyed by context and dropped by
//! [`forget`].
//!
//! `structuredClone` and `BroadcastChannel.postMessage` are both built on it.
//!
//! # Brands, not `instanceof`
//!
//! HTML branches on internal slots ([[DateValue]], [[MapData]], [[ErrorData]],
//! [[ArrayBufferData]], …). Those are read from the cell's own type
//! (`rjs_brand`), so a forged `Symbol.toStringTag` or a swapped prototype
//! changes nothing, and nothing user-visible runs except what the spec itself
//! runs: property getters, `name` on an Error, `ToString` of a message, and —
//! for `structuredClone`'s `transfer` — the iterator protocol. Map/Set
//! contents are read through `Map.prototype.entries` / `Set.prototype.values`
//! and the iterator `next` CAPTURED AT INSTALL, so overriding them later does
//! not reach in either.
//!
//! # The wire format (version 1, frozen)
//!
//! ```text
//! header   "RJS"  u8 version (=1)  u8 flags (bit 0 = written for storage)
//! body     exactly one value; trailing bytes = corrupt
//! varint   unsigned LEB128 (≤ 10 bytes)        f64  8 bytes little-endian
//! string   0x08 varint n, n Latin-1 bytes  |  0x09 varint n, n UTF-16LE units
//! ```
//!
//! | tag  | value                    | payload                                              |
//! |------|--------------------------|------------------------------------------------------|
//! | 0x01 | undefined                |                                                      |
//! | 0x02 | null                     |                                                      |
//! | 0x03 | false                    |                                                      |
//! | 0x04 | true                     |                                                      |
//! | 0x05 | int32                    | varint zigzag                                        |
//! | 0x06 | double                   | f64 (NaN is canonicalised on read)                   |
//! | 0x07 | BigInt                   | varint n, n bytes of decimal ASCII (`-?[0-9]+`)      |
//! | 0x08 | string, Latin-1          | see above                                            |
//! | 0x09 | string, UTF-16           | see above (lone surrogates survive)                  |
//! | 0x0A | key: array index         | varint (key position only; ≤ 2^32 − 2)               |
//! | 0x0F | end of a list            |                                                      |
//! | 0x10 | reference                | varint object id                                     |
//! | 0x11 | Object                   | (key value)* 0x0F                                    |
//! | 0x12 | Array                    | varint length, (key value)* 0x0F                     |
//! | 0x13 | —                        | reserved                                             |
//! | 0x14 | Date                     | f64 [[DateValue]]                                    |
//! | 0x15 | RegExp                   | string source, string flags                          |
//! | 0x16 | Map                      | (key value)* 0x0F                                    |
//! | 0x17 | Set                      | value* 0x0F                                          |
//! | 0x18 | Boolean object           | u8                                                   |
//! | 0x19 | Number object            | f64                                                  |
//! | 0x1A | String object            | string                                               |
//! | 0x1B | BigInt object            | as 0x07 without the tag                              |
//! | 0x1C | Error                    | u8 name, u8 mask (1 message, 2 stack, 4 cause), [string], [string], [value] |
//! | 0x20 | ArrayBuffer              | varint n, n bytes                                    |
//! | 0x21 | resizable ArrayBuffer    | varint n, varint max, n bytes                        |
//! | 0x22 | view                     | u8 kind, varint byteOffset, varint length+1 (0 = length-tracking), buffer value |
//! | 0x30 | Blob (inline)            | string type, varint n, n bytes                       |
//! | 0x31 | File                     | reserved                                             |
//! | 0x32 | FileList                 | reserved                                             |
//! | 0x33 | ImageData                | varint w, varint h, u8 colorSpace (0 srgb), data value |
//! | 0x34 | DOMPoint(ReadOnly)       | u8 read-only, 4 × f64 (x y z w)                      |
//! | 0x35 | DOMRect(ReadOnly)        | reserved                                             |
//! | 0x36 | DOMQuad                  | reserved                                             |
//! | 0x37 | DOMMatrix(ReadOnly)      | u8 read-only, u8 is2D, 6 × f64 (a b c d e f) or 16 × f64 (m11…m44) |
//! | 0x38 | DOMException             | u8 kind (0, 1 = QuotaExceededError), string name, string message, u8 mask (1 stack, 2 quota, 4 requested), [string], [f64], [f64] |
//! | 0x39–0x3F | —                   | reserved (platform objects)                          |
//! | 0x40 | transferred ArrayBuffer  | varint attachment                                    |
//! | 0x41 | MessagePort              | reserved — varint attachment                         |
//! | 0x42 | ImageBitmap              | varint attachment                                    |
//! | 0x43 | Blob by reference        | varint attachment                                    |
//! | 0x44 | SharedArrayBuffer        | reserved — varint attachment                         |
//! | 0x45 | File by reference        | reserved — varint attachment                         |
//! | 0x46–0x4F | —                   | reserved (attachments)                               |
//!
//! Error names: 0 Error, 1 EvalError, 2 RangeError, 3 ReferenceError,
//! 4 SyntaxError, 5 TypeError, 6 URIError (HTML's list; anything else is
//! "Error"). View kinds are `RjsTaKind`: 1 Int8 … 11 BigUint64, 12 DataView,
//! 13 Float16.
//!
//! **Identity.** Every object tag (0x11–0x38) takes the next object id AT THE
//! TAG, before its children, in the writer and the reader alike, so a later
//! 0x10 can point at an object still being written — cycles. Attachment tags
//! (0x4x) take no id: the same attachment index IS the same object (the
//! reader caches by index).
//!
//! **Attachments.** Indices run over the transfer list first (in its order),
//! then the extras: Blobs by reference (the bytes are an `Arc`, never copied)
//! and ImageBitmap copies. Storage bytes have no attachment table, so
//! [`deserialize_storage`] rejects every 0x4x tag; a Blob written for storage
//! goes inline (0x30), an ImageBitmap is a DataCloneError (HTML: `forStorage`).
//!
//! # Transfer
//!
//! HTML's order exactly: validate the list (transferable, not shared, no
//! duplicates) → serialize, with each listed object standing for its own
//! attachment → then per item: still not detached, take the data, detach. A
//! throw anywhere before that last phase leaves every listed buffer intact.
//! P0 copies a transferred buffer's bytes on the way out and again on the way
//! in; moving the JSC `ArrayBufferContents` instead is the Workers zero-copy
//! follow-up and changes no byte of the format.
//!
//! # Depth
//!
//! The walk is native recursion, bounded by the VM's own soft stack limit on
//! the current thread: a graph too deep to walk is the RangeError a browser
//! throws, never a native overflow. A worker thread that clones deep graphs
//! needs the stack JS itself would (≥ 4 MiB).

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use super::util::*;
use crate::glue::{self, brand, RjsBrandInfo};

pub const MAGIC: [u8; 3] = *b"RJS";
pub const VERSION: u8 = 1;
/// Header flag: written with `forStorage` (no attachment table).
pub const FLAG_STORAGE: u8 = 1;

/// Wire tags. Reserved ones are listed so nothing else ever takes them.
pub mod tag {
    pub const UNDEFINED: u8 = 0x01;
    pub const NULL: u8 = 0x02;
    pub const FALSE: u8 = 0x03;
    pub const TRUE: u8 = 0x04;
    pub const INT32: u8 = 0x05;
    pub const DOUBLE: u8 = 0x06;
    pub const BIGINT: u8 = 0x07;
    pub const STRING_LATIN1: u8 = 0x08;
    pub const STRING_UTF16: u8 = 0x09;
    pub const KEY_INDEX: u8 = 0x0A;
    pub const END: u8 = 0x0F;

    pub const OBJECT_REF: u8 = 0x10;
    pub const OBJECT: u8 = 0x11;
    pub const ARRAY: u8 = 0x12;
    pub const RESERVED_13: u8 = 0x13;
    pub const DATE: u8 = 0x14;
    pub const REGEXP: u8 = 0x15;
    pub const MAP: u8 = 0x16;
    pub const SET: u8 = 0x17;
    pub const BOOLEAN_OBJECT: u8 = 0x18;
    pub const NUMBER_OBJECT: u8 = 0x19;
    pub const STRING_OBJECT: u8 = 0x1A;
    pub const BIGINT_OBJECT: u8 = 0x1B;
    pub const ERROR: u8 = 0x1C;

    pub const ARRAY_BUFFER: u8 = 0x20;
    pub const RESIZABLE_ARRAY_BUFFER: u8 = 0x21;
    pub const VIEW: u8 = 0x22;

    pub const BLOB: u8 = 0x30;
    pub const FILE: u8 = 0x31;
    pub const FILE_LIST: u8 = 0x32;
    pub const IMAGE_DATA: u8 = 0x33;
    pub const DOMPOINT: u8 = 0x34;
    pub const DOMRECT: u8 = 0x35;
    pub const DOMQUAD: u8 = 0x36;
    pub const DOMMATRIX: u8 = 0x37;
    pub const DOMEXCEPTION: u8 = 0x38;

    pub const TRANSFERRED_BUFFER: u8 = 0x40;
    pub const MESSAGE_PORT: u8 = 0x41;
    pub const IMAGE_BITMAP: u8 = 0x42;
    pub const BLOB_REF: u8 = 0x43;
    pub const SHARED_BUFFER: u8 = 0x44;
    pub const FILE_REF: u8 = 0x45;
}

/// HTML's Error names, in wire order.
pub const ERROR_NAMES: [&str; 7] = ["Error", "EvalError", "RangeError", "ReferenceError", "SyntaxError", "TypeError", "URIError"];

/// View constructors by `RjsTaKind` (index 0 unused).
const VIEW_CTORS: [&str; 14] = [
    "",
    "Int8Array",
    "Uint8Array",
    "Uint8ClampedArray",
    "Int16Array",
    "Uint16Array",
    "Int32Array",
    "Uint32Array",
    "Float32Array",
    "Float64Array",
    "BigInt64Array",
    "BigUint64Array",
    "DataView",
    "Float16Array",
];
const ELEM_SIZE: [u64; 14] = [0, 1, 1, 1, 2, 2, 4, 4, 4, 8, 8, 8, 1, 2];

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

pub struct Options<'a> {
    /// HTML `forStorage`: no attachments, Blobs inline, ImageBitmap refused.
    pub for_storage: bool,
    /// Objects to transfer (ArrayBuffer, ImageBitmap). The caller keeps them
    /// alive for the duration of the call.
    pub transfer: &'a [u64],
}

/// One entry of the attachment table — plain Rust data.
#[derive(Debug, Clone)]
pub enum Attachment {
    /// A transferred ArrayBuffer's bytes; `max_byte_length` for a resizable one.
    TransferredBuffer { bytes: Vec<u8>, max_byte_length: Option<u64> },
    /// An ImageBitmap's pixels (moved when transferred, copied when not).
    Bitmap { width: u32, height: u32, rgba: Vec<u8> },
    /// A Blob, by reference: the same immutable bytes, shared.
    BlobRef { bytes: Arc<Vec<u8>>, offset: usize, len: usize, mime: String },
}

/// The output of [`serialize`]: no JSC value anywhere, hence `Send`.
#[derive(Debug)]
pub struct Serialized {
    pub bytes: Vec<u8>,
    pub attachments: Vec<Attachment>,
}

/// Serializes `v`. `Err` carries the exception VALUE (a DataCloneError
/// DOMException, or whatever a getter threw) — not yet thrown.
///
/// # Safety
/// `g` must be a live context with the web API installed, on its thread.
pub unsafe fn serialize(g: *mut c_void, v: u64, opts: &Options) -> Result<Serialized, u64> {
    let _lock = Locked::new(g);
    let Some(it) = intrinsics(g) else {
        return Err(error_value(g, 1, "structured serialization is not installed in this context"));
    };
    if opts.for_storage && !opts.transfer.is_empty() {
        return Err(dce(g, "A value serialized for storage cannot transfer anything."));
    }
    let mut e = Enc {
        g,
        it,
        out: Vec::with_capacity(64),
        ids: HashMap::new(),
        next_id: 0,
        attach_of: HashMap::new(),
        attachments: Vec::new(),
        for_storage: opts.for_storage,
        keep: KeepAlive::new(g),
        stack: StackGuard::new(g),
    };
    e.out.extend_from_slice(&MAGIC);
    e.out.push(VERSION);
    e.out.push(if opts.for_storage { FLAG_STORAGE } else { 0 });
    e.validate_transfer(opts.transfer)?;
    e.value(v, 0)?;
    e.transfer(opts.transfer)?;
    let bytes = core::mem::take(&mut e.out);
    let attachments = core::mem::take(&mut e.attachments).into_iter().map(|a| a.expect("every attachment slot is filled")).collect();
    Ok(Serialized { bytes, attachments })
}

/// Deserializes, consuming the attachments (transferred data moves in).
///
/// # Safety
/// As [`serialize`].
pub unsafe fn deserialize(g: *mut c_void, s: Serialized) -> Result<u64, u64> {
    let Serialized { bytes, attachments } = s;
    decode(g, &bytes, Source::Owned(attachments.into_iter().map(Some).collect()), false)
}

/// Deserializes without consuming — once per receiver (BroadcastChannel).
///
/// # Safety
/// As [`serialize`].
pub unsafe fn deserialize_ref(g: *mut c_void, s: &Serialized) -> Result<u64, u64> {
    decode(g, &s.bytes, Source::Borrowed(&s.attachments), false)
}

/// Deserializes bytes written with `for_storage` (from disk): any attachment
/// tag is corruption.
///
/// # Safety
/// As [`serialize`].
pub unsafe fn deserialize_storage(g: *mut c_void, bytes: &[u8]) -> Result<u64, u64> {
    decode(g, bytes, Source::None, true)
}

// ---------------------------------------------------------------------------
// Per-context intrinsics (captured at install, before any user script)
// ---------------------------------------------------------------------------

struct Intrinsics {
    date: u64,
    regexp: u64,
    map: u64,
    map_set: u64,
    map_entries: u64,
    map_next: u64,
    set: u64,
    set_add: u64,
    set_values: u64,
    set_next: u64,
    errors: [u64; 7],
    array_buffer: u64,
    views: [u64; 14],
    bigint: u64,
    object: u64,
    /// ReadableStream / WritableStream / TransformStream prototypes.
    streams: [u64; 3],
    a_name: *mut c_void,
    a_message: *mut c_void,
    a_stack: *mut c_void,
    a_cause: *mut c_void,
    a_done: *mut c_void,
    a_value: *mut c_void,
    a_next: *mut c_void,
    a_transfer: *mut c_void,
    a_iterator: *mut c_void,
}

unsafe impl Send for Intrinsics {}
unsafe impl Sync for Intrinsics {}

fn registry() -> &'static Mutex<HashMap<usize, &'static Intrinsics>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static Intrinsics>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn intrinsics(g: *mut c_void) -> Option<&'static Intrinsics> {
    registry().lock().unwrap().get(&(g as usize)).copied()
}

pub(crate) fn forget(gone: crate::context::Gone) {
    registry().lock().unwrap().remove(&gone.key);
}

pub unsafe fn install(g: *mut c_void) {
    let global = glue::rjs_global(g);
    let keep = |v: u64| -> u64 {
        if is_cell(v) {
            crate::ffi::redjs_protect(g, v);
        }
        v
    };
    let ctor = |name: &str| keep(get(g, global, name));
    let proto_fn = |c: u64, name: &str| keep(get(g, get(g, c, "prototype"), name));
    // %MapIteratorPrototype%.next and %SetIteratorPrototype%.next: reachable
    // only through an iterator instance.
    let iter_next = |c: u64, make_iter: u64| -> u64 {
        let Ok(inst) = construct(g, c, &[]) else { return UNDEF };
        let Ok(iter) = call(g, make_iter, inst, &[]) else { return UNDEF };
        let p = crate::capi::JSObjectGetPrototype(g, iter as usize as crate::capi::JSObjectRef) as usize as u64;
        keep(get(g, p, "next"))
    };

    let map = ctor("Map");
    let set = ctor("Set");
    let map_entries = proto_fn(map, "entries");
    let set_values = proto_fn(set, "values");
    let streams = ["ReadableStream", "WritableStream", "TransformStream"].map(|n| keep(get(g, get(g, global, n), "prototype")));
    let it = Intrinsics {
        date: ctor("Date"),
        regexp: ctor("RegExp"),
        map,
        map_set: proto_fn(map, "set"),
        map_entries,
        map_next: iter_next(map, map_entries),
        set,
        set_add: proto_fn(set, "add"),
        set_values,
        set_next: iter_next(set, set_values),
        errors: ERROR_NAMES.map(|n| ctor(n)),
        array_buffer: ctor("ArrayBuffer"),
        views: core::array::from_fn(|k| if k == 0 { UNDEF } else { ctor(VIEW_CTORS[k]) }),
        bigint: ctor("BigInt"),
        object: ctor("Object"),
        streams,
        a_name: atom(g, "name"),
        a_message: atom(g, "message"),
        a_stack: atom(g, "stack"),
        a_cause: atom(g, "cause"),
        a_done: atom(g, "done"),
        a_value: atom(g, "value"),
        a_next: atom(g, "next"),
        a_transfer: atom(g, "transfer"),
        a_iterator: glue::rjs_atom_wellknown(g, 0),
    };
    registry().lock().unwrap().insert(g as usize, Box::leak(Box::new(it)));

    let n16: Vec<u16> = "structuredClone".encode_utf16().collect();
    let mut f = UNDEF;
    glue::rjs_function_create(g, n16.as_ptr(), n16.len(), 1, structured_clone as *const c_void, &mut f);
    // A WebIDL operation on the global: writable, enumerable, configurable.
    define(g, global, "structuredClone", f, 0);
}

// ---------------------------------------------------------------------------
// structuredClone(value, { transfer })
// ---------------------------------------------------------------------------

unsafe extern "sysv64" fn structured_clone(g: *mut c_void, cf: *mut c_void) -> u64 {
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "Failed to execute 'structuredClone': 1 argument required, but only 0 present.");
    }
    let Some(it) = intrinsics(g) else {
        return throw_type_error(g, "structuredClone is not installed in this context");
    };
    let mut keep = KeepAlive::new(g);
    let transfer = match transfer_option(g, it, cf_arg(cf, 1), &mut keep) {
        Ok(t) => t,
        Err(e) => {
            glue::rjs_throw(g, e);
            return UNDEF;
        }
    };
    let r = serialize(g, cf_arg(cf, 0), &Options { for_storage: false, transfer: &transfer }).and_then(|s| deserialize(g, s));
    drop(keep);
    match r {
        Ok(v) => v,
        Err(e) => {
            glue::rjs_throw(g, e);
            UNDEF
        }
    }
}

/// WebIDL: `StructuredSerializeOptions` → `sequence<object> transfer`.
unsafe fn transfer_option(g: *mut c_void, it: &Intrinsics, options: u64, keep: &mut KeepAlive) -> Result<Vec<u64>, u64> {
    const WHAT: &str = "Failed to execute 'structuredClone'";
    if options == UNDEF || options == NULL {
        return Ok(Vec::new());
    }
    if !is_object(g, options) {
        return Err(error_value(g, 1, &format!("{WHAT}: The provided value is not of type 'StructuredSerializeOptions'.")));
    }
    let t = get_x(g, options, it.a_transfer)?;
    if t == UNDEF {
        return Ok(Vec::new());
    }
    let not_seq = || error_value(g, 1, &format!("{WHAT}: Failed to read the 'transfer' property from 'StructuredSerializeOptions': The provided value cannot be converted to a sequence."));
    if !is_object(g, t) {
        return Err(not_seq());
    }
    let method = get_x(g, t, it.a_iterator)?;
    if method == UNDEF || method == NULL || glue::rjs_kind(g, method) != 9 {
        return Err(not_seq());
    }
    let iter = call(g, method, t, &[])?;
    if !is_object(g, iter) {
        return Err(error_value(g, 1, "Result of the Symbol.iterator method is not an object"));
    }
    let next = get_x(g, iter, it.a_next)?;
    if glue::rjs_kind(g, next) != 9 {
        return Err(error_value(g, 1, "The iterator's 'next' method is not callable"));
    }
    let mut out = Vec::new();
    loop {
        let r = call(g, next, iter, &[])?;
        if !is_object(g, r) {
            return Err(error_value(g, 1, "Iterator result is not an object"));
        }
        let done = get_x(g, r, it.a_done)?;
        if glue::rjs_to_boolean(g, done) != 0 {
            break;
        }
        let v = get_x(g, r, it.a_value)?;
        if !is_object(g, v) {
            return Err(error_value(g, 1, &format!("{WHAT}: Failed to read the 'transfer' property from 'StructuredSerializeOptions': Iterator value is not an object.")));
        }
        keep.hold(v);
        out.push(v);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------

type R<T = ()> = Result<T, u64>;

/// The VM lock for the whole walk: without an outer holder, every glue call
/// would be an outermost release — and JSC drains microtasks there, so user
/// JS could run between two reads of the same graph.
struct Locked(*mut c_void);

impl Locked {
    unsafe fn new(g: *mut c_void) -> Locked {
        glue::rjs_lock(g);
        Locked(g)
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        unsafe { glue::rjs_unlock(self.0) }
    }
}

/// Values only Rust heap memory points at (memo tables, Map entry lists),
/// rooted in one protected array for as long as the walk runs.
struct KeepAlive {
    g: *mut c_void,
    arr: u64,
    n: u32,
}

impl KeepAlive {
    unsafe fn new(g: *mut c_void) -> KeepAlive {
        let mut arr = UNDEF;
        glue::rjs_array_create(g, 0, &mut arr);
        crate::ffi::redjs_protect(g, arr);
        KeepAlive { g, arr, n: 0 }
    }

    unsafe fn hold(&mut self, v: u64) {
        if is_cell(v) {
            glue::rjs_define_index(self.g, self.arr, self.n, v);
            self.n += 1;
        }
    }
}

impl Drop for KeepAlive {
    fn drop(&mut self) {
        unsafe { crate::ffi::redjs_unprotect(self.g, self.arr) }
    }
}

/// Native-recursion bound: the VM's soft stack limit plus a margin for the
/// frames between two checks and for building the RangeError itself.
#[derive(Clone, Copy)]
struct StackGuard {
    limit: usize,
}

const STACK_MARGIN: usize = 64 * 1024;
/// Only when the VM reports no limit (never entered on this thread).
const DEPTH_FALLBACK: u32 = 2048;

impl StackGuard {
    unsafe fn new(g: *mut c_void) -> StackGuard {
        StackGuard { limit: glue::rjs_stack_soft_limit(g) }
    }

    #[inline(never)]
    fn ok(&self, depth: u32) -> bool {
        let marker = 0u8;
        let sp = core::hint::black_box(&marker) as *const u8 as usize;
        if self.limit != 0 {
            sp > self.limit.saturating_add(STACK_MARGIN)
        } else {
            depth < DEPTH_FALLBACK
        }
    }
}

unsafe fn error_value(g: *mut c_void, kind: u32, msg: &str) -> u64 {
    let m: Vec<u16> = msg.encode_utf16().collect();
    let mut e = UNDEF;
    glue::rjs_error_create(g, kind, m.as_ptr(), m.len(), &mut e);
    e
}

unsafe fn dce(g: *mut c_void, msg: &str) -> u64 {
    super::domexception::create(g, "DataCloneError", msg)
}

unsafe fn stack_overflow(g: *mut c_void) -> u64 {
    error_value(g, 2, "Maximum call stack size exceeded.")
}

unsafe fn is_object(g: *mut c_void, v: u64) -> bool {
    is_cell(v) && glue::rjs_kind(g, v) >= 8
}

/// `? Get(obj, key)` with the exception as a value.
unsafe fn get_x(g: *mut c_void, obj: u64, a: *mut c_void) -> R<u64> {
    let mut out = UNDEF;
    if glue::rjs_get(g, obj, a, &mut out) != 0 {
        return Err(out);
    }
    Ok(out)
}

unsafe fn construct(g: *mut c_void, ctor: u64, args: &[u64]) -> R<u64> {
    let mut out = UNDEF;
    match glue::rjs_construct(g, ctor, args.as_ptr(), args.len() as u32, &mut out) {
        0 => Ok(out),
        1 => Err(out),
        _ => Err(error_value(g, 1, "not a constructor")),
    }
}

/// An own DATA property's value (1), else `None`; `Err` = it threw.
unsafe fn own_data(g: *mut c_void, obj: u64, a: *mut c_void) -> R<Option<u64>> {
    let mut out = UNDEF;
    match glue::rjs_get_own_data(g, obj, a, &mut out) {
        1 => Ok(Some(out)),
        0 => Ok(None),
        _ => Err(out),
    }
}

/// A canonical array index (`"0"`, `"17"`, never `"01"`), ≤ 2^32 − 2.
fn canonical_index<T: Copy + Into<u32>>(units: &[T]) -> Option<u32> {
    let n = units.len();
    if n == 0 || n > 10 {
        return None;
    }
    let digit = |c: T| -> Option<u64> {
        let c: u32 = c.into();
        (0x30..=0x39).contains(&c).then(|| (c - 0x30) as u64)
    };
    if n > 1 && digit(units[0]) == Some(0) {
        return None;
    }
    let mut v: u64 = 0;
    for &c in units {
        v = v * 10 + digit(c)?;
    }
    (v <= 0xFFFF_FFFE).then_some(v as u32)
}

enum JsStr<'a> {
    Latin1(&'a [u8]),
    Utf16(&'a [u16]),
}

/// A JS string's storage, borrowed (valid until the next allocation).
unsafe fn js_str<'a>(g: *mut c_void, s: u64) -> Option<JsStr<'a>> {
    let mut info = crate::ffi::RjsStr::default();
    if glue::rjs_string_info(g, s, &mut info) != 0 {
        return None;
    }
    let n = info.len as usize;
    if n == 0 {
        return Some(JsStr::Latin1(&[]));
    }
    Some(if info.is8bit != 0 {
        JsStr::Latin1(core::slice::from_raw_parts(info.ptr as *const u8, n))
    } else {
        JsStr::Utf16(core::slice::from_raw_parts(info.ptr as *const u16, n))
    })
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

struct Enc {
    g: *mut c_void,
    it: &'static Intrinsics,
    out: Vec<u8>,
    /// HTML's memory: object → id.
    ids: HashMap<u64, u32>,
    next_id: u32,
    /// Transfer-list objects and by-reference extras → (attachment, tag).
    attach_of: HashMap<u64, (u32, u8)>,
    /// `None` = a transfer slot, filled after the walk.
    attachments: Vec<Option<Attachment>>,
    for_storage: bool,
    keep: KeepAlive,
    stack: StackGuard,
}

impl Enc {
    fn put(&mut self, b: u8) {
        self.out.push(b);
    }

    fn varint(&mut self, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.out.push(b);
                return;
            }
            self.out.push(b | 0x80);
        }
    }

    fn f64(&mut self, d: f64) {
        self.out.extend_from_slice(&d.to_le_bytes());
    }

    unsafe fn memo(&mut self, v: u64) {
        self.ids.insert(v, self.next_id);
        self.next_id += 1;
        self.keep.hold(v);
    }

    unsafe fn string(&mut self, s: u64) -> R {
        let Some(js) = js_str(self.g, s) else {
            return Err(dce(self.g, "A string could not be read."));
        };
        match js {
            JsStr::Latin1(b) => {
                self.out.push(tag::STRING_LATIN1);
                self.varint(b.len() as u64);
                self.out.extend_from_slice(b);
            }
            JsStr::Utf16(u) => {
                self.out.push(tag::STRING_UTF16);
                self.varint(u.len() as u64);
                self.out.reserve(u.len() * 2);
                for &c in u {
                    self.out.extend_from_slice(&c.to_le_bytes());
                }
            }
        }
        Ok(())
    }

    /// A Rust string (a Blob's type), Latin-1 when it fits.
    fn rust_string(&mut self, s: &str) {
        if s.chars().all(|c| (c as u32) <= 0xFF) {
            self.put(tag::STRING_LATIN1);
            self.varint(s.chars().count() as u64);
            for c in s.chars() {
                self.out.push(c as u32 as u8);
            }
        } else {
            let u: Vec<u16> = s.encode_utf16().collect();
            self.put(tag::STRING_UTF16);
            self.varint(u.len() as u64);
            for c in u {
                self.out.extend_from_slice(&c.to_le_bytes());
            }
        }
    }

    /// Decimal digits of a BigInt, untagged.
    unsafe fn bigint_digits(&mut self, b: u64) -> R {
        let mut s = UNDEF;
        if glue::rjs_to_string(self.g, b, &mut s) != 0 {
            return Err(s);
        }
        let digits: Vec<u8> = match js_str(self.g, s) {
            Some(JsStr::Latin1(d)) => d.to_vec(),
            Some(JsStr::Utf16(u)) => u.iter().map(|&c| c as u8).collect(),
            None => return Err(dce(self.g, "A BigInt could not be read.")),
        };
        self.varint(digits.len() as u64);
        self.out.extend_from_slice(&digits);
        Ok(())
    }

    unsafe fn validate_transfer(&mut self, transfer: &[u64]) -> R {
        let g = self.g;
        for (i, &t) in transfer.iter().enumerate() {
            if !is_object(g, t) {
                return Err(dce(g, &format!("Value at index {i} of the transfer list is not an object.")));
            }
            let mut info = RjsBrandInfo::default();
            let b = glue::rjs_brand(g, t, &mut info);
            let tg = if b == brand::ARRAY_BUFFER {
                tag::TRANSFERRED_BUFFER
            } else if b == brand::SHARED_ARRAY_BUFFER {
                return Err(dce(g, &format!("SharedArrayBuffer at index {i} is not transferable.")));
            } else if super::image::bitmap_of(g, t).is_some() {
                tag::IMAGE_BITMAP
            } else if self.it.streams.iter().any(|&p| proto_chain_has(g, t, p)) {
                advise_stream_transfer();
                return Err(dce(g, &format!("Value at index {i} is a stream; transferring streams is not supported in this runtime.")));
            } else {
                return Err(dce(g, &format!("Value at index {i} does not have a transferable type.")));
            };
            if self.attach_of.contains_key(&t) {
                return Err(dce(g, &format!("Value at index {i} is a duplicate of an earlier entry in the transfer list.")));
            }
            self.attach_of.insert(t, (i as u32, tg));
            self.attachments.push(None);
        }
        Ok(())
    }

    /// HTML StructuredSerializeWithTransfer step 5: per listed object, in
    /// order — still attached, take its data, detach it.
    unsafe fn transfer(&mut self, transfer: &[u64]) -> R {
        let g = self.g;
        for (i, &t) in transfer.iter().enumerate() {
            let (_, tg) = self.attach_of[&t];
            if tg == tag::TRANSFERRED_BUFFER {
                let mut info = RjsBrandInfo::default();
                glue::rjs_brand(g, t, &mut info);
                if info.flags & brand::F_DETACHED != 0 {
                    return Err(dce(g, &format!("ArrayBuffer at index {i} is already detached.")));
                }
                let n = info.byte_length as usize;
                let bytes = if n == 0 || info.data.is_null() {
                    Vec::new()
                } else {
                    core::slice::from_raw_parts(info.data as *const u8, n).to_vec()
                };
                let max_byte_length = (info.flags & brand::F_RESIZABLE != 0).then_some(info.max_byte_length);
                if glue::rjs_arraybuffer_detach(g, t) != 0 {
                    return Err(error_value(g, 1, &format!("ArrayBuffer at index {i} cannot be detached.")));
                }
                self.attachments[i] = Some(Attachment::TransferredBuffer { bytes, max_byte_length });
            } else {
                let Some(b) = super::image::bitmap_of(g, t) else {
                    return Err(dce(g, &format!("Value at index {i} does not have a transferable type.")));
                };
                if b.width == 0 || b.height == 0 {
                    return Err(dce(g, &format!("ImageBitmap at index {i} is already detached.")));
                }
                let rgba = core::mem::take(&mut b.rgba);
                let (width, height) = (b.width, b.height);
                b.width = 0;
                b.height = 0;
                self.attachments[i] = Some(Attachment::Bitmap { width, height, rgba });
            }
        }
        Ok(())
    }

    unsafe fn extra(&mut self, v: u64, tg: u8, a: Attachment) {
        let idx = self.attachments.len() as u32;
        self.attachments.push(Some(a));
        self.attach_of.insert(v, (idx, tg));
        self.keep.hold(v);
        self.put(tg);
        self.varint(idx as u64);
    }

    unsafe fn value(&mut self, v: u64, depth: u32) -> R {
        match v {
            UNDEF => self.put(tag::UNDEFINED),
            NULL => self.put(tag::NULL),
            FALSE => self.put(tag::FALSE),
            TRUE => self.put(tag::TRUE),
            _ if (v & TAG_NUMBER) == TAG_NUMBER => {
                let i = v as u32 as i32;
                self.put(tag::INT32);
                self.varint(((i << 1) ^ (i >> 31)) as u32 as u64);
            }
            _ if (v & TAG_NUMBER) != 0 => {
                self.put(tag::DOUBLE);
                self.f64(dec_f64(v).unwrap_or(f64::NAN));
            }
            _ => match glue::rjs_kind(self.g, v) {
                5 => return self.string(v),
                6 => return Err(dce(self.g, "A Symbol could not be cloned.")),
                7 => {
                    self.put(tag::BIGINT);
                    return self.bigint_digits(v);
                }
                _ if is_cell(v) => return self.object(v, depth),
                _ => return Err(dce(self.g, "This value could not be cloned.")),
            },
        }
        Ok(())
    }

    unsafe fn object(&mut self, v: u64, depth: u32) -> R {
        let g = self.g;
        if let Some(&(idx, tg)) = self.attach_of.get(&v) {
            self.put(tg);
            self.varint(idx as u64);
            return Ok(());
        }
        if let Some(&id) = self.ids.get(&v) {
            self.put(tag::OBJECT_REF);
            self.varint(id as u64);
            return Ok(());
        }
        if !self.stack.ok(depth) {
            return Err(stack_overflow(g));
        }
        let mut info = RjsBrandInfo::default();
        match glue::rjs_brand(g, v, &mut info) {
            brand::BOOLEAN_OBJECT | brand::NUMBER_OBJECT | brand::STRING_OBJECT | brand::BIGINT_OBJECT => {
                let b = info.brand;
                let mut prim = UNDEF;
                glue::rjs_wrapper_value(g, v, &mut prim);
                self.memo(v);
                match b {
                    brand::BOOLEAN_OBJECT => {
                        self.put(tag::BOOLEAN_OBJECT);
                        self.put((prim == TRUE) as u8);
                    }
                    brand::NUMBER_OBJECT => {
                        self.put(tag::NUMBER_OBJECT);
                        self.f64(dec_f64(prim).unwrap_or(f64::NAN));
                    }
                    brand::STRING_OBJECT => {
                        self.put(tag::STRING_OBJECT);
                        self.string(prim)?;
                    }
                    _ => {
                        self.put(tag::BIGINT_OBJECT);
                        self.bigint_digits(prim)?;
                    }
                }
                Ok(())
            }
            brand::DATE => {
                self.memo(v);
                self.put(tag::DATE);
                self.f64(info.number);
                Ok(())
            }
            brand::REGEXP => {
                let (mut p, mut f) = (UNDEF, UNDEF);
                glue::rjs_regexp_parts(g, v, &mut p, &mut f);
                self.memo(v);
                self.put(tag::REGEXP);
                self.string(p)?;
                self.string(f)
            }
            brand::ARRAY_BUFFER => {
                if info.flags & brand::F_DETACHED != 0 {
                    return Err(dce(g, "An ArrayBuffer is detached and could not be cloned."));
                }
                self.memo(v);
                let n = info.byte_length;
                if info.flags & brand::F_RESIZABLE != 0 {
                    self.put(tag::RESIZABLE_ARRAY_BUFFER);
                    self.varint(n);
                    self.varint(info.max_byte_length);
                } else {
                    self.put(tag::ARRAY_BUFFER);
                    self.varint(n);
                }
                if n > 0 && !info.data.is_null() {
                    self.out.extend_from_slice(core::slice::from_raw_parts(info.data as *const u8, n as usize));
                }
                Ok(())
            }
            brand::SHARED_ARRAY_BUFFER => Err(dce(g, "A SharedArrayBuffer could not be cloned: this context is not cross-origin isolated.")),
            brand::VIEW => self.view(v, &info, depth),
            brand::MAP => self.map(v, depth),
            brand::SET => self.set(v, depth),
            brand::ERROR if info.flags & brand::F_REDJS_SLOT != 0 => self.dom_exception(v),
            brand::ERROR => self.error(v, depth),
            brand::ARRAY => {
                let len = array_len(g, v);
                self.memo(v);
                self.put(tag::ARRAY);
                self.varint(len as u64);
                let mut dense = 0u32;
                if glue::rjs_array_dense(g, v, &mut dense) == 1 {
                    for i in 0..dense {
                        self.index_prop(v, i, depth)?;
                    }
                } else {
                    self.props(v, depth)?;
                }
                self.put(tag::END);
                Ok(())
            }
            brand::PLAIN if info.flags & brand::F_REDJS_SLOT != 0 => match super::platform::image_data_parts(g, v) {
                Some((data, w, h)) => {
                    self.memo(v);
                    self.put(tag::IMAGE_DATA);
                    self.varint(w as u64);
                    self.varint(h as u64);
                    self.put(0); // "srgb"
                    self.value(data, depth + 1)
                }
                None => Err(dce(g, "This platform object could not be cloned.")),
            },
            brand::PLAIN => {
                self.memo(v);
                self.put(tag::OBJECT);
                self.props(v, depth)?;
                self.put(tag::END);
                Ok(())
            }
            brand::OTHER => self.host_object(v),
            brand::FUNCTION => Err(dce(g, "A function could not be cloned.")),
            brand::PROXY => Err(dce(g, "A Proxy could not be cloned.")),
            brand::PROMISE => Err(dce(g, "A Promise could not be cloned.")),
            brand::GLOBAL => Err(dce(g, "The global object could not be cloned.")),
            brand::SYMBOL_OBJECT => Err(dce(g, "A Symbol object could not be cloned.")),
            _ => Err(dce(g, "This object could not be cloned.")),
        }
    }

    /// HTML's property loop over a snapshot of the enumerable own keys:
    /// each one still own → `? Get` → serialize.
    unsafe fn props(&mut self, v: u64, depth: u32) -> R {
        let g = self.g;
        let mut keys = UNDEF;
        if glue::rjs_own_keys(g, v, &mut keys) != 0 {
            return Err(keys);
        }
        let n = array_len(g, keys);
        for i in 0..n {
            let key = get_i(g, keys, i);
            let index = match js_str(g, key) {
                Some(JsStr::Latin1(b)) => canonical_index(b),
                Some(JsStr::Utf16(u)) => canonical_index(u),
                None => None,
            };
            if let Some(ix) = index {
                self.index_prop(v, ix, depth)?;
                continue;
            }
            let mut val = UNDEF;
            match glue::rjs_own_get_v(g, v, key, &mut val) {
                1 => {}
                0 => continue,
                _ => return Err(val),
            }
            self.string(key)?;
            self.value(val, depth + 1)?;
        }
        Ok(())
    }

    unsafe fn index_prop(&mut self, v: u64, i: u32, depth: u32) -> R {
        let mut val = UNDEF;
        match glue::rjs_own_get_index(self.g, v, i, &mut val) {
            1 => {}
            0 => return Ok(()),
            _ => return Err(val),
        }
        self.put(tag::KEY_INDEX);
        self.varint(i as u64);
        self.value(val, depth + 1)
    }

    unsafe fn view(&mut self, v: u64, info: &RjsBrandInfo, depth: u32) -> R {
        let g = self.g;
        let kind = info.ta_kind as usize;
        if !(1..=13).contains(&kind) {
            return Err(dce(g, "This view could not be cloned."));
        }
        let mut buf = UNDEF;
        match glue::rjs_view_buffer(g, v, &mut buf) {
            0 => {}
            1 => return Err(buf),
            _ => return Err(dce(g, "This view could not be cloned.")),
        }
        let mut binfo = RjsBrandInfo::default();
        glue::rjs_brand(g, buf, &mut binfo);
        if info.flags & brand::F_DETACHED != 0 || binfo.flags & brand::F_DETACHED != 0 {
            return Err(dce(g, "An ArrayBuffer is detached and could not be cloned."));
        }
        // IsArrayBufferViewOutOfBounds (a resizable buffer that shrank).
        let tracking = info.flags & brand::F_LENGTH_TRACKING != 0;
        let blen = binfo.byte_length;
        let end = info.byte_offset.saturating_add(if tracking { 0 } else { info.length.saturating_mul(ELEM_SIZE[kind]) });
        if binfo.brand == brand::ARRAY_BUFFER && end > blen {
            return Err(dce(g, "A view that is out of bounds could not be cloned."));
        }
        self.memo(v);
        self.put(tag::VIEW);
        self.put(kind as u8);
        self.varint(info.byte_offset);
        self.varint(if tracking { 0 } else { info.length + 1 });
        self.object(buf, depth + 1)
    }

    unsafe fn map(&mut self, v: u64, depth: u32) -> R {
        let (g, it) = (self.g, self.it);
        self.memo(v);
        self.put(tag::MAP);
        // copiedList first: what the walk below does to the map is not seen.
        let iter = call(g, it.map_entries, v, &[])?;
        let mut pairs = Vec::new();
        loop {
            let r = call(g, it.map_next, iter, &[])?;
            if get_a(g, r, it.a_done) == TRUE {
                break;
            }
            let pair = get_a(g, r, it.a_value);
            let (k, x) = (get_i(g, pair, 0), get_i(g, pair, 1));
            self.keep.hold(k);
            self.keep.hold(x);
            pairs.push((k, x));
        }
        for (k, x) in pairs {
            self.value(k, depth + 1)?;
            self.value(x, depth + 1)?;
        }
        self.put(tag::END);
        Ok(())
    }

    unsafe fn set(&mut self, v: u64, depth: u32) -> R {
        let (g, it) = (self.g, self.it);
        self.memo(v);
        self.put(tag::SET);
        let iter = call(g, it.set_values, v, &[])?;
        let mut items = Vec::new();
        loop {
            let r = call(g, it.set_next, iter, &[])?;
            if get_a(g, r, it.a_done) == TRUE {
                break;
            }
            let x = get_a(g, r, it.a_value);
            self.keep.hold(x);
            items.push(x);
        }
        for x in items {
            self.value(x, depth + 1)?;
        }
        self.put(tag::END);
        Ok(())
    }

    unsafe fn error(&mut self, v: u64, depth: u32) -> R {
        let (g, it) = (self.g, self.it);
        // name = ? Get(value, "name"), outside HTML's list → "Error".
        let name = get_x(g, v, it.a_name)?;
        let name_ix = match js_str(g, name) {
            Some(JsStr::Latin1(b)) => ERROR_NAMES.iter().position(|n| n.as_bytes() == b),
            Some(JsStr::Utf16(u)) => ERROR_NAMES.iter().position(|n| n.encode_utf16().eq(u.iter().copied())),
            None => None,
        }
        .unwrap_or(0);
        // message only from an own DATA property, then ? ToString.
        let message = match own_data(g, v, it.a_message)? {
            Some(m) => {
                let mut s = UNDEF;
                if glue::rjs_kind(g, m) == 5 {
                    s = m;
                } else if glue::rjs_to_string(g, m, &mut s) != 0 {
                    return Err(s);
                }
                Some(s)
            }
            None => None,
        };
        let stack = own_data(g, v, it.a_stack)?.filter(|&s| glue::rjs_kind(g, s) == 5);
        let cause = own_data(g, v, it.a_cause)?;
        self.memo(v);
        self.put(tag::ERROR);
        self.put(name_ix as u8);
        self.put((message.is_some() as u8) | ((stack.is_some() as u8) << 1) | ((cause.is_some() as u8) << 2));
        if let Some(m) = message {
            self.string(m)?;
        }
        if let Some(s) = stack {
            self.string(s)?;
        }
        if let Some(c) = cause {
            self.value(c, depth + 1)?;
        }
        Ok(())
    }

    unsafe fn dom_exception(&mut self, v: u64) -> R {
        let g = self.g;
        let Some((name, message, quota)) = super::domexception::raw_parts(g, v) else {
            return Err(dce(g, "This platform object could not be cloned."));
        };
        let stack = own_data(g, v, self.it.a_stack)?.filter(|&s| glue::rjs_kind(g, s) == 5);
        let (q, r) = quota.map(|(q, r)| (dec_f64(q), dec_f64(r))).unwrap_or((None, None));
        self.memo(v);
        self.put(tag::DOMEXCEPTION);
        self.put(quota.is_some() as u8);
        self.string(name)?;
        self.string(message)?;
        self.put((stack.is_some() as u8) | ((q.is_some() as u8) << 1) | ((r.is_some() as u8) << 2));
        if let Some(s) = stack {
            self.string(s)?;
        }
        if let Some(q) = q {
            self.f64(q);
        }
        if let Some(r) = r {
            self.f64(r);
        }
        Ok(())
    }

    /// C-API host objects: the serializable platform classes, or refused.
    unsafe fn host_object(&mut self, v: u64) -> R {
        let g = self.g;
        if let Some(b) = super::blob::data_of(g, v) {
            if self.for_storage {
                let (mime, bytes) = (b.mime.clone(), b.slice_bytes());
                self.memo(v);
                self.put(tag::BLOB);
                self.rust_string(&mime);
                self.varint(bytes.len() as u64);
                self.out.extend_from_slice(bytes);
            } else {
                let a = Attachment::BlobRef { bytes: Arc::clone(&b.bytes), offset: b.offset, len: b.len, mime: b.mime.clone() };
                self.extra(v, tag::BLOB_REF, a);
            }
            return Ok(());
        }
        if let Some(b) = super::image::bitmap_of(g, v) {
            if b.width == 0 || b.height == 0 {
                return Err(dce(g, "An ImageBitmap is detached and could not be cloned."));
            }
            if self.for_storage {
                return Err(dce(g, "An ImageBitmap could not be serialized for storage."));
            }
            let a = Attachment::Bitmap { width: b.width, height: b.height, rgba: b.rgba.clone() };
            self.extra(v, tag::IMAGE_BITMAP, a);
            return Ok(());
        }
        if let Some((p, read_only)) = super::geometry::point_parts(g, v) {
            self.memo(v);
            self.put(tag::DOMPOINT);
            self.put(read_only as u8);
            for c in p {
                self.f64(c);
            }
            return Ok(());
        }
        if let Some((m, read_only)) = super::geometry::matrix_parts(g, v) {
            self.memo(v);
            self.put(tag::DOMMATRIX);
            self.put(read_only as u8);
            self.put(m.is2d as u8);
            if m.is2d {
                for i in [0, 1, 4, 5, 12, 13] {
                    self.f64(m.a[i]);
                }
            } else {
                for c in m.a {
                    self.f64(c);
                }
            }
            return Ok(());
        }
        Err(dce(g, "This platform object could not be cloned."))
    }
}

fn advise_stream_transfer() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        eprintln!(
            "[redjs] structuredClone/postMessage: transferring a ReadableStream / WritableStream / \
             TransformStream is not implemented in this runtime (a browser would move the stream) — \
             it is a DataCloneError here. Read the chunks out and send those instead."
        );
    });
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

enum Source<'a> {
    Owned(Vec<Option<Attachment>>),
    Borrowed(&'a [Attachment]),
    /// Storage bytes: no attachment table at all.
    None,
}

/// A reserved id whose object is still being built (view, ImageData).
const PLACEHOLDER: u64 = 0;

struct Dec<'a> {
    g: *mut c_void,
    it: &'static Intrinsics,
    buf: &'a [u8],
    pos: usize,
    ids: Vec<u64>,
    keep: KeepAlive,
    src: Source<'a>,
    cache: HashMap<u32, u64>,
    stack: StackGuard,
}

unsafe fn decode(g: *mut c_void, bytes: &[u8], src: Source, storage: bool) -> Result<u64, u64> {
    let _lock = Locked::new(g);
    let Some(it) = intrinsics(g) else {
        return Err(error_value(g, 1, "structured serialization is not installed in this context"));
    };
    if bytes.len() < 5 || bytes[..3] != MAGIC || bytes[3] != VERSION || bytes[4] & !FLAG_STORAGE != 0 {
        return Err(corrupt(g));
    }
    let src = if storage { Source::None } else { src };
    let mut d = Dec { g, it, buf: bytes, pos: 5, ids: Vec::new(), keep: KeepAlive::new(g), src, cache: HashMap::new(), stack: StackGuard::new(g) };
    let v = d.value(0)?;
    if d.pos != bytes.len() {
        return Err(corrupt(g));
    }
    Ok(v)
}

unsafe fn corrupt(g: *mut c_void) -> u64 {
    dce(g, "The serialized data is corrupt and could not be deserialized.")
}

impl<'a> Dec<'a> {
    unsafe fn bad<T>(&self) -> R<T> {
        Err(corrupt(self.g))
    }

    unsafe fn byte(&mut self) -> R<u8> {
        match self.buf.get(self.pos) {
            Some(&b) => {
                self.pos += 1;
                Ok(b)
            }
            None => self.bad(),
        }
    }

    unsafe fn varint(&mut self) -> R<u64> {
        let mut v: u64 = 0;
        for shift in (0..70).step_by(7) {
            let b = self.byte()?;
            if shift == 63 && b > 1 {
                return self.bad();
            }
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        self.bad()
    }

    unsafe fn u32v(&mut self) -> R<u32> {
        let v = self.varint()?;
        if v > u32::MAX as u64 {
            return self.bad();
        }
        Ok(v as u32)
    }

    unsafe fn f64(&mut self) -> R<f64> {
        let b = self.bytes(8)?;
        let d = f64::from_le_bytes(b.try_into().unwrap());
        // A NaN with a payload is not a valid JSValue double; one NaN.
        Ok(if d.is_nan() { f64::NAN } else { d })
    }

    unsafe fn bytes(&mut self, n: u64) -> R<&'a [u8]> {
        let buf: &'a [u8] = self.buf;
        if n > (buf.len() - self.pos) as u64 {
            return self.bad();
        }
        let s = &buf[self.pos..self.pos + n as usize];
        self.pos += n as usize;
        Ok(s)
    }

    unsafe fn memo(&mut self, v: u64) {
        self.ids.push(v);
        self.keep.hold(v);
    }

    fn reserve(&mut self) -> usize {
        self.ids.push(PLACEHOLDER);
        self.ids.len() - 1
    }

    unsafe fn fill(&mut self, slot: usize, v: u64) {
        self.ids[slot] = v;
        self.keep.hold(v);
    }

    unsafe fn string_body(&mut self, t: u8) -> R<u64> {
        let n = self.varint()?;
        let mut out = UNDEF;
        if t == tag::STRING_LATIN1 {
            let b = self.bytes(n)?;
            glue::rjs_string_latin1(self.g, b.as_ptr(), b.len(), &mut out);
        } else {
            let b = self.bytes(n.checked_mul(2).unwrap_or(u64::MAX))?;
            let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            glue::rjs_string16(self.g, units.as_ptr(), units.len(), &mut out);
        }
        Ok(out)
    }

    /// A position that must hold a string.
    unsafe fn string(&mut self) -> R<u64> {
        let t = self.byte()?;
        if t != tag::STRING_LATIN1 && t != tag::STRING_UTF16 {
            return self.bad();
        }
        self.string_body(t)
    }

    unsafe fn rust_string(&mut self) -> R<String> {
        let s = self.string()?;
        Ok(read_string(self.g, s).unwrap_or_default())
    }

    unsafe fn bigint(&mut self) -> R<u64> {
        let n = self.varint()?;
        let d = self.bytes(n)?;
        let body = d.strip_prefix(b"-").unwrap_or(d);
        if body.is_empty() || !body.iter().all(u8::is_ascii_digit) {
            return self.bad();
        }
        let mut s = UNDEF;
        glue::rjs_string_latin1(self.g, d.as_ptr(), d.len(), &mut s);
        call(self.g, self.it.bigint, UNDEF, &[s]).or_else(|_| self.bad())
    }

    unsafe fn value(&mut self, depth: u32) -> R<u64> {
        let t = self.byte()?;
        match t {
            tag::UNDEFINED => Ok(UNDEF),
            tag::NULL => Ok(NULL),
            tag::FALSE => Ok(FALSE),
            tag::TRUE => Ok(TRUE),
            tag::INT32 => {
                let z = self.u32v()?;
                Ok(enc_i32(((z >> 1) as i32) ^ -((z & 1) as i32)))
            }
            tag::DOUBLE => Ok(enc_f64(self.f64()?)),
            tag::BIGINT => self.bigint(),
            tag::STRING_LATIN1 | tag::STRING_UTF16 => self.string_body(t),
            tag::OBJECT_REF => {
                let id = self.varint()?;
                match self.ids.get(id as usize) {
                    Some(&v) if v != PLACEHOLDER => Ok(v),
                    _ => self.bad(),
                }
            }
            tag::TRANSFERRED_BUFFER | tag::IMAGE_BITMAP | tag::BLOB_REF => self.attachment(t),
            _ => {
                if !self.stack.ok(depth) {
                    return Err(stack_overflow(self.g));
                }
                self.object(t, depth)
            }
        }
    }

    unsafe fn props(&mut self, o: u64, depth: u32) -> R {
        let g = self.g;
        loop {
            let t = self.byte()?;
            match t {
                tag::END => return Ok(()),
                tag::KEY_INDEX => {
                    let i = self.u32v()?;
                    if i == u32::MAX {
                        return self.bad();
                    }
                    let v = self.value(depth + 1)?;
                    if glue::rjs_define_index(g, o, i, v) != 0 {
                        return self.bad();
                    }
                }
                tag::STRING_LATIN1 | tag::STRING_UTF16 => {
                    let k = self.string_body(t)?;
                    let v = self.value(depth + 1)?;
                    if glue::rjs_define_v(g, o, k, v) != 0 {
                        return self.bad();
                    }
                }
                _ => return self.bad(),
            }
        }
    }

    /// The message/stack/cause tail shared by Error and DOMException: the
    /// engine's own capture of `stack` is materialised first, so the carried
    /// one (or its absence) is what stays.
    unsafe fn set_stack(&mut self, e: u64, stack: Option<u64>) {
        let a = self.it.a_stack;
        let _ = get_a(self.g, e, a);
        match stack {
            Some(s) => {
                glue::rjs_define(self.g, e, a, s, 2);
            }
            None => {
                glue::rjs_delete(self.g, e, a);
            }
        }
    }

    unsafe fn object(&mut self, t: u8, depth: u32) -> R<u64> {
        let (g, it) = (self.g, self.it);
        match t {
            tag::OBJECT => {
                let o = new_object(g, UNDEF);
                self.memo(o);
                self.props(o, depth)?;
                Ok(o)
            }
            tag::ARRAY => {
                let len = self.u32v()?;
                let mut a = UNDEF;
                if glue::rjs_array_create(g, len, &mut a) != 0 {
                    return Err(a);
                }
                self.memo(a);
                self.props(a, depth)?;
                Ok(a)
            }
            tag::DATE => {
                let t = self.f64()?;
                let d = construct(g, it.date, &[enc_f64(t)]).or_else(|_| self.bad())?;
                self.memo(d);
                Ok(d)
            }
            tag::REGEXP => {
                let p = self.string()?;
                let f = self.string()?;
                let r = construct(g, it.regexp, &[p, f]).or_else(|_| self.bad())?;
                self.memo(r);
                Ok(r)
            }
            tag::MAP => {
                let m = construct(g, it.map, &[]).or_else(|_| self.bad())?;
                self.memo(m);
                loop {
                    if self.buf.get(self.pos) == Some(&tag::END) {
                        self.pos += 1;
                        return Ok(m);
                    }
                    let k = self.value(depth + 1)?;
                    let v = self.value(depth + 1)?;
                    call(g, it.map_set, m, &[k, v])?;
                }
            }
            tag::SET => {
                let s = construct(g, it.set, &[]).or_else(|_| self.bad())?;
                self.memo(s);
                loop {
                    if self.buf.get(self.pos) == Some(&tag::END) {
                        self.pos += 1;
                        return Ok(s);
                    }
                    let v = self.value(depth + 1)?;
                    call(g, it.set_add, s, &[v])?;
                }
            }
            tag::BOOLEAN_OBJECT | tag::NUMBER_OBJECT | tag::STRING_OBJECT | tag::BIGINT_OBJECT => {
                let prim = match t {
                    tag::BOOLEAN_OBJECT => match self.byte()? {
                        0 => FALSE,
                        1 => TRUE,
                        _ => return self.bad(),
                    },
                    tag::NUMBER_OBJECT => enc_f64(self.f64()?),
                    tag::STRING_OBJECT => self.string()?,
                    _ => self.bigint()?,
                };
                let w = call(g, it.object, UNDEF, &[prim]).or_else(|_| self.bad())?;
                self.memo(w);
                Ok(w)
            }
            tag::ERROR => {
                let name = self.byte()? as usize;
                let mask = self.byte()?;
                if name >= ERROR_NAMES.len() || mask & !7 != 0 {
                    return self.bad();
                }
                let e = construct(g, it.errors[name], &[]).or_else(|_| self.bad())?;
                self.memo(e);
                if mask & 1 != 0 {
                    let m = self.string()?;
                    glue::rjs_define(g, e, it.a_message, m, 2);
                }
                let stack = if mask & 2 != 0 { Some(self.string()?) } else { None };
                self.set_stack(e, stack);
                if mask & 4 != 0 {
                    let c = self.value(depth + 1)?;
                    glue::rjs_define(g, e, it.a_cause, c, 2);
                }
                Ok(e)
            }
            tag::ARRAY_BUFFER | tag::RESIZABLE_ARRAY_BUFFER => {
                let n = self.varint()?;
                let max = if t == tag::RESIZABLE_ARRAY_BUFFER { Some(self.varint()?) } else { None };
                if max.is_some_and(|m| m < n) {
                    return self.bad();
                }
                let bytes = self.bytes(n)?;
                let ab = new_buffer(g, it, bytes, max).or_else(|_| self.bad())?;
                self.memo(ab);
                Ok(ab)
            }
            tag::VIEW => {
                let kind = self.byte()? as usize;
                let offset = self.varint()?;
                let len_p1 = self.varint()?;
                if !(1..=13).contains(&kind) || !is_cell(it.views[kind]) {
                    return self.bad();
                }
                let slot = self.reserve();
                let buffer = self.value(depth + 1)?;
                let mut info = RjsBrandInfo::default();
                if glue::rjs_brand(g, buffer, &mut info) != brand::ARRAY_BUFFER {
                    return self.bad();
                }
                let mut args = vec![buffer, enc_f64(offset as f64)];
                if len_p1 != 0 {
                    args.push(enc_f64((len_p1 - 1) as f64));
                }
                let v = construct(g, it.views[kind], &args).or_else(|_| self.bad())?;
                self.fill(slot, v);
                Ok(v)
            }
            tag::BLOB => {
                let mime = self.rust_string()?;
                let n = self.varint()?;
                let bytes = self.bytes(n)?.to_vec();
                let b = super::blob::create(g, UNDEF, bytes, mime);
                self.memo(b);
                Ok(b)
            }
            tag::IMAGE_DATA => {
                let w = self.u32v()?;
                let h = self.u32v()?;
                if self.byte()? != 0 {
                    return self.bad();
                }
                let slot = self.reserve();
                let data = self.value(depth + 1)?;
                let Some(v) = super::platform::create_image_data(g, data, w, h) else { return self.bad() };
                self.fill(slot, v);
                Ok(v)
            }
            tag::DOMPOINT => {
                let read_only = match self.byte()? {
                    0 => false,
                    1 => true,
                    _ => return self.bad(),
                };
                let mut p = [0.0; 4];
                for c in &mut p {
                    *c = self.f64()?;
                }
                let Some(v) = super::geometry::create_point(g, p, read_only) else { return self.bad() };
                self.memo(v);
                Ok(v)
            }
            tag::DOMMATRIX => {
                let read_only = self.byte()?;
                let is2d = self.byte()?;
                if read_only > 1 || is2d > 1 {
                    return self.bad();
                }
                let m = if is2d == 1 {
                    let mut v = [0.0; 6];
                    for c in &mut v {
                        *c = self.f64()?;
                    }
                    super::geometry::Mat::from_2d(v)
                } else {
                    let mut v = [0.0; 16];
                    for c in &mut v {
                        *c = self.f64()?;
                    }
                    super::geometry::Mat::from_16(v)
                };
                let Some(v) = super::geometry::create_matrix(g, m, read_only == 1) else { return self.bad() };
                self.memo(v);
                Ok(v)
            }
            tag::DOMEXCEPTION => {
                let kind = self.byte()?;
                let name = self.string()?;
                let message = self.string()?;
                let mask = self.byte()?;
                if kind > 1 || mask & !7 != 0 || (kind == 0 && mask & 6 != 0) {
                    return self.bad();
                }
                let stack = if mask & 1 != 0 { Some(self.string()?) } else { None };
                let q = if mask & 2 != 0 { enc_f64(self.f64()?) } else { NULL };
                let r = if mask & 4 != 0 { enc_f64(self.f64()?) } else { NULL };
                let quota = (kind == 1).then_some((q, r));
                let Some(e) = super::domexception::from_raw(g, name, message, quota) else { return self.bad() };
                self.memo(e);
                self.set_stack(e, stack);
                Ok(e)
            }
            _ => self.bad(),
        }
    }

    unsafe fn attachment(&mut self, t: u8) -> R<u64> {
        let g = self.g;
        let i = self.u32v()?;
        if let Some(&v) = self.cache.get(&i) {
            return Ok(v);
        }
        let ix = i as usize;
        // Owned: the entry moves out (transferred data moves in). Borrowed:
        // a copy per receiver — a Blob's is just another `Arc`.
        let a: Option<Attachment> = match &mut self.src {
            Source::None => None,
            Source::Owned(list) => list.get_mut(ix).and_then(Option::take),
            Source::Borrowed(list) => list.get(ix).cloned(),
        };
        let v = match (t, a) {
            (tag::TRANSFERRED_BUFFER, Some(Attachment::TransferredBuffer { bytes, max_byte_length })) => {
                new_buffer(g, self.it, &bytes, max_byte_length).or_else(|_| self.bad())?
            }
            (tag::IMAGE_BITMAP, Some(Attachment::Bitmap { width, height, rgba })) => {
                if width == 0 || height == 0 || rgba.len() as u64 != width as u64 * height as u64 * 4 {
                    return self.bad();
                }
                super::image::create_bitmap(g, super::image::BitmapData { width, height, rgba })
            }
            (tag::BLOB_REF, Some(Attachment::BlobRef { bytes, offset, len, mime })) => {
                if !matches!(offset.checked_add(len), Some(end) if end <= bytes.len()) {
                    return self.bad();
                }
                super::blob::from_data(g, super::blob::BlobData { bytes, offset, len, mime })
            }
            _ => return self.bad(),
        };
        self.cache.insert(i, v);
        self.keep.hold(v);
        Ok(v)
    }
}

/// A fresh ArrayBuffer in this realm holding `bytes` (resizable to `max`).
unsafe fn new_buffer(g: *mut c_void, it: &Intrinsics, bytes: &[u8], max: Option<u64>) -> R<u64> {
    let n = enc_f64(bytes.len() as f64);
    let ab = match max {
        Some(m) => {
            let opts = new_object(g, NULL);
            define(g, opts, "maxByteLength", enc_f64(m as f64), 0);
            construct(g, it.array_buffer, &[n, opts])?
        }
        None => construct(g, it.array_buffer, &[n])?,
    };
    if !bytes.is_empty() {
        let mut info = RjsBrandInfo::default();
        glue::rjs_brand(g, ab, &mut info);
        if info.data.is_null() || (info.byte_length as usize) < bytes.len() {
            return Err(error_value(g, 2, "Array buffer allocation failed"));
        }
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), info.data as *mut u8, bytes.len());
    }
    Ok(ab)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webapi::testing::{ctx, ok, s, throws};

    /// `name/code` of what `src` throws, through JS so the DOMException is real.
    fn thrown(c: &crate::api::Context, src: &str) -> String {
        s(c, &format!("(() => {{ try {{ {src}; return 'no throw'; }} catch (e) {{ return (e instanceof DOMException ? 'DOMException:' : '') + e.name + '/' + (e.code ?? '-'); }} }})()"))
    }

    #[test]
    fn primitives_round_trip_exactly() {
        let c = ctx();
        ok(&c, "(() => { const v = [undefined, null, true, false, 0, -0, 1.5, NaN, Infinity, -Infinity, -2147483648, 2147483647, 2**31, 2**53, -1e-300, \
                 10n, -(2n ** 80n), 0n, 'abc', '\\u00e9t\\u00e9', '\\u{1F600}', '\\uD800x', '']; \
                 const r = structuredClone(v); \
                 return r.length === v.length && r.every((x, i) => Object.is(x, v[i])); })()");
    }

    #[test]
    fn objects_arrays_keys_and_holes() {
        let c = ctx();
        ok(&c, "(() => { const a = [1, , 3]; a.extra = 'x'; a[10] = 'ten'; \
                 const r = structuredClone(a); \
                 return Array.isArray(r) && r.length === 11 && !(1 in r) && r[2] === 3 && r.extra === 'x' && r[10] === 'ten'; })()");
        ok(&c, "(() => { const o = { b: 1, a: 2, 2: 'two', 1: 'one' }; Object.defineProperty(o, 'hidden', { value: 1, enumerable: false }); \
                 o.__proto__x = 0; const r = structuredClone(o); \
                 return Object.keys(r).join() === '1,2,b,a,__proto__x' && !('hidden' in r) && Object.getPrototypeOf(r) === Object.prototype; })()");
        // An own "__proto__" key is a key, not a prototype.
        ok(&c, "(() => { const o = JSON.parse('{\"__proto__\": {\"x\": 1}}'); const r = structuredClone(o); \
                 return Object.getPrototypeOf(r) === Object.prototype && Object.hasOwn(r, '__proto__') && r.__proto__.x === 1; })()");
        // Null-prototype objects come back with Object.prototype (HTML).
        ok(&c, "(() => { const o = Object.create(null); o.k = 1; const r = structuredClone(o); return Object.getPrototypeOf(r) === Object.prototype && r.k === 1; })()");
        // Class instances lose their class, keep their own data.
        ok(&c, "(() => { class P { #secret = 1; constructor() { this.x = 2; } get y() { return 3; } } const r = structuredClone(new P()); \
                 return Object.getPrototypeOf(r) === Object.prototype && r.x === 2 && !('y' in r); })()");
    }

    #[test]
    fn getters_run_once_and_deleted_keys_are_skipped() {
        let c = ctx();
        ok(&c, "(() => { let n = 0; const o = { get a() { n++; delete this.b; return 'A'; }, b: 'B', c: 'C' }; \
                 const r = structuredClone(o); \
                 return n === 1 && r.a === 'A' && !('b' in r) && r.c === 'C' && Object.getOwnPropertyDescriptor(r, 'a').value === 'A'; })()");
        // A getter that throws propagates as itself.
        assert_eq!(
            s(&c, "(() => { try { structuredClone({ get a() { throw new RangeError('boom'); } }); } catch (e) { return e.name + ':' + e.message; } })()"),
            "RangeError:boom"
        );
        // Array: a getter shrinking the array mid-walk.
        ok(&c, "(() => { const a = [0, 1, 2, 3]; Object.defineProperty(a, 1, { get() { a.length = 2; return 'one'; }, enumerable: true, configurable: true }); \
                 const r = structuredClone(a); return r.length === 4 && r[1] === 'one' && !(2 in r) && !(3 in r); })()");
    }

    #[test]
    fn identity_and_cycles() {
        let c = ctx();
        ok(&c, "(() => { const shared = { s: 1 }; const o = { a: shared, b: shared, list: [shared] }; o.self = o; o.list.push(o.list); \
                 const r = structuredClone(o); \
                 return r.a === r.b && r.list[0] === r.a && r.self === r && r.list[1] === r.list && r.a !== shared; })()");
        ok(&c, "(() => { const k = {}; const m = new Map([[k, k], ['x', 1]]); m.set('m', m); const st = new Set([k, m]); \
                 const r = structuredClone({ m, st }); const rk = [...r.m.keys()][0]; \
                 return r.m.get(rk) === rk && r.m.get('m') === r.m && r.st.has(rk) && r.st.has(r.m) && r.m.get('x') === 1; })()");
        // Two views over one buffer stay over one buffer.
        ok(&c, "(() => { const b = new ArrayBuffer(8); const u = new Uint8Array(b); const d = new DataView(b, 2, 4); u[3] = 9; \
                 const r = structuredClone({ u, d, b }); \
                 return r.u.buffer === r.b && r.d.buffer === r.b && r.d.byteOffset === 2 && r.d.byteLength === 4 && r.d.getUint8(1) === 9; })()");
        // An Error whose cause is itself.
        ok(&c, "(() => { const e = new Error('loop'); e.cause = e; const r = structuredClone(e); return r.cause === r && r.message === 'loop'; })()");
    }

    #[test]
    fn builtins_round_trip() {
        let c = ctx();
        ok(&c, "(() => { const d = new Date(12345); const bad = new Date(NaN); const re = /a+b/giu; re.lastIndex = 3; \
                 const r = structuredClone([d, bad, re, new Boolean(false), new Number(-0), new String('s\\uD800'), Object(5n)]); \
                 return r[0] instanceof Date && r[0].getTime() === 12345 && Number.isNaN(r[1].getTime()) \
                   && r[2] instanceof RegExp && r[2].source === 'a+b' && r[2].flags === 'giu' && r[2].lastIndex === 0 \
                   && typeof r[3] === 'object' && r[3].valueOf() === false && Object.is(r[4].valueOf(), -0) \
                   && r[5].valueOf() === 's\\uD800' && typeof r[6] === 'object' && r[6].valueOf() === 5n; })()");
        // Map/Set keep insertion order; a Map subclass comes back a Map.
        ok(&c, "(() => { class M extends Map {} const m = new M([[3, 'c'], [1, 'a']]); const r = structuredClone(m); \
                 return Object.getPrototypeOf(r) === Map.prototype && [...r.keys()].join() === '3,1'; })()");
        // Overriding the iteration surface after install changes nothing.
        ok(&c, "(() => { const src = new Map([['k', 1]]); const P = Map.prototype, ent = P.entries, it = P[Symbol.iterator], set = P.set; \
                 P.entries = P[Symbol.iterator] = function* () { yield ['evil', 1]; }; \
                 P.set = function () { throw new Error('set called'); }; \
                 try { const r = structuredClone(src); return r.size === 1 && ent.call(r).next().value[0] === 'k'; } \
                 finally { P.entries = ent; P[Symbol.iterator] = it; P.set = set; } })()");
    }

    #[test]
    fn errors_follow_html() {
        let c = ctx();
        ok(&c, "(() => { const r = structuredClone([new TypeError('t'), new RangeError('r'), new SyntaxError('s'), new URIError('u'), \
                 new EvalError('e'), new ReferenceError('f'), new AggregateError([], 'agg')]); \
                 return r.map(e => e.constructor.name + ':' + e.message).join() === \
                   'TypeError:t,RangeError:r,SyntaxError:s,URIError:u,EvalError:e,ReferenceError:f,Error:agg'; })()");
        // Name from Get, normalized; message only from an own data property.
        ok(&c, "(() => { const e = new Error('m'); e.name = 'TypeError'; const f = new Error('m2'); f.name = 'Custom'; \
                 const g = new Error(); Object.defineProperty(g, 'message', { get() { return 'getter'; } }); \
                 const [re, rf, rg] = structuredClone([e, f, g]); \
                 return re instanceof TypeError && re.message === 'm' && rf.constructor === Error && rf.name === 'Error' \
                   && !Object.hasOwn(rg, 'message') && rg.message === ''; })()");
        // Stack and cause come along; message is non-enumerable as from the constructor.
        ok(&c, "(() => { const e = new Error('x', { cause: { why: 1 } }); const r = structuredClone(e); \
                 return r.stack === e.stack && r.cause.why === 1 && r.cause !== e.cause \
                   && !Object.getOwnPropertyDescriptor(r, 'message').enumerable; })()");
        // A message whose ToString throws: the throw propagates.
        assert_eq!(
            s(&c, "(() => { const e = new Error(); e.message = { toString() { throw new Error('ts'); } }; try { structuredClone(e); } catch (x) { return x.message; } })()"),
            "ts"
        );
    }

    #[test]
    fn dom_exceptions_round_trip() {
        let c = ctx();
        ok(&c, "(() => { const e = new DOMException('gone', 'NotFoundError'); const r = structuredClone(e); \
                 return r instanceof DOMException && r !== e && r.name === 'NotFoundError' && r.message === 'gone' && r.code === 8 && r.stack === e.stack; })()");
        ok(&c, "(() => { const q = new QuotaExceededError('full', { quota: 10, requested: 12 }); const r = structuredClone(q); \
                 const q2 = structuredClone(new QuotaExceededError('x')); \
                 return r instanceof QuotaExceededError && r.quota === 10 && r.requested === 12 && r.code === 22 \
                   && q2.quota === null && q2.requested === null; })()");
    }

    #[test]
    fn buffers_and_views() {
        let c = ctx();
        ok(&c, "(() => { const kinds = [Int8Array, Uint8Array, Uint8ClampedArray, Int16Array, Uint16Array, Int32Array, Uint32Array, \
                   Float32Array, Float64Array, BigInt64Array, BigUint64Array]; \
                 return kinds.every(K => { const b = new ArrayBuffer(64); const v = new K(b, 8, 3); v[1] = K.name.startsWith('Big') ? 7n : 7; \
                   const r = structuredClone(v); return r instanceof K && r.byteOffset === 8 && r.length === 3 && r[1] == 7 && r.buffer.byteLength === 64; }); })()");
        ok(&c, "(() => { const rab = new ArrayBuffer(4, { maxByteLength: 16 }); const tracking = new Uint8Array(rab); const fixed = new Uint8Array(rab, 1, 2); \
                 const r = structuredClone({ rab, tracking, fixed }); r.rab.resize(10); \
                 return r.rab.resizable && r.rab.maxByteLength === 16 && r.tracking.length === 10 && r.fixed.length === 2; })()");
        assert_eq!(thrown(&c, "const rab = new ArrayBuffer(8, { maxByteLength: 8 }); const v = new Uint8Array(rab, 4, 4); rab.resize(2); structuredClone(v)"), "DOMException:DataCloneError/25");
        assert_eq!(thrown(&c, "const b = new ArrayBuffer(4); structuredClone(0, { transfer: [b] }); structuredClone(b)"), "DOMException:DataCloneError/25");
        assert_eq!(thrown(&c, "const b = new ArrayBuffer(4); const v = new Uint8Array(b); structuredClone(0, { transfer: [b] }); structuredClone(v)"), "DOMException:DataCloneError/25");
    }

    #[test]
    fn transfer_follows_html() {
        let c = ctx();
        ok(&c, "(() => { const b = new ArrayBuffer(4); new Uint8Array(b)[0] = 42; const v = new Uint8Array(b); \
                 const r = structuredClone({ v, b }, { transfer: [b] }); \
                 return b.byteLength === 0 && v.length === 0 && r.b.byteLength === 4 && r.v.buffer === r.b && r.v[0] === 42; })()");
        // Listed but unreferenced: still detached.
        ok(&c, "(() => { const b = new ArrayBuffer(4); structuredClone(1, { transfer: [b] }); return b.byteLength === 0; })()");
        // From any iterable.
        ok(&c, "(() => { const b = new ArrayBuffer(2); const r = structuredClone(b, { transfer: (function* () { yield b; })() }); return b.byteLength === 0 && r.byteLength === 2; })()");
        // Resizable transfers stay resizable.
        ok(&c, "(() => { const b = new ArrayBuffer(2, { maxByteLength: 8 }); const r = structuredClone(b, { transfer: [b] }); return b.byteLength === 0 && r.resizable && r.maxByteLength === 8 && r.byteLength === 2; })()");
        // Refusals, and a failed serialize detaches nothing.
        assert_eq!(thrown(&c, "const b = new ArrayBuffer(1); structuredClone(0, { transfer: [b, b] })"), "DOMException:DataCloneError/25");
        assert_eq!(thrown(&c, "structuredClone(0, { transfer: [{}] })"), "DOMException:DataCloneError/25");
        assert_eq!(thrown(&c, "structuredClone(0, { transfer: [new Uint8Array(1)] })"), "DOMException:DataCloneError/25");
        ok(&c, "(() => { const b = new ArrayBuffer(4); try { structuredClone({ b, f() {} }, { transfer: [b] }); } catch (e) {} return b.byteLength === 4; })()");
        // Detached mid-walk by a getter → DataCloneError at the detach step.
        assert_eq!(
            thrown(&c, "const b = new ArrayBuffer(4); structuredClone({ get x() { structuredClone(0, { transfer: [b] }); return 1; } }, { transfer: [b] })"),
            "DOMException:DataCloneError/25"
        );
        // WebIDL conversion of the options.
        assert_eq!(thrown(&c, "structuredClone()"), "TypeError/-");
        assert_eq!(thrown(&c, "structuredClone(1, 5)"), "TypeError/-");
        assert_eq!(thrown(&c, "structuredClone(1, { transfer: 5 })"), "TypeError/-");
        assert_eq!(thrown(&c, "structuredClone(1, { transfer: [1] })"), "TypeError/-");
        assert_eq!(s(&c, "structuredClone(7, null) + structuredClone(1, { transfer: undefined })"), "8");
        // A stream in the list: refused, with the one-shot advisory.
        assert_eq!(thrown(&c, "structuredClone(0, { transfer: [new ReadableStream()] })"), "DOMException:DataCloneError/25");
    }

    #[test]
    fn refusals_are_data_clone_errors() {
        let c = ctx();
        for src in [
            "structuredClone(() => 1)",
            "structuredClone(Symbol('s'))",
            "structuredClone({ s: Symbol.iterator })",
            "structuredClone(new Proxy({}, {}))",
            "structuredClone(Promise.resolve())",
            "structuredClone(new WeakMap())",
            "structuredClone(Object(Symbol()))",
            "structuredClone(globalThis)",
            "structuredClone(new Event('x'))",
            "structuredClone(new EventTarget())",
            "structuredClone(new (class extends EventTarget {})())",
            "structuredClone(new AbortController().signal)",
            "structuredClone(new Headers())",
        ] {
            assert_eq!(thrown(&c, src), "DOMException:DataCloneError/25", "{src}");
        }
        assert!(throws(&c, "structuredClone({ f() {} })").contains("function"));
    }

    #[test]
    fn platform_objects_round_trip() {
        let c = ctx();
        ok(&c, "(() => { const b = new Blob(['hello'], { type: 'text/plain' }); const s = b.slice(1, 4); \
                 const r = structuredClone({ b, s, again: b }); \
                 return r.b instanceof Blob && r.b !== b && r.b.size === 5 && r.b.type === 'text/plain' && r.s.size === 3 && r.again === r.b; })()");
        ok(&c, "(() => { const d = new Uint8ClampedArray(8); d[5] = 200; const img = new ImageData(d, 2, 1); \
                 const r = structuredClone({ img, d }); \
                 return r.img instanceof ImageData && r.img.width === 2 && r.img.height === 1 && r.img.data === r.d && r.d[5] === 200; })()");
        ok(&c, "(() => { const p = new DOMPoint(1, 2, 3, 4); const q = new DOMPointReadOnly(5); \
                 const m = new DOMMatrix([1, 2, 3, 4, 5, 6]); const m3 = new DOMMatrixReadOnly([1, 0, 0, 0, 0, 1, 0.5, 0, 0, 0, 1, 0, 0, 0, 0, 1]); \
                 const [rp, rq, rm, rm3] = structuredClone([p, q, m, m3]); \
                 return rp instanceof DOMPoint && rp.w === 4 && !(rq instanceof DOMPoint) && rq instanceof DOMPointReadOnly && rq.x === 5 \
                   && rm instanceof DOMMatrix && rm.is2D && rm.f === 6 && !(rm3 instanceof DOMMatrix) && rm3 instanceof DOMMatrixReadOnly \
                   && !rm3.is2D && rm3.m23 === 0.5; })()");
    }

    #[test]
    fn image_bitmaps_copy_and_transfer() {
        let c = ctx();
        unsafe {
            let g = c.raw();
            let bm = super::super::image::create_bitmap(g, super::super::image::BitmapData { width: 1, height: 1, rgba: vec![1, 2, 3, 4] });
            define(g, glue::rjs_global(g), "bm", bm, 0);
        }
        ok(&c, "(() => { const r = structuredClone(bm); return r instanceof ImageBitmap && r !== bm && r.width === 1 && bm.width === 1; })()");
        ok(&c, "(() => { const r = structuredClone({ a: bm, b: bm }); return r.a === r.b; })()");
        ok(&c, "(() => { const r = structuredClone(bm, { transfer: [bm] }); return r.width === 1 && bm.width === 0; })()");
        assert_eq!(thrown(&c, "structuredClone(bm)"), "DOMException:DataCloneError/25");
    }

    unsafe fn ser(c: &crate::api::Context, src: &str, for_storage: bool) -> Result<Serialized, String> {
        let v = c.eval(src).unwrap();
        serialize(c.raw(), v.0, &Options { for_storage, transfer: &[] }).map_err(|e| c.error_info(crate::api::Value(e)).0)
    }

    #[test]
    fn wire_format_is_frozen() {
        let c = ctx();
        unsafe {
            let got = ser(&c, "({ a: 1, b: [true] })", false).unwrap();
            assert_eq!(
                got.bytes,
                [b'R', b'J', b'S', 1, 0, 0x11, 0x08, 1, b'a', 0x05, 2, 0x08, 1, b'b', 0x12, 1, 0x0A, 0, 0x04, 0x0F, 0x0F]
            );
            assert!(got.attachments.is_empty());
            let got = ser(&c, "(() => { const o = {}; return [o, o, -1, 'é\\u0100', 2n, new Date(0)]; })()", true).unwrap();
            let mut want = vec![b'R', b'J', b'S', 1, FLAG_STORAGE, 0x12, 6];
            want.extend([0x0A, 0, 0x11, 0x0F]); // [0] = {} (id 1; the array is id 0)
            want.extend([0x0A, 1, 0x10, 1]); // [1] = ref 1
            want.extend([0x0A, 2, 0x05, 1]); // -1 zigzag
            want.extend([0x0A, 3, 0x09, 2, 0xE9, 0, 0x00, 0x01]); // UTF-16LE
            want.extend([0x0A, 4, 0x07, 1, b'2']);
            want.extend([0x0A, 5, 0x14]);
            want.extend(0f64.to_le_bytes());
            want.push(0x0F);
            assert_eq!(got.bytes, want);
        }
    }

    #[test]
    fn storage_mode() {
        let c = ctx();
        unsafe {
            let g = c.raw();
            let s = ser(&c, "({ b: new Blob(['xyz'], { type: 'a/b' }), m: new Map([[1, new Set([2])]]) })", true).unwrap();
            assert!(s.attachments.is_empty());
            let v = deserialize_storage(g, &s.bytes).unwrap();
            define(g, glue::rjs_global(g), "fromDisk", v, 0);
            ok(&c, "fromDisk.b instanceof Blob && fromDisk.b.type === 'a/b' && fromDisk.b.size === 3 && fromDisk.m.get(1).has(2)");

            let bm = super::super::image::create_bitmap(g, super::super::image::BitmapData { width: 1, height: 1, rgba: vec![0; 4] });
            define(g, glue::rjs_global(g), "bm2", bm, 0);
            let e = ser(&c, "({ bm2 })", true).unwrap_err();
            assert!(e.contains("storage"), "{e}");

            // Attachment tags never come out of storage.
            let not_storage = ser(&c, "new Blob(['q'])", false).unwrap();
            assert_eq!(not_storage.bytes[5], tag::BLOB_REF);
            assert!(deserialize_storage(g, &not_storage.bytes).is_err());
        }
    }

    #[test]
    fn corrupt_bytes_are_data_clone_errors() {
        let c = ctx();
        unsafe {
            let g = c.raw();
            let good = ser(&c, "({ k: [1, 2, 3], s: 'str' })", true).unwrap().bytes;
            assert!(deserialize_storage(g, &good).is_ok());
            let mut cases: Vec<Vec<u8>> = Vec::new();
            for n in 0..good.len() {
                cases.push(good[..n].to_vec()); // every truncation
            }
            let mut trailing = good.clone();
            trailing.push(0x01);
            cases.push(trailing);
            cases.push(b"XYZ\x01\x00\x01".to_vec()); // magic
            cases.push(b"RJS\x02\x00\x01".to_vec()); // version
            cases.push(b"RJS\x01\x80\x01".to_vec()); // unknown flag
            cases.push(b"RJS\x01\x00\x10\x00".to_vec()); // ref to nothing
            cases.push(b"RJS\x01\x00\x13".to_vec()); // reserved tag
            cases.push(b"RJS\x01\x00\x22\x02\x00\x00\x10\x00".to_vec()); // view whose buffer is itself
            cases.push(b"RJS\x01\x00\x07\x02\x31\x2e".to_vec()); // "1." as a BigInt
            cases.push(b"RJS\x01\x00\x08\xff\xff\xff\xff\x0f".to_vec()); // string longer than the data
            cases.push(b"RJS\x01\x00\x1C\x07\x00".to_vec()); // error name out of range
            cases.push(b"RJS\x01\x00\x05\xff\xff\xff\xff\xff\xff\xff\xff\xff\x7f".to_vec()); // varint overflow
            for bad in &cases {
                let e = deserialize_storage(g, bad).expect_err("corrupt bytes decoded");
                define(g, glue::rjs_global(g), "corruptErr", e, 0);
                ok(&c, "corruptErr instanceof DOMException && corruptErr.name === 'DataCloneError'");
            }
            // A NaN with a payload reads as the one NaN.
            let mut nan = b"RJS\x01\x00\x06".to_vec();
            nan.extend(0xFFF8_0000_DEAD_BEEFu64.to_le_bytes());
            let v = deserialize_storage(g, &nan).unwrap();
            define(g, glue::rjs_global(g), "nanV", v, 0);
            ok(&c, "Number.isNaN(nanV)");
        }
    }

    /// Bytes made in one context decode in another context on another thread
    /// (the Worker seam): nothing in a `Serialized` points into a heap.
    #[test]
    fn bytes_cross_threads() {
        let c = ctx();
        let s = unsafe {
            ser(&c, "(() => { const o = { n: 1.25, s: 'x\\u{1F600}', big: 2n ** 70n, d: new Date(5), u: new Uint16Array([1, 65535]), \
                       e: new TypeError('t'), b: new Blob(['blob']) }; o.me = o; return o; })()", false).unwrap()
        };
        drop(c);
        let out = std::thread::spawn(move || {
            let c2 = ctx();
            unsafe {
                let v = deserialize(c2.raw(), s).unwrap();
                define(c2.raw(), glue::rjs_global(c2.raw()), "got", v, 0);
            }
            crate::webapi::testing::s(&c2, "[got.me === got, got.n, got.s === 'x\\u{1F600}', got.big === 2n ** 70n, got.d.getTime(), \
                 got.u[1], got.e instanceof TypeError, got.b instanceof Blob, got.b.size].join()")
        })
        .join()
        .unwrap();
        assert_eq!(out, "true,1.25,true,true,5,65535,true,true,4");
    }

    #[test]
    fn deep_graphs_throw_range_error_not_overflow() {
        let c = ctx();
        ok(&c, "(() => { let o = {}; for (let i = 0; i < 500; i++) o = { o }; let r = structuredClone(o); let n = 0; while (r.o) { r = r.o; n++; } return n === 500; })()");
        assert_eq!(
            s(&c, "(() => { let a = []; for (let i = 0; i < 100000; i++) a = [a]; try { structuredClone(a); return 'cloned'; } catch (e) { return e.constructor.name; } })()"),
            "RangeError"
        );
        // And the context is fine afterwards.
        ok(&c, "structuredClone([1])[0] === 1");
    }
}
