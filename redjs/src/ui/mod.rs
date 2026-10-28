//! Native UI host classes for the RedPew player. Today that is exactly one:
//! [`yoga::RedYoga`](yoga) — the vendored Yoga 3.2.1 flexbox engine (`cpp/yoga`,
//! compiled by `build.rs`) exposed as a JS host class so the shipped Windows
//! player never instantiates the yoga **wasm** module.
//!
//! Why this exists at all is a MEMORY argument, not a CPU one
//! (`.refactor/UI-LAYOUT-NATIVE-DESIGN.md` §1.3, §3 "Measured — memory"):
//! compiling yoga's 71,736 B module costs +7.98 MB of JSC working set at
//! `new WebAssembly.Module` alone and +8–20 MB hot, essentially all of it BBQ/OMG
//! JIT rather than heap. Yoga is also the **last** wasm module the native player
//! actually instantiates (§1.6), so removing it is what makes `JSC_useWasm=0`
//! reachable at all.
//!
//! Conventions are the ones every `Red*` host class in this crate follows (see
//! [`crate::hostbuf`]): JS holds dense u32 slot ids, never native pointers;
//! fixed-size payloads ride a shared Rust-owned `Float64Array` scratch buffer
//! exposed zero-copy, so a batched call crosses the boundary once instead of N
//! times. The TS side of the seam is `ILayoutEngine`/`ILayoutNode`
//! (`packages/ui/src/layout/ILayoutEngine.ts`); the wasm adapter it must stay
//! bit-identical to is `packages/ui/src/layout/YogaLayoutEngine.ts`.

#[cfg(feature = "css-host")]
pub mod css;
#[cfg(feature = "yoga-host")]
pub mod yoga;

use core::ffi::c_void;

/// Installs the UI host classes on the global object. Called from
/// `webapi::install`, next to `crate::physics::install` / `crate::mesh::install`
/// (idempotence rides each class's own per-global state map).
pub unsafe fn install(g: *mut c_void) {
    #[cfg(feature = "yoga-host")]
    yoga::install(g);
    #[cfg(feature = "css-host")]
    css::install(g);
    #[cfg(not(any(feature = "yoga-host", feature = "css-host")))]
    let _ = g;
}

/// `crate::context` fan-out. `css` keeps no per-context registry.
pub(crate) fn forget(gone: crate::context::Gone) {
    #[cfg(feature = "yoga-host")]
    yoga::forget(gone);
    #[cfg(not(feature = "yoga-host"))]
    let _ = gone;
}
