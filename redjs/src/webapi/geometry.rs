//! Native `DOMPointReadOnly` / `DOMPoint` / `DOMMatrixReadOnly` / `DOMMatrix`
//! (Geometry Interfaces Module Level 1).
//!
//! All the arithmetic is Rust ([`Mat`], unit-tested without a JS context); the
//! JS half is only the prototype chain, the accessors and `instanceof`.
//! `DOMMatrix` extends `DOMMatrixReadOnly` for real (spec §DOMMatrix), so the
//! read-only surface is inherited rather than duplicated and
//! `m instanceof DOMMatrixReadOnly` is true for both.
//!
//! Storage is the spec's own order — `m11, m12, m13, m14, m21, …, m44`, which
//! is column-major — so `toFloat64Array` is a straight copy and the transform
//! is `x' = m11·x + m21·y + m31·z + m41·w` (spec §"transform a point").
//!
//! ## Deliberate narrowing
//!
//! `new DOMMatrix(string)` and `setMatrixValue(string)` parse a **CSS transform
//! list** (spec §"create a DOMMatrix from a DOMString"). That is a CSS value
//! grammar, and this repo has exactly one CSS front end — the `redcss` crate —
//! which exposes no transform-list entry point. Rather than grow a second,
//! divergent CSS parser here, both REFUSE with a named error. Nothing silently
//! returns identity: a caller that passes a string finds out immediately.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::util::*;
use crate::capi;
use crate::glue;

// ---------------------------------------------------------------------------
// The math (no JS)
// ---------------------------------------------------------------------------

/// A 4×4 transform in the spec's `mIJ` order: `a[(I-1)*4 + (J-1)] == mIJ`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mat {
    pub a: [f64; 16],
    /// Spec `is2D` flag. Sticky-false: once an operation makes the matrix 3D it
    /// never returns to 2D, even if the 3D components later cancel
    /// (spec §"DOMMatrixReadOnly.is2D" — the flag tracks provenance, not values).
    pub is2d: bool,
}

pub const IDENTITY: Mat = Mat {
    a: [1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1.],
    is2d: true,
};

impl Mat {
    #[inline]
    pub fn get(&self, i: usize, j: usize) -> f64 {
        self.a[(i - 1) * 4 + (j - 1)]
    }
    #[inline]
    pub fn set(&mut self, i: usize, j: usize, v: f64) {
        self.a[(i - 1) * 4 + (j - 1)] = v;
    }

    /// 2D interop: `[a, b, c, d, e, f]` == `[m11, m12, m21, m22, m41, m42]`.
    pub fn from_2d(v: [f64; 6]) -> Mat {
        let mut m = IDENTITY;
        m.a[0] = v[0];
        m.a[1] = v[1];
        m.a[4] = v[2];
        m.a[5] = v[3];
        m.a[12] = v[4];
        m.a[13] = v[5];
        m
    }

    /// Spec §"create a DOMMatrix from a sequence": the 16-number form sets
    /// `is2D` to false regardless of the values.
    pub fn from_16(v: [f64; 16]) -> Mat {
        Mat { a: v, is2d: false }
    }

    /// `self · other` — post-multiplication, the direction every DOMMatrix
    /// method uses (spec §"multiply a matrix"): the result applies `other`
    /// first and `self` second.
    pub fn multiply(&self, other: &Mat) -> Mat {
        let mut r = [0.0f64; 16];
        for i in 0..4 {
            for j in 0..4 {
                let mut s = 0.0;
                for k in 0..4 {
                    s += self.a[k * 4 + j] * other.a[i * 4 + k];
                }
                r[i * 4 + j] = s;
            }
        }
        Mat { a: r, is2d: self.is2d && other.is2d }
    }

    pub fn translate(tx: f64, ty: f64, tz: f64) -> Mat {
        let mut m = IDENTITY;
        m.a[12] = tx;
        m.a[13] = ty;
        m.a[14] = tz;
        m.is2d = tz == 0.0;
        m
    }

    pub fn scale(sx: f64, sy: f64, sz: f64) -> Mat {
        let mut m = IDENTITY;
        m.a[0] = sx;
        m.a[5] = sy;
        m.a[10] = sz;
        m.is2d = sz == 1.0;
        m
    }

    pub fn rotate_z(deg: f64) -> Mat {
        let (s, c) = deg.to_radians().sin_cos();
        let mut m = IDENTITY;
        m.a[0] = c;
        m.a[1] = s;
        m.a[4] = -s;
        m.a[5] = c;
        m
    }

    pub fn rotate_y(deg: f64) -> Mat {
        let (s, c) = deg.to_radians().sin_cos();
        let mut m = IDENTITY;
        m.a[0] = c;
        m.a[2] = -s;
        m.a[8] = s;
        m.a[10] = c;
        m.is2d = false;
        m
    }

    pub fn rotate_x(deg: f64) -> Mat {
        let (s, c) = deg.to_radians().sin_cos();
        let mut m = IDENTITY;
        m.a[5] = c;
        m.a[6] = s;
        m.a[9] = -s;
        m.a[10] = c;
        m.is2d = false;
        m
    }

    /// Rodrigues rotation about an arbitrary axis (spec §"rotateAxisAngle").
    /// A zero-length axis yields identity, the spec's degenerate case.
    pub fn rotate_axis_angle(x: f64, y: f64, z: f64, deg: f64) -> Mat {
        let len = (x * x + y * y + z * z).sqrt();
        if len == 0.0 {
            return IDENTITY;
        }
        let (x, y, z) = (x / len, y / len, z / len);
        let (s, c) = deg.to_radians().sin_cos();
        let t = 1.0 - c;
        let mut m = IDENTITY;
        m.set(1, 1, t * x * x + c);
        m.set(1, 2, t * x * y + s * z);
        m.set(1, 3, t * x * z - s * y);
        m.set(2, 1, t * x * y - s * z);
        m.set(2, 2, t * y * y + c);
        m.set(2, 3, t * y * z + s * x);
        m.set(3, 1, t * x * z + s * y);
        m.set(3, 2, t * y * z - s * x);
        m.set(3, 3, t * z * z + c);
        // Rotation about z alone stays 2D; any other axis does not.
        m.is2d = x == 0.0 && y == 0.0;
        m
    }

    pub fn skew_x(deg: f64) -> Mat {
        let mut m = IDENTITY;
        m.a[4] = deg.to_radians().tan();
        m
    }

    pub fn skew_y(deg: f64) -> Mat {
        let mut m = IDENTITY;
        m.a[1] = deg.to_radians().tan();
        m
    }

    /// Spec §"transform a point": `p' = M · p`, column-vector convention.
    pub fn transform_point(&self, p: [f64; 4]) -> [f64; 4] {
        let mut out = [0.0f64; 4];
        for j in 0..4 {
            out[j] =
                self.a[j] * p[0] + self.a[4 + j] * p[1] + self.a[8 + j] * p[2] + self.a[12 + j] * p[3];
        }
        out
    }

    pub fn is_identity(&self) -> bool {
        self.a == IDENTITY.a
    }

    /// Full 4×4 inverse. `None` when the matrix is singular — the caller then
    /// sets every component to NaN and `is2D` to false (spec §"invert self"),
    /// which is what makes a non-invertible matrix impossible to mistake for a
    /// working one.
    pub fn inverse(&self) -> Option<Mat> {
        let m = &self.a;
        let mut inv = [0.0f64; 16];
        inv[0] = m[5] * m[10] * m[15] - m[5] * m[11] * m[14] - m[9] * m[6] * m[15]
            + m[9] * m[7] * m[14] + m[13] * m[6] * m[11] - m[13] * m[7] * m[10];
        inv[4] = -m[4] * m[10] * m[15] + m[4] * m[11] * m[14] + m[8] * m[6] * m[15]
            - m[8] * m[7] * m[14] - m[12] * m[6] * m[11] + m[12] * m[7] * m[10];
        inv[8] = m[4] * m[9] * m[15] - m[4] * m[11] * m[13] - m[8] * m[5] * m[15]
            + m[8] * m[7] * m[13] + m[12] * m[5] * m[11] - m[12] * m[7] * m[9];
        inv[12] = -m[4] * m[9] * m[14] + m[4] * m[10] * m[13] + m[8] * m[5] * m[14]
            - m[8] * m[6] * m[13] - m[12] * m[5] * m[10] + m[12] * m[6] * m[9];
        inv[1] = -m[1] * m[10] * m[15] + m[1] * m[11] * m[14] + m[9] * m[2] * m[15]
            - m[9] * m[3] * m[14] - m[13] * m[2] * m[11] + m[13] * m[3] * m[10];
        inv[5] = m[0] * m[10] * m[15] - m[0] * m[11] * m[14] - m[8] * m[2] * m[15]
            + m[8] * m[3] * m[14] + m[12] * m[2] * m[11] - m[12] * m[3] * m[10];
        inv[9] = -m[0] * m[9] * m[15] + m[0] * m[11] * m[13] + m[8] * m[1] * m[15]
            - m[8] * m[3] * m[13] - m[12] * m[1] * m[11] + m[12] * m[3] * m[9];
        inv[13] = m[0] * m[9] * m[14] - m[0] * m[10] * m[13] - m[8] * m[1] * m[14]
            + m[8] * m[2] * m[13] + m[12] * m[1] * m[10] - m[12] * m[2] * m[9];
        inv[2] = m[1] * m[6] * m[15] - m[1] * m[7] * m[14] - m[5] * m[2] * m[15]
            + m[5] * m[3] * m[14] + m[13] * m[2] * m[7] - m[13] * m[3] * m[6];
        inv[6] = -m[0] * m[6] * m[15] + m[0] * m[7] * m[14] + m[4] * m[2] * m[15]
            - m[4] * m[3] * m[14] - m[12] * m[2] * m[7] + m[12] * m[3] * m[6];
        inv[10] = m[0] * m[5] * m[15] - m[0] * m[7] * m[13] - m[4] * m[1] * m[15]
            + m[4] * m[3] * m[13] + m[12] * m[1] * m[7] - m[12] * m[3] * m[5];
        inv[14] = -m[0] * m[5] * m[14] + m[0] * m[6] * m[13] + m[4] * m[1] * m[14]
            - m[4] * m[2] * m[13] - m[12] * m[1] * m[6] + m[12] * m[2] * m[5];
        inv[3] = -m[1] * m[6] * m[11] + m[1] * m[7] * m[10] + m[5] * m[2] * m[11]
            - m[5] * m[3] * m[10] - m[9] * m[2] * m[7] + m[9] * m[3] * m[6];
        inv[7] = m[0] * m[6] * m[11] - m[0] * m[7] * m[10] - m[4] * m[2] * m[11]
            + m[4] * m[3] * m[10] + m[8] * m[2] * m[7] - m[8] * m[3] * m[6];
        inv[11] = -m[0] * m[5] * m[11] + m[0] * m[7] * m[9] + m[4] * m[1] * m[11]
            - m[4] * m[3] * m[9] - m[8] * m[1] * m[7] + m[8] * m[3] * m[5];
        inv[15] = m[0] * m[5] * m[10] - m[0] * m[6] * m[9] - m[4] * m[1] * m[10]
            + m[4] * m[2] * m[9] + m[8] * m[1] * m[6] - m[8] * m[2] * m[5];

        let det = m[0] * inv[0] + m[1] * inv[4] + m[2] * inv[8] + m[3] * inv[12];
        if det == 0.0 || !det.is_finite() {
            return None;
        }
        for v in inv.iter_mut() {
            *v /= det;
        }
        Some(Mat { a: inv, is2d: self.is2d })
    }

    /// Spec §"stringification behavior": `matrix(a, b, c, d, e, f)` when 2D,
    /// `matrix3d(…16…)` otherwise. `None` when a component is non-finite —
    /// the caller's InvalidStateError case.
    pub fn to_css(&self) -> Option<String> {
        if self.a.iter().any(|v| !v.is_finite()) {
            return None;
        }
        let n = |v: f64| {
            let s = format!("{v}");
            if s == "-0" { "0".to_string() } else { s }
        };
        if self.is2d {
            Some(format!(
                "matrix({}, {}, {}, {}, {}, {})",
                n(self.a[0]), n(self.a[1]), n(self.a[4]), n(self.a[5]), n(self.a[12]), n(self.a[13])
            ))
        } else {
            let parts: Vec<String> = self.a.iter().map(|&v| n(v)).collect();
            Some(format!("matrix3d({})", parts.join(", ")))
        }
    }
}

pub struct MatrixData {
    pub m: Mat,
}

pub struct PointData {
    pub p: [f64; 4],
}

// ---------------------------------------------------------------------------
// Per-context classes
// ---------------------------------------------------------------------------

struct GeomClasses {
    matrix_class: capi::JSClassRef,
    ro: Class,
    rw: Class,
    point_class: capi::JSClassRef,
    point_rw: Class,
}

unsafe impl Send for GeomClasses {}
unsafe impl Sync for GeomClasses {}

fn registry() -> &'static Mutex<HashMap<usize, &'static GeomClasses>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static GeomClasses>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> &'static GeomClasses {
    registry().lock().unwrap().get(&(g as usize)).copied().expect("webapi::geometry not installed")
}

unsafe fn this_mat<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut MatrixData> {
    host_data::<MatrixData>(g, classes(g).matrix_class, cf_this(cf))
}

unsafe fn this_point<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut PointData> {
    host_data::<PointData>(g, classes(g).point_class, cf_this(cf))
}

/// Build a `DOMMatrix` (the mutable flavour) around `m`.
pub unsafe fn new_matrix(g: *mut c_void, m: Mat) -> u64 {
    let c = classes(g);
    host_create(g, c.matrix_class, c.rw.proto, MatrixData { m })
}

pub unsafe fn new_point(g: *mut c_void, p: [f64; 4]) -> u64 {
    let c = classes(g);
    host_create(g, c.point_class, c.point_rw.proto, PointData { p })
}

/// True when `v` is one of this module's matrix objects.
pub unsafe fn is_matrix(g: *mut c_void, v: u64) -> bool {
    host_data::<MatrixData>(g, classes(g).matrix_class, v).is_some()
}

/// Reads a DOMMatrix-ish argument: a real matrix object, or any object carrying
/// `m11…m44` / `a…f` (spec §"create a DOMMatrix from a dictionary").
pub unsafe fn mat_of(g: *mut c_void, v: u64) -> Option<Mat> {
    let c = classes(g);
    if let Some(d) = host_data::<MatrixData>(g, c.matrix_class, v) {
        return Some(d.m);
    }
    if !is_cell(v) {
        return None;
    }
    if get(g, v, "m11") != UNDEF {
        let mut m = IDENTITY;
        for i in 1..=4usize {
            for j in 1..=4usize {
                let got = get(g, v, &format!("m{i}{j}"));
                if got != UNDEF {
                    m.set(i, j, read_number(g, got));
                }
            }
        }
        m.is2d = truthy_flag(get(g, v, "is2D"));
        return Some(m);
    }
    if get(g, v, "a") != UNDEF {
        let f = |k: &str, d: f64| {
            let got = get(g, v, k);
            if got == UNDEF { d } else { read_number(g, got) }
        };
        return Some(Mat::from_2d([
            f("a", 1.), f("b", 0.), f("c", 0.), f("d", 1.), f("e", 0.), f("f", 0.),
        ]));
    }
    None
}

/// Prototype for an instance under construction: new.target's, else the class
/// default (spec OrdinaryCreateFromConstructor — subclassing works).
unsafe fn ordinary_proto(g: *mut c_void, new_target: u64, fallback: u64) -> u64 {
    let p = if is_cell(new_target) { get(g, new_target, "prototype") } else { UNDEF };
    if is_cell(p) { p } else { fallback }
}

// ---------------------------------------------------------------------------
// DOMPoint
// ---------------------------------------------------------------------------

macro_rules! point_accessors {
    ($get:ident, $set:ident, $idx:expr) => {
        unsafe extern "sysv64" fn $get(g: *mut c_void, cf: *mut c_void) -> u64 {
            match this_point(g, cf) {
                Some(p) => enc_f64(p.p[$idx]),
                None => throw_type_error(g, "DOMPoint getter called on a non-DOMPoint"),
            }
        }
        unsafe extern "sysv64" fn $set(g: *mut c_void, cf: *mut c_void) -> u64 {
            let v = arg_num(g, cf, 0, 0.0);
            match this_point(g, cf) {
                Some(p) => {
                    p.p[$idx] = v;
                    UNDEF
                }
                None => throw_type_error(g, "DOMPoint setter called on a non-DOMPoint"),
            }
        }
    };
}
point_accessors!(pt_get_x, pt_set_x, 0);
point_accessors!(pt_get_y, pt_set_y, 1);
point_accessors!(pt_get_z, pt_set_z, 2);
point_accessors!(pt_get_w, pt_set_w, 3);

unsafe extern "sysv64" fn point_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let p = [
        arg_num(g, cf, 0, 0.0),
        arg_num(g, cf, 1, 0.0),
        arg_num(g, cf, 2, 0.0),
        arg_num(g, cf, 3, 1.0),
    ];
    let proto = ordinary_proto(g, cf_new_target(cf), c.point_rw.proto);
    host_create(g, c.point_class, proto, PointData { p })
}

unsafe extern "sysv64" fn point_matrix_transform(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(pt) = this_point(g, cf) else {
        return throw_type_error(g, "DOMPoint.matrixTransform called on a non-DOMPoint");
    };
    let src = pt.p;
    match mat_of(g, cf_arg(cf, 0)) {
        Some(m) => new_point(g, m.transform_point(src)),
        None => throw_type_error(g, "DOMPoint.matrixTransform: argument is not a matrix"),
    }
}

unsafe extern "sysv64" fn point_to_json(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(pt) = this_point(g, cf) else {
        return throw_type_error(g, "DOMPoint.toJSON called on a non-DOMPoint");
    };
    let v = pt.p;
    let o = new_object(g, UNDEF);
    for (k, i) in [("x", 0usize), ("y", 1), ("z", 2), ("w", 3)] {
        define(g, o, k, enc_f64(v[i]), 0);
    }
    o
}

// ---------------------------------------------------------------------------
// DOMMatrix — component accessors
// ---------------------------------------------------------------------------

/// Writing a component outside the 2D subset makes the matrix 3D — but only
/// when the value differs from that component's identity value, so
/// `m.m33 = 1` on a 2D matrix leaves it 2D (spec §"DOMMatrix attributes").
const TWO_D_INDICES: [usize; 6] = [0, 1, 4, 5, 12, 13];

unsafe fn comp_get_impl(g: *mut c_void, cf: *mut c_void, idx: usize) -> u64 {
    match this_mat(g, cf) {
        Some(d) => enc_f64(d.m.a[idx]),
        None => throw_type_error(g, "DOMMatrix component getter called on a non-DOMMatrix"),
    }
}

unsafe fn comp_set_impl(g: *mut c_void, cf: *mut c_void, idx: usize) -> u64 {
    let value = arg_num(g, cf, 0, 0.0);
    let Some(d) = this_mat(g, cf) else {
        return throw_type_error(g, "DOMMatrix component setter called on a non-DOMMatrix");
    };
    d.m.a[idx] = value;
    if !TWO_D_INDICES.contains(&idx) {
        let identity_value = if idx == 10 || idx == 15 { 1.0 } else { 0.0 };
        if value != identity_value {
            d.m.is2d = false;
        }
    }
    UNDEF
}

/// One host function per accessor. Deliberately NOT a single shared function
/// carrying the index in a bound argument: `Function.prototype.bind` also pins
/// `this`, which would sever every accessor from its instance.
macro_rules! matrix_accessors {
    ($($get:ident, $set:ident, $idx:expr;)*) => {
        $(
            unsafe extern "sysv64" fn $get(g: *mut c_void, cf: *mut c_void) -> u64 {
                comp_get_impl(g, cf, $idx)
            }
            unsafe extern "sysv64" fn $set(g: *mut c_void, cf: *mut c_void) -> u64 {
                comp_set_impl(g, cf, $idx)
            }
        )*
    };
}

matrix_accessors! {
    g_m11, s_m11, 0;  g_m12, s_m12, 1;  g_m13, s_m13, 2;  g_m14, s_m14, 3;
    g_m21, s_m21, 4;  g_m22, s_m22, 5;  g_m23, s_m23, 6;  g_m24, s_m24, 7;
    g_m31, s_m31, 8;  g_m32, s_m32, 9;  g_m33, s_m33, 10; g_m34, s_m34, 11;
    g_m41, s_m41, 12; g_m42, s_m42, 13; g_m43, s_m43, 14; g_m44, s_m44, 15;
    g_a, s_a, 0; g_b, s_b, 1; g_c, s_c, 4; g_d, s_d, 5; g_e, s_e, 12; g_f, s_f, 13;
}

/// `(name, index, getter, setter)`. The index is carried alongside so the
/// accessor table and the storage layout are asserted against each other by
/// `the_accessor_table_matches_the_storage_layout` instead of by inspection.
const COMPONENTS: &[(&str, usize, HostFn, HostFn)] = &[
    ("m11", 0, g_m11, s_m11), ("m12", 1, g_m12, s_m12), ("m13", 2, g_m13, s_m13), ("m14", 3, g_m14, s_m14),
    ("m21", 4, g_m21, s_m21), ("m22", 5, g_m22, s_m22), ("m23", 6, g_m23, s_m23), ("m24", 7, g_m24, s_m24),
    ("m31", 8, g_m31, s_m31), ("m32", 9, g_m32, s_m32), ("m33", 10, g_m33, s_m33), ("m34", 11, g_m34, s_m34),
    ("m41", 12, g_m41, s_m41), ("m42", 13, g_m42, s_m42), ("m43", 14, g_m43, s_m43), ("m44", 15, g_m44, s_m44),
    ("a", 0, g_a, s_a), ("b", 1, g_b, s_b), ("c", 4, g_c, s_c),
    ("d", 5, g_d, s_d), ("e", 12, g_e, s_e), ("f", 13, g_f, s_f),
];

unsafe fn define_components(g: *mut c_void, proto: u64, writable: bool) {
    for (name, _, getf, setf) in COMPONENTS {
        let getter = make_fn(g, &format!("get {name}"), 0, *getf);
        let setter = if writable { make_fn(g, &format!("set {name}"), 1, *setf) } else { UNDEF };
        let a = atom(g, name);
        glue::rjs_define_accessor(g, proto, a, getter, setter, 2);
        glue::rjs_atom_free(g, a);
    }
}

// ---------------------------------------------------------------------------
// DOMMatrix — transforms
// ---------------------------------------------------------------------------

unsafe extern "sysv64" fn mat_get_is_2d(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_mat(g, cf) {
        Some(d) => enc_bool(d.m.is2d),
        None => throw_type_error(g, "DOMMatrixReadOnly.is2D called on a non-DOMMatrix"),
    }
}

unsafe extern "sysv64" fn mat_get_is_identity(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_mat(g, cf) {
        Some(d) => enc_bool(d.m.is_identity()),
        None => throw_type_error(g, "DOMMatrixReadOnly.isIdentity called on a non-DOMMatrix"),
    }
}

/// Post-multiply `rhs` and either mutate `this` (the `*Self` methods on
/// DOMMatrix) or return a fresh DOMMatrix (the read-only methods).
unsafe fn finish(g: *mut c_void, cf: *mut c_void, mutate: bool, rhs: Option<Mat>, name: &str) -> u64 {
    let Some(rhs) = rhs else {
        return throw_type_error(g, &format!("DOMMatrix.{name}: invalid argument"));
    };
    let Some(d) = this_mat(g, cf) else {
        return throw_type_error(g, &format!("DOMMatrix.{name} called on a non-DOMMatrix"));
    };
    let result = d.m.multiply(&rhs);
    if mutate {
        d.m = result;
        cf_this(cf)
    } else {
        new_matrix(g, result)
    }
}

unsafe fn build_translate(g: *mut c_void, cf: *mut c_void) -> Option<Mat> {
    Some(Mat::translate(arg_num(g, cf, 0, 0.), arg_num(g, cf, 1, 0.), arg_num(g, cf, 2, 0.)))
}

unsafe fn build_scale(g: *mut c_void, cf: *mut c_void) -> Option<Mat> {
    // scale(sx, sy?, sz?, ox?, oy?, oz?) — sy defaults to sx (spec §"scale()").
    let sx = arg_num(g, cf, 0, 1.);
    let sy = if cf_arg(cf, 1) == UNDEF { sx } else { read_number(g, cf_arg(cf, 1)) };
    let sz = arg_num(g, cf, 2, 1.);
    let (ox, oy, oz) = (arg_num(g, cf, 3, 0.), arg_num(g, cf, 4, 0.), arg_num(g, cf, 5, 0.));
    Some(
        Mat::translate(ox, oy, oz)
            .multiply(&Mat::scale(sx, sy, sz))
            .multiply(&Mat::translate(-ox, -oy, -oz)),
    )
}

unsafe fn build_scale3d(g: *mut c_void, cf: *mut c_void) -> Option<Mat> {
    let s = arg_num(g, cf, 0, 1.);
    let (ox, oy, oz) = (arg_num(g, cf, 1, 0.), arg_num(g, cf, 2, 0.), arg_num(g, cf, 3, 0.));
    let mut sc = Mat::scale(s, s, s);
    sc.is2d = false; // scale3d is a 3D operation by definition
    Some(Mat::translate(ox, oy, oz).multiply(&sc).multiply(&Mat::translate(-ox, -oy, -oz)))
}

unsafe fn build_rotate(g: *mut c_void, cf: *mut c_void) -> Option<Mat> {
    // Spec §"rotate()": "If rotY and rotZ are both missing, set rotZ to rotX,
    // then set rotX and rotY to 0" — one argument is a Z rotation.
    let a0 = arg_num(g, cf, 0, 0.);
    let (rx, ry, rz) = if cf_arg(cf, 1) == UNDEF && cf_arg(cf, 2) == UNDEF {
        (0., 0., a0)
    } else {
        (a0, arg_num(g, cf, 1, 0.), arg_num(g, cf, 2, 0.))
    };
    Some(Mat::rotate_z(rz).multiply(&Mat::rotate_y(ry)).multiply(&Mat::rotate_x(rx)))
}

unsafe fn build_rotate_from_vector(g: *mut c_void, cf: *mut c_void) -> Option<Mat> {
    let (x, y) = (arg_num(g, cf, 0, 0.), arg_num(g, cf, 1, 0.));
    // Spec: the angle is 0 when x and y are both 0.
    let deg = if x == 0.0 && y == 0.0 { 0.0 } else { y.atan2(x).to_degrees() };
    Some(Mat::rotate_z(deg))
}

unsafe fn build_rotate_axis_angle(g: *mut c_void, cf: *mut c_void) -> Option<Mat> {
    Some(Mat::rotate_axis_angle(
        arg_num(g, cf, 0, 0.),
        arg_num(g, cf, 1, 0.),
        arg_num(g, cf, 2, 0.),
        arg_num(g, cf, 3, 0.),
    ))
}

unsafe fn build_skew_x(g: *mut c_void, cf: *mut c_void) -> Option<Mat> {
    Some(Mat::skew_x(arg_num(g, cf, 0, 0.)))
}

unsafe fn build_skew_y(g: *mut c_void, cf: *mut c_void) -> Option<Mat> {
    Some(Mat::skew_y(arg_num(g, cf, 0, 0.)))
}

unsafe fn build_multiply(g: *mut c_void, cf: *mut c_void) -> Option<Mat> {
    mat_of(g, cf_arg(cf, 0))
}

unsafe fn build_flip_x(_g: *mut c_void, _cf: *mut c_void) -> Option<Mat> {
    Some(Mat::from_2d([-1., 0., 0., 1., 0., 0.]))
}

unsafe fn build_flip_y(_g: *mut c_void, _cf: *mut c_void) -> Option<Mat> {
    Some(Mat::from_2d([1., 0., 0., -1., 0., 0.]))
}

/// Read-only and mutating flavours from ONE builder, so the pair cannot drift.
macro_rules! transform_pair {
    ($($ro:ident, $rw:ident, $build:path, $name:literal;)*) => {
        $(
            unsafe extern "sysv64" fn $ro(g: *mut c_void, cf: *mut c_void) -> u64 {
                let r = $build(g, cf);
                finish(g, cf, false, r, $name)
            }
            unsafe extern "sysv64" fn $rw(g: *mut c_void, cf: *mut c_void) -> u64 {
                let r = $build(g, cf);
                finish(g, cf, true, r, $name)
            }
        )*
    };
}

transform_pair! {
    x_translate, x_translate_self, build_translate, "translate";
    x_scale, x_scale_self, build_scale, "scale";
    x_scale3d, x_scale3d_self, build_scale3d, "scale3d";
    x_rotate, x_rotate_self, build_rotate, "rotate";
    x_rot_vec, x_rot_vec_self, build_rotate_from_vector, "rotateFromVector";
    x_rot_axis, x_rot_axis_self, build_rotate_axis_angle, "rotateAxisAngle";
    x_skew_x, x_skew_x_self, build_skew_x, "skewX";
    x_skew_y, x_skew_y_self, build_skew_y, "skewY";
    x_multiply, x_multiply_self, build_multiply, "multiply";
    x_flip_x, x_flip_x_self, build_flip_x, "flipX";
    x_flip_y, x_flip_y_self, build_flip_y, "flipY";
}

/// `(read-only name, mutating name, arity, ro fn, self fn)`.
const TRANSFORMS: &[(&str, &str, u32, HostFn, HostFn)] = &[
    ("translate", "translateSelf", 3, x_translate, x_translate_self),
    ("scale", "scaleSelf", 6, x_scale, x_scale_self),
    ("scale3d", "scale3dSelf", 4, x_scale3d, x_scale3d_self),
    ("rotate", "rotateSelf", 3, x_rotate, x_rotate_self),
    ("rotateFromVector", "rotateFromVectorSelf", 2, x_rot_vec, x_rot_vec_self),
    ("rotateAxisAngle", "rotateAxisAngleSelf", 4, x_rot_axis, x_rot_axis_self),
    ("skewX", "skewXSelf", 1, x_skew_x, x_skew_x_self),
    ("skewY", "skewYSelf", 1, x_skew_y, x_skew_y_self),
    ("multiply", "multiplySelf", 1, x_multiply, x_multiply_self),
    ("flipX", "flipXSelf", 0, x_flip_x, x_flip_x_self),
    ("flipY", "flipYSelf", 0, x_flip_y, x_flip_y_self),
];

unsafe extern "sysv64" fn op_pre_multiply_self(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(other) = mat_of(g, cf_arg(cf, 0)) else {
        return throw_type_error(g, "DOMMatrix.preMultiplySelf: argument is not a matrix");
    };
    let Some(d) = this_mat(g, cf) else {
        return throw_type_error(g, "DOMMatrix.preMultiplySelf called on a non-DOMMatrix");
    };
    d.m = other.multiply(&d.m);
    cf_this(cf)
}

/// Spec §"invert self": a non-invertible matrix becomes all-NaN with `is2D`
/// false. NOT an exception, and NOT the original matrix — the point is that
/// every subsequent read is visibly poison rather than plausibly fine.
unsafe fn invert_impl(g: *mut c_void, cf: *mut c_void, mutate: bool) -> u64 {
    let Some(d) = this_mat(g, cf) else {
        return throw_type_error(g, "DOMMatrix.inverse called on a non-DOMMatrix");
    };
    let result = d.m.inverse().unwrap_or(Mat { a: [f64::NAN; 16], is2d: false });
    if mutate {
        d.m = result;
        cf_this(cf)
    } else {
        new_matrix(g, result)
    }
}

unsafe extern "sysv64" fn op_inverse(g: *mut c_void, cf: *mut c_void) -> u64 {
    invert_impl(g, cf, false)
}

unsafe extern "sysv64" fn op_invert_self(g: *mut c_void, cf: *mut c_void) -> u64 {
    invert_impl(g, cf, true)
}

unsafe extern "sysv64" fn op_transform_point(g: *mut c_void, cf: *mut c_void) -> u64 {
    let arg = cf_arg(cf, 0);
    let p = if is_cell(arg) {
        match host_data::<PointData>(g, classes(g).point_class, arg) {
            Some(pt) => pt.p,
            None => {
                let f = |k: &str, d: f64| {
                    let v = get(g, arg, k);
                    if v == UNDEF { d } else { read_number(g, v) }
                };
                [f("x", 0.), f("y", 0.), f("z", 0.), f("w", 1.)]
            }
        }
    } else {
        [0., 0., 0., 1.]
    };
    let Some(d) = this_mat(g, cf) else {
        return throw_type_error(g, "DOMMatrixReadOnly.transformPoint called on a non-DOMMatrix");
    };
    let out = d.m.transform_point(p);
    new_point(g, out)
}

unsafe fn to_typed(g: *mut c_void, cf: *mut c_void, kind: u32) -> u64 {
    let Some(d) = this_mat(g, cf) else {
        return throw_type_error(g, "DOMMatrixReadOnly.toFloatNArray called on a non-DOMMatrix");
    };
    let vals = d.m.a;
    let mut out = UNDEF;
    if crate::ffi::redjs_typedarray_create(g, kind, 16, &mut out) != 0 {
        return UNDEF;
    }
    let mut ta = crate::ffi::RjsTaInfo::default();
    if glue::rjs_typedarray_info(g, out, &mut ta) == 0 && !ta.data.is_null() {
        if kind == 8 {
            let p = ta.data as *mut f32;
            for (i, v) in vals.iter().enumerate() {
                *p.add(i) = *v as f32;
            }
        } else {
            let p = ta.data as *mut f64;
            for (i, v) in vals.iter().enumerate() {
                *p.add(i) = *v;
            }
        }
    }
    out
}

unsafe extern "sysv64" fn op_to_f32(g: *mut c_void, cf: *mut c_void) -> u64 {
    to_typed(g, cf, 8)
}

unsafe extern "sysv64" fn op_to_f64(g: *mut c_void, cf: *mut c_void) -> u64 {
    to_typed(g, cf, 9)
}

unsafe extern "sysv64" fn op_to_string(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(d) = this_mat(g, cf) else {
        return throw_type_error(g, "DOMMatrixReadOnly.toString called on a non-DOMMatrix");
    };
    match d.m.to_css() {
        Some(s) => js_string(g, &s),
        // Spec: "If one or more of m11 … m44 are a non-finite number, then
        // throw an InvalidStateError". There is no DOMException here, so it is
        // a TypeError carrying the spec's name verbatim.
        None => throw_type_error(
            g,
            "InvalidStateError: DOMMatrix.toString on a matrix with non-finite components",
        ),
    }
}

const CSS_REFUSAL: &str = "parses a CSS <transform-list>, which this runtime does not implement: \
    the engine's only CSS front end is the redcss crate and it exposes no transform-list entry \
    point. Build the matrix from components or a 6/16-number sequence instead.";

unsafe extern "sysv64" fn op_set_matrix_value(g: *mut c_void, _cf: *mut c_void) -> u64 {
    throw_type_error(g, &format!("DOMMatrix.setMatrixValue {CSS_REFUSAL}"))
}

unsafe extern "sysv64" fn op_to_json(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(d) = this_mat(g, cf) else {
        return throw_type_error(g, "DOMMatrixReadOnly.toJSON called on a non-DOMMatrix");
    };
    let m = d.m;
    let o = new_object(g, UNDEF);
    for (name, idx, _, _) in COMPONENTS {
        define(g, o, name, enc_f64(m.a[*idx]), 0);
    }
    define(g, o, "is2D", enc_bool(m.is2d), 0);
    define(g, o, "isIdentity", enc_bool(m.is_identity()), 0);
    o
}

// ---------------------------------------------------------------------------
// Constructors and statics
// ---------------------------------------------------------------------------

unsafe fn matrix_from_init(g: *mut c_void, init: u64) -> Result<Mat, String> {
    if init == UNDEF || init == NULL {
        return Ok(IDENTITY);
    }
    if glue::rjs_kind(g, init) == 5 {
        return Err(format!("DOMMatrix(string) {CSS_REFUSAL}"));
    }
    // Sequence form: 6 or 16 numbers (spec §"create a DOMMatrix from a sequence").
    if is_cell(init) && get(g, init, "length") != UNDEF && !is_matrix(g, init) {
        let n = array_len(g, init);
        let mut v: Vec<f64> = Vec::with_capacity(n as usize);
        for i in 0..n {
            v.push(read_number(g, get_i(g, init, i)));
        }
        return match v.len() {
            6 => Ok(Mat::from_2d([v[0], v[1], v[2], v[3], v[4], v[5]])),
            16 => {
                let mut a = [0.0f64; 16];
                a.copy_from_slice(&v);
                Ok(Mat::from_16(a))
            }
            other => Err(format!(
                "DOMMatrix: a sequence initializer must have 6 or 16 numbers, got {other}"
            )),
        };
    }
    match mat_of(g, init) {
        Some(m) => Ok(m),
        None => Err("DOMMatrix: unsupported initializer".into()),
    }
}

macro_rules! matrix_ctor {
    ($name:ident, $proto:ident) => {
        unsafe extern "sysv64" fn $name(g: *mut c_void, cf: *mut c_void) -> u64 {
            let c = classes(g);
            match matrix_from_init(g, cf_arg(cf, 0)) {
                Ok(m) => {
                    let proto = ordinary_proto(g, cf_new_target(cf), c.$proto.proto);
                    host_create(g, c.matrix_class, proto, MatrixData { m })
                }
                Err(e) => throw_type_error(g, &e),
            }
        }
    };
}
matrix_ctor!(matrix_construct_rw, rw);
matrix_ctor!(matrix_construct_ro, ro);

unsafe extern "sysv64" fn static_from_matrix(g: *mut c_void, cf: *mut c_void) -> u64 {
    match mat_of(g, cf_arg(cf, 0)) {
        Some(m) => new_matrix(g, m),
        None => new_matrix(g, IDENTITY),
    }
}

unsafe extern "sysv64" fn static_from_array(g: *mut c_void, cf: *mut c_void) -> u64 {
    let arg = cf_arg(cf, 0);
    let mut ta = crate::ffi::RjsTaInfo::default();
    let mut v: Vec<f64> = Vec::new();
    if glue::rjs_typedarray_info(g, arg, &mut ta) == 0 && !ta.data.is_null() && ta.detached == 0 {
        let n = ta.length as usize;
        if ta.kind == 8 {
            let p = ta.data as *const f32;
            for i in 0..n {
                v.push(*p.add(i) as f64);
            }
        } else {
            let p = ta.data as *const f64;
            for i in 0..n {
                v.push(*p.add(i));
            }
        }
    } else if is_cell(arg) {
        let n = array_len(g, arg);
        for i in 0..n {
            v.push(read_number(g, get_i(g, arg, i)));
        }
    }
    match v.len() {
        6 => new_matrix(g, Mat::from_2d([v[0], v[1], v[2], v[3], v[4], v[5]])),
        16 => {
            let mut a = [0.0f64; 16];
            a.copy_from_slice(&v);
            new_matrix(g, Mat::from_16(a))
        }
        other => throw_type_error(
            g,
            &format!("DOMMatrix.fromFloat32Array/fromFloat64Array: expected 6 or 16 elements, got {other}"),
        ),
    }
}

pub unsafe fn install(g: *mut c_void) {
    let point_class = make_host_class::<PointData>("DOMPoint");
    let point_ro = define_class(g, &ClassSpec {
        name: "DOMPointReadOnly",
        arity: 4,
        construct: Some(point_construct),
        parent_proto: 0,
        methods: &[("matrixTransform", 1, point_matrix_transform), ("toJSON", 0, point_to_json)],
        getters: &[("x", pt_get_x), ("y", pt_get_y), ("z", pt_get_z), ("w", pt_get_w)],
        accessors: &[],
    });
    let point_rw = define_class(g, &ClassSpec {
        name: "DOMPoint",
        arity: 4,
        construct: Some(point_construct),
        parent_proto: point_ro.proto,
        methods: &[],
        getters: &[],
        accessors: &[
            ("x", pt_get_x, pt_set_x),
            ("y", pt_get_y, pt_set_y),
            ("z", pt_get_z, pt_set_z),
            ("w", pt_get_w, pt_set_w),
        ],
    });

    let matrix_class = make_host_class::<MatrixData>("DOMMatrix");
    let ro = define_class(g, &ClassSpec {
        name: "DOMMatrixReadOnly",
        arity: 1,
        construct: Some(matrix_construct_ro),
        parent_proto: 0,
        methods: &[
            ("transformPoint", 1, op_transform_point),
            ("inverse", 0, op_inverse),
            ("toFloat32Array", 0, op_to_f32),
            ("toFloat64Array", 0, op_to_f64),
            ("toString", 0, op_to_string),
            ("toJSON", 0, op_to_json),
        ],
        getters: &[("is2D", mat_get_is_2d), ("isIdentity", mat_get_is_identity)],
        accessors: &[],
    });
    define_components(g, ro.proto, false);
    for (name, _, arity, f, _) in TRANSFORMS {
        define(g, ro.proto, name, make_fn(g, name, *arity, *f), 2);
    }

    let rw = define_class(g, &ClassSpec {
        name: "DOMMatrix",
        arity: 1,
        construct: Some(matrix_construct_rw),
        parent_proto: ro.proto,
        methods: &[
            ("preMultiplySelf", 1, op_pre_multiply_self),
            ("invertSelf", 0, op_invert_self),
            ("setMatrixValue", 1, op_set_matrix_value),
        ],
        getters: &[],
        accessors: &[],
    });
    // The mutable component accessors shadow the read-only ones inherited from
    // DOMMatrixReadOnly.prototype; the transform methods do NOT — `multiply`
    // still returns a new matrix on a DOMMatrix, which is why the chain is real
    // rather than two flat copies.
    define_components(g, rw.proto, true);
    for (_, self_name, arity, _, f) in TRANSFORMS {
        define(g, rw.proto, self_name, make_fn(g, self_name, *arity, *f), 2);
    }

    for ctor in [ro.ctor, rw.ctor] {
        define(g, ctor, "fromMatrix", make_fn(g, "fromMatrix", 1, static_from_matrix), 2);
        define(g, ctor, "fromFloat32Array", make_fn(g, "fromFloat32Array", 1, static_from_array), 2);
        define(g, ctor, "fromFloat64Array", make_fn(g, "fromFloat64Array", 1, static_from_array), 2);
    }

    let boxed: &'static GeomClasses =
        Box::leak(Box::new(GeomClasses { matrix_class, ro, rw, point_class, point_rw }));
    registry().lock().unwrap().insert(g as usize, boxed);
}

// ---------------------------------------------------------------------------
// Tests — the arithmetic, no JS context needed
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn multiply_is_post_multiplication_so_the_argument_applies_first() {
        // The direction is easy to get backwards, and backwards still produces a
        // plausible-looking matrix. Pin it with a case whose two orders DISAGREE.
        let t = Mat::translate(10.0, 0.0, 0.0);
        let s = Mat::scale(2.0, 2.0, 1.0);

        // t.multiply(s) applies s first, then t: (1,0) → (2,0) → (12,0).
        let p = t.multiply(&s).transform_point([1.0, 0.0, 0.0, 1.0]);
        assert!(close(p[0], 12.0), "{p:?}");

        // s.multiply(t) applies t first, then s: (1,0) → (11,0) → (22,0).
        let q = s.multiply(&t).transform_point([1.0, 0.0, 0.0, 1.0]);
        assert!(close(q[0], 22.0), "{q:?}");
    }

    #[test]
    fn rotate_z_ninety_degrees_maps_x_onto_y() {
        let p = Mat::rotate_z(90.0).transform_point([1.0, 0.0, 0.0, 1.0]);
        assert!(close(p[0], 0.0) && close(p[1], 1.0), "{p:?}");
    }

    #[test]
    fn rotate_x_and_y_leave_the_two_d_flag_behind() {
        assert!(Mat::rotate_z(30.0).is2d);
        assert!(!Mat::rotate_x(30.0).is2d);
        assert!(!Mat::rotate_y(30.0).is2d);
        assert!(!Mat::rotate_z(30.0).multiply(&Mat::rotate_x(30.0)).is2d);
    }

    #[test]
    fn inverse_round_trips_a_nontrivial_transform() {
        let m = Mat::translate(3.0, -7.0, 2.0)
            .multiply(&Mat::rotate_axis_angle(1.0, 2.0, 3.0, 37.0))
            .multiply(&Mat::scale(2.0, 0.5, 4.0));
        let inv = m.inverse().expect("invertible");
        for (i, v) in m.multiply(&inv).a.iter().enumerate() {
            assert!(close(*v, IDENTITY.a[i]), "component {i} was {v}");
        }
    }

    #[test]
    fn a_singular_matrix_has_no_inverse() {
        assert!(Mat::scale(0.0, 1.0, 1.0).inverse().is_none());
    }

    #[test]
    fn rotate_axis_angle_about_z_matches_rotate_z() {
        let a = Mat::rotate_axis_angle(0.0, 0.0, 1.0, 42.0);
        let b = Mat::rotate_z(42.0);
        for i in 0..16 {
            assert!(close(a.a[i], b.a[i]), "component {i}: {} vs {}", a.a[i], b.a[i]);
        }
    }

    #[test]
    fn skew_x_shears_y_into_x() {
        let p = Mat::skew_x(45.0).transform_point([0.0, 1.0, 0.0, 1.0]);
        assert!(close(p[0], 1.0) && close(p[1], 1.0), "{p:?}");
    }

    #[test]
    fn the_two_d_serialization_is_the_css_matrix_function() {
        assert_eq!(Mat::from_2d([1., 2., 3., 4., 5., 6.]).to_css().unwrap(), "matrix(1, 2, 3, 4, 5, 6)");
    }

    #[test]
    fn a_three_d_matrix_serializes_as_matrix3d_with_sixteen_components() {
        let s = Mat::rotate_x(90.0).to_css().unwrap();
        assert!(s.starts_with("matrix3d("), "{s}");
        assert_eq!(s.matches(',').count(), 15, "{s}");
    }

    #[test]
    fn a_non_finite_component_has_no_serialization() {
        let mut m = IDENTITY;
        m.a[0] = f64::INFINITY;
        assert!(m.to_css().is_none());
    }

    #[test]
    fn the_accessor_table_matches_the_storage_layout() {
        // 22 hand-written accessor entries is 22 chances to transpose an index,
        // and a transposed index is a wrong picture rather than a crash. Check
        // every entry against `Mat::get`, which the layout test below pins.
        let mut m = IDENTITY;
        for (i, v) in m.a.iter_mut().enumerate() {
            *v = 100.0 + i as f64;
        }
        let expect = |name: &str| -> f64 {
            match name {
                "a" => m.get(1, 1), "b" => m.get(1, 2), "c" => m.get(2, 1),
                "d" => m.get(2, 2), "e" => m.get(4, 1), "f" => m.get(4, 2),
                _ => {
                    let b = name.as_bytes();
                    m.get((b[1] - b'0') as usize, (b[2] - b'0') as usize)
                }
            }
        };
        for (name, idx, _, _) in COMPONENTS {
            assert_eq!(m.a[*idx], expect(name), "accessor {name} points at index {idx}");
        }
    }

    #[test]
    fn the_2d_alias_layout_matches_the_spec_component_names() {
        // a,b,c,d,e,f == m11,m12,m21,m22,m41,m42.
        let m = Mat::from_2d([1., 2., 3., 4., 5., 6.]);
        assert_eq!((m.get(1, 1), m.get(1, 2)), (1.0, 2.0));
        assert_eq!((m.get(2, 1), m.get(2, 2)), (3.0, 4.0));
        assert_eq!((m.get(4, 1), m.get(4, 2)), (5.0, 6.0));
    }
}

/// Through a real JSC context — the half the arithmetic tests above cannot see.
#[cfg(test)]
mod jsc_tests {
    use crate::webapi::testing::*;

    #[test]
    fn the_prototype_chain_is_real() {
        let c = ctx();
        ok(&c, "new DOMMatrix() instanceof DOMMatrix");
        ok(&c, "new DOMMatrix() instanceof DOMMatrixReadOnly");
        ok(&c, "!(new DOMMatrixReadOnly() instanceof DOMMatrix)");
        ok(&c, "new DOMPoint() instanceof DOMPointReadOnly");
        // Inheritance, not two flat copies: the read-only method is found on the
        // parent prototype and still returns a NEW matrix from a DOMMatrix.
        ok(&c, "!Object.getOwnPropertyNames(DOMMatrix.prototype).includes('translate')");
        ok(&c, "(m => m.translate(1,2) !== m)(new DOMMatrix())");
    }

    #[test]
    fn the_component_accessors_reach_the_right_storage() {
        let c = ctx();
        ok(&c, "(m => m.a===1&&m.b===2&&m.c===3&&m.d===4&&m.e===5&&m.f===6)(new DOMMatrix([1,2,3,4,5,6]))");
        ok(&c, "(m => m.m11===1&&m.m12===2&&m.m21===3&&m.m22===4&&m.m41===5&&m.m42===6)(new DOMMatrix([1,2,3,4,5,6]))");
        // The alias and the canonical name are ONE storage slot.
        ok(&c, "(m => { m.a = 9; return m.m11 === 9; })(new DOMMatrix())");
    }

    #[test]
    fn read_only_components_do_not_write() {
        let c = ctx();
        // No setter on DOMMatrixReadOnly: a sloppy-mode assignment is silently
        // dropped, so assert the VALUE rather than expecting a throw.
        ok(&c, "(m => { try { m.a = 5; } catch (e) {} return m.a === 1; })(new DOMMatrixReadOnly())");
    }

    #[test]
    fn self_methods_mutate_and_return_this_while_plain_ones_do_not() {
        let c = ctx();
        ok(&c, "(m => m.translateSelf(10, 0) === m && m.e === 10)(new DOMMatrix())");
        ok(&c, "(m => { m.translate(10, 0); return m.e === 0; })(new DOMMatrix())");
    }

    #[test]
    fn transform_point_returns_a_dompoint_with_the_transformed_coordinates() {
        let c = ctx();
        ok(&c, "(p => p instanceof DOMPoint && p.x === 12 && p.y === 0)(new DOMMatrix().translateSelf(10,0).scaleSelf(2).transformPoint({x:1,y:0}))");
    }

    #[test]
    fn to_float64_array_is_sixteen_components_in_spec_order() {
        let c = ctx();
        ok(&c, "(a => a instanceof Float64Array && a.length === 16 && a[0] === 1 && a[1] === 2)(new DOMMatrix([1,2,3,4,5,6]).toFloat64Array())");
    }

    #[test]
    fn a_string_initializer_refuses_by_name_instead_of_returning_identity() {
        // The failure this guards against is the plausible one: silently
        // yielding identity, which draws everything at the origin unrotated and
        // looks like a bug anywhere but here.
        let c = ctx();
        let msg = throws(&c, "new DOMMatrix('translate(10px, 20px)')");
        assert!(msg.contains("transform-list"), "{msg}");
        assert!(msg.contains("redcss"), "{msg}");
        let msg2 = throws(&c, "new DOMMatrix().setMatrixValue('scale(2)')");
        assert!(msg2.contains("transform-list"), "{msg2}");
    }

    #[test]
    fn a_bad_sequence_length_says_what_it_got() {
        let c = ctx();
        let msg = throws(&c, "new DOMMatrix([1,2,3])");
        assert!(msg.contains("6 or 16"), "{msg}");
        assert!(msg.contains('3'), "{msg}");
    }

    #[test]
    fn an_accessor_on_a_foreign_object_is_a_type_error() {
        let c = ctx();
        let msg = throws(&c, "Object.getOwnPropertyDescriptor(DOMMatrix.prototype,'a').get.call({})");
        assert!(msg.contains("non-DOMMatrix"), "{msg}");
    }

    #[test]
    fn a_singular_matrix_inverts_to_nan_rather_than_to_itself() {
        let c = ctx();
        ok(&c, "Number.isNaN(new DOMMatrix().scaleSelf(0).inverse().m11)");
        ok(&c, "new DOMMatrix().scaleSelf(0).inverse().is2D === false");
    }
}
