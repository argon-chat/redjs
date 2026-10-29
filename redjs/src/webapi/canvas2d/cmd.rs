//! The command stream: what a 2D context records, and what every backend
//! replays, in order, on top of the bitmap it already holds.
//!
//! This is the contract between the context (which owns the HTML semantics:
//! state, transforms, the current path, argument rules) and a raster backend
//! (which owns pixels). Nothing in it is specific to one backend: the CPU
//! backend here (vello_cpu) and a GPU one (vello_gpu, installed by an embedder)
//! consume exactly these values. Geometry is kurbo's, colours are straight
//! (non-premultiplied) sRGB, and every image is premultiplied RGBA8 with an id,
//! so a GPU backend can cache its upload.
//!
//! Semantics every backend must honour (HTML §"drawing model"):
//!
//! * a draw's `alpha` multiplies the source before compositing (globalAlpha);
//! * `op` composites the source onto the bitmap. The five ops HTML defines as
//!   unbounded (`source-in`, `source-out`, `destination-in`,
//!   `destination-atop`, `copy`) also clear the destination OUTSIDE the
//!   drawn shape — within the clip only;
//! * `clip` is a chain of paths in DEVICE space, intersected; nothing outside
//!   it changes, whatever the op;
//! * `Clear` sets the covered pixels (inside the clip) to transparent black,
//!   ignoring alpha and op;
//! * `Put` writes pixels as they are, ignoring transform, alpha, op and clip.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use kurbo::{Affine, BezPath, Rect, Stroke};
use peniko::{BlendMode, Compose, Mix};

static NEXT_IMAGE: AtomicU64 = AtomicU64::new(1);

/// Premultiplied RGBA8 pixels, row-major, no padding.
#[derive(Debug)]
pub struct Image {
    /// Unique for the process: a backend may key an upload cache on it.
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    /// HTML's origin-clean flag of the source these pixels came from. Carried,
    /// not enforced: see `Surface::readable`.
    pub origin_clean: bool,
}

pub type ImageRef = Arc<Image>;

impl Image {
    pub fn from_premul(width: u32, height: u32, data: Vec<u8>, origin_clean: bool) -> Image {
        debug_assert_eq!(data.len(), width as usize * height as usize * 4);
        Image { id: NEXT_IMAGE.fetch_add(1, Ordering::Relaxed), width, height, data, origin_clean }
    }

    pub fn from_straight(width: u32, height: u32, mut rgba: Vec<u8>, origin_clean: bool) -> Image {
        premultiply(&mut rgba);
        Image::from_premul(width, height, rgba, origin_clean)
    }

    /// The pixels in `(x, y, w, h)`, which must lie inside the image.
    pub fn crop(&self, x: u32, y: u32, w: u32, h: u32) -> Image {
        let mut out = Vec::with_capacity(w as usize * h as usize * 4);
        for row in y..y + h {
            let at = (row as usize * self.width as usize + x as usize) * 4;
            out.extend_from_slice(&self.data[at..at + w as usize * 4]);
        }
        Image::from_premul(w, h, out, self.origin_clean)
    }
}

/// `c * a / 255`, rounded — the conversion a browser's premultiplied canvas makes.
pub fn premultiply(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a == 255 {
            continue;
        }
        for c in &mut px[..3] {
            *c = ((*c as u32 * a + 127) / 255) as u8;
        }
    }
}

/// The inverse, rounded; a fully transparent pixel reads as transparent black.
pub fn unpremultiply(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a == 255 {
            continue;
        }
        if a == 0 {
            px[..3].fill(0);
            continue;
        }
        for c in &mut px[..3] {
            *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
        }
    }
}

/// `globalCompositeOperation`'s 26 values (Compositing and Blending §9, §10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompositeOp {
    SourceOver,
    SourceIn,
    SourceOut,
    SourceAtop,
    DestinationOver,
    DestinationIn,
    DestinationOut,
    DestinationAtop,
    Lighter,
    Copy,
    Xor,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
    Hue,
    Saturation,
    Color,
    Luminosity,
}

const OPS: [(&str, CompositeOp); 26] = [
    ("source-over", CompositeOp::SourceOver),
    ("source-in", CompositeOp::SourceIn),
    ("source-out", CompositeOp::SourceOut),
    ("source-atop", CompositeOp::SourceAtop),
    ("destination-over", CompositeOp::DestinationOver),
    ("destination-in", CompositeOp::DestinationIn),
    ("destination-out", CompositeOp::DestinationOut),
    ("destination-atop", CompositeOp::DestinationAtop),
    ("lighter", CompositeOp::Lighter),
    ("copy", CompositeOp::Copy),
    ("xor", CompositeOp::Xor),
    ("multiply", CompositeOp::Multiply),
    ("screen", CompositeOp::Screen),
    ("overlay", CompositeOp::Overlay),
    ("darken", CompositeOp::Darken),
    ("lighten", CompositeOp::Lighten),
    ("color-dodge", CompositeOp::ColorDodge),
    ("color-burn", CompositeOp::ColorBurn),
    ("hard-light", CompositeOp::HardLight),
    ("soft-light", CompositeOp::SoftLight),
    ("difference", CompositeOp::Difference),
    ("exclusion", CompositeOp::Exclusion),
    ("hue", CompositeOp::Hue),
    ("saturation", CompositeOp::Saturation),
    ("color", CompositeOp::Color),
    ("luminosity", CompositeOp::Luminosity),
];

impl CompositeOp {
    /// Case-sensitive, as HTML has it: anything else is ignored by the setter.
    pub fn parse(s: &str) -> Option<CompositeOp> {
        OPS.iter().find(|(n, _)| *n == s).map(|(_, op)| *op)
    }

    pub fn name(self) -> &'static str {
        OPS.iter().find(|(_, op)| *op == self).map(|(n, _)| *n).unwrap_or("source-over")
    }

    /// The Porter-Duff operator and separable/non-separable blend as peniko spells them.
    pub fn blend_mode(self) -> BlendMode {
        use CompositeOp as O;
        let compose = match self {
            O::SourceIn => Compose::SrcIn,
            O::SourceOut => Compose::SrcOut,
            O::SourceAtop => Compose::SrcAtop,
            O::DestinationOver => Compose::DestOver,
            O::DestinationIn => Compose::DestIn,
            O::DestinationOut => Compose::DestOut,
            O::DestinationAtop => Compose::DestAtop,
            O::Lighter => Compose::Plus,
            O::Copy => Compose::Copy,
            O::Xor => Compose::Xor,
            _ => Compose::SrcOver,
        };
        let mix = match self {
            O::Multiply => Mix::Multiply,
            O::Screen => Mix::Screen,
            O::Overlay => Mix::Overlay,
            O::Darken => Mix::Darken,
            O::Lighten => Mix::Lighten,
            O::ColorDodge => Mix::ColorDodge,
            O::ColorBurn => Mix::ColorBurn,
            O::HardLight => Mix::HardLight,
            O::SoftLight => Mix::SoftLight,
            O::Difference => Mix::Difference,
            O::Exclusion => Mix::Exclusion,
            O::Hue => Mix::Hue,
            O::Saturation => Mix::Saturation,
            O::Color => Mix::Color,
            O::Luminosity => Mix::Luminosity,
            _ => Mix::Normal,
        };
        BlendMode::new(mix, compose)
    }

    /// HTML: these clear the destination outside the source shape, too.
    pub fn is_unbounded(self) -> bool {
        matches!(
            self,
            CompositeOp::SourceIn
                | CompositeOp::SourceOut
                | CompositeOp::DestinationIn
                | CompositeOp::DestinationAtop
                | CompositeOp::Copy
        )
    }
}

/// Straight-alpha sRGB, each channel 0..=1.
pub type Rgba = [f32; 4];

#[derive(Clone, Debug)]
pub enum GradientKind {
    Linear { x0: f64, y0: f64, x1: f64, y1: f64 },
    Radial { x0: f64, y0: f64, r0: f64, x1: f64, y1: f64, r1: f64 },
    /// `createConicGradient(angle, x, y)`: clockwise from `angle` radians.
    Conic { angle: f64, x: f64, y: f64 },
}

/// A gradient as it was when the draw was recorded (its stops are copied: a
/// later `addColorStop` changes later draws only). In user space.
#[derive(Clone, Debug)]
pub struct Gradient {
    pub kind: GradientKind,
    /// Sorted by offset, stable: equal offsets keep their insertion order.
    pub stops: Vec<(f32, Rgba)>,
}

#[derive(Clone, Debug)]
pub struct Pattern {
    pub image: ImageRef,
    pub repeat_x: bool,
    pub repeat_y: bool,
    /// The pattern's own `setTransform`, applied before the draw's transform.
    pub transform: Affine,
}

#[derive(Clone, Debug)]
pub enum Paint {
    Solid(Rgba),
    Gradient(Arc<Gradient>),
    Pattern(Arc<Pattern>),
}

/// One `clip()`: a path in device space, intersected with its parent's.
#[derive(Debug)]
pub struct Clip {
    pub path: BezPath,
    pub even_odd: bool,
    pub parent: Option<Arc<Clip>>,
}

impl Clip {
    /// Outermost first.
    pub fn chain(clip: &Option<Arc<Clip>>) -> Vec<&Clip> {
        let mut out = Vec::new();
        let mut at = clip.as_deref();
        while let Some(c) = at {
            out.push(c);
            at = c.parent.as_deref();
        }
        out.reverse();
        out
    }
}

/// What every painting command shares.
#[derive(Clone, Debug)]
pub struct Draw {
    pub op: CompositeOp,
    /// `globalAlpha`, 0..=1.
    pub alpha: f32,
    pub clip: Option<Arc<Clip>>,
}

/// `imageSmoothingEnabled` + `imageSmoothingQuality`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    /// Smoothing off: nearest neighbour.
    Nearest,
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug)]
pub enum Cmd {
    /// `path` is in user space; `transform` maps it (and the paint) to device space.
    Fill { path: BezPath, even_odd: bool, transform: Affine, paint: Paint, draw: Draw },
    /// `path` and `style` in user space, stroked there, then transformed.
    Stroke { path: BezPath, style: Stroke, transform: Affine, paint: Paint, draw: Draw },
    /// The `src` rect of `image` (image pixels, inside the image) onto the
    /// `dst` rect (user space), then `transform`.
    Image { image: ImageRef, src: Rect, dst: Rect, transform: Affine, quality: Quality, draw: Draw },
    /// `clearRect`: `rect` in user space, then `transform`; honours `clip` only.
    Clear { rect: Rect, transform: Affine, clip: Option<Arc<Clip>> },
    /// `putImageData`: `image` written at device pixel (x, y), cropped to the bitmap.
    Put { x: i32, y: i32, image: ImageRef },
}

impl Cmd {
    /// True when this command overwrites every pixel of a `w`×`h` bitmap with
    /// something that does not depend on what was there: every earlier command
    /// is then dead. Conservative.
    pub fn covers_all(&self, w: u32, h: u32) -> bool {
        let full = Rect::new(0.0, 0.0, w as f64, h as f64);
        match self {
            Cmd::Clear { rect, transform, clip: None } => covers(rect, transform, full),
            Cmd::Put { x, y, image } => {
                *x <= 0 && *y <= 0 && *x as i64 + image.width as i64 >= w as i64 && *y as i64 + image.height as i64 >= h as i64
            }
            _ => false,
        }
    }
}

/// `rect` under an axis-aligned `transform` contains `full`.
fn covers(rect: &Rect, transform: &Affine, full: Rect) -> bool {
    let [a, b, c, d, _, _] = transform.as_coeffs();
    if b != 0.0 || c != 0.0 || a <= 0.0 || d <= 0.0 {
        return false;
    }
    let r = transform.transform_rect_bbox(*rect);
    r.x0 <= full.x0 && r.y0 <= full.y0 && r.x1 >= full.x1 && r.y1 >= full.y1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premultiply_round_trips_opaque_and_clears_transparent() {
        let mut px = vec![200, 100, 50, 255, 200, 100, 50, 128, 9, 9, 9, 0];
        premultiply(&mut px);
        assert_eq!(&px[..4], &[200, 100, 50, 255]);
        assert_eq!(&px[4..8], &[100, 50, 25, 128]);
        unpremultiply(&mut px);
        assert_eq!(&px[..4], &[200, 100, 50, 255]);
        assert_eq!(&px[4..8], &[199, 100, 50, 128]);
        assert_eq!(&px[8..], &[0, 0, 0, 0]);
    }

    #[test]
    fn every_op_name_round_trips_and_only_five_are_unbounded() {
        for (name, op) in OPS {
            assert_eq!(CompositeOp::parse(name), Some(op));
            assert_eq!(op.name(), name);
        }
        assert_eq!(CompositeOp::parse("Source-Over"), None);
        assert_eq!(OPS.iter().filter(|(_, op)| op.is_unbounded()).count(), 5);
    }

    #[test]
    fn a_full_clear_covers_and_a_rotated_one_does_not() {
        let clear = |t: Affine| Cmd::Clear { rect: Rect::new(0.0, 0.0, 10.0, 10.0), transform: t, clip: None };
        assert!(clear(Affine::IDENTITY).covers_all(10, 10));
        assert!(!clear(Affine::IDENTITY).covers_all(11, 10));
        assert!(clear(Affine::scale(2.0)).covers_all(20, 20));
        assert!(!clear(Affine::rotate(0.1)).covers_all(5, 5));
    }
}
