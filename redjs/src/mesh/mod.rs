//! `RedMesh` — the native meshopt decoder for `.rmesh` payloads
//! (`.refactor/RMESH-FORMAT-DESIGN.md` §5). A plain stateless namespace object,
//! not a class: its shape is npm `MeshoptDecoder` 1:1, so the TS codec
//! (`render2d/src/mesh/rmesh.ts`) feeds either this or the wasm decoder through
//! the single `RMeshDecoder` interface.
//!
//! Decode writes straight into the JS-owned target (`typedarray_info().data`) —
//! no intermediate Vec. Argument violations throw TypeError and a nonzero
//! meshopt status throws Error: a corrupt mesh must never reach the GPU as
//! silent garbage.

pub mod codec;

use core::ffi::c_void;

use codec::Filter;

use crate::glue;
use crate::webapi::util::{
    bufferish, cf_arg, dec_f64, define, make_fn, read_string, throw_error, throw_type_error, NULL,
    UNDEF,
};

/// meshopt's vertex-stride ceiling (mirrored by `VSTR.stride` in the format).
const MAX_STRIDE: usize = 256;
/// The vertex codec walks a stride in 4-byte groups and reads `ceil(stride/4)` of them; a stride
/// that is not a multiple of 4 makes it read past the source. meshopt only asserts this
/// (`vertexcodec.cpp`), and asserts are compiled out under NDEBUG — every build the player ships.
const STRIDE_ALIGN: usize = 4;

/// Non-negative integral argument as a byte/element count; rejects NaN, inf,
/// fractions and anything a u32 count field could not hold.
fn read_len(v: u64) -> Option<usize> {
    let d = dec_f64(v)?;
    if !d.is_finite() || d < 0.0 || d.fract() != 0.0 || d > u32::MAX as f64 {
        return None;
    }
    Some(d as usize)
}

/// Writable bytes of a typed array or ArrayBuffer (pointer into JS memory —
/// nothing may re-enter the VM while it is held). Detached → null + zero length;
/// callers reject the null explicitly, since a `need` of 0 would otherwise pass
/// the size check and hand a NULL destination to the meshopt C API.
unsafe fn writable(g: *mut c_void, v: u64) -> Option<(*mut u8, usize)> {
    let mut ta = crate::ffi::RjsTaInfo::default();
    if glue::rjs_typedarray_info(g, v, &mut ta) == 0 {
        if ta.detached != 0 || ta.data.is_null() {
            return Some((core::ptr::null_mut(), 0));
        }
        return Some((ta.data as *mut u8, ta.byte_length as usize));
    }
    let mut ab = crate::ffi::RjsAbInfo::default();
    if glue::rjs_arraybuffer_info(g, v, &mut ab) == 0 {
        if ab.detached != 0 || ab.data.is_null() {
            return Some((core::ptr::null_mut(), 0));
        }
        return Some((ab.data as *mut u8, ab.byte_length as usize));
    }
    None
}

/// `decodeVertexBuffer(target, count, stride, source, filter?)`
unsafe extern "sysv64" fn decode_vertex_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    let (Some(count), Some(stride)) = (read_len(cf_arg(cf, 1)), read_len(cf_arg(cf, 2))) else {
        return throw_type_error(
            g,
            "RedMesh.decodeVertexBuffer: count and stride must be non-negative integers",
        );
    };
    if stride == 0 || stride > MAX_STRIDE {
        return throw_type_error(
            g,
            &format!("RedMesh.decodeVertexBuffer: stride must be 1..={MAX_STRIDE}, got {stride}"),
        );
    }
    if stride % STRIDE_ALIGN != 0 {
        return throw_type_error(
            g,
            &format!(
                "RedMesh.decodeVertexBuffer: stride must be a multiple of {STRIDE_ALIGN}, got {stride}"
            ),
        );
    }

    // ToString can run JS, so the filter is resolved before any buffer pointer
    // is taken (a GC could move nothing here, but a detach could invalidate it).
    let filter_v = cf_arg(cf, 4);
    let filter = if filter_v == UNDEF || filter_v == NULL {
        Filter::None
    } else {
        match read_string(g, filter_v).as_deref().and_then(Filter::parse) {
            Some(f) => f,
            None => {
                return throw_type_error(
                    g,
                    "RedMesh.decodeVertexBuffer: filter must be NONE, OCTAHEDRAL, QUATERNION or EXPONENTIAL",
                )
            }
        }
    };
    if !filter.stride_ok(stride) {
        return throw_type_error(
            g,
            &format!(
                "RedMesh.decodeVertexBuffer: this filter requires stride {}, got {stride}",
                filter.stride_requirement()
            ),
        );
    }

    let Some((dst, dst_len)) = writable(g, cf_arg(cf, 0)) else {
        return throw_type_error(
            g,
            "RedMesh.decodeVertexBuffer: target must be a typed array or ArrayBuffer",
        );
    };
    let (src, src_len) = match bufferish(g, cf_arg(cf, 3)) {
        Some(s) => (s.as_ptr(), s.len()),
        None => {
            return throw_type_error(
                g,
                "RedMesh.decodeVertexBuffer: source must be a typed array or ArrayBuffer",
            )
        }
    };

    let need = count * stride;
    if dst.is_null() || dst_len < need {
        return throw_type_error(
            g,
            &format!("RedMesh.decodeVertexBuffer: target holds {dst_len} bytes, need {need}"),
        );
    }

    let rc = codec::decode_vertex(dst, count, stride, src, src_len, filter);
    if rc != 0 {
        return throw_error(g, 0, &format!("RedMesh.decodeVertexBuffer: meshopt decode failed ({rc})"));
    }
    UNDEF
}

/// `decodeIndexBuffer(target, count, indexSize, source)`
unsafe extern "sysv64" fn decode_index_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    let (Some(count), Some(index_size)) = (read_len(cf_arg(cf, 1)), read_len(cf_arg(cf, 2))) else {
        return throw_type_error(
            g,
            "RedMesh.decodeIndexBuffer: count and indexSize must be non-negative integers",
        );
    };
    if index_size != 2 && index_size != 4 {
        return throw_type_error(
            g,
            &format!("RedMesh.decodeIndexBuffer: indexSize must be 2 or 4, got {index_size}"),
        );
    }
    // The index codec decodes whole triangles: it loops `i += 3` and writes three elements per
    // iteration regardless of `count`, so a count that is not a multiple of 3 writes up to two
    // elements past the target. Like the stride rules, meshopt only asserts it.
    if count % 3 != 0 {
        return throw_type_error(
            g,
            &format!("RedMesh.decodeIndexBuffer: count must be a multiple of 3, got {count}"),
        );
    }

    let Some((dst, dst_len)) = writable(g, cf_arg(cf, 0)) else {
        return throw_type_error(
            g,
            "RedMesh.decodeIndexBuffer: target must be a typed array or ArrayBuffer",
        );
    };
    let (src, src_len) = match bufferish(g, cf_arg(cf, 3)) {
        Some(s) => (s.as_ptr(), s.len()),
        None => {
            return throw_type_error(
                g,
                "RedMesh.decodeIndexBuffer: source must be a typed array or ArrayBuffer",
            )
        }
    };

    let need = count * index_size;
    if dst.is_null() || dst_len < need {
        return throw_type_error(
            g,
            &format!("RedMesh.decodeIndexBuffer: target holds {dst_len} bytes, need {need}"),
        );
    }

    let rc = codec::decode_index(dst, count, index_size, src, src_len);
    if rc != 0 {
        return throw_error(g, 0, &format!("RedMesh.decodeIndexBuffer: meshopt decode failed ({rc})"));
    }
    UNDEF
}

/// Installs the `RedMesh` namespace on the global object. Called from
/// `webapi::install` (idempotence rides its `has_state` gate).
pub unsafe fn install(g: *mut c_void) {
    let obj = crate::webapi::util::new_object(g, UNDEF);
    define(g, obj, "decodeVertexBuffer", make_fn(g, "decodeVertexBuffer", 5, decode_vertex_buffer), 2);
    define(g, obj, "decodeIndexBuffer", make_fn(g, "decodeIndexBuffer", 4, decode_index_buffer), 2);
    define(g, glue::rjs_global(g), "RedMesh", obj, 2);
    crate::ffi::redjs_protect(g, obj);
}
