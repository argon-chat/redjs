//! A canvas bitmap: a raster backend plus the commands not yet given to it.
//!
//! Recording is cheap and rasterizing is not, so a `Surface` batches: draws
//! queue up as [`Cmd`]s and reach the backend only when something needs the
//! pixels — a readback (`getImageData`, `toDataURL`, `toBlob`,
//! `convertToBlob`), the canvas used as an image source, a presentation (the
//! embedder's frame, [`present`]) — or when the queue grows past
//! [`FLUSH_AT`]. A command that overwrites the whole bitmap (a full
//! `clearRect`, a full `putImageData`) drops everything queued before it, so
//! the usual animation loop — clear, then draw — never rasterizes a frame
//! nobody looked at.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

use super::cmd::{unpremultiply, Cmd, Image, ImageRef};

/// A raster backend: owns one bitmap and replays commands onto it.
///
/// `Send` because a surface is dropped wherever its context's JS object is
/// finalized, which may be a GC thread.
pub trait Backend: Send {
    /// For diagnostics and tests ("vello_cpu", "vello_gpu", ...).
    fn name(&self) -> &str;
    /// Drops the bitmap and starts a `width`×`height` one, transparent black.
    fn reset(&mut self, width: u32, height: u32);
    /// Applies `cmds` in order, on top of the current bitmap (module docs of `cmd`).
    fn submit(&mut self, cmds: &[Cmd]);
    /// Premultiplied RGBA8 of `(x, y, w, h)`, which lies inside the bitmap.
    fn read(&mut self, x: u32, y: u32, w: u32, h: u32) -> Vec<u8>;
    /// The whole bitmap, premultiplied.
    fn snapshot(&mut self) -> ImageRef;
}

/// What `getContext("2d", options)` asked for.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// `alpha: false` (an opaque canvas). Not honoured yet: advised.
    pub opaque: bool,
    /// `willReadFrequently: true`. A browser keeps such a canvas on the CPU;
    /// a host with a GPU backend should too.
    pub will_read_frequently: bool,
}

/// The embedder's raster backends. Without one, every surface is vello_cpu.
pub trait CanvasHost: Send + Sync {
    /// A backend for a new `width`×`height` bitmap, or `None` for the built-in one.
    fn create(&self, width: u32, height: u32, options: &Options) -> Option<Box<dyn Backend>>;
}

fn host_slot() -> &'static RwLock<Option<Arc<dyn CanvasHost>>> {
    static H: OnceLock<RwLock<Option<Arc<dyn CanvasHost>>>> = OnceLock::new();
    H.get_or_init(|| RwLock::new(None))
}

/// Installs (or, with `None`, removes) the embedder's backends. Read per new
/// surface: existing canvases keep the backend they were made with.
pub fn set_host(host: Option<Arc<dyn CanvasHost>>) {
    *host_slot().write().unwrap() = host;
}

fn new_backend(width: u32, height: u32, options: &Options) -> Box<dyn Backend> {
    let host = host_slot().read().unwrap().clone();
    if let Some(b) = host.and_then(|h| h.create(width, height, options)) {
        return b;
    }
    Box::new(super::cpu::CpuBackend::new(width, height))
}

/// The largest bitmap a canvas gets (Chromium's limits: 32767 a side,
/// 2^28 pixels). Past it a canvas is 0×0 — draws do nothing, `toDataURL` is
/// `"data:,"` — and says so once.
pub const MAX_SIDE: u32 = 32767;
pub const MAX_AREA: u64 = 1 << 28;

/// Queue length that forces a flush, so an unread canvas cannot grow without bound.
pub const FLUSH_AT: usize = 2048;

pub struct Surface {
    backend: Box<dyn Backend>,
    width: u32,
    height: u32,
    pending: Vec<Cmd>,
    /// Bumped by every change; what a presenter compares.
    generation: u64,
    snap: Option<(u64, ImageRef)>,
    /// HTML's origin-clean flag: false once a cross-origin image was drawn.
    pub origin_clean: bool,
    pub options: Options,
}

pub type SurfaceRef = Arc<Mutex<Surface>>;

/// `width`×`height` within the limits, or 0×0.
pub fn clamp_size(width: u32, height: u32) -> (u32, u32) {
    if width > MAX_SIDE || height > MAX_SIDE || width as u64 * height as u64 > MAX_AREA {
        super::advise(
            "canvas:size",
            format!(
                "canvas: a {width}x{height} bitmap is past the {MAX_SIDE}px / 2^28-pixel limit; \
                 the canvas is 0x0 instead (draws do nothing, toDataURL() is \"data:,\"), as in Chromium"
            ),
        );
        return (0, 0);
    }
    (width, height)
}

impl Surface {
    pub fn new(width: u32, height: u32, options: Options) -> Surface {
        let (width, height) = clamp_size(width, height);
        Surface {
            backend: new_backend(width, height, &options),
            width,
            height,
            pending: Vec::new(),
            generation: 1,
            snap: None,
            origin_clean: true,
            options,
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn backend_name(&self) -> &str {
        self.backend.name()
    }

    /// Records one command.
    pub fn push(&mut self, cmd: Cmd) {
        if self.width == 0 || self.height == 0 {
            return;
        }
        if cmd.covers_all(self.width, self.height) {
            self.pending.clear();
        }
        self.pending.push(cmd);
        self.generation += 1;
        if self.pending.len() >= FLUSH_AT {
            self.flush();
        }
    }

    /// Hands every queued command to the backend.
    pub fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let cmds = std::mem::take(&mut self.pending);
        self.backend.submit(&cmds);
    }

    /// Setting `width`/`height`: a new, transparent bitmap.
    pub fn reset(&mut self, width: u32, height: u32) {
        let (width, height) = clamp_size(width, height);
        self.pending.clear();
        self.width = width;
        self.height = height;
        self.backend.reset(width, height);
        self.generation += 1;
        self.snap = None;
        self.origin_clean = true;
    }

    /// Straight RGBA of `(x, y, w, h)`, which may reach outside the bitmap
    /// (those pixels are transparent black, as `getImageData` has them).
    pub fn read_straight(&mut self, x: i64, y: i64, w: u32, h: u32) -> Vec<u8> {
        self.flush();
        let mut out = vec![0u8; w as usize * h as usize * 4];
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w as i64).min(self.width as i64);
        let y1 = (y + h as i64).min(self.height as i64);
        if x0 < x1 && y0 < y1 {
            let (iw, ih) = ((x1 - x0) as u32, (y1 - y0) as u32);
            let inner = self.backend.read(x0 as u32, y0 as u32, iw, ih);
            for row in 0..ih as usize {
                let src = row * iw as usize * 4;
                let dst = ((y0 - y) as usize + row) * w as usize * 4 + (x0 - x) as usize * 4;
                out[dst..dst + iw as usize * 4].copy_from_slice(&inner[src..src + iw as usize * 4]);
            }
        }
        unpremultiply(&mut out);
        out
    }

    /// The whole bitmap as an image (premultiplied), shared until it changes.
    /// Its `origin_clean` is the backend's; the surface's own flag is
    /// [`Surface::origin_clean`].
    pub fn snapshot(&mut self) -> ImageRef {
        if let Some((generation, img)) = &self.snap {
            if *generation == self.generation {
                return img.clone();
            }
        }
        self.flush();
        let img = if self.width == 0 || self.height == 0 {
            Arc::new(Image::from_premul(0, 0, Vec::new(), true))
        } else {
            self.backend.snapshot()
        };
        self.snap = Some((self.generation, img.clone()));
        img
    }

    /// The ONE check point for HTML's origin-clean rule: a readback of a
    /// canvas that drew a cross-origin image throws `SecurityError`.
    ///
    /// NOT enforced, and advised instead: an embedder whose pages live on a
    /// private origin (vue-native's `app://`) sees every https image as
    /// cross-origin, so enforcing it would break any page that crops an
    /// avatar it fetched. Enforcing it is changing the `Ok` below.
    pub fn readable(&self) -> Result<(), &'static str> {
        if !self.origin_clean {
            super::advise(
                "canvas:taint",
                "canvas: this canvas drew a cross-origin image fetched without CORS; a browser \
                 would now refuse getImageData/toDataURL/toBlob/convertToBlob with a SecurityError. \
                 This runtime does not taint canvases: the readback goes ahead"
                    .to_string(),
            );
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Presentation: how an embedder paints an on-screen canvas
// ---------------------------------------------------------------------------

fn presented() -> &'static Mutex<HashMap<u64, Weak<Mutex<Surface>>>> {
    static P: OnceLock<Mutex<HashMap<u64, Weak<Mutex<Surface>>>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Publishes `surface` under the embedder's `key` (vue-native: the host node
/// id of the `<canvas>`). A later registration under the same key wins.
pub fn register_present(key: u64, surface: &SurfaceRef) {
    presented().lock().unwrap().insert(key, Arc::downgrade(surface));
}

pub fn unregister_present(key: u64) {
    presented().lock().unwrap().remove(&key);
}

/// Unregisters `key` only while it still names `surface` (a newer canvas may
/// have taken the key since).
pub(crate) fn unregister_present_if(key: u64, surface: &SurfaceRef) {
    let mut p = presented().lock().unwrap();
    if p.get(&key).map(|w| std::ptr::eq(w.as_ptr(), Arc::as_ptr(surface))).unwrap_or(false) {
        p.remove(&key);
    }
}

/// The bitmap published under `key`, flushed, when it changed after
/// `since` (a generation this returned before; 0 for "any"): `(generation,
/// pixels)`. `None` when nothing is published there or nothing changed.
pub fn present(key: u64, since: u64) -> Option<(u64, ImageRef)> {
    let weak = presented().lock().unwrap().get(&key).cloned()?;
    let Some(surface) = weak.upgrade() else {
        presented().lock().unwrap().remove(&key);
        return None;
    };
    let mut s = surface.lock().unwrap();
    if s.generation() == since {
        return None;
    }
    let img = s.snapshot();
    Some((s.generation(), img))
}

/// The size published under `key`: what an embedder lays out as the
/// canvas's intrinsic size.
pub fn present_size(key: u64) -> Option<(u32, u32)> {
    let weak = presented().lock().unwrap().get(&key).cloned()?;
    let s = weak.upgrade()?;
    let s = s.lock().unwrap();
    Some((s.width(), s.height()))
}
