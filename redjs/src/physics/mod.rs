//! Native physics host classes for the RedPew player: `RedPhysics2D` (p2d.rs,
//! rapier2d) and `RedPhysics3D` (p3d.rs, rapier3d). Both share the plumbing in
//! this module and coexist — a level may hold 2D and 3D bodies at once, each
//! driven by its own subsystem. Protocol and rationale:
//! `.refactor/PHYSICS-NATIVE-DESIGN.md` (2D) / `.refactor/3D-TOOLKIT-DESIGN.md` §4 (3D).
//!
//! Wire conventions (shared by all dimensions):
//! - JS never sees rapier handles — only dense u32 slot ids minted by [`Slab`].
//! - Hot data crosses as Rust-owned buffers exposed zero-copy ([`ExposedF32`] /
//!   [`ExposedF64`]); growth regenerates the JS views and bumps the world's
//!   `generation()`, detaching the old ArrayBuffer so stale references read
//!   empty instead of dangling.
//! - Fixed-size descriptors and small multi-value returns ride the f64
//!   `scratch` buffer; variable-size data (polygon/hull vertices, trimesh
//!   indices) comes in as a typed-array argument.

pub mod p2d;
pub mod p3d;

use core::ffi::c_void;

/// The buffer/slot primitives these host classes are built on. They live in
/// [`crate::hostbuf`] because every native host class family needs them (UI
/// layout too), and are re-exported here so `p2d`/`p3d` keep reading them as
/// `use super::{ExposedF32, ExposedF64, Slab}`.
pub use crate::hostbuf::{ExposedF32, ExposedF64, Slab};

/// Installs the physics host classes on the global object. Called from
/// `webapi::install` (idempotence rides its `has_state` gate).
pub unsafe fn install(g: *mut c_void) {
    p2d::install(g);
    p3d::install(g);
}
