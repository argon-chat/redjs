//! Where the `redjs-webgpu` crate plugs in. WebGPU lives in its own crate so
//! that wgpu's tree and this crate compile in parallel; the places here that
//! must reach a GPU canvas (OffscreenCanvas, createImageBitmap, the pump, the
//! context fan-out) go through the one implementation registered here. With
//! none registered, `navigator.gpu` is absent and `getContext("webgpu")` is null.

use core::ffi::c_void;
use std::sync::OnceLock;

pub type Pixels = Option<Result<(Vec<u8>, u32, u32), String>>;

pub trait GpuExtension: Send + Sync {
    /// Called by `webshim_install` after `navigator` exists.
    unsafe fn install(&self, g: *mut c_void);
    fn forget(&self, gone: crate::context::Gone);
    /// End of a timer/rAF pump: present the canvases drawn this frame.
    unsafe fn pump(&self, g: *mut c_void);
    /// A `GPUCanvasContext` for an OffscreenCanvas.
    unsafe fn create_context(&self, g: *mut c_void, owner: u64, w: u32, h: u32) -> u64;
    unsafe fn resize(&self, g: *mut c_void, ctx: u64, w: u32, h: u32);
    unsafe fn is_context(&self, g: *mut c_void, v: u64) -> bool;
    /// Straight RGBA of a GPU canvas context; `None` when `v` is not one.
    unsafe fn pixels(&self, g: *mut c_void, v: u64) -> Pixels;
    unsafe fn premul_pixels(&self, g: *mut c_void, v: u64) -> Pixels;
    unsafe fn convert_to_blob(&self, g: *mut c_void, ctx: u64, options: u64) -> u64;
    unsafe fn transfer_to_image_bitmap(&self, g: *mut c_void, ctx: u64) -> u64;
}

static EXTENSION: OnceLock<&'static dyn GpuExtension> = OnceLock::new();

/// Before the first `webshim_install` that should have WebGPU. The first
/// registration wins.
pub fn register(ext: &'static dyn GpuExtension) {
    let _ = EXTENSION.set(ext);
}

pub(crate) fn get() -> Option<&'static dyn GpuExtension> {
    EXTENSION.get().copied()
}
