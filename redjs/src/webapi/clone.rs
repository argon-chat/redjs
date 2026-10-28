//! Native `structuredClone` (HTML spec subset — the shapes the engine
//! actually clones: plain objects, arrays, primitives, typed arrays /
//! ArrayBuffers, Date, RegExp, Map, Set, Blob, ImageBitmap; cycle-safe).
//! Functions/symbols throw a DataCloneError-shaped TypeError per spec.

use core::ffi::c_void;
use std::collections::HashMap;

use super::util::*;
use crate::glue;

unsafe extern "sysv64" fn structured_clone(g: *mut c_void, cf: *mut c_void) -> u64 {
    let v = cf_arg(cf, 0);
    let mut seen: HashMap<u64, u64> = HashMap::new();
    match clone_value(g, v, &mut seen, 0) {
        Ok(c) => c,
        Err(msg) => super::domexception::throw(g, "DataCloneError", &msg),
    }
}

unsafe fn ctor_is(g: *mut c_void, v: u64, global_name: &str) -> bool {
    let ctor = get(g, v, "constructor");
    is_cell(ctor) && ctor == get(g, glue::rjs_global(g), global_name)
}

/// Structured clone for other native modules (BroadcastChannel.postMessage).
pub unsafe fn clone(g: *mut c_void, v: u64) -> Result<u64, String> {
    clone_value(g, v, &mut HashMap::new(), 0)
}

unsafe fn clone_value(g: *mut c_void, v: u64, seen: &mut HashMap<u64, u64>, depth: u32) -> Result<u64, String> {
    if depth > 256 {
        return Err("structuredClone: object graph too deep".into());
    }
    if !is_cell(v) {
        return Ok(v); // numbers, bools, undefined, null
    }
    if let Some(c) = seen.get(&v) {
        return Ok(*c);
    }
    match glue::rjs_kind(g, v) {
        5 => Ok(v),             // strings are immutable — share
        7 => Ok(v),             // BigInt — immutable
        6 => Err("structuredClone: a Symbol could not be cloned (DataCloneError)".into()),
        9 => Err("structuredClone: a function could not be cloned (DataCloneError)".into()),
        11 => {
            // Typed array / DataView: copy bytes into a fresh view of the same kind.
            let mut ta = crate::ffi::RjsTaInfo::default();
            glue::rjs_typedarray_info(g, v, &mut ta);
            if ta.kind == 12 {
                return Err("structuredClone: DataView cloning is not supported in the player".into());
            }
            let mut out = UNDEF;
            if crate::ffi::redjs_typedarray_create(g, ta.kind, ta.length as usize, &mut out) != 0 {
                return Err("structuredClone: typed array allocation failed".into());
            }
            let mut dst = crate::ffi::RjsTaInfo::default();
            glue::rjs_typedarray_info(g, out, &mut dst);
            if ta.detached == 0 && !ta.data.is_null() && !dst.data.is_null() {
                core::ptr::copy_nonoverlapping(ta.data as *const u8, dst.data as *mut u8, ta.byte_length as usize);
            }
            seen.insert(v, out);
            Ok(out)
        }
        12 => {
            // ArrayBuffer → fresh buffer with the same bytes.
            let mut ab = crate::ffi::RjsAbInfo::default();
            glue::rjs_arraybuffer_info(g, v, &mut ab);
            let bytes = if ab.detached == 0 && !ab.data.is_null() {
                core::slice::from_raw_parts(ab.data as *const u8, ab.byte_length as usize).to_vec()
            } else {
                Vec::new()
            };
            let u8v = make_u8(g, &bytes);
            let buf = get(g, u8v, "buffer");
            seen.insert(v, buf);
            Ok(buf)
        }
        10 => {
            // Array.
            let mut out = UNDEF;
            glue::rjs_array_create(g, 0, &mut out);
            seen.insert(v, out);
            let n = array_len(g, v);
            for i in 0..n {
                let item = clone_value(g, get_i(g, v, i), seen, depth + 1)?;
                set_i(g, out, i, item);
            }
            Ok(out)
        }
        13 => Err("structuredClone: a Promise could not be cloned (DataCloneError)".into()),
        _ => clone_object(g, v, seen, depth),
    }
}

unsafe fn clone_object(g: *mut c_void, v: u64, seen: &mut HashMap<u64, u64>, depth: u32) -> Result<u64, String> {
    let st = state(g);

    // Serializable platform objects.
    if let Some(b) = host_data::<super::blob::BlobData>(g, st.blob_class, v) {
        let bytes = b.slice_bytes().to_vec();
        let mime = b.mime.clone();
        let c = super::blob::create(g, UNDEF, bytes, mime);
        seen.insert(v, c);
        return Ok(c);
    }
    if let Some(b) = host_data::<super::image::BitmapData>(g, st.bitmap_class, v) {
        let data = super::image::BitmapData { width: b.width, height: b.height, rgba: b.rgba.clone() };
        let c = super::image::create_bitmap(g, data);
        seen.insert(v, c);
        return Ok(c);
    }

    // Date / RegExp / Map / Set via their constructors.
    if ctor_is(g, v, "Date") {
        let get_time = get(g, v, "getTime");
        let t = call(g, get_time, v, &[]).map_err(|_| "structuredClone: Date".to_string())?;
        let date_ctor = get(g, glue::rjs_global(g), "Date");
        let mut out = UNDEF;
        glue::rjs_construct(g, date_ctor, [t].as_ptr(), 1, &mut out);
        seen.insert(v, out);
        return Ok(out);
    }
    if ctor_is(g, v, "RegExp") {
        let source = get(g, v, "source");
        let flags = get(g, v, "flags");
        let ctor = get(g, glue::rjs_global(g), "RegExp");
        let mut out = UNDEF;
        glue::rjs_construct(g, ctor, [source, flags].as_ptr(), 2, &mut out);
        seen.insert(v, out);
        return Ok(out);
    }
    if ctor_is(g, v, "Map") {
        let ctor = get(g, glue::rjs_global(g), "Map");
        let mut out = UNDEF;
        glue::rjs_construct(g, ctor, core::ptr::null(), 0, &mut out);
        seen.insert(v, out);
        // Entries via Array.from(v) — [ [k, v], … ].
        let entries = array_from(g, v)?;
        let set_fn = get(g, out, "set");
        let n = array_len(g, entries);
        for i in 0..n {
            let pair = get_i(g, entries, i);
            let k = clone_value(g, get_i(g, pair, 0), seen, depth + 1)?;
            let val = clone_value(g, get_i(g, pair, 1), seen, depth + 1)?;
            let _ = call(g, set_fn, out, &[k, val]);
        }
        return Ok(out);
    }
    if ctor_is(g, v, "Set") {
        let ctor = get(g, glue::rjs_global(g), "Set");
        let mut out = UNDEF;
        glue::rjs_construct(g, ctor, core::ptr::null(), 0, &mut out);
        seen.insert(v, out);
        let entries = array_from(g, v)?;
        let add_fn = get(g, out, "add");
        let n = array_len(g, entries);
        for i in 0..n {
            let item = clone_value(g, get_i(g, entries, i), seen, depth + 1)?;
            let _ = call(g, add_fn, out, &[item]);
        }
        return Ok(out);
    }

    // Plain object: own enumerable string keys, fresh Object.prototype (spec).
    let out = new_object(g, UNDEF);
    seen.insert(v, out);
    let mut keys = UNDEF;
    if glue::rjs_own_keys(g, v, &mut keys) != 0 {
        return Err("structuredClone: failed to enumerate keys".into());
    }
    let n = array_len(g, keys);
    for i in 0..n {
        let key = get_i(g, keys, i);
        let key_s = read_string(g, key).unwrap_or_default();
        let mut val = UNDEF;
        let a = glue::rjs_atom8(g, key_s.as_ptr(), key_s.len());
        glue::rjs_get(g, v, a, &mut val);
        let cloned = match clone_value(g, val, seen, depth + 1) {
            Ok(c) => c,
            Err(e) => {
                glue::rjs_atom_free(g, a);
                return Err(e);
            }
        };
        glue::rjs_define(g, out, a, cloned, 0);
        glue::rjs_atom_free(g, a);
    }
    Ok(out)
}

unsafe fn array_from(g: *mut c_void, iterable: u64) -> Result<u64, String> {
    let array_ctor = get(g, glue::rjs_global(g), "Array");
    let from = get(g, array_ctor, "from");
    call(g, from, array_ctor, &[iterable]).map_err(|_| "structuredClone: iteration failed".to_string())
}

pub unsafe fn install(g: *mut c_void) {
    let n16: Vec<u16> = "structuredClone".encode_utf16().collect();
    let mut f = UNDEF;
    glue::rjs_function_create(g, n16.as_ptr(), n16.len(), 1, structured_clone as *const c_void, &mut f);
    define(g, glue::rjs_global(g), "structuredClone", f, 2);
}
