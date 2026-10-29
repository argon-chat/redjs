//! The built-in raster backend: vello_cpu.
//!
//! A canvas keeps its pixels between draws; a vello scene is rendered whole.
//! A batch whose every draw is plain `source-over` renders straight onto the
//! bitmap (`TargetInit::SrcOver`). Any other batch makes the current bitmap
//! the scene's first image and renders the lot into a fresh bitmap: vello
//! isolates its root, so a `destination-in` or a `multiply` needs the old
//! pixels INSIDE the scene to have a backdrop at all. A vello_gpu backend
//! replays the same stream the same way.
//!
//! `putImageData` and an axis-aligned, pixel-aligned, unclipped `clearRect`
//! write the bitmap directly, between renders.

use std::collections::HashMap;
use std::sync::Arc;

use kurbo::{Affine, Rect};
use peniko::{BlendMode, Color, Compose, Extend, Fill, ImageQuality, ImageSampler, InterpolationAlphaSpace, Mix};
use vello_cpu::{
    Image as VImage, ImageId, ImageSource, PaintType, PixelMetadata, Pixmap, RasterizerSettings, RenderContext,
    Resources, TargetInit,
};

use super::cmd::{Clip, Cmd, CompositeOp, Draw, GradientKind, Image, ImageRef, Paint, Quality};
use super::surface::Backend;

pub struct CpuBackend {
    width: u32,
    height: u32,
    pix: Pixmap,
    ctx: Option<RenderContext>,
    res: Resources,
}

fn pixmap_of(img: &Image) -> Pixmap {
    Pixmap::from_parts(
        img.data.clone(),
        img.width as u16,
        img.height as u16,
        PixelMetadata::new(peniko::ImageAlphaType::AlphaPremultiplied, true),
    )
}

/// The images a batch registered, by `Image::id` (and crop).
type Registered = HashMap<(u64, [u32; 4]), ImageId>;

impl CpuBackend {
    pub fn new(width: u32, height: u32) -> CpuBackend {
        CpuBackend { width, height, pix: Pixmap::new(width as u16, height as u16), ctx: None, res: Resources::new() }
    }

    fn empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    fn draw(&mut self, cmds: &[Cmd]) {
        if cmds.is_empty() || self.empty() {
            return;
        }
        let (w, h) = (self.width as u16, self.height as u16);
        let plain = cmds.iter().all(is_plain);
        let mut ctx = match self.ctx.take() {
            Some(mut c) if c.width() == w && c.height() == h => {
                c.reset();
                c
            }
            _ => RenderContext::new(w, h),
        };
        let mut reg = Registered::new();
        let mut backdrop = None;
        if !plain {
            let prev = std::mem::replace(&mut self.pix, Pixmap::new(0, 0));
            let id = self.res.register_image(Arc::new(prev));
            ctx.set_transform(Affine::IDENTITY);
            ctx.set_paint_transform(Affine::IDENTITY);
            ctx.set_paint(VImage { image: ImageSource::opaque_id(id), sampler: ImageSampler::new().with_quality(ImageQuality::Low) });
            ctx.fill_rect(&Rect::new(0.0, 0.0, w as f64, h as f64));
            backdrop = Some(id);
        }
        for c in cmds {
            self.encode(&mut ctx, c, &mut reg);
        }
        ctx.flush();
        if plain {
            let settings = RasterizerSettings { target_init: TargetInit::SrcOver, ..Default::default() };
            ctx.render_with(&mut self.pix, &mut self.res, settings);
        } else {
            let mut out = Pixmap::new(w, h);
            ctx.render_with(&mut out, &mut self.res, RasterizerSettings::default());
            self.pix = out;
        }
        if let Some(id) = backdrop {
            self.res.destroy_image(id);
        }
        for id in reg.into_values() {
            self.res.destroy_image(id);
        }
        self.ctx = Some(ctx);
    }

    fn image_id(&mut self, reg: &mut Registered, img: &ImageRef, crop: [u32; 4]) -> ImageId {
        let key = (img.id, crop);
        if let Some(id) = reg.get(&key) {
            return *id;
        }
        let pix = if crop == [0, 0, img.width, img.height] {
            pixmap_of(img)
        } else {
            pixmap_of(&img.crop(crop[0], crop[1], crop[2], crop[3]))
        };
        let id = self.res.register_image(Arc::new(pix));
        reg.insert(key, id);
        id
    }

    /// Sets `paint` (user space: the draw's transform applies to it), with `alpha` folded in.
    fn set_paint(&mut self, ctx: &mut RenderContext, paint: &Paint, alpha: f32, reg: &mut Registered) {
        ctx.set_paint_transform(Affine::IDENTITY);
        match paint {
            Paint::Solid(c) => ctx.set_paint(color(*c, alpha)),
            Paint::Gradient(g) => {
                let stops: Vec<peniko::ColorStop> = g.stops.iter().map(|(o, c)| (*o, color(*c, 1.0)).into()).collect();
                let pg = match g.kind {
                    GradientKind::Linear { x0, y0, x1, y1 } => peniko::Gradient::new_linear((x0, y0), (x1, y1)),
                    GradientKind::Radial { x0, y0, r0, x1, y1, r1 } => {
                        peniko::Gradient::new_two_point_radial((x0, y0), r0 as f32, (x1, y1), r1 as f32)
                    }
                    GradientKind::Conic { angle, x, y } => {
                        peniko::Gradient::new_sweep((x, y), angle as f32, (angle + std::f64::consts::TAU) as f32)
                    }
                };
                let pg = pg
                    .with_interpolation_alpha_space(InterpolationAlphaSpace::Unpremultiplied)
                    .with_stops(stops.as_slice())
                    .multiply_alpha(alpha);
                ctx.set_paint(PaintType::Gradient(pg));
            }
            Paint::Pattern(p) => {
                let id = self.image_id(reg, &p.image, [0, 0, p.image.width, p.image.height]);
                let sampler = ImageSampler::new()
                    .with_x_extend(if p.repeat_x { Extend::Repeat } else { Extend::Pad })
                    .with_y_extend(if p.repeat_y { Extend::Repeat } else { Extend::Pad })
                    .with_quality(ImageQuality::Medium)
                    .with_alpha(alpha);
                ctx.set_paint(VImage { image: ImageSource::opaque_id(id), sampler });
                ctx.set_paint_transform(p.transform);
            }
        }
    }

    /// A no-repeat pattern paints its image's rect only.
    fn pattern_clip(ctx: &mut RenderContext, paint: &Paint, transform: Affine) -> bool {
        let Paint::Pattern(p) = paint else { return false };
        if p.repeat_x && p.repeat_y {
            return false;
        }
        const FAR: f64 = 1.0e7;
        let (x0, x1) = if p.repeat_x { (-FAR, FAR) } else { (0.0, p.image.width as f64) };
        let (y0, y1) = if p.repeat_y { (-FAR, FAR) } else { (0.0, p.image.height as f64) };
        ctx.set_transform(transform * p.transform);
        ctx.set_fill_rule(Fill::NonZero);
        ctx.push_clip_path(&kurbo::Shape::to_path(&Rect::new(x0, y0, x1, y1), 0.1));
        true
    }

    fn encode(&mut self, ctx: &mut RenderContext, c: &Cmd, reg: &mut Registered) {
        match c {
            Cmd::Fill { path, even_odd, transform, paint, draw } => {
                let depth = begin(ctx, draw);
                let pclip = Self::pattern_clip(ctx, paint, *transform);
                self.set_paint(ctx, paint, draw.alpha, reg);
                ctx.set_transform(*transform);
                ctx.set_fill_rule(if *even_odd { Fill::EvenOdd } else { Fill::NonZero });
                ctx.fill_path(path);
                if pclip {
                    ctx.pop_clip();
                }
                end(ctx, draw, depth);
            }
            Cmd::Stroke { path, style, transform, paint, draw } => {
                let depth = begin(ctx, draw);
                let pclip = Self::pattern_clip(ctx, paint, *transform);
                self.set_paint(ctx, paint, draw.alpha, reg);
                ctx.set_transform(*transform);
                ctx.set_stroke(style.clone());
                ctx.stroke_path(path);
                if pclip {
                    ctx.pop_clip();
                }
                end(ctx, draw, depth);
            }
            Cmd::Image { image, src, dst, transform, quality, draw } => {
                if image.width == 0 || image.height == 0 || src.width() <= 0.0 || src.height() <= 0.0 {
                    return;
                }
                // Sample only the source rect: crop to its pixel bounds, so a
                // smoothing filter never reads a neighbour from outside it.
                let cx0 = src.x0.floor().max(0.0) as u32;
                let cy0 = src.y0.floor().max(0.0) as u32;
                let cx1 = (src.x1.ceil() as u32).min(image.width);
                let cy1 = (src.y1.ceil() as u32).min(image.height);
                if cx1 <= cx0 || cy1 <= cy0 {
                    return;
                }
                let id = self.image_id(reg, image, [cx0, cy0, cx1 - cx0, cy1 - cy0]);
                let sx = dst.width() / src.width();
                let sy = dst.height() / src.height();
                let paint_xf = Affine::translate((dst.x0, dst.y0))
                    * Affine::scale_non_uniform(sx, sy)
                    * Affine::translate((-(src.x0 - cx0 as f64), -(src.y0 - cy0 as f64)));
                let q = match quality {
                    Quality::Nearest => ImageQuality::Low,
                    Quality::Low | Quality::Medium => ImageQuality::Medium,
                    Quality::High => ImageQuality::High,
                };
                let depth = begin(ctx, draw);
                ctx.set_transform(*transform);
                ctx.set_paint(VImage {
                    image: ImageSource::opaque_id(id),
                    sampler: ImageSampler::new().with_quality(q).with_alpha(draw.alpha),
                });
                ctx.set_paint_transform(paint_xf);
                ctx.fill_rect(dst);
                end(ctx, draw, depth);
            }
            Cmd::Clear { rect, transform, clip } => {
                let depth = push_clips(ctx, clip);
                ctx.set_transform(*transform);
                ctx.set_paint_transform(Affine::IDENTITY);
                ctx.set_paint(Color::BLACK);
                ctx.set_fill_rule(Fill::NonZero);
                ctx.set_blend_mode(BlendMode::new(Mix::Normal, Compose::Clear));
                ctx.fill_rect(rect);
                ctx.set_blend_mode(BlendMode::default());
                pop_clips(ctx, depth);
            }
            Cmd::Put { .. } => unreachable!("putImageData is applied between renders"),
        }
    }

    fn put(&mut self, x: i32, y: i32, image: &Image) {
        let (w, h) = (self.width as i64, self.height as i64);
        let data = self.pix.data_as_u8_slice_mut();
        for row in 0..image.height as i64 {
            let ty = y as i64 + row;
            if ty < 0 || ty >= h {
                continue;
            }
            let sx0 = (-(x as i64)).max(0);
            let sx1 = (w - x as i64).min(image.width as i64);
            if sx1 <= sx0 {
                continue;
            }
            let src = (row * image.width as i64 + sx0) as usize * 4;
            let dst = (ty * w + x as i64 + sx0) as usize * 4;
            let n = (sx1 - sx0) as usize * 4;
            data[dst..dst + n].copy_from_slice(&image.data[src..src + n]);
        }
        self.pix.set_may_have_transparency(true);
    }

    /// An axis-aligned, pixel-aligned, unclipped clear: zero the pixels.
    fn clear_direct(&mut self, c: &Cmd) -> bool {
        let Cmd::Clear { rect, transform, clip: None } = c else { return false };
        let [a, b, cc, d, _, _] = transform.as_coeffs();
        if b != 0.0 || cc != 0.0 || a == 0.0 || d == 0.0 {
            return false;
        }
        let r = transform.transform_rect_bbox(*rect);
        let aligned = |v: f64| v.is_finite() && (v - v.round()).abs() < 1e-9;
        if ![r.x0, r.y0, r.x1, r.y1].iter().all(|v| aligned(*v)) {
            return false;
        }
        let x0 = (r.x0.round() as i64).clamp(0, self.width as i64) as usize;
        let x1 = (r.x1.round() as i64).clamp(0, self.width as i64) as usize;
        let y0 = (r.y0.round() as i64).clamp(0, self.height as i64) as usize;
        let y1 = (r.y1.round() as i64).clamp(0, self.height as i64) as usize;
        let w = self.width as usize;
        let data = self.pix.data_as_u8_slice_mut();
        for y in y0..y1 {
            data[(y * w + x0) * 4..(y * w + x1) * 4].fill(0);
        }
        true
    }
}

/// `source-over` and nothing that needs the old pixels inside the scene.
fn is_plain(c: &Cmd) -> bool {
    match c {
        Cmd::Fill { draw, .. } | Cmd::Stroke { draw, .. } | Cmd::Image { draw, .. } => draw.op == CompositeOp::SourceOver,
        Cmd::Clear { .. } | Cmd::Put { .. } => false,
    }
}

fn color(c: [f32; 4], alpha: f32) -> Color {
    Color::new([c[0], c[1], c[2], c[3] * alpha])
}

/// The canvas clip as NON-isolated clip paths: an isolated clip layer would
/// give an unbounded op inside it a transparent backdrop to act on, and
/// `copy` under a clip would then leave the old pixels standing.
fn push_clips(ctx: &mut RenderContext, clip: &Option<Arc<Clip>>) -> usize {
    let chain = Clip::chain(clip);
    for c in &chain {
        ctx.set_transform(Affine::IDENTITY);
        ctx.set_fill_rule(if c.even_odd { Fill::EvenOdd } else { Fill::NonZero });
        ctx.push_clip_path(&c.path);
    }
    chain.len()
}

fn pop_clips(ctx: &mut RenderContext, depth: usize) {
    for _ in 0..depth {
        ctx.pop_clip();
    }
}

/// Clips, then the composite: a bounded op blends per draw under the clip
/// paths; an unbounded one gets a layer (so it also acts where the shape is
/// not) whose OWN clip is the canvas clip — vello bounds a layer's composite
/// by the layer's clip, not by the clip paths around it.
fn begin(ctx: &mut RenderContext, draw: &Draw) -> usize {
    if !draw.op.is_unbounded() {
        let depth = push_clips(ctx, &draw.clip);
        ctx.set_blend_mode(draw.op.blend_mode());
        return depth;
    }
    let chain = Clip::chain(&draw.clip);
    ctx.set_transform(Affine::IDENTITY);
    match chain.split_last() {
        Some((last, outer)) => {
            if !outer.is_empty() {
                super::advise(
                    "canvas:clip-chain-unbounded",
                    format!(
                        "canvas: '{}' under {} nested clip() regions is bounded by the innermost one only",
                        draw.op.name(),
                        chain.len()
                    ),
                );
            }
            ctx.set_fill_rule(if last.even_odd { Fill::EvenOdd } else { Fill::NonZero });
            ctx.push_layer(Some(&last.path), Some(draw.op.blend_mode()), None, None, None);
        }
        None => ctx.push_layer(None, Some(draw.op.blend_mode()), None, None, None),
    }
    0
}

fn end(ctx: &mut RenderContext, draw: &Draw, depth: usize) {
    if draw.op.is_unbounded() {
        ctx.pop_layer();
    } else {
        ctx.set_blend_mode(BlendMode::default());
    }
    pop_clips(ctx, depth);
}

impl Backend for CpuBackend {
    fn name(&self) -> &str {
        "vello_cpu"
    }

    fn reset(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.pix = Pixmap::new(width as u16, height as u16);
        self.ctx = None;
    }

    fn submit(&mut self, cmds: &[Cmd]) {
        let mut start = 0;
        for (i, c) in cmds.iter().enumerate() {
            match c {
                Cmd::Put { x, y, image } => {
                    self.draw(&cmds[start..i]);
                    self.put(*x, *y, image);
                    start = i + 1;
                }
                Cmd::Clear { .. } => {
                    // Only a direct clear splits the batch; any other stays in it.
                    if matches!(c, Cmd::Clear { clip: None, .. }) {
                        self.draw(&cmds[start..i]);
                        if self.clear_direct(c) {
                            start = i + 1;
                        } else {
                            start = i;
                        }
                    }
                }
                _ => {}
            }
        }
        self.draw(&cmds[start..]);
    }

    fn read(&mut self, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(w as usize * h as usize * 4);
        let data = self.pix.data_as_u8_slice();
        for row in y..y + h {
            let at = (row as usize * self.width as usize + x as usize) * 4;
            out.extend_from_slice(&data[at..at + w as usize * 4]);
        }
        out
    }

    fn snapshot(&mut self) -> ImageRef {
        Arc::new(Image::from_premul(self.width, self.height, self.pix.data_as_u8_slice().to_vec(), true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webapi::canvas2d::cmd::{Draw, Gradient};
    use kurbo::{BezPath, Shape};

    fn draw(op: CompositeOp) -> Draw {
        Draw { op, alpha: 1.0, clip: None }
    }
    fn rect(x: f64, y: f64, w: f64, h: f64) -> BezPath {
        Rect::new(x, y, x + w, y + h).to_path(0.1)
    }
    fn fill(path: BezPath, c: [f32; 4], op: CompositeOp) -> Cmd {
        Cmd::Fill { path, even_odd: false, transform: Affine::IDENTITY, paint: Paint::Solid(c), draw: draw(op) }
    }
    fn px(b: &mut CpuBackend, x: u32, y: u32) -> [u8; 4] {
        let v = b.read(x, y, 1, 1);
        [v[0], v[1], v[2], v[3]]
    }
    const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
    const BLUE: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

    #[test]
    fn source_over_keeps_what_was_drawn_before() {
        let mut b = CpuBackend::new(8, 8);
        b.submit(&[fill(rect(0.0, 0.0, 4.0, 8.0), RED, CompositeOp::SourceOver)]);
        b.submit(&[fill(rect(4.0, 0.0, 4.0, 8.0), BLUE, CompositeOp::SourceOver)]);
        assert_eq!(px(&mut b, 1, 1), [255, 0, 0, 255]);
        assert_eq!(px(&mut b, 6, 1), [0, 0, 255, 255]);
    }

    #[test]
    fn destination_in_clears_outside_the_shape() {
        let mut b = CpuBackend::new(8, 8);
        b.submit(&[fill(rect(0.0, 0.0, 8.0, 8.0), RED, CompositeOp::SourceOver)]);
        b.submit(&[fill(rect(0.0, 0.0, 2.0, 2.0), BLUE, CompositeOp::DestinationIn)]);
        assert_eq!(px(&mut b, 1, 1), [255, 0, 0, 255], "inside: the destination stays");
        assert_eq!(px(&mut b, 5, 5), [0, 0, 0, 0], "outside: unbounded, cleared");
    }

    #[test]
    fn copy_replaces_everything() {
        let mut b = CpuBackend::new(8, 8);
        b.submit(&[fill(rect(0.0, 0.0, 8.0, 8.0), RED, CompositeOp::SourceOver)]);
        b.submit(&[fill(rect(0.0, 0.0, 2.0, 2.0), BLUE, CompositeOp::Copy)]);
        assert_eq!(px(&mut b, 1, 1), [0, 0, 255, 255]);
        assert_eq!(px(&mut b, 5, 5), [0, 0, 0, 0]);
    }

    #[test]
    fn source_atop_tints_only_what_is_there() {
        let mut b = CpuBackend::new(8, 8);
        b.submit(&[
            fill(rect(0.0, 0.0, 4.0, 8.0), RED, CompositeOp::SourceOver),
            fill(rect(0.0, 0.0, 8.0, 8.0), BLUE, CompositeOp::SourceAtop),
        ]);
        assert_eq!(px(&mut b, 1, 1), [0, 0, 255, 255], "tinted");
        assert_eq!(px(&mut b, 6, 1), [0, 0, 0, 0], "nothing there, nothing tinted");
    }

    #[test]
    fn clear_rect_clears_with_and_without_a_transform() {
        let mut b = CpuBackend::new(8, 8);
        b.submit(&[
            fill(rect(0.0, 0.0, 8.0, 8.0), RED, CompositeOp::SourceOver),
            Cmd::Clear { rect: Rect::new(0.0, 0.0, 2.0, 2.0), transform: Affine::IDENTITY, clip: None },
            Cmd::Clear { rect: Rect::new(0.0, 0.0, 1.0, 1.0), transform: Affine::translate((5.5, 5.5)), clip: None },
        ]);
        assert_eq!(px(&mut b, 1, 1), [0, 0, 0, 0]);
        assert_eq!(px(&mut b, 3, 3), [255, 0, 0, 255]);
        let half = px(&mut b, 5, 5);
        assert!(half[3] > 100 && half[3] < 255, "a half-covered pixel is partly cleared: {half:?}");
    }

    #[test]
    fn a_clip_bounds_even_an_unbounded_op() {
        let mut b = CpuBackend::new(8, 8);
        b.submit(&[fill(rect(0.0, 0.0, 8.0, 8.0), RED, CompositeOp::SourceOver)]);
        let clip = Some(Arc::new(Clip { path: rect(0.0, 0.0, 4.0, 8.0), even_odd: false, parent: None }));
        b.submit(&[Cmd::Fill {
            path: rect(0.0, 0.0, 1.0, 1.0),
            even_odd: false,
            transform: Affine::IDENTITY,
            paint: Paint::Solid(BLUE),
            draw: Draw { op: CompositeOp::Copy, alpha: 1.0, clip },
        }]);
        assert_eq!(px(&mut b, 0, 0), [0, 0, 255, 255]);
        assert_eq!(px(&mut b, 2, 5), [0, 0, 0, 0], "inside the clip: copied (cleared)");
        assert_eq!(px(&mut b, 6, 5), [255, 0, 0, 255], "outside the clip: untouched");
    }

    #[test]
    fn put_writes_pixels_as_they_are() {
        let mut b = CpuBackend::new(4, 4);
        let img = Arc::new(Image::from_straight(2, 1, vec![10, 20, 30, 255, 255, 0, 0, 0], true));
        b.submit(&[fill(rect(0.0, 0.0, 4.0, 4.0), BLUE, CompositeOp::SourceOver), Cmd::Put { x: 3, y: 1, image: img }]);
        assert_eq!(px(&mut b, 3, 1), [10, 20, 30, 255]);
        assert_eq!(px(&mut b, 2, 1), [0, 0, 255, 255], "cropped at the edge, nothing left of it moves");
    }

    #[test]
    fn an_image_scales_into_its_destination() {
        let mut b = CpuBackend::new(8, 8);
        let img = Arc::new(Image::from_straight(2, 2, [255, 0, 0, 255].repeat(4), true));
        b.submit(&[Cmd::Image {
            image: img,
            src: Rect::new(0.0, 0.0, 2.0, 2.0),
            dst: Rect::new(0.0, 0.0, 8.0, 4.0),
            transform: Affine::IDENTITY,
            quality: Quality::Nearest,
            draw: draw(CompositeOp::SourceOver),
        }]);
        assert_eq!(px(&mut b, 7, 3), [255, 0, 0, 255]);
        assert_eq!(px(&mut b, 7, 4), [0, 0, 0, 0]);
    }

    #[test]
    fn a_linear_gradient_runs_from_stop_to_stop() {
        let mut b = CpuBackend::new(100, 1);
        let g = Gradient { kind: GradientKind::Linear { x0: 0.0, y0: 0.0, x1: 100.0, y1: 0.0 }, stops: vec![(0.0, RED), (1.0, BLUE)] };
        b.submit(&[Cmd::Fill {
            path: rect(0.0, 0.0, 100.0, 1.0),
            even_odd: false,
            transform: Affine::IDENTITY,
            paint: Paint::Gradient(Arc::new(g)),
            draw: draw(CompositeOp::SourceOver),
        }]);
        let l = px(&mut b, 0, 0);
        let r = px(&mut b, 99, 0);
        assert!(l[0] > 240 && l[2] < 15, "{l:?}");
        assert!(r[2] > 240 && r[0] < 15, "{r:?}");
    }
}
