//! redjs — full-control Rust embedding layer over Bun's JavaScriptCore
//! (Windows prebuilt), designed as the ClearScript replacement seam for the
//! RedPew player.
//!
//! Layers:
//! - [`glue`]: raw externs into our C++ glue (`cpp/glue.cpp`, JSC internal API).
//! - [`capi`]: raw externs into JSC's stable C API (the subset we use).
//! - [`trampoline`]: the SysV→MS host-function trampoline pool (C# entry).
//! - [`errors`]: the one funnel for exceptions no JS code will ever see —
//!   pumped-callback throws, listener throws and unhandled promise rejections.
//! - [`context`]: a monotonic, never-reused id per context, and the one list
//!   of per-context registries cleared when an address stops meaning its
//!   context.
//! - [`ffi`]: the exported `redjs_*` C ABI for C# (csbindgen input).
//! - [`api`]: safe-ish Rust API over the same machinery (tests, tools, and
//!   any Rust-side host logic).
//!
//! Values across every boundary are JSC `EncodedJSValue` bits (`u64`,
//! JSVALUE64 NaN-boxing). Numbers/bools/undefined/null en/decode without FFI
//! using the constants from [`ffi::RjsLayout`].
//!
//! # Lifetime & GC contract
//! A `u64` value held on the native stack while the thread is inside the VM
//! is protected by JSC's conservative stack scan. Anything stored on the heap
//! (C# fields, Rust collections) must be pinned with `redjs_protect` /
//! [`api::Context::protect`] and released with `redjs_unprotect`.

pub mod api;
pub mod capi;
pub mod bytecode;
pub mod context;
pub mod errors;
pub mod ffi;
pub mod glue;
pub mod hostbuf;
pub mod icu;
#[cfg(feature = "mesh")]
pub mod mesh;
#[cfg(feature = "physics")]
pub mod physics;
pub mod trampoline;
pub mod ui;
pub mod web;
pub mod webapi;

#[cfg(feature = "native-aot")]
pub mod aot;
