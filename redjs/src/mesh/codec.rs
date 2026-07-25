//! meshopt decode over raw pointers into JS-owned memory. Target and source
//! both alias JS buffers, so this layer takes pointers (never Rust references:
//! a target that overlaps its source would alias `&mut`/`&`) and neither
//! allocates nor re-enters the VM.

use core::ffi::c_void;

/// Post-decode vertex filter (`VSTR.filter`, RMESH-FORMAT-DESIGN §1.2). The JS
/// names are npm `MeshoptDecoder`'s.
#[derive(Clone, Copy, PartialEq)]
pub enum Filter {
    None,
    Octahedral,
    Quaternion,
    Exponential,
}

impl Filter {
    pub fn parse(name: &str) -> Option<Filter> {
        Some(match name {
            "NONE" => Filter::None,
            "OCTAHEDRAL" => Filter::Octahedral,
            "QUATERNION" => Filter::Quaternion,
            "EXPONENTIAL" => Filter::Exponential,
            _ => return None,
        })
    }

    /// meshopt's per-filter stride requirement. Violating it is UB in the
    /// release build (the C++ side only asserts), so callers must gate on this.
    pub fn stride_ok(self, stride: usize) -> bool {
        match self {
            Filter::None => true,
            Filter::Octahedral => stride == 4 || stride == 8,
            Filter::Quaternion => stride == 8,
            Filter::Exponential => stride % 4 == 0,
        }
    }

    pub fn stride_requirement(self) -> &'static str {
        match self {
            Filter::None => "any",
            Filter::Octahedral => "4 or 8",
            Filter::Quaternion => "8",
            Filter::Exponential => "a multiple of 4",
        }
    }
}

/// Decodes `count` × `stride` bytes into `dst`, then applies `filter` in place.
/// Returns meshopt's status (0 = ok). `dst` must hold `count * stride` bytes.
pub unsafe fn decode_vertex(
    dst: *mut u8,
    count: usize,
    stride: usize,
    src: *const u8,
    src_len: usize,
    filter: Filter,
) -> i32 {
    let rc = meshopt::ffi::meshopt_decodeVertexBuffer(dst as *mut c_void, count, stride, src, src_len);
    if rc != 0 {
        return rc;
    }
    match filter {
        Filter::None => {}
        Filter::Octahedral => meshopt::ffi::meshopt_decodeFilterOct(dst as *mut c_void, count, stride),
        Filter::Quaternion => meshopt::ffi::meshopt_decodeFilterQuat(dst as *mut c_void, count, stride),
        Filter::Exponential => meshopt::ffi::meshopt_decodeFilterExp(dst as *mut c_void, count, stride),
    }
    0
}

/// Decodes `count` indices of `index_size` bytes into `dst` (which must hold
/// `count * index_size` bytes). Returns meshopt's status (0 = ok).
pub unsafe fn decode_index(
    dst: *mut u8,
    count: usize,
    index_size: usize,
    src: *const u8,
    src_len: usize,
) -> i32 {
    meshopt::ffi::meshopt_decodeIndexBuffer(dst as *mut c_void, count, index_size, src, src_len)
}
