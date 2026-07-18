//! Native physics host classes for the RedPew player. `RedPhysics2D` (p2d.rs,
//! rapier2d) is the live one; a future `RedPhysics3D` (rapier3d) reuses the
//! plumbing in this module when the engine grows a 3D path. Protocol and
//! rationale: `.refactor/PHYSICS-NATIVE-DESIGN.md`.
//!
//! Wire conventions (shared by all dimensions):
//! - JS never sees rapier handles — only dense u32 slot ids minted by [`Slab`].
//! - Hot data crosses as Rust-owned buffers exposed zero-copy ([`ExposedF32`] /
//!   [`ExposedF64`]); growth regenerates the JS views and bumps the world's
//!   `generation()`, detaching the old ArrayBuffer so stale references read
//!   empty instead of dangling.
//! - Fixed-size descriptors and small multi-value returns ride the f64
//!   `scratch` buffer; variable-size data (polygon vertices) comes in as a
//!   typed-array argument.

pub mod p2d;

use core::ffi::c_void;

use crate::webapi::util::{is_cell, UNDEF};

/// Installs the physics host classes on the global object. Called from
/// `webapi::install` (idempotence rides its `has_state` gate).
pub unsafe fn install(g: *mut c_void) {
    p2d::install(g);
}

// ---- zero-copy exposed buffers ---------------------------------------------

const TA_F32: u32 = 8;
const TA_F64: u32 = 9;

macro_rules! exposed_buf {
    ($name:ident, $elem:ty, $kind:expr) => {
        /// Rust-owned buffer exposed to JS as (ArrayBuffer, typed-array view),
        /// zero copy. The Vec is NEVER grown in place (the JS view aliases its
        /// memory); growth goes through [`Self::ensure`] which reallocates and
        /// regenerates the views.
        pub struct $name {
            vec: Vec<$elem>,
            ab: u64,
            ta: u64,
        }

        impl $name {
            pub unsafe fn new(g: *mut c_void, len: usize) -> Self {
                let mut s = Self { vec: vec![0.0; len], ab: UNDEF, ta: UNDEF };
                s.make_views(g);
                s
            }

            unsafe fn make_views(&mut self, g: *mut c_void) {
                let bytes = self.vec.len() * core::mem::size_of::<$elem>();
                let mut ab = UNDEF;
                crate::ffi::redjs_arraybuffer_external(
                    g,
                    self.vec.as_mut_ptr() as *mut c_void,
                    bytes,
                    core::ptr::null(), // host keeps ownership; detach before the Vec dies
                    core::ptr::null_mut(),
                    &mut ab,
                );
                let mut ta = UNDEF;
                crate::ffi::redjs_typedarray_wrap(g, $kind, ab, 0, self.vec.len(), &mut ta);
                crate::ffi::redjs_protect(g, ab);
                crate::ffi::redjs_protect(g, ta);
                self.ab = ab;
                self.ta = ta;
            }

            /// The JS typed-array view over this buffer.
            pub fn view(&self) -> u64 {
                self.ta
            }

            pub fn data(&mut self) -> &mut [$elem] {
                &mut self.vec
            }

            pub fn len(&self) -> usize {
                self.vec.len()
            }

            /// Ensures room for `len` elements. Returns true when the views
            /// were regenerated (caller must bump its generation counter).
            /// Contents are NOT preserved — callers rewrite the buffer anyway.
            pub unsafe fn ensure(&mut self, g: *mut c_void, len: usize) -> bool {
                if len <= self.vec.len() {
                    return false;
                }
                let new_len = len.next_power_of_two();
                self.release(g);
                self.vec = vec![0.0; new_len];
                self.make_views(g);
                true
            }

            /// Detaches + releases the JS views. Safe to call more than once.
            /// MUST run before the Vec is dropped while JS can still run; on a
            /// pure-GC teardown (finalizer thread) it must NOT run — the
            /// finalizer never touches JS, so protected views leak by design
            /// (`free()` is the supported teardown path).
            pub unsafe fn release(&mut self, g: *mut c_void) {
                if is_cell(self.ab) {
                    crate::ffi::redjs_arraybuffer_detach(g, self.ab);
                    crate::ffi::redjs_unprotect(g, self.ab);
                    crate::ffi::redjs_unprotect(g, self.ta);
                }
                self.ab = UNDEF;
                self.ta = UNDEF;
            }
        }

        impl Drop for $name {
            fn drop(&mut self) {
                // GC-finalizer teardown (free() never called): the protected JS
                // views still alias this Vec's memory and the finalizer cannot
                // detach them (no JS access on GC threads) — LEAK the Vec rather
                // than free memory under live views (use-after-free otherwise).
                if is_cell(self.ab) {
                    let vec = core::mem::take(&mut self.vec);
                    core::mem::forget(vec);
                }
            }
        }
    };
}

exposed_buf!(ExposedF32, f32, TA_F32);
exposed_buf!(ExposedF64, f64, TA_F64);

// ---- slot allocator ---------------------------------------------------------

/// Dense u32-id slot map: the JS-facing handle space. Ids are reused after
/// removal (the TS adapter owns liveness, mirroring guid⇄slot maps).
pub struct Slab<T> {
    items: Vec<Option<T>>,
    free: Vec<u32>,
}

impl<T> Slab<T> {
    pub fn new() -> Self {
        Slab { items: Vec::new(), free: Vec::new() }
    }

    pub fn insert(&mut self, value: T) -> u32 {
        if let Some(id) = self.free.pop() {
            self.items[id as usize] = Some(value);
            id
        } else {
            self.items.push(Some(value));
            (self.items.len() - 1) as u32
        }
    }

    pub fn remove(&mut self, id: u32) -> Option<T> {
        let slot = self.items.get_mut(id as usize)?;
        let value = slot.take();
        if value.is_some() {
            self.free.push(id);
        }
        value
    }

    pub fn get(&self, id: u32) -> Option<&T> {
        self.items.get(id as usize)?.as_ref()
    }

    pub fn get_mut(&mut self, id: u32) -> Option<&mut T> {
        self.items.get_mut(id as usize)?.as_mut()
    }

    /// Number of allocated slots (upper bound of live ids + 1).
    pub fn capacity(&self) -> usize {
        self.items.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (u32, &T)> {
        self.items
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.as_ref().map(|v| (i as u32, v)))
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.free.clear();
    }
}
