//! Native `Blob` (W3C File API subset): immutable byte container with
//! spec-shaped `slice` (shares the underlying bytes via Arc — zero copy),
//! promise-returning `arrayBuffer`/`bytes`/`text`, `size`/`type` getters,
//! constructor from BlobParts (strings / ArrayBuffer(views) / Blobs).

use core::ffi::c_void;
use std::sync::Arc;

use super::util::*;
use crate::glue;

pub struct BlobData {
    pub bytes: Arc<Vec<u8>>,
    pub offset: usize,
    pub len: usize,
    pub mime: String,
}

impl BlobData {
    pub fn slice_bytes(&self) -> &[u8] {
        &self.bytes[self.offset..self.offset + self.len]
    }
}

unsafe fn this_blob<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut BlobData> {
    let st = state(g);
    host_data::<BlobData>(g, st.blob_class, cf_this(cf))
}

/// Creates a Blob instance from raw bytes (fetch/Response internals too).
pub unsafe fn create(g: *mut c_void, proto: u64, bytes: Vec<u8>, mime: String) -> u64 {
    let st = state(g);
    let proto = if is_cell(proto) { proto } else { st.blob.proto };
    let len = bytes.len();
    host_create(g, st.blob_class, proto, BlobData { bytes: Arc::new(bytes), offset: 0, len, mime })
}

unsafe extern "sysv64" fn construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let st = state(g);
    let mut bytes: Vec<u8> = Vec::new();

    let parts = cf_arg(cf, 0);
    if is_cell(parts) {
        let n = array_len(g, parts);
        for i in 0..n {
            let p = get_i(g, parts, i);
            if let Some(b) = host_data::<BlobData>(g, st.blob_class, p) {
                bytes.extend_from_slice(b.slice_bytes());
            } else if let Some(b) = bufferish(g, p) {
                bytes.extend_from_slice(b);
            } else if let Some(s) = read_string(g, p) {
                bytes.extend_from_slice(s.as_bytes());
            }
        }
    } else if parts != UNDEF {
        return throw_type_error(g, "Blob constructor: first argument must be a sequence");
    }

    // options.type: spec lowercases; invalid chars → "".
    let mut mime = String::new();
    let options = cf_arg(cf, 1);
    if is_cell(options) {
        let t = get(g, options, "type");
        if t != UNDEF {
            let s = read_string(g, t).unwrap_or_default();
            if s.chars().all(|c| ('\u{20}'..='\u{7e}').contains(&c)) {
                mime = s.to_ascii_lowercase();
            }
        }
    }

    let proto = {
        let nt = cf_new_target(cf);
        let mut p = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
        if !is_cell(p) {
            p = st.blob.proto;
        }
        p
    };
    let len = bytes.len();
    host_create(g, st.blob_class, proto, BlobData { bytes: Arc::new(bytes), offset: 0, len, mime })
}

unsafe extern "sysv64" fn get_size(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_blob(g, cf) {
        Some(b) => enc_f64(b.len as f64),
        None => throw_type_error(g, "Blob.prototype.size called on a non-Blob"),
    }
}

unsafe extern "sysv64" fn get_type(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_blob(g, cf) {
        Some(b) => {
            let mime = b.mime.clone();
            js_string(g, &mime)
        }
        None => throw_type_error(g, "Blob.prototype.type called on a non-Blob"),
    }
}

unsafe extern "sysv64" fn array_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(b) = this_blob(g, cf) else {
        return rejected_promise(g, 1, "Blob.prototype.arrayBuffer called on a non-Blob");
    };
    let data = b.slice_bytes().to_vec();
    // Fresh ArrayBuffer: make a u8 view then hand out its buffer.
    let u8v = make_u8(g, &data);
    let buffer = get(g, u8v, "buffer");
    resolved_promise(g, buffer)
}

unsafe extern "sysv64" fn bytes_fn(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(b) = this_blob(g, cf) else {
        return rejected_promise(g, 1, "Blob.prototype.bytes called on a non-Blob");
    };
    let data = b.slice_bytes().to_vec();
    let u8v = make_u8(g, &data);
    resolved_promise(g, u8v)
}

unsafe extern "sysv64" fn text(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(b) = this_blob(g, cf) else {
        return rejected_promise(g, 1, "Blob.prototype.text called on a non-Blob");
    };
    let s = String::from_utf8_lossy(b.slice_bytes()).into_owned();
    resolved_promise(g, js_string(g, &s))
}

unsafe extern "sysv64" fn slice(g: *mut c_void, cf: *mut c_void) -> u64 {
    let st = state(g);
    let Some(b) = this_blob(g, cf) else {
        return throw_type_error(g, "Blob.prototype.slice called on a non-Blob");
    };
    let size = b.len as i64;
    let rel = |v: u64, def: i64| -> i64 {
        match dec_f64(v) {
            Some(d) => {
                let d = d as i64;
                if d < 0 { (size + d).max(0) } else { d.min(size) }
            }
            None => def,
        }
    };
    let start = rel(cf_arg(cf, 0), 0);
    let end = rel(cf_arg(cf, 1), size);
    let span = (end - start).max(0) as usize;

    let mime = match read_string(g, cf_arg(cf, 2)) {
        Some(s) if cf_arg(cf, 2) != UNDEF => s.to_ascii_lowercase(),
        _ => String::new(),
    };

    let data = BlobData {
        bytes: Arc::clone(&b.bytes),
        offset: b.offset + start as usize,
        len: span,
        mime,
    };
    host_create(g, st.blob_class, st.blob.proto, data)
}

unsafe extern "sysv64" fn stream(g: *mut c_void, _cf: *mut c_void) -> u64 {
    throw_type_error(g, "Blob.prototype.stream is not supported in the player")
}

pub unsafe fn install(g: *mut c_void) -> (crate::capi::JSClassRef, Class) {
    let cls = make_host_class::<BlobData>("Blob");
    let class = define_class(g, &ClassSpec {
        name: "Blob",
        arity: 0,
        construct: Some(construct),
        parent_proto: 0,
        methods: &[
            ("arrayBuffer", 0, array_buffer),
            ("bytes", 0, bytes_fn),
            ("text", 0, text),
            ("slice", 2, slice),
            ("stream", 0, stream),
        ],
        getters: &[("size", get_size), ("type", get_type)],
        accessors: &[],
    });
    (cls, class)
}

/// Raw bytes of a Blob value (fetch/createImageBitmap internals).
pub unsafe fn bytes_of(g: *mut c_void, v: u64) -> Option<Vec<u8>> {
    let st = state(g);
    host_data::<BlobData>(g, st.blob_class, v).map(|b| b.slice_bytes().to_vec())
}
