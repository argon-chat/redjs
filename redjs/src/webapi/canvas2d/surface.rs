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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

use super::cmd::{unpremultiply, Cmd, Image, ImageRef};

/// Generations are unique across every surface of the process, so a
/// presenter's `since` can never match a different surface that took the
/// same key.
fn next_generation() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Set when a presented surface changes; see [`take_presented_changed`].
static PRESENTED_CHANGED: AtomicBool = AtomicBool::new(false);

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
    /// Moved by every change, to a number no other surface has had; what a
    /// presenter compares.
    generation: u64,
    snap: Option<(u64, ImageRef)>,
    /// Published with [`register_present`]: a change sets
    /// [`PRESENTED_CHANGED`].
    presented: bool,
    /// The bitmap is known to be fully transparent: new, resized, or last
    /// cleared whole. Clearing it whole again changes no pixel, so it is not a
    /// change (see [`Surface::push`]).
    blank: bool,
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
            generation: next_generation(),
            snap: None,
            presented: false,
            blank: true,
            origin_clean: true,
            options,
        }
    }

    /// The bitmap changed (or is about to, once the queue is flushed).
    fn bump(&mut self) {
        self.generation = next_generation();
        if self.presented {
            PRESENTED_CHANGED.store(true, Ordering::Release);
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
    ///
    /// A whole-bitmap `clearRect` on a bitmap that is already transparent is
    /// dropped, and the generation does not move: not one pixel changes, so a
    /// presenter has nothing to repaint and a snapshot stays valid. The case is
    /// the animation loop that clears and then draws nothing (a source with no
    /// frame yet): vue-native redrew the whole window for it at the display rate.
    pub fn push(&mut self, cmd: Cmd) {
        if self.width == 0 || self.height == 0 {
            return;
        }
        let covers_all = cmd.covers_all(self.width, self.height);
        let clears_all = covers_all && matches!(cmd, Cmd::Clear { .. });
        if clears_all && self.blank {
            return;
        }
        if covers_all {
            self.pending.clear();
        }
        self.pending.push(cmd);
        self.blank = clears_all;
        self.bump();
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
        self.blank = true;
        self.bump();
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

/// Marks `surface` presented or not; either way the embedder has something
/// new to paint. The caller holds no lock (the registry's is always taken
/// before a surface's, never inside one).
fn set_presented(surface: &Mutex<Surface>, on: bool) {
    surface.lock().unwrap().presented = on;
    PRESENTED_CHANGED.store(true, Ordering::Release);
}

/// Publishes `surface` under the embedder's `key` (vue-native: the host node
/// id of the `<canvas>`). A later registration under the same key wins.
pub fn register_present(key: u64, surface: &SurfaceRef) {
    let old = presented().lock().unwrap().insert(key, Arc::downgrade(surface));
    if let Some(old) = old.and_then(|w| w.upgrade()) {
        if !Arc::ptr_eq(&old, surface) {
            set_presented(&old, false);
        }
    }
    set_presented(surface, true);
}

pub fn unregister_present(key: u64) {
    let old = presented().lock().unwrap().remove(&key);
    if let Some(old) = old.and_then(|w| w.upgrade()) {
        set_presented(&old, false);
    }
}

/// Unregisters `key` only while it still names `surface` (a newer canvas may
/// have taken the key since).
pub(crate) fn unregister_present_if(key: u64, surface: &SurfaceRef) {
    let removed = {
        let mut p = presented().lock().unwrap();
        let ours = p.get(&key).map(|w| std::ptr::eq(w.as_ptr(), Arc::as_ptr(surface))).unwrap_or(false);
        ours && p.remove(&key).is_some()
    };
    if removed {
        set_presented(surface, false);
    }
}

/// True once after any presented surface changed — drew, was resized, or was
/// published or withdrawn — since the last call. What an embedder's frame
/// loop polls to decide that a canvas needs a repaint: one atomic swap a
/// frame, whatever the number of canvases, and nothing for an offscreen one.
pub fn take_presented_changed() -> bool {
    PRESENTED_CHANGED.swap(false, Ordering::AcqRel)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn put(rgba: [u8; 4]) -> Cmd {
        Cmd::Put { x: 0, y: 0, image: Arc::new(Image::from_premul(1, 1, rgba.to_vec(), true)) }
    }

    /// The presenter's protocol, as vue-native's host drives it: a frame that
    /// asks again without a change gets nothing, a draw on a presented surface
    /// raises the flag once, and an offscreen one never does.
    #[test]
    fn a_presenter_sees_each_change_once() {
        // Only this test reads the flag in this binary; the key is out of the
        // way of anything else that registers.
        let key = u64::MAX - 7;
        let shown: SurfaceRef = Arc::new(Mutex::new(Surface::new(2, 1, Options::default())));
        register_present(key, &shown);
        assert!(take_presented_changed(), "publishing is a change");
        assert!(!take_presented_changed());

        let (g1, img) = present(key, 0).expect("published");
        assert_eq!((img.width, img.height), (2, 1));
        assert!(present(key, g1).is_none(), "nothing moved since g1");
        assert_eq!(present_size(key), Some((2, 1)));

        let hidden = Arc::new(Mutex::new(Surface::new(2, 1, Options::default())));
        hidden.lock().unwrap().push(put([9, 9, 9, 255]));
        assert!(!take_presented_changed(), "an offscreen surface never asks for a frame");

        shown.lock().unwrap().push(put([0, 0, 255, 255]));
        shown.lock().unwrap().push(put([255, 0, 0, 255]));
        assert!(take_presented_changed());
        assert!(!take_presented_changed(), "two draws, one frame");
        let (g2, img) = present(key, g1).expect("moved");
        assert!(g2 != g1);
        assert_eq!(&img.data[..4], &[255, 0, 0, 255]);

        // A new surface under the same key never matches the old one's
        // generation, even if its own count happened to line up.
        let next: SurfaceRef = Arc::new(Mutex::new(Surface::new(1, 1, Options::default())));
        register_present(key, &next);
        assert!(take_presented_changed());
        assert!(present(key, g2).is_some());
        shown.lock().unwrap().push(put([1, 1, 1, 255]));
        assert!(!take_presented_changed(), "the replaced surface is not presented any more");

        unregister_present_if(key, &shown);
        assert_eq!(present_size(key), Some((1, 1)), "only the surface the key names is withdrawn");
        unregister_present(key);
        assert!(take_presented_changed(), "withdrawing is a change");
        assert!(present(key, 0).is_none() && present_size(key).is_none());
    }

    /// Clearing a transparent bitmap whole is not a change; anything else is,
    /// including the first clear after a draw, a partial clear, and a resize.
    #[test]
    fn clearing_a_blank_bitmap_is_not_a_change() {
        let clear = |w: f64, h: f64| Cmd::Clear {
            rect: kurbo::Rect::new(0.0, 0.0, w, h),
            transform: kurbo::Affine::IDENTITY,
            clip: None,
        };
        let mut s = Surface::new(4, 4, Options::default());
        let g0 = s.generation();
        s.push(clear(4.0, 4.0));
        assert_eq!(s.generation(), g0, "a new bitmap is already clear");

        s.push(put([255, 0, 0, 255]));
        let g1 = s.generation();
        assert_ne!(g1, g0);
        s.push(clear(4.0, 4.0));
        let g2 = s.generation();
        assert_ne!(g2, g1, "the clear after a draw is a change");
        s.push(clear(4.0, 4.0));
        assert_eq!(s.generation(), g2, "…the one after that is not");
        assert_eq!(&s.snapshot().data[..4], &[0, 0, 0, 0]);

        s.push(clear(2.0, 2.0));
        assert_ne!(s.generation(), g2, "a partial clear does not prove the bitmap blank");
        s.push(clear(4.0, 4.0));
        let g3 = s.generation();
        s.reset(4, 4);
        assert_ne!(s.generation(), g3, "a resize is a change, as it always was");
        let g4 = s.generation();
        s.push(clear(4.0, 4.0));
        assert_eq!(s.generation(), g4, "and leaves the bitmap blank");
    }
}
