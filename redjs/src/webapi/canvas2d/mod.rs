//! Canvas 2D: `CanvasRenderingContext2D`, `OffscreenCanvas` and friends
//! (HTML §4.12.5), over a raster backend the embedder may replace.
//!
//! ## Layers
//!
//! * [`context`]: the JS classes and every HTML rule — state and its stack,
//!   transforms, the current path, argument handling, image sources. It
//!   RECORDS: each draw becomes a [`cmd::Cmd`].
//! * [`cmd`]: the command stream, the contract with a backend. Backend-neutral
//!   (kurbo geometry, straight sRGB colours, premultiplied images with ids).
//! * [`surface`]: one canvas bitmap — a [`Backend`] plus the commands not yet
//!   given to it, flushed when something needs the pixels.
//! * `cpu`: the built-in backend, vello_cpu. An embedder may install another
//!   through [`set_host`] while keeping the same command stream
//!   and the context does not change.
//!
//! ## What an embedder gets
//!
//! * `OffscreenCanvas` is complete as a 2D canvas. Without a `<canvas>`
//!   element of its own, an embedder builds one from the hidden natives
//!   (`__rjs_canvas_context`, `_resize`, `_data_url`, `_blob`,
//!   `_source_hook`, `_info`; see `context.rs`), and paints it from
//!   [`present`] under the key it registered. [`take_presented_changed`]
//!   tells its frame loop that some presented canvas drew since the last
//!   frame, so an idle canvas costs one atomic load a frame.
//!
//! ## Text and effects
//!
//! `fillText` and `strokeText` shape platform or registered CSS fonts into
//! ordinary fill/stroke paths. `measureText` uses shaped advances, font metrics
//! and actual ink bounds. Alignment, baselines, spacing, kerning, direction and
//! `maxWidth` use the same layout; no separate glyph backend is required.
//! Embedders register decoded web fonts with [`text::register_font`] before
//! applying private family renames. Registrations and layout caches belong to
//! one JS context and are released with it. Colour-only glyphs use an outline
//! fallback; this is not a colour-emoji rasterizer.
//!
//! The built-in CPU backend applies `shadowColor`, `shadowBlur`, offsets and
//! lists of CSS `blur(<px>)` filters in bitmap space, before the final canvas
//! clip. Other CSS filters emit an advisory. Replacement backends must honour
//! [`cmd::Draw::effects`] as well as the existing paint/composite/clip state.
//!
//! ## Remaining limits
//!
//! `{ alpha: false }` keeps an alpha channel, and a canvas is never tainted
//! (the origin-clean flag is tracked, see `Surface::readable`).

pub mod cmd;
mod color;
mod context;
mod cpu;
mod effects;
pub(crate) mod encode;
pub mod surface;
pub mod text;

use core::ffi::c_void;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

pub use surface::{
    present, present_size, register_present, set_host, take_presented_changed, unregister_present, Backend, CanvasHost,
    Options,
};

fn advised() -> &'static Mutex<(HashSet<String>, Vec<String>)> {
    static A: OnceLock<Mutex<(HashSet<String>, Vec<String>)>> = OnceLock::new();
    A.get_or_init(|| Mutex::new((HashSet::new(), Vec::new())))
}

/// Says `msg` once per `key`, for the process.
pub(crate) fn advise(key: &str, msg: String) {
    let mut a = advised().lock().unwrap();
    if !a.0.insert(key.to_string()) {
        return;
    }
    eprintln!("[redjs] {msg}");
    a.1.push(msg);
}

/// Every canvas advisory said so far, in order.
pub fn advisories() -> Vec<String> {
    advised().lock().unwrap().1.clone()
}

pub(crate) unsafe fn install(g: *mut c_void, event_target_proto: u64) {
    context::install(g, event_target_proto);
}

pub(crate) fn forget(gone: crate::context::Gone) {
    text::forget(gone.id);
    context::forget(gone);
}

/// The pixels of a canvas source (OffscreenCanvas, a context, or what the
/// embedder's source hook maps the value to), straight RGBA — for
/// `createImageBitmap` and the other pixel readers in `image.rs`.
pub(crate) unsafe fn canvas_pixels(g: *mut c_void, v: u64) -> Option<(Vec<u8>, u32, u32)> {
    context::canvas_pixels(g, v)
}

#[cfg(test)]
mod jsc_tests;

#[cfg(feature = "webgpu")]
pub(crate) unsafe fn external_pixels(g: *mut c_void, v: u64, depth: u8) -> Result<super::image::ExternalImage, ()> {
    context::external_pixels(g, v, depth)
}
