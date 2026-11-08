//! `CanvasRenderingContext2D`, `OffscreenCanvasRenderingContext2D`,
//! `CanvasGradient`, `CanvasPattern`, `TextMetrics` and `OffscreenCanvas`:
//! the JS half. State, the current path and every argument rule of HTML's
//! 2D context live here; pixels live behind [`Surface`].
//!
//! Coordinates: the current default path is kept in DEVICE space (HTML:
//! points are transformed when they are added), and a fill or stroke
//! records it back in user space under the transform current at that call,
//! which is what makes a stroke's width and a gradient follow the transform
//! of the moment they are painted with.
//!
//! An embedder's `<canvas>` element (vue-native's `canvas.ts`) gets its
//! context from `__rjs_canvas_context(canvas, w, h, options, key)` and drives
//! it through the other hidden `__rjs_canvas_*` natives; `key` publishes the
//! bitmap for presentation ([`super::present`]).

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use kurbo::{Affine, BezPath, Cap, Join, Point, Rect, Shape, Stroke};

use super::cmd::{Clip, Cmd, CompositeOp, Draw, Gradient, GradientKind, Image, ImageRef, Paint, Pattern, Quality};
use super::color::{self, Parsed};
use super::surface::{register_present, Options, Surface, SurfaceRef};
use super::super::path2d::{self, Path, Seg};
use super::super::util::*;
use crate::{capi, glue};

// ---------------------------------------------------------------------------
// Per-context classes
// ---------------------------------------------------------------------------

struct Classes {
    ctx_cls: capi::JSClassRef,
    ctx2d: Class,
    offctx: Class,
    grad_cls: capi::JSClassRef,
    grad: Class,
    pat_cls: capi::JSClassRef,
    pat: Class,
    off_cls: capi::JSClassRef,
    off: Class,
    metrics: Class,
    /// context → its canvas (element or OffscreenCanvas)
    s_canvas: *mut c_void,
    /// context → Array of the gradient/pattern objects per save level
    s_styles: *mut c_void,
    /// OffscreenCanvas → its context
    s_ctx: *mut c_void,
}
unsafe impl Send for Classes {}
unsafe impl Sync for Classes {}

fn registry() -> &'static Mutex<HashMap<usize, &'static Classes>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static Classes>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> &'static Classes {
    registry().lock().unwrap().get(&(g as usize)).copied().expect("webapi::canvas2d not installed")
}

fn classes_opt(g: *mut c_void) -> Option<&'static Classes> {
    registry().lock().unwrap().get(&(g as usize)).copied()
}

/// The embedder's source hook, per context (protected function).
fn hooks() -> &'static Mutex<HashMap<usize, u64>> {
    static H: OnceLock<Mutex<HashMap<usize, u64>>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn forget(gone: crate::context::Gone) {
    registry().lock().unwrap().remove(&gone.key);
    hooks().lock().unwrap().remove(&gone.key);
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Style {
    Color([u8; 4]),
    Gradient(Arc<Mutex<Gradient>>),
    Pattern(Arc<Mutex<Pattern>>),
}

#[derive(Clone)]
struct State {
    xf: Affine,
    fill: Style,
    stroke: Style,
    alpha: f64,
    op: CompositeOp,
    line_width: f64,
    cap: Cap,
    join: Join,
    miter: f64,
    dash: Vec<f64>,
    dash_offset: f64,
    smoothing: bool,
    quality: &'static str,
    clip: Option<Arc<Clip>>,
    shadow_blur: f64,
    shadow_color: [u8; 4],
    shadow_x: f64,
    shadow_y: f64,
    font: String,
    text_align: &'static str,
    text_baseline: &'static str,
    direction: &'static str,
    filter: String,
    letter_spacing: String,
    word_spacing: String,
    font_kerning: &'static str,
    font_stretch: &'static str,
    font_variant_caps: &'static str,
    text_rendering: &'static str,
    lang: String,
}

impl Default for State {
    fn default() -> State {
        State {
            xf: Affine::IDENTITY,
            fill: Style::Color([0, 0, 0, 255]),
            stroke: Style::Color([0, 0, 0, 255]),
            alpha: 1.0,
            op: CompositeOp::SourceOver,
            line_width: 1.0,
            cap: Cap::Butt,
            join: Join::Miter,
            miter: 10.0,
            dash: Vec::new(),
            dash_offset: 0.0,
            smoothing: true,
            quality: "low",
            clip: None,
            shadow_blur: 0.0,
            shadow_color: [0, 0, 0, 0],
            shadow_x: 0.0,
            shadow_y: 0.0,
            font: "10px sans-serif".into(),
            text_align: "start",
            text_baseline: "alphabetic",
            direction: "inherit",
            filter: "none".into(),
            letter_spacing: "0px".into(),
            word_spacing: "0px".into(),
            font_kerning: "auto",
            font_stretch: "normal",
            font_variant_caps: "normal",
            text_rendering: "auto",
            lang: "inherit".into(),
        }
    }
}

/// A context's native data.
pub(crate) struct Ctx {
    pub surface: SurfaceRef,
    st: State,
    stack: Vec<State>,
    /// The current default path, in device space.
    path: Path,
    present_key: Option<u64>,
}

impl Drop for Ctx {
    fn drop(&mut self) {
        if let Some(key) = self.present_key {
            super::surface::unregister_present_if(key, &self.surface);
        }
    }
}

impl Ctx {
    fn pt(&self, x: f64, y: f64) -> (f64, f64) {
        let p = self.st.xf * Point::new(x, y);
        (p.x, p.y)
    }

    fn inverse(&self) -> Option<Affine> {
        let d = self.st.xf.determinant();
        (d != 0.0 && d.is_finite()).then(|| self.st.xf.inverse())
    }

    fn draw(&self) -> Draw {
        Draw { op: self.st.op, alpha: self.st.alpha as f32, clip: self.st.clip.clone() }
    }

    fn quality(&self) -> Quality {
        if !self.st.smoothing {
            return Quality::Nearest;
        }
        match self.st.quality {
            "high" => Quality::High,
            "medium" => Quality::Medium,
            _ => Quality::Low,
        }
    }

    fn stroke_style(&self) -> Stroke {
        let mut s = Stroke::new(self.st.line_width).with_join(self.st.join).with_miter_limit(self.st.miter).with_caps(self.st.cap);
        if !self.st.dash.is_empty() {
            s = s.with_dashes(self.st.dash_offset, self.st.dash.iter().copied());
        }
        s
    }

    fn advise_unpainted(&self) {
        let s = &self.st;
        if s.shadow_color[3] > 0 && (s.shadow_blur > 0.0 || s.shadow_x != 0.0 || s.shadow_y != 0.0) {
            super::advise(
                "canvas:shadow",
                "canvas: shadows (shadowBlur/shadowColor/shadowOffset*) are not drawn yet; the shape is drawn without one".into(),
            );
        }
        if s.filter != "none" {
            super::advise("canvas:filter", format!("canvas: ctx.filter = \"{}\" is not applied yet; drawing unfiltered", s.filter));
        }
    }

    fn push(&self, cmd: Cmd) {
        self.surface.lock().unwrap().push(cmd);
    }

    fn reset_all(&mut self) {
        self.st = State::default();
        self.stack.clear();
        self.path = Path::new();
    }
}

/// A style as the command stream wants it, frozen at this moment.
fn paint_of(style: &Style) -> Paint {
    const NONE: Paint = Paint::Solid([0.0; 4]);
    match style {
        Style::Color(c) => Paint::Solid(color::to_f32(*c)),
        Style::Gradient(g) => {
            let g = g.lock().unwrap().clone();
            let degenerate = match g.kind {
                GradientKind::Linear { x0, y0, x1, y1 } => x0 == x1 && y0 == y1,
                GradientKind::Radial { x0, y0, r0, x1, y1, r1 } => x0 == x1 && y0 == y1 && r0 == r1,
                GradientKind::Conic { .. } => false,
            };
            match g.stops.len() {
                _ if degenerate => NONE,
                0 => NONE,
                1 => Paint::Solid(g.stops[0].1),
                _ => Paint::Gradient(Arc::new(g)),
            }
        }
        Style::Pattern(p) => Paint::Pattern(Arc::new(p.lock().unwrap().clone())),
    }
}

/// A redjs `Path` as kurbo geometry. After a close, the next segment starts
/// at the closed subpath's first point, as HTML has it.
pub(crate) fn bez_of(path: &Path) -> BezPath {
    let mut b = BezPath::new();
    let mut start = (0.0, 0.0);
    let mut reopen = false;
    for s in &path.segs {
        match *s {
            Seg::Move(x, y) => {
                b.move_to((x, y));
                start = (x, y);
                reopen = false;
            }
            Seg::Line(x, y) => {
                if reopen {
                    b.move_to(start);
                    reopen = false;
                }
                b.line_to((x, y));
            }
            Seg::Cubic([c1x, c1y, c2x, c2y, x, y]) => {
                if reopen {
                    b.move_to(start);
                    reopen = false;
                }
                b.curve_to((c1x, c1y), (c2x, c2y), (x, y));
            }
            Seg::Close => {
                b.close_path();
                reopen = true;
            }
        }
    }
    b
}

// ---------------------------------------------------------------------------
// Construction (shared by OffscreenCanvas and the embedder's element)
// ---------------------------------------------------------------------------

pub(crate) unsafe fn new_context(g: *mut c_void, canvas: u64, w: u32, h: u32, options: Options, key: Option<u64>, offscreen: bool) -> u64 {
    let c = classes(g);
    let surface: SurfaceRef = Arc::new(Mutex::new(Surface::new(w, h, options)));
    if options.opaque {
        super::advise(
            "canvas:opaque",
            "canvas: getContext('2d', { alpha: false }) is not honoured yet: the bitmap keeps an alpha channel \
             (it starts transparent, not opaque black)"
                .into(),
        );
    }
    if let Some(k) = key {
        register_present(k, &surface);
    }
    let ctx = Ctx { surface, st: State::default(), stack: Vec::new(), path: Path::new(), present_key: key };
    let proto = if offscreen { c.offctx.proto } else { c.ctx2d.proto };
    let obj = host_create(g, c.ctx_cls, proto, ctx);
    glue::rjs_define(g, obj, c.s_canvas, canvas, 0);
    glue::rjs_define(g, obj, c.s_styles, new_array(g), 0);
    obj
}

pub(crate) unsafe fn ctx_of<'a>(g: *mut c_void, v: u64) -> Option<&'a mut Ctx> {
    let c = classes_opt(g)?;
    host_data::<Ctx>(g, c.ctx_cls, v)
}

unsafe fn read_options(g: *mut c_void, v: u64) -> Result<Options, ()> {
    if !is_cell(v) {
        return Ok(Options::default());
    }
    let alpha = try_get(g, v, "alpha")?;
    let opaque = alpha != UNDEF && glue::rjs_to_boolean(g, alpha) == 0;
    let frequent = try_get(g, v, "willReadFrequently")?;
    Ok(Options {
        opaque,
        will_read_frequently: glue::rjs_to_boolean(g, frequent) != 0,
    })
}

/// Setting a canvas's width or height: a fresh bitmap and a context back at
/// its defaults (HTML "reset the rendering context to its default state").
pub(crate) fn resize(ctx: &mut Ctx, w: u32, h: u32) {
    ctx.surface.lock().unwrap().reset(w, h);
    ctx.reset_all();
}

// ---------------------------------------------------------------------------
// Image sources
// ---------------------------------------------------------------------------

pub(crate) enum Source {
    /// Draw these pixels.
    Pixels(ImageRef, bool),
    /// A valid source with nothing to draw yet (an image still loading).
    Nothing,
}

/// HTML "check the usability of the image argument", and the pixels.
/// `Err(())` = an exception is pending.
pub(crate) unsafe fn image_source(g: *mut c_void, v: u64) -> Result<Source, ()> {
    let st = state(g);
    if let Some(b) = super::super::image::bitmap_of(g, v) {
        if b.width == 0 || b.height == 0 {
            domexception_throw(g, "InvalidStateError", "The image source is detached (a closed ImageBitmap)");
            return Err(());
        }
        let clean = b.origin_clean;
        return Ok(Source::Pixels(Arc::new(Image::from_straight(b.width, b.height, b.rgba.clone(), clean)), clean));
    }
    if is_cell(v) && proto_chain_has(g, v, st.image.proto) {
        let bmp = get_a(g, v, st.s_image);
        if let Some(b) = super::super::image::bitmap_of(g, bmp) {
            if b.width == 0 || b.height == 0 {
                return Ok(Source::Nothing);
            }
            let clean = image_origin_clean(g, v);
            return Ok(Source::Pixels(Arc::new(Image::from_straight(b.width, b.height, b.rgba.clone(), clean)), clean));
        }
        return Ok(Source::Nothing);
    }
    let c = classes(g);
    if let Some(off) = host_data::<Off>(g, c.off_cls, v) {
        if off.width == 0 || off.height == 0 {
            domexception_throw(g, "InvalidStateError", "The canvas image source has a zero width or height");
            return Err(());
        }
        let (width, height, kind) = (off.width, off.height, off.kind);
        let ctx = get_a(g, v, c.s_ctx);
        #[cfg(feature = "webgpu")]
        if kind == 3 {
            return gpu_source(g, ctx);
        }
        let _ = kind;
        return Ok(match ctx_of(g, ctx) {
            Some(cx) => surface_source(&cx.surface),
            None => blank(width, height),
        });
    }
    if let Some(cx) = ctx_of(g, v) {
        return Ok(surface_source(&cx.surface));
    }
    #[cfg(feature = "webgpu")]
    if super::super::webgpu::canvas::is_context(g, v) {
        return gpu_source(g, v);
    }
    let hook = hooks().lock().unwrap().get(&(g as usize)).copied();
    if let Some(f) = hook {
        let got = match call(g, f, UNDEF, &[v]) {
            Ok(r) => r,
            Err(e) => {
                glue::rjs_throw(g, e);
                return Err(());
            }
        };
        if got == NULL {
            return Ok(if let Some((w, h)) = blank_mirror(g, v) {
                blank(w, h)
            } else {
                Source::Nothing
            });
        }
        if got != UNDEF && got != v {
            return image_source(g, got);
        }
    }
    throw_type_error(
        g,
        "The provided value is not of type '(HTMLImageElement or HTMLCanvasElement or ImageBitmap or OffscreenCanvas or HTMLVideoElement)'",
    );
    Err(())
}

#[cfg(feature = "webgpu")]
unsafe fn gpu_source(g: *mut c_void, v: u64) -> Result<Source, ()> {
    match super::super::webgpu::canvas::premul_pixels(g, v) {
        Some(Ok((data, w, h))) => Ok(Source::Pixels(
            Arc::new(Image::from_premul(w, h, data, true)),
            true,
        )),
        Some(Err(e)) => {
            domexception_throw(g, "OperationError", &e);
            Err(())
        }
        None => {
            throw_type_error(g, "Not a GPU canvas source");
            Err(())
        }
    }
}

unsafe fn blank_mirror(g: *mut c_void, v: u64) -> Option<(u32, u32)> {
    if read_string(g, get(g, v, "tagName"))?.as_str() != "CANVAS"
        || dec_f64(get(g, v, "__vnId")).is_none()
    {
        return None;
    }
    let w = dec_f64(get(g, v, "width"))?;
    let h = dec_f64(get(g, v, "height"))?;
    if !w.is_finite()
        || !h.is_finite()
        || w < 0.0
        || h < 0.0
        || w > u32::MAX as f64
        || h > u32::MAX as f64
    {
        return None;
    }
    Some(super::surface::clamp_size(w as u32, h as u32))
}

fn surface_source(s: &SurfaceRef) -> Source {
    let mut s = s.lock().unwrap();
    let clean = s.origin_clean;
    let img = s.snapshot();
    if img.width == 0 || img.height == 0 {
        return Source::Nothing;
    }
    Source::Pixels(img, clean)
}

fn blank(w: u32, h: u32) -> Source {
    Source::Pixels(Arc::new(Image::from_premul(w, h, vec![0; w as usize * h as usize * 4], true)), true)
}

/// An `Image` fetched over http(s) without `crossOrigin` is cross-origin to
/// any page: HTML's origin-clean flag goes false (recorded, not enforced).
unsafe fn image_origin_clean(g: *mut c_void, img: u64) -> bool {
    let src = read_string(g, get(g, img, "__rjs_src")).unwrap_or_default();
    let remote = src.starts_with("http://") || src.starts_with("https://");
    let cors = get(g, img, "crossOrigin");
    !remote || (cors != NULL && cors != UNDEF)
}

unsafe fn domexception_throw(g: *mut c_void, name: &str, msg: &str) {
    super::super::domexception::throw(g, name, msg);
}

/// The canvas pixels behind `v`, straight RGBA, for `createImageBitmap`:
/// an OffscreenCanvas, a context, or what the embedder's hook maps `v` to.
/// `None` when `v` is none of those.
pub(crate) unsafe fn canvas_pixels(g: *mut c_void, v: u64) -> Option<(Vec<u8>, u32, u32)> {
    let c = classes_opt(g)?;
    #[cfg(feature = "webgpu")]
    if let Some(result) = super::super::webgpu::canvas::pixels(g, v) {
        return result.ok();
    }
    let surface = if let Some(off) = host_data::<Off>(g, c.off_cls, v) {
        #[cfg(feature = "webgpu")]
        if off.kind == 3 {
            let ctx = get_a(g, v, c.s_ctx);
            return super::super::webgpu::canvas::pixels(g, ctx)?.ok();
        }
        match ctx_of(g, get_a(g, v, c.s_ctx)) {
            Some(cx) => cx.surface.clone(),
            None => return Some((vec![0; off.width as usize * off.height as usize * 4], off.width, off.height)),
        }
    } else if let Some(cx) = ctx_of(g, v) {
        cx.surface.clone()
    } else {
        let f = hooks().lock().unwrap().get(&(g as usize)).copied()?;
        let got = call(g, f, UNDEF, &[v]).ok()?;
        if got == NULL {
            if let Some((w, h)) = blank_mirror(g, v) {
                return Some((vec![0; w as usize * h as usize * 4], w, h));
            }
        }
        if got == v || !is_cell(got) {
            return None;
        }
        // A context, or an ImageBitmap the embedder made of its own image.
        return super::super::image::source_pixels(g, got);
    };
    let mut s = surface.lock().unwrap();
    let _ = s.readable();
    let (w, h) = (s.width(), s.height());
    Some((s.read_straight(0, 0, w, h), w, h))
}

/// External uploads preserve premultiplied canvas pixels; they do not pass
/// through ImageData or silently mark an origin-unclean bitmap clean.
#[cfg(feature = "webgpu")]
pub(crate) unsafe fn external_pixels(
    g: *mut c_void,
    v: u64,
    depth: u8,
) -> Result<super::super::image::ExternalImage, ()> {
    use super::super::image::external::fail;
    use super::super::image::{external_pixels as resolve, ExternalImage};
    let c = classes(g);
    let mut gpu = v;
    let mut surface = None;
    if let Some(off) = host_data::<Off>(g, c.off_cls, v) {
        let (w, h, kind) = (off.width, off.height, off.kind);
        if w == 0 || h == 0 {
            return fail(g, "InvalidStateError", "Canvas has no pixels");
        }
        let ctx = get_a(g, v, c.s_ctx);
        if kind == 3 {
            gpu = ctx;
        } else if let Some(cx) = ctx_of(g, ctx) {
            surface = Some(cx.surface.clone());
        } else {
            return Ok(ExternalImage {
                width: w,
                height: h,
                rgba: vec![0; w as usize * h as usize * 4],
                premultiplied: true,
                origin_clean: true,
            });
        }
    } else if let Some(cx) = ctx_of(g, v) {
        surface = Some(cx.surface.clone());
    }
    if let Some(surface) = surface {
        let mut surface = surface.lock().unwrap();
        let clean = surface.origin_clean;
        let image = surface.snapshot();
        return Ok(ExternalImage {
            width: image.width,
            height: image.height,
            rgba: image.data.clone(),
            premultiplied: true,
            origin_clean: clean,
        });
    }
    if let Some(result) = super::super::webgpu::canvas::premul_pixels(g, gpu) {
        return match result {
            Ok((rgba, width, height)) if width > 0 && height > 0 => Ok(ExternalImage {
                width,
                height,
                rgba,
                premultiplied: true,
                origin_clean: true,
            }),
            Ok(_) => fail(g, "InvalidStateError", "Canvas has no pixels"),
            Err(e) => fail(g, "OperationError", &e),
        };
    }
    let hook = hooks().lock().unwrap().get(&(g as usize)).copied();
    if let Some(hook) = hook {
        let got = call(g, hook, UNDEF, &[v]).map_err(|e| {
            glue::rjs_throw(g, e);
        })?;
        if got != NULL && got != UNDEF && got != v {
            return resolve(g, got, depth + 1);
        }
        if got == NULL {
            if let Some((w, h)) = blank_mirror(g, v) {
                if w == 0 || h == 0 {
                    return fail(g, "InvalidStateError", "Canvas has no pixels");
                }
                return Ok(ExternalImage {
                    width: w,
                    height: h,
                    rgba: vec![0; w as usize * h as usize * 4],
                    premultiplied: true,
                    origin_clean: true,
                });
            }
            return fail(
                g,
                "InvalidStateError",
                "External image source has no readable frame",
            );
        }
    }
    fail(g, "TypeError", "Unsupported external image source")
}

// ---------------------------------------------------------------------------
// Natives: helpers
// ---------------------------------------------------------------------------

macro_rules! this_ctx {
    ($g:ident, $cf:ident, $what:literal) => {
        match ctx_of($g, cf_this($cf)) {
            Some(c) => c,
            None => {
                return throw_type_error($g, concat!("CanvasRenderingContext2D.", $what, ": illegal invocation"));
            }
        }
    };
}

unsafe fn n(g: *mut c_void, cf: *mut c_void, i: u32) -> f64 {
    arg_num(g, cf, i, f64::NAN)
}

fn finite(vs: &[f64]) -> bool {
    vs.iter().all(|v| v.is_finite())
}

/// ToString of argument 0, or `Err` with the exception pending.
unsafe fn arg_str(g: *mut c_void, cf: *mut c_void, i: u32) -> Result<String, ()> {
    let v = to_js_string(g, cf_arg(cf, i))?;
    Ok(read_string(g, v).unwrap_or_default())
}

unsafe fn styles_slot(g: *mut c_void, this: u64) -> u64 {
    let c = classes(g);
    let arr = get_a(g, this, c.s_styles);
    if is_cell(arr) {
        return arr;
    }
    let arr = new_array(g);
    glue::rjs_define(g, this, c.s_styles, arr, 0);
    arr
}

// ---------------------------------------------------------------------------
// Natives: state
// ---------------------------------------------------------------------------

unsafe extern "sysv64" fn get_canvas(g: *mut c_void, cf: *mut c_void) -> u64 {
    let _ = this_ctx!(g, cf, "canvas");
    get_a(g, cf_this(cf), classes(g).s_canvas)
}

unsafe extern "sysv64" fn m_save(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let arr = styles_slot(g, this);
    let cx = this_ctx!(g, cf, "save");
    let d = cx.stack.len() as u32;
    cx.stack.push(cx.st.clone());
    let (f, s) = (get_i(g, arr, d * 2), get_i(g, arr, d * 2 + 1));
    set_i(g, arr, d * 2 + 2, f);
    set_i(g, arr, d * 2 + 3, s);
    UNDEF
}

unsafe extern "sysv64" fn m_restore(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cx = this_ctx!(g, cf, "restore");
    if let Some(s) = cx.stack.pop() {
        cx.st = s;
    }
    UNDEF
}

unsafe extern "sysv64" fn m_reset(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let cx = this_ctx!(g, cf, "reset");
    let (w, h) = {
        let s = cx.surface.lock().unwrap();
        (s.width(), s.height())
    };
    resize(cx, w, h);
    glue::rjs_define(g, this, classes(g).s_styles, new_array(g), 0);
    UNDEF
}

unsafe extern "sysv64" fn m_is_context_lost(g: *mut c_void, cf: *mut c_void) -> u64 {
    let _ = this_ctx!(g, cf, "isContextLost");
    FALSE
}

unsafe extern "sysv64" fn m_get_context_attributes(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cx = this_ctx!(g, cf, "getContextAttributes");
    let o = cx.surface.lock().unwrap().options;
    let out = new_object(g, UNDEF);
    define(g, out, "alpha", enc_bool(!o.opaque), 0);
    define(g, out, "colorSpace", js_string(g, "srgb"), 0);
    define(g, out, "desynchronized", FALSE, 0);
    define(g, out, "willReadFrequently", enc_bool(o.will_read_frequently), 0);
    out
}

macro_rules! num_attr {
    ($get:ident, $set:ident, $field:ident, $what:literal, |$v:ident| $ok:expr) => {
        unsafe extern "sysv64" fn $get(g: *mut c_void, cf: *mut c_void) -> u64 {
            let cx = this_ctx!(g, cf, $what);
            enc_f64(cx.st.$field as f64)
        }
        unsafe extern "sysv64" fn $set(g: *mut c_void, cf: *mut c_void) -> u64 {
            let Ok($v) = to_number(g, cf_arg(cf, 0)) else { return UNDEF };
            let cx = this_ctx!(g, cf, $what);
            if $v.is_finite() && $ok {
                cx.st.$field = $v;
            }
            UNDEF
        }
    };
}

num_attr!(get_global_alpha, set_global_alpha, alpha, "globalAlpha", |v| (0.0..=1.0).contains(&v));
num_attr!(get_line_width, set_line_width, line_width, "lineWidth", |v| v > 0.0);
num_attr!(get_miter_limit, set_miter_limit, miter, "miterLimit", |v| v > 0.0);
num_attr!(get_line_dash_offset, set_line_dash_offset, dash_offset, "lineDashOffset", |v| true);
num_attr!(get_shadow_blur, set_shadow_blur, shadow_blur, "shadowBlur", |v| v >= 0.0);
num_attr!(get_shadow_x, set_shadow_x, shadow_x, "shadowOffsetX", |v| true);
num_attr!(get_shadow_y, set_shadow_y, shadow_y, "shadowOffsetY", |v| true);

/// An enumerated attribute: the setter ignores anything not in `$values`.
macro_rules! enum_attr {
    ($get:ident, $set:ident, $field:ident, $what:literal, $values:expr) => {
        unsafe extern "sysv64" fn $get(g: *mut c_void, cf: *mut c_void) -> u64 {
            let cx = this_ctx!(g, cf, $what);
            js_string(g, cx.st.$field)
        }
        unsafe extern "sysv64" fn $set(g: *mut c_void, cf: *mut c_void) -> u64 {
            let Ok(s) = arg_str(g, cf, 0) else { return UNDEF };
            let cx = this_ctx!(g, cf, $what);
            let values: &[&'static str] = &$values;
            if let Some(v) = values.iter().find(|v| **v == s) {
                cx.st.$field = v;
            }
            UNDEF
        }
    };
}

enum_attr!(get_quality, set_quality, quality, "imageSmoothingQuality", ["low", "medium", "high"]);
enum_attr!(get_text_align, set_text_align, text_align, "textAlign", ["start", "end", "left", "right", "center"]);
enum_attr!(
    get_text_baseline,
    set_text_baseline,
    text_baseline,
    "textBaseline",
    ["top", "hanging", "middle", "alphabetic", "ideographic", "bottom"]
);
enum_attr!(get_direction, set_direction, direction, "direction", ["ltr", "rtl", "inherit"]);
enum_attr!(get_font_kerning, set_font_kerning, font_kerning, "fontKerning", ["auto", "normal", "none"]);
enum_attr!(
    get_font_stretch,
    set_font_stretch,
    font_stretch,
    "fontStretch",
    ["ultra-condensed", "extra-condensed", "condensed", "semi-condensed", "normal", "semi-expanded", "expanded", "extra-expanded", "ultra-expanded"]
);
enum_attr!(
    get_font_variant_caps,
    set_font_variant_caps,
    font_variant_caps,
    "fontVariantCaps",
    ["normal", "small-caps", "all-small-caps", "petite-caps", "all-petite-caps", "unicase", "titling-caps"]
);
enum_attr!(
    get_text_rendering,
    set_text_rendering,
    text_rendering,
    "textRendering",
    ["auto", "optimizeSpeed", "optimizeLegibility", "geometricPrecision"]
);

macro_rules! string_attr {
    ($get:ident, $set:ident, $field:ident, $what:literal, |$s:ident| $ok:expr) => {
        unsafe extern "sysv64" fn $get(g: *mut c_void, cf: *mut c_void) -> u64 {
            let cx = this_ctx!(g, cf, $what);
            js_string(g, &cx.st.$field)
        }
        unsafe extern "sysv64" fn $set(g: *mut c_void, cf: *mut c_void) -> u64 {
            let Ok($s) = arg_str(g, cf, 0) else { return UNDEF };
            let cx = this_ctx!(g, cf, $what);
            if $ok {
                cx.st.$field = $s.trim().to_string();
            }
            UNDEF
        }
    };
}

/// A CSS length, as `letterSpacing` / `wordSpacing` take it.
fn is_length(s: &str) -> bool {
    let s = s.trim();
    let num_end = s.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+')).unwrap_or(s.len());
    let (num, unit) = s.split_at(num_end);
    num.parse::<f64>().is_ok() && ["px", "em", "rem", "pt", "pc", "in", "cm", "mm", "ex", "ch", "vw", "vh", ""].contains(&unit)
        && !(unit.is_empty() && num.parse::<f64>().ok() != Some(0.0))
}

/// A CSS `font` shorthand: at least a size and a family.
fn is_font(s: &str) -> bool {
    let toks: Vec<&str> = s.split_whitespace().collect();
    toks.iter().enumerate().any(|(i, t)| {
        let size = t.split('/').next().unwrap_or("");
        let unit_ok = ["px", "pt", "em", "rem", "%", "pc", "in", "cm", "mm", "ex", "ch", "vw", "vh"]
            .iter()
            .any(|u| size.strip_suffix(u).map(|n| n.parse::<f64>().is_ok()).unwrap_or(false))
            || ["xx-small", "x-small", "small", "medium", "large", "x-large", "xx-large", "smaller", "larger"].contains(&size);
        unit_ok && i + 1 < toks.len()
    })
}

string_attr!(get_font, set_font, font, "font", |s| is_font(&s));
string_attr!(get_filter, set_filter, filter, "filter", |s| !s.trim().is_empty());
string_attr!(get_letter_spacing, set_letter_spacing, letter_spacing, "letterSpacing", |s| is_length(&s));
string_attr!(get_word_spacing, set_word_spacing, word_spacing, "wordSpacing", |s| is_length(&s));
string_attr!(get_lang, set_lang, lang, "lang", |s| true);

unsafe extern "sysv64" fn get_composite(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cx = this_ctx!(g, cf, "globalCompositeOperation");
    js_string(g, cx.st.op.name())
}

unsafe extern "sysv64" fn set_composite(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Ok(s) = arg_str(g, cf, 0) else { return UNDEF };
    let cx = this_ctx!(g, cf, "globalCompositeOperation");
    if let Some(op) = CompositeOp::parse(&s) {
        cx.st.op = op;
    }
    UNDEF
}

unsafe extern "sysv64" fn get_smoothing(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cx = this_ctx!(g, cf, "imageSmoothingEnabled");
    enc_bool(cx.st.smoothing)
}

unsafe extern "sysv64" fn set_smoothing(g: *mut c_void, cf: *mut c_void) -> u64 {
    let v = truthy_flag(cf_arg(cf, 0));
    let cx = this_ctx!(g, cf, "imageSmoothingEnabled");
    cx.st.smoothing = v;
    UNDEF
}

unsafe extern "sysv64" fn get_line_cap(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cx = this_ctx!(g, cf, "lineCap");
    js_string(g, match cx.st.cap {
        Cap::Butt => "butt",
        Cap::Round => "round",
        Cap::Square => "square",
    })
}

unsafe extern "sysv64" fn set_line_cap(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Ok(s) = arg_str(g, cf, 0) else { return UNDEF };
    let cx = this_ctx!(g, cf, "lineCap");
    match s.as_str() {
        "butt" => cx.st.cap = Cap::Butt,
        "round" => cx.st.cap = Cap::Round,
        "square" => cx.st.cap = Cap::Square,
        _ => {}
    }
    UNDEF
}

unsafe extern "sysv64" fn get_line_join(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cx = this_ctx!(g, cf, "lineJoin");
    js_string(g, match cx.st.join {
        Join::Miter => "miter",
        Join::Round => "round",
        Join::Bevel => "bevel",
    })
}

unsafe extern "sysv64" fn set_line_join(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Ok(s) = arg_str(g, cf, 0) else { return UNDEF };
    let cx = this_ctx!(g, cf, "lineJoin");
    match s.as_str() {
        "miter" => cx.st.join = Join::Miter,
        "round" => cx.st.join = Join::Round,
        "bevel" => cx.st.join = Join::Bevel,
        _ => {}
    }
    UNDEF
}

unsafe extern "sysv64" fn get_shadow_color(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cx = this_ctx!(g, cf, "shadowColor");
    js_string(g, &color::serialize(cx.st.shadow_color))
}

unsafe extern "sysv64" fn set_shadow_color(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Ok(s) = arg_str(g, cf, 0) else { return UNDEF };
    let cx = this_ctx!(g, cf, "shadowColor");
    match color::parse(&s) {
        Some(Parsed::Rgba(c)) => cx.st.shadow_color = c,
        Some(Parsed::CurrentColor) => cx.st.shadow_color = [0, 0, 0, 255],
        None => {}
    }
    UNDEF
}

unsafe fn get_style(g: *mut c_void, cf: *mut c_void, stroke: bool) -> u64 {
    let this = cf_this(cf);
    let arr = styles_slot(g, this);
    let cx = this_ctx!(g, cf, "fillStyle");
    let style = if stroke { &cx.st.stroke } else { &cx.st.fill };
    match style {
        Style::Color(c) => js_string(g, &color::serialize(*c)),
        _ => get_i(g, arr, cx.stack.len() as u32 * 2 + stroke as u32),
    }
}

unsafe fn set_style(g: *mut c_void, cf: *mut c_void, stroke: bool) -> u64 {
    let this = cf_this(cf);
    let v = cf_arg(cf, 0);
    let c = classes(g);
    let (style, obj) = if let Some(gr) = host_data::<Arc<Mutex<Gradient>>>(g, c.grad_cls, v) {
        (Style::Gradient(gr.clone()), v)
    } else if let Some(p) = host_data::<Arc<Mutex<Pattern>>>(g, c.pat_cls, v) {
        (Style::Pattern(p.clone()), v)
    } else {
        let Ok(s) = to_js_string(g, v) else { return UNDEF };
        let s = read_string(g, s).unwrap_or_default();
        match color::parse(&s) {
            Some(Parsed::Rgba(c)) => (Style::Color(c), UNDEF),
            Some(Parsed::CurrentColor) => {
                super::advise(
                    "canvas:currentcolor",
                    "canvas: 'currentcolor' as a fill/stroke style is black here; a browser uses the canvas element's \
                     computed color"
                        .into(),
                );
                (Style::Color([0, 0, 0, 255]), UNDEF)
            }
            None => return UNDEF,
        }
    };
    let arr = styles_slot(g, this);
    let cx = this_ctx!(g, cf, "fillStyle");
    set_i(g, arr, cx.stack.len() as u32 * 2 + stroke as u32, obj);
    if stroke {
        cx.st.stroke = style;
    } else {
        cx.st.fill = style;
    }
    UNDEF
}

unsafe extern "sysv64" fn get_fill_style(g: *mut c_void, cf: *mut c_void) -> u64 {
    get_style(g, cf, false)
}
unsafe extern "sysv64" fn set_fill_style(g: *mut c_void, cf: *mut c_void) -> u64 {
    set_style(g, cf, false)
}
unsafe extern "sysv64" fn get_stroke_style(g: *mut c_void, cf: *mut c_void) -> u64 {
    get_style(g, cf, true)
}
unsafe extern "sysv64" fn set_stroke_style(g: *mut c_void, cf: *mut c_void) -> u64 {
    set_style(g, cf, true)
}

unsafe extern "sysv64" fn m_set_line_dash(g: *mut c_void, cf: *mut c_void) -> u64 {
    let v = cf_arg(cf, 0);
    if !is_cell(v) {
        return throw_type_error(g, "CanvasRenderingContext2D.setLineDash: the argument is not a sequence");
    }
    let len = array_len(g, v);
    let mut segs = Vec::with_capacity(len as usize);
    for i in 0..len {
        let Ok(x) = to_number(g, get_i(g, v, i)) else { return UNDEF };
        if !x.is_finite() || x < 0.0 {
            return UNDEF;
        }
        segs.push(x);
    }
    if segs.len() % 2 == 1 {
        segs.extend_from_within(..);
    }
    let cx = this_ctx!(g, cf, "setLineDash");
    cx.st.dash = if segs.iter().all(|s| *s == 0.0) { Vec::new() } else { segs };
    UNDEF
}

unsafe extern "sysv64" fn m_get_line_dash(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cx = this_ctx!(g, cf, "getLineDash");
    let dash = cx.st.dash.clone();
    let arr = new_array(g);
    for (i, d) in dash.iter().enumerate() {
        set_i(g, arr, i as u32, enc_f64(*d));
    }
    arr
}

// ---------------------------------------------------------------------------
// Natives: transforms
// ---------------------------------------------------------------------------

unsafe extern "sysv64" fn m_scale(g: *mut c_void, cf: *mut c_void) -> u64 {
    let (x, y) = (n(g, cf, 0), n(g, cf, 1));
    let cx = this_ctx!(g, cf, "scale");
    if finite(&[x, y]) {
        cx.st.xf = cx.st.xf * Affine::scale_non_uniform(x, y);
    }
    UNDEF
}

unsafe extern "sysv64" fn m_rotate(g: *mut c_void, cf: *mut c_void) -> u64 {
    let a = n(g, cf, 0);
    let cx = this_ctx!(g, cf, "rotate");
    if a.is_finite() {
        cx.st.xf = cx.st.xf * Affine::rotate(a);
    }
    UNDEF
}

unsafe extern "sysv64" fn m_translate(g: *mut c_void, cf: *mut c_void) -> u64 {
    let (x, y) = (n(g, cf, 0), n(g, cf, 1));
    let cx = this_ctx!(g, cf, "translate");
    if finite(&[x, y]) {
        cx.st.xf = cx.st.xf * Affine::translate((x, y));
    }
    UNDEF
}

unsafe fn six(g: *mut c_void, cf: *mut c_void) -> [f64; 6] {
    [n(g, cf, 0), n(g, cf, 1), n(g, cf, 2), n(g, cf, 3), n(g, cf, 4), n(g, cf, 5)]
}

unsafe extern "sysv64" fn m_transform(g: *mut c_void, cf: *mut c_void) -> u64 {
    let m = six(g, cf);
    let cx = this_ctx!(g, cf, "transform");
    if finite(&m) {
        cx.st.xf = cx.st.xf * Affine::new(m);
    }
    UNDEF
}

unsafe extern "sysv64" fn m_set_transform(g: *mut c_void, cf: *mut c_void) -> u64 {
    let m = if cf_argc(cf) <= 1 {
        let v = cf_arg(cf, 0);
        if v == UNDEF {
            [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]
        } else {
            let Some(m) = super::super::geometry::mat_of(g, v) else {
                return throw_type_error(g, "CanvasRenderingContext2D.setTransform: the argument is not a DOMMatrix2DInit");
            };
            [m.get(1, 1), m.get(1, 2), m.get(2, 1), m.get(2, 2), m.get(4, 1), m.get(4, 2)]
        }
    } else {
        six(g, cf)
    };
    let cx = this_ctx!(g, cf, "setTransform");
    if finite(&m) {
        cx.st.xf = Affine::new(m);
    }
    UNDEF
}

unsafe extern "sysv64" fn m_reset_transform(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cx = this_ctx!(g, cf, "resetTransform");
    cx.st.xf = Affine::IDENTITY;
    UNDEF
}

unsafe extern "sysv64" fn m_get_transform(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cx = this_ctx!(g, cf, "getTransform");
    let m = cx.st.xf.as_coeffs();
    super::super::geometry::new_matrix(g, super::super::geometry::Mat::from_2d(m))
}

// ---------------------------------------------------------------------------
// Natives: the current path
// ---------------------------------------------------------------------------

unsafe extern "sysv64" fn m_begin_path(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cx = this_ctx!(g, cf, "beginPath");
    cx.path = Path::new();
    UNDEF
}

unsafe extern "sysv64" fn m_close_path(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cx = this_ctx!(g, cf, "closePath");
    cx.path.close();
    UNDEF
}

unsafe extern "sysv64" fn m_move_to(g: *mut c_void, cf: *mut c_void) -> u64 {
    let (x, y) = (n(g, cf, 0), n(g, cf, 1));
    let cx = this_ctx!(g, cf, "moveTo");
    if finite(&[x, y]) {
        let (px, py) = cx.pt(x, y);
        cx.path.move_to(px, py);
    }
    UNDEF
}

unsafe extern "sysv64" fn m_line_to(g: *mut c_void, cf: *mut c_void) -> u64 {
    let (x, y) = (n(g, cf, 0), n(g, cf, 1));
    let cx = this_ctx!(g, cf, "lineTo");
    if finite(&[x, y]) {
        let (px, py) = cx.pt(x, y);
        cx.path.line_to(px, py);
    }
    UNDEF
}

unsafe extern "sysv64" fn m_quad_to(g: *mut c_void, cf: *mut c_void) -> u64 {
    let a = [n(g, cf, 0), n(g, cf, 1), n(g, cf, 2), n(g, cf, 3)];
    let cx = this_ctx!(g, cf, "quadraticCurveTo");
    if finite(&a) {
        let (c0, c1) = cx.pt(a[0], a[1]);
        let (p0, p1) = cx.pt(a[2], a[3]);
        cx.path.quad_to(c0, c1, p0, p1);
    }
    UNDEF
}

unsafe extern "sysv64" fn m_bezier_to(g: *mut c_void, cf: *mut c_void) -> u64 {
    let a = six(g, cf);
    let cx = this_ctx!(g, cf, "bezierCurveTo");
    if finite(&a) {
        let (c0, c1) = cx.pt(a[0], a[1]);
        let (c2, c3) = cx.pt(a[2], a[3]);
        let (p0, p1) = cx.pt(a[4], a[5]);
        cx.path.cubic_to(c0, c1, c2, c3, p0, p1);
    }
    UNDEF
}

unsafe extern "sysv64" fn m_rect(g: *mut c_void, cf: *mut c_void) -> u64 {
    let a = [n(g, cf, 0), n(g, cf, 1), n(g, cf, 2), n(g, cf, 3)];
    let cx = this_ctx!(g, cf, "rect");
    if finite(&a) {
        let [x, y, w, h] = a;
        let p = [cx.pt(x, y), cx.pt(x + w, y), cx.pt(x + w, y + h), cx.pt(x, y + h)];
        cx.path.move_to(p[0].0, p[0].1);
        for q in &p[1..] {
            cx.path.line_to(q.0, q.1);
        }
        cx.path.close();
        cx.path.move_to(p[0].0, p[0].1);
    }
    UNDEF
}

/// Builds arc-like geometry in USER space on a scratch path that starts at
/// the current point (inverse-transformed), then appends it, transformed.
fn user_geometry(cx: &mut Ctx, f: impl FnOnce(&mut Path) -> Result<(), &'static str>) -> Result<(), &'static str> {
    let Some(inv) = cx.inverse() else {
        return Ok(());
    };
    let mut scratch = Path::new();
    let had = cx.path.current_point().map(|(x, y)| inv * Point::new(x, y));
    if let Some(p) = had {
        scratch.move_to(p.x, p.y);
    }
    f(&mut scratch)?;
    let skip = had.is_some() as usize;
    for s in scratch.segs.iter().skip(skip) {
        match *s {
            Seg::Move(x, y) => {
                let (a, b) = cx.pt(x, y);
                cx.path.move_to(a, b);
            }
            Seg::Line(x, y) => {
                let (a, b) = cx.pt(x, y);
                cx.path.line_to(a, b);
            }
            Seg::Cubic([c1x, c1y, c2x, c2y, x, y]) => {
                let (a, b) = cx.pt(c1x, c1y);
                let (c, d) = cx.pt(c2x, c2y);
                let (e, f) = cx.pt(x, y);
                cx.path.cubic_to(a, b, c, d, e, f);
            }
            Seg::Close => cx.path.close(),
        }
    }
    Ok(())
}

unsafe extern "sysv64" fn m_arc(g: *mut c_void, cf: *mut c_void) -> u64 {
    let a = [n(g, cf, 0), n(g, cf, 1), n(g, cf, 2), n(g, cf, 3), n(g, cf, 4)];
    let ccw = truthy_flag(cf_arg(cf, 5));
    let cx = this_ctx!(g, cf, "arc");
    match user_geometry(cx, |p| p.arc(a[0], a[1], a[2], a[3], a[4], ccw)) {
        Ok(()) => UNDEF,
        Err(e) => path2d::throw_geometry_error(g, e),
    }
}

unsafe extern "sysv64" fn m_ellipse(g: *mut c_void, cf: *mut c_void) -> u64 {
    let a = [n(g, cf, 0), n(g, cf, 1), n(g, cf, 2), n(g, cf, 3), n(g, cf, 4), n(g, cf, 5), n(g, cf, 6)];
    let ccw = truthy_flag(cf_arg(cf, 7));
    let cx = this_ctx!(g, cf, "ellipse");
    match user_geometry(cx, |p| p.ellipse(a[0], a[1], a[2], a[3], a[4], a[5], a[6], ccw)) {
        Ok(()) => UNDEF,
        Err(e) => path2d::throw_geometry_error(g, e),
    }
}

unsafe extern "sysv64" fn m_arc_to(g: *mut c_void, cf: *mut c_void) -> u64 {
    let a = [n(g, cf, 0), n(g, cf, 1), n(g, cf, 2), n(g, cf, 3), n(g, cf, 4)];
    let cx = this_ctx!(g, cf, "arcTo");
    match user_geometry(cx, |p| p.arc_to(a[0], a[1], a[2], a[3], a[4])) {
        Ok(()) => UNDEF,
        Err(e) => path2d::throw_geometry_error(g, e),
    }
}

unsafe extern "sysv64" fn m_round_rect(g: *mut c_void, cf: *mut c_void) -> u64 {
    let radii = match path2d::read_radii(g, cf_arg(cf, 4)) {
        Ok(r) => r,
        Err(e) => return throw_error(g, 2, &e),
    };
    let a = [n(g, cf, 0), n(g, cf, 1), n(g, cf, 2), n(g, cf, 3)];
    let cx = this_ctx!(g, cf, "roundRect");
    match user_geometry(cx, |p| p.round_rect(a[0], a[1], a[2], a[3], radii)) {
        Ok(()) => UNDEF,
        Err(e) => path2d::throw_geometry_error(g, e),
    }
}

// ---------------------------------------------------------------------------
// Natives: painting
// ---------------------------------------------------------------------------

unsafe fn fill_rule(g: *mut c_void, v: u64) -> Result<bool, ()> {
    if v == UNDEF {
        return Ok(false);
    }
    let s = read_string(g, to_js_string(g, v)?).unwrap_or_default();
    match s.as_str() {
        "nonzero" => Ok(false),
        "evenodd" => Ok(true),
        _ => {
            throw_type_error(g, &format!("The provided value '{s}' is not a valid enum value of type CanvasFillRule."));
            Err(())
        }
    }
}

/// `(path2d?, fillRule?)`: the Path2D argument (user space) if one was given,
/// and the rule.
unsafe fn path_and_rule(g: *mut c_void, cf: *mut c_void) -> Result<(Option<BezPath>, bool), ()> {
    let a0 = cf_arg(cf, 0);
    if let Some(p) = path2d::path_of(g, a0) {
        let b = bez_of(&p.p);
        return Ok((Some(b), fill_rule(g, cf_arg(cf, 1))?));
    }
    Ok((None, fill_rule(g, a0)?))
}

unsafe extern "sysv64" fn m_fill(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Ok((user, even_odd)) = path_and_rule(g, cf) else { return UNDEF };
    let cx = this_ctx!(g, cf, "fill");
    let Some(inv) = cx.inverse() else { return UNDEF };
    let path = user.unwrap_or_else(|| inv * bez_of(&cx.path));
    cx.advise_unpainted();
    cx.push(Cmd::Fill { path, even_odd, transform: cx.st.xf, paint: paint_of(&cx.st.fill), draw: cx.draw() });
    UNDEF
}

unsafe extern "sysv64" fn m_stroke(g: *mut c_void, cf: *mut c_void) -> u64 {
    let user = path2d::path_of(g, cf_arg(cf, 0)).map(|p| bez_of(&p.p));
    let cx = this_ctx!(g, cf, "stroke");
    let Some(inv) = cx.inverse() else { return UNDEF };
    let path = user.unwrap_or_else(|| inv * bez_of(&cx.path));
    cx.advise_unpainted();
    cx.push(Cmd::Stroke { path, style: cx.stroke_style(), transform: cx.st.xf, paint: paint_of(&cx.st.stroke), draw: cx.draw() });
    UNDEF
}

unsafe extern "sysv64" fn m_clip(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Ok((user, even_odd)) = path_and_rule(g, cf) else { return UNDEF };
    let cx = this_ctx!(g, cf, "clip");
    let path = match user {
        Some(u) => cx.st.xf * u,
        None => bez_of(&cx.path),
    };
    cx.st.clip = Some(Arc::new(Clip { path, even_odd, parent: cx.st.clip.take() }));
    UNDEF
}

unsafe extern "sysv64" fn m_is_point_in_path(g: *mut c_void, cf: *mut c_void) -> u64 {
    let a0 = cf_arg(cf, 0);
    let (user, x, y, rule_arg) = match path2d::path_of(g, a0) {
        Some(p) => (Some(bez_of(&p.p)), n(g, cf, 1), n(g, cf, 2), cf_arg(cf, 3)),
        None => (None, n(g, cf, 0), n(g, cf, 1), cf_arg(cf, 2)),
    };
    let Ok(even_odd) = fill_rule(g, rule_arg) else { return UNDEF };
    let cx = this_ctx!(g, cf, "isPointInPath");
    if !finite(&[x, y]) {
        return FALSE;
    }
    let device = match user {
        Some(u) => cx.st.xf * u,
        None => bez_of(&cx.path),
    };
    let w = device.winding(Point::new(x, y));
    enc_bool(if even_odd { w % 2 != 0 } else { w != 0 })
}

unsafe extern "sysv64" fn m_is_point_in_stroke(g: *mut c_void, cf: *mut c_void) -> u64 {
    let a0 = cf_arg(cf, 0);
    let (user, x, y) = match path2d::path_of(g, a0) {
        Some(p) => (Some(bez_of(&p.p)), n(g, cf, 1), n(g, cf, 2)),
        None => (None, n(g, cf, 0), n(g, cf, 1)),
    };
    let cx = this_ctx!(g, cf, "isPointInStroke");
    if !finite(&[x, y]) {
        return FALSE;
    }
    let Some(inv) = cx.inverse() else { return FALSE };
    let path = user.unwrap_or_else(|| inv * bez_of(&cx.path));
    let outline = kurbo::stroke(path.iter(), &cx.stroke_style(), &kurbo::StrokeOpts::default(), 0.05);
    enc_bool((cx.st.xf * outline).winding(Point::new(x, y)) != 0)
}

unsafe fn rect_args(g: *mut c_void, cf: *mut c_void) -> Option<Rect> {
    let a = [n(g, cf, 0), n(g, cf, 1), n(g, cf, 2), n(g, cf, 3)];
    finite(&a).then(|| Rect::new(a[0], a[1], a[0] + a[2], a[1] + a[3]))
}

unsafe extern "sysv64" fn m_clear_rect(g: *mut c_void, cf: *mut c_void) -> u64 {
    let r = rect_args(g, cf);
    let cx = this_ctx!(g, cf, "clearRect");
    if let Some(r) = r {
        if r.width() != 0.0 && r.height() != 0.0 {
            cx.push(Cmd::Clear { rect: r.abs(), transform: cx.st.xf, clip: cx.st.clip.clone() });
        }
    }
    UNDEF
}

unsafe extern "sysv64" fn m_fill_rect(g: *mut c_void, cf: *mut c_void) -> u64 {
    let r = rect_args(g, cf);
    let cx = this_ctx!(g, cf, "fillRect");
    if let Some(r) = r {
        if r.width() != 0.0 && r.height() != 0.0 {
            cx.advise_unpainted();
            cx.push(Cmd::Fill {
                path: r.abs().to_path(0.1),
                even_odd: false,
                transform: cx.st.xf,
                paint: paint_of(&cx.st.fill),
                draw: cx.draw(),
            });
        }
    }
    UNDEF
}

unsafe extern "sysv64" fn m_stroke_rect(g: *mut c_void, cf: *mut c_void) -> u64 {
    let r = rect_args(g, cf);
    let cx = this_ctx!(g, cf, "strokeRect");
    let Some(r) = r else { return UNDEF };
    if r.width() == 0.0 && r.height() == 0.0 {
        return UNDEF;
    }
    // HTML: a zero-width or zero-height rect strokes as a single line.
    let mut path = BezPath::new();
    if r.width() == 0.0 || r.height() == 0.0 {
        path.move_to((r.x0, r.y0));
        path.line_to((r.x1, r.y1));
    } else {
        path = Rect::new(r.x0, r.y0, r.x1, r.y1).to_path(0.1);
    }
    cx.advise_unpainted();
    cx.push(Cmd::Stroke { path, style: cx.stroke_style(), transform: cx.st.xf, paint: paint_of(&cx.st.stroke), draw: cx.draw() });
    UNDEF
}

unsafe extern "sysv64" fn m_draw_image(g: *mut c_void, cf: *mut c_void) -> u64 {
    let argc = cf_argc(cf);
    if argc != 3 && argc != 5 && argc != 9 {
        return throw_type_error(g, &format!("CanvasRenderingContext2D.drawImage: {argc} arguments is not a valid overload (3, 5 or 9)"));
    }
    if ctx_of(g, cf_this(cf)).is_none() {
        return throw_type_error(g, "CanvasRenderingContext2D.drawImage: illegal invocation");
    }
    let src = match image_source(g, cf_arg(cf, 0)) {
        Ok(Source::Pixels(img, clean)) => (img, clean),
        Ok(Source::Nothing) => return UNDEF,
        Err(()) => return UNDEF,
    };
    let a: Vec<f64> = (1..argc).map(|i| n(g, cf, i)).collect();
    let cx = this_ctx!(g, cf, "drawImage");
    if !finite(&a) {
        return UNDEF;
    }
    let (img, clean) = src;
    let (iw, ih) = (img.width as f64, img.height as f64);
    let (mut s, mut d) = match argc {
        3 => (Rect::new(0.0, 0.0, iw, ih), Rect::new(a[0], a[1], a[0] + iw, a[1] + ih)),
        5 => (Rect::new(0.0, 0.0, iw, ih), Rect::new(a[0], a[1], a[0] + a[2], a[1] + a[3])),
        _ => (Rect::new(a[0], a[1], a[0] + a[2], a[1] + a[3]), Rect::new(a[4], a[5], a[4] + a[6], a[5] + a[7])),
    };
    // Negative sizes flip into place; then the source is clipped to the image
    // and the destination shrinks in proportion (HTML drawImage step 5).
    s = s.abs();
    d = d.abs();
    if s.width() == 0.0 || s.height() == 0.0 || d.width() == 0.0 || d.height() == 0.0 {
        return UNDEF;
    }
    let clipped = s.intersect(Rect::new(0.0, 0.0, iw, ih));
    if clipped.width() <= 0.0 || clipped.height() <= 0.0 {
        return UNDEF;
    }
    let (kx, ky) = (d.width() / s.width(), d.height() / s.height());
    let d = Rect::new(
        d.x0 + (clipped.x0 - s.x0) * kx,
        d.y0 + (clipped.y0 - s.y0) * ky,
        d.x0 + (clipped.x1 - s.x0) * kx,
        d.y0 + (clipped.y1 - s.y0) * ky,
    );
    if !clean {
        cx.surface.lock().unwrap().origin_clean = false;
    }
    cx.advise_unpainted();
    cx.push(Cmd::Image { image: img, src: clipped, dst: d, transform: cx.st.xf, quality: cx.quality(), draw: cx.draw() });
    UNDEF
}

// ---------------------------------------------------------------------------
// Natives: pixels
// ---------------------------------------------------------------------------

unsafe fn new_image_data(g: *mut c_void, w: u32, h: u32, bytes: Option<&[u8]>) -> u64 {
    let len = w as usize * h as usize * 4;
    let mut ta = UNDEF;
    if crate::ffi::redjs_typedarray_create(g, 3, len, &mut ta) != 0 {
        return UNDEF;
    }
    if let Some(b) = bytes {
        let mut info = crate::ffi::RjsTaInfo::default();
        if glue::rjs_typedarray_info(g, ta, &mut info) == 0 && !info.data.is_null() {
            core::ptr::copy_nonoverlapping(b.as_ptr(), info.data as *mut u8, len);
        }
    }
    super::super::platform::create_image_data(g, ta, w, h).unwrap_or(UNDEF)
}

/// `(sw, sh)` for createImageData / getImageData: non-zero, within limits.
unsafe fn checked_size(g: *mut c_void, w: f64, h: f64, what: &str) -> Option<(u32, u32)> {
    let (w, h) = (w.abs().trunc(), h.abs().trunc());
    if w == 0.0 || h == 0.0 || !w.is_finite() || !h.is_finite() {
        domexception_throw(g, "IndexSizeError", &format!("CanvasRenderingContext2D.{what}: the source width and height must be non-zero"));
        return None;
    }
    if w * h > super::surface::MAX_AREA as f64 {
        throw_error(g, 2, &format!("CanvasRenderingContext2D.{what}: {w}x{h} is too large"));
        return None;
    }
    Some((w as u32, h as u32))
}

unsafe extern "sysv64" fn m_create_image_data(g: *mut c_void, cf: *mut c_void) -> u64 {
    if ctx_of(g, cf_this(cf)).is_none() {
        return throw_type_error(g, "CanvasRenderingContext2D.createImageData: illegal invocation");
    }
    let a0 = cf_arg(cf, 0);
    if let Some((_, w, h)) = super::super::platform::image_data_parts(g, a0) {
        return new_image_data(g, w, h, None);
    }
    if cf_argc(cf) < 2 {
        return throw_type_error(g, "CanvasRenderingContext2D.createImageData: expected (sw, sh) or (imagedata)");
    }
    let Some((w, h)) = checked_size(g, n(g, cf, 0), n(g, cf, 1), "createImageData") else { return UNDEF };
    new_image_data(g, w, h, None)
}

unsafe extern "sysv64" fn m_get_image_data(g: *mut c_void, cf: *mut c_void) -> u64 {
    let a = [n(g, cf, 0), n(g, cf, 1), n(g, cf, 2), n(g, cf, 3)];
    if ctx_of(g, cf_this(cf)).is_none() {
        return throw_type_error(g, "CanvasRenderingContext2D.getImageData: illegal invocation");
    }
    if !finite(&a) {
        return throw_type_error(g, "CanvasRenderingContext2D.getImageData: the arguments must be finite");
    }
    let Some((w, h)) = checked_size(g, a[2], a[3], "getImageData") else { return UNDEF };
    let (mut x, mut y) = (a[0].trunc() as i64, a[1].trunc() as i64);
    if a[2] < 0.0 {
        x -= w as i64;
    }
    if a[3] < 0.0 {
        y -= h as i64;
    }
    let cx = this_ctx!(g, cf, "getImageData");
    let px = {
        let mut s = cx.surface.lock().unwrap();
        if s.readable().is_err() {
            drop(s);
            domexception_throw(g, "SecurityError", "The canvas has been tainted by cross-origin data.");
            return UNDEF;
        }
        s.read_straight(x, y, w, h)
    };
    new_image_data(g, w, h, Some(&px))
}

unsafe extern "sysv64" fn m_put_image_data(g: *mut c_void, cf: *mut c_void) -> u64 {
    let argc = cf_argc(cf);
    if argc != 3 && argc != 7 {
        return throw_type_error(g, "CanvasRenderingContext2D.putImageData: expected 3 or 7 arguments");
    }
    let Some((data, w, h)) = super::super::platform::image_data_parts(g, cf_arg(cf, 0)) else {
        return throw_type_error(g, "CanvasRenderingContext2D.putImageData: the argument is not an ImageData");
    };
    let a: Vec<f64> = (1..argc).map(|i| n(g, cf, i)).collect();
    if !finite(&a) {
        return UNDEF;
    }
    let Some(bytes) = bufferish(g, data) else { return UNDEF };
    if bytes.len() < w as usize * h as usize * 4 {
        return domexception_throw_val(g, "InvalidStateError", "putImageData: the ImageData's buffer is detached");
    }
    let (dx, dy) = (a[0].trunc() as i64, a[1].trunc() as i64);
    let (mut rx, mut ry, mut rw, mut rh) = if argc == 7 {
        (a[2].trunc(), a[3].trunc(), a[4].trunc(), a[5].trunc())
    } else {
        (0.0, 0.0, w as f64, h as f64)
    };
    if rw < 0.0 {
        rx += rw;
        rw = -rw;
    }
    if rh < 0.0 {
        ry += rh;
        rh = -rh;
    }
    if rx < 0.0 {
        rw += rx;
        rx = 0.0;
    }
    if ry < 0.0 {
        rh += ry;
        ry = 0.0;
    }
    rw = rw.min(w as f64 - rx);
    rh = rh.min(h as f64 - ry);
    if rw <= 0.0 || rh <= 0.0 {
        return UNDEF;
    }
    let (rx, ry, rw, rh) = (rx as u32, ry as u32, rw as u32, rh as u32);
    let mut px = Vec::with_capacity(rw as usize * rh as usize * 4);
    for row in ry..ry + rh {
        let at = (row as usize * w as usize + rx as usize) * 4;
        px.extend_from_slice(&bytes[at..at + rw as usize * 4]);
    }
    let cx = this_ctx!(g, cf, "putImageData");
    let image = Arc::new(Image::from_straight(rw, rh, px, true));
    let (x, y) = (dx + rx as i64, dy + ry as i64);
    if x > i32::MAX as i64 || y > i32::MAX as i64 || x < i32::MIN as i64 || y < i32::MIN as i64 {
        return UNDEF;
    }
    cx.push(Cmd::Put { x: x as i32, y: y as i32, image });
    UNDEF
}

unsafe fn domexception_throw_val(g: *mut c_void, name: &str, msg: &str) -> u64 {
    super::super::domexception::throw(g, name, msg)
}

// ---------------------------------------------------------------------------
// Natives: text (not drawn yet)
// ---------------------------------------------------------------------------

fn advise_text() {
    super::advise(
        "canvas:text",
        "canvas: fillText/strokeText draw nothing yet, and measureText's widths are estimates from the font size; \
         text on a canvas lands with the glyph renderer"
            .into(),
    );
}

unsafe extern "sysv64" fn m_fill_text(g: *mut c_void, cf: *mut c_void) -> u64 {
    let _ = this_ctx!(g, cf, "fillText");
    advise_text();
    UNDEF
}

/// The px size in a `font` shorthand (10 when unreadable).
fn font_px(font: &str) -> f64 {
    for t in font.split_whitespace() {
        let size = t.split('/').next().unwrap_or("");
        for (unit, k) in [("px", 1.0), ("pt", 4.0 / 3.0), ("rem", 16.0), ("em", 10.0), ("%", 0.1)] {
            if let Some(v) = size.strip_suffix(unit).and_then(|v| v.parse::<f64>().ok()) {
                return v * k;
            }
        }
    }
    10.0
}

unsafe extern "sysv64" fn m_measure_text(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Ok(text) = arg_str(g, cf, 0) else { return UNDEF };
    let cx = this_ctx!(g, cf, "measureText");
    advise_text();
    let px = font_px(&cx.st.font);
    let width = text.chars().map(|c| if c.is_whitespace() { 0.28 } else if c.is_ascii() { 0.55 } else { 1.0 }).sum::<f64>() * px;
    let c = classes(g);
    let m = new_object(g, c.metrics.proto);
    let (ascent, descent) = (px * 0.8, px * 0.2);
    let left = match cx.st.text_align {
        "center" => width / 2.0,
        "right" | "end" => width,
        _ => 0.0,
    };
    for (k, v) in [
        ("width", width),
        ("actualBoundingBoxLeft", left),
        ("actualBoundingBoxRight", width - left),
        ("fontBoundingBoxAscent", ascent),
        ("fontBoundingBoxDescent", descent),
        ("actualBoundingBoxAscent", ascent),
        ("actualBoundingBoxDescent", descent),
        ("emHeightAscent", ascent),
        ("emHeightDescent", descent),
        ("hangingBaseline", ascent * 0.8),
        ("alphabeticBaseline", 0.0),
        ("ideographicBaseline", -descent),
    ] {
        define(g, m, k, enc_f64(v), 1);
    }
    m
}

unsafe extern "sysv64" fn m_noop(_g: *mut c_void, _cf: *mut c_void) -> u64 {
    UNDEF
}

// ---------------------------------------------------------------------------
// CanvasGradient / CanvasPattern
// ---------------------------------------------------------------------------

unsafe fn new_gradient(g: *mut c_void, kind: GradientKind) -> u64 {
    let c = classes(g);
    host_create(g, c.grad_cls, c.grad.proto, Arc::new(Mutex::new(Gradient { kind, stops: Vec::new() })))
}

unsafe extern "sysv64" fn m_linear_gradient(g: *mut c_void, cf: *mut c_void) -> u64 {
    let a = [n(g, cf, 0), n(g, cf, 1), n(g, cf, 2), n(g, cf, 3)];
    let _ = this_ctx!(g, cf, "createLinearGradient");
    if !finite(&a) {
        return throw_type_error(g, "CanvasRenderingContext2D.createLinearGradient: the arguments must be finite");
    }
    new_gradient(g, GradientKind::Linear { x0: a[0], y0: a[1], x1: a[2], y1: a[3] })
}

unsafe extern "sysv64" fn m_radial_gradient(g: *mut c_void, cf: *mut c_void) -> u64 {
    let a = six(g, cf);
    let _ = this_ctx!(g, cf, "createRadialGradient");
    if !finite(&a) {
        return throw_type_error(g, "CanvasRenderingContext2D.createRadialGradient: the arguments must be finite");
    }
    if a[2] < 0.0 || a[5] < 0.0 {
        return domexception_throw_val(g, "IndexSizeError", "createRadialGradient: a radius is negative");
    }
    new_gradient(g, GradientKind::Radial { x0: a[0], y0: a[1], r0: a[2], x1: a[3], y1: a[4], r1: a[5] })
}

unsafe extern "sysv64" fn m_conic_gradient(g: *mut c_void, cf: *mut c_void) -> u64 {
    let a = [n(g, cf, 0), n(g, cf, 1), n(g, cf, 2)];
    let _ = this_ctx!(g, cf, "createConicGradient");
    if !finite(&a) {
        return throw_type_error(g, "CanvasRenderingContext2D.createConicGradient: the arguments must be finite");
    }
    new_gradient(g, GradientKind::Conic { angle: a[0], x: a[1], y: a[2] })
}

unsafe extern "sysv64" fn m_add_color_stop(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Ok(offset) = to_number(g, cf_arg(cf, 0)) else { return UNDEF };
    let Ok(text) = arg_str(g, cf, 1) else { return UNDEF };
    let c = classes(g);
    let Some(gr) = host_data::<Arc<Mutex<Gradient>>>(g, c.grad_cls, cf_this(cf)) else {
        return throw_type_error(g, "CanvasGradient.addColorStop: illegal invocation");
    };
    if !offset.is_finite() {
        return throw_type_error(g, "CanvasGradient.addColorStop: the offset must be finite");
    }
    if !(0.0..=1.0).contains(&offset) {
        return domexception_throw_val(g, "IndexSizeError", &format!("CanvasGradient.addColorStop: the offset {offset} is outside 0..1"));
    }
    let rgba = match color::parse(&text) {
        Some(Parsed::Rgba(c)) => c,
        Some(Parsed::CurrentColor) => [0, 0, 0, 255],
        None => {
            return domexception_throw_val(g, "SyntaxError", &format!("CanvasGradient.addColorStop: '{text}' could not be parsed as a color"))
        }
    };
    let mut gr = gr.lock().unwrap();
    let at = gr.stops.iter().position(|(o, _)| *o as f64 > offset).unwrap_or(gr.stops.len());
    gr.stops.insert(at, (offset as f32, color::to_f32(rgba)));
    UNDEF
}

unsafe extern "sysv64" fn m_create_pattern(g: *mut c_void, cf: *mut c_void) -> u64 {
    if ctx_of(g, cf_this(cf)).is_none() {
        return throw_type_error(g, "CanvasRenderingContext2D.createPattern: illegal invocation");
    }
    let img = match image_source(g, cf_arg(cf, 0)) {
        Ok(Source::Pixels(img, _)) => img,
        Ok(Source::Nothing) => return NULL,
        Err(()) => return UNDEF,
    };
    let rep = cf_arg(cf, 1);
    let rep = if rep == NULL {
        String::new()
    } else {
        let Ok(s) = arg_str(g, cf, 1) else { return UNDEF };
        s
    };
    let (rx, ry) = match rep.as_str() {
        "" | "repeat" => (true, true),
        "repeat-x" => (true, false),
        "repeat-y" => (false, true),
        "no-repeat" => (false, false),
        _ => return domexception_throw_val(g, "SyntaxError", &format!("createPattern: '{rep}' is not a repetition")),
    };
    let c = classes(g);
    host_create(
        g,
        c.pat_cls,
        c.pat.proto,
        Arc::new(Mutex::new(Pattern { image: img, repeat_x: rx, repeat_y: ry, transform: Affine::IDENTITY })),
    )
}

unsafe extern "sysv64" fn m_pattern_set_transform(g: *mut c_void, cf: *mut c_void) -> u64 {
    let m = super::super::geometry::mat_of(g, cf_arg(cf, 0));
    let c = classes(g);
    let Some(p) = host_data::<Arc<Mutex<Pattern>>>(g, c.pat_cls, cf_this(cf)) else {
        return throw_type_error(g, "CanvasPattern.setTransform: illegal invocation");
    };
    let xf = match m {
        Some(m) => Affine::new([m.get(1, 1), m.get(1, 2), m.get(2, 1), m.get(2, 2), m.get(4, 1), m.get(4, 2)]),
        None if cf_arg(cf, 0) == UNDEF => Affine::IDENTITY,
        None => return throw_type_error(g, "CanvasPattern.setTransform: the argument is not a DOMMatrix2DInit"),
    };
    if finite(&xf.as_coeffs()) {
        p.lock().unwrap().transform = xf;
    }
    UNDEF
}

// ---------------------------------------------------------------------------
// OffscreenCanvas
// ---------------------------------------------------------------------------

pub(crate) struct Off {
    width: u32,
    height: u32,
    /// 0 = unset, 1 = 2D, 3 = WebGPU; 4/5 = native context creation in progress.
    kind: u8,
}

unsafe fn enforce_dim(g: *mut c_void, v: u64, what: &str) -> Option<u32> {
    let Ok(d) = to_number(g, v) else { return None };
    if !d.is_finite() || d < 0.0 || d > 4294967295.0 {
        throw_type_error(g, &format!("OffscreenCanvas: the {what} {d} is outside the range of unsigned long long"));
        return None;
    }
    Some(d.trunc() as u32)
}

unsafe extern "sysv64" fn off_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(w) = enforce_dim(g, cf_arg(cf, 0), "width") else { return UNDEF };
    let Some(h) = enforce_dim(g, cf_arg(cf, 1), "height") else { return UNDEF };
    let c = classes(g);
    let nt = cf_new_target(cf);
    let proto = {
        let p = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
        if is_cell(p) { p } else { c.off.proto }
    };
    host_create(g, c.off_cls, proto, Off { width: w, height: h, kind: 0 })
}

macro_rules! this_off {
    ($g:ident, $cf:ident, $what:literal) => {
        match host_data::<Off>($g, classes($g).off_cls, cf_this($cf)) {
            Some(o) => o,
            None => return throw_type_error($g, concat!("OffscreenCanvas.", $what, ": illegal invocation")),
        }
    };
}

unsafe extern "sysv64" fn off_get_width(g: *mut c_void, cf: *mut c_void) -> u64 {
    let o = this_off!(g, cf, "width");
    enc_f64(o.width as f64)
}
unsafe extern "sysv64" fn off_get_height(g: *mut c_void, cf: *mut c_void) -> u64 {
    let o = this_off!(g, cf, "height");
    enc_f64(o.height as f64)
}

unsafe fn off_set_dim(g: *mut c_void, cf: *mut c_void, height: bool) -> u64 {
    let Some(v) = enforce_dim(g, cf_arg(cf, 0), if height { "height" } else { "width" }) else {
        return UNDEF;
    };
    let (w, h, kind) = {
        let o = this_off!(g, cf, "width");
        if height {
            o.height = v;
        } else {
            o.width = v;
        }
        (o.width, o.height, o.kind)
    };
    let ctx = get_a(g, cf_this(cf), classes(g).s_ctx);
    #[cfg(feature = "webgpu")]
    if kind == 3 {
        let _ = super::super::webgpu::canvas::resize(g, ctx, w, h);
        return UNDEF;
    }
    let _ = kind;
    if let Some(cx) = ctx_of(g, ctx) {
        resize(cx, w, h);
    }
    UNDEF
}

unsafe extern "sysv64" fn off_set_width(g: *mut c_void, cf: *mut c_void) -> u64 {
    off_set_dim(g, cf, false)
}
unsafe extern "sysv64" fn off_set_height(g: *mut c_void, cf: *mut c_void) -> u64 {
    off_set_dim(g, cf, true)
}

unsafe extern "sysv64" fn off_get_context(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let Ok(kind) = arg_str(g, cf, 0) else {
        return UNDEF;
    };
    let c = classes(g);
    let wanted = match kind.as_str() {
        "2d" => 1,
        "webgpu" => 3,
        "webgl" | "webgl2" | "bitmaprenderer" => return NULL,
        _ => return throw_type_error(g, "Invalid OffscreenRenderingContextId"),
    };
    #[cfg(not(feature = "webgpu"))]
    if wanted == 3 {
        return NULL;
    }
    let old = {
        let o = this_off!(g, cf, "getContext");
        o.kind
    };
    if old != 0 {
        return if old == wanted {
            get_a(g, this, c.s_ctx)
        } else {
            NULL
        };
    }
    {
        let o = this_off!(g, cf, "getContext");
        o.kind = if wanted == 1 { 4 } else { 5 };
    }
    let options = if wanted == 1 {
        match read_options(g, cf_arg(cf, 1)) {
            Ok(o) => o,
            Err(()) => {
                let o = this_off!(g, cf, "getContext");
                o.kind = 0;
                return UNDEF;
            }
        }
    } else {
        Options::default()
    };
    let (w, h) = {
        let o = this_off!(g, cf, "getContext");
        (o.width, o.height)
    };
    let ctx = if wanted == 1 {
        new_context(g, this, w, h, options, None, true)
    } else {
        #[cfg(feature = "webgpu")]
        {
            super::super::webgpu::canvas::create(g, this, w, h, None)
        }
        #[cfg(not(feature = "webgpu"))]
        {
            NULL
        }
    };
    glue::rjs_define(g, this, c.s_ctx, ctx, 0);
    {
        let o = this_off!(g, cf, "getContext");
        o.kind = if ctx == NULL || ctx == UNDEF {
            0
        } else {
            wanted
        };
    }
    ctx
}

/// Straight RGBA of an OffscreenCanvas's bitmap (transparent without a context).
unsafe fn off_pixels(g: *mut c_void, this: u64, w: u32, h: u32) -> Vec<u8> {
    match ctx_of(g, get_a(g, this, classes(g).s_ctx)) {
        Some(cx) => {
            let mut s = cx.surface.lock().unwrap();
            let _ = s.readable();
            s.read_straight(0, 0, w, h)
        }
        None => vec![0; w as usize * h as usize * 4],
    }
}

unsafe fn type_and_quality(g: *mut c_void, ty: u64, q: u64) -> (String, Option<f64>) {
    let t = if ty == UNDEF { "image/png".to_string() } else { read_string(g, ty).unwrap_or_default() };
    (t, dec_f64(q))
}

unsafe extern "sysv64" fn off_convert_to_blob(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let opts = cf_arg(cf, 0);
    let (ty, q) = if is_cell(opts) { type_and_quality(g, get(g, opts, "type"), get(g, opts, "quality")) } else { ("image/png".into(), None) };
    let Some(o) = host_data::<Off>(g, classes(g).off_cls, this) else {
        return super::super::domexception::rejected(g, "TypeError", "OffscreenCanvas.convertToBlob: illegal invocation");
    };
    let (w, h, kind) = (o.width, o.height, o.kind);
    #[cfg(feature = "webgpu")]
    if kind == 3 {
        let ctx = get_a(g, this, classes(g).s_ctx);
        return super::super::webgpu::canvas::convert_to_blob(g, ctx, opts);
    }
    let _ = kind;
    if w == 0 || h == 0 {
        return super::super::domexception::rejected(g, "IndexSizeError", "OffscreenCanvas.convertToBlob: the canvas has no pixels");
    }
    let px = off_pixels(g, this, w, h);
    let (mime, bytes) = super::encode::encode(&px, w, h, &ty, q);
    let blob = super::super::blob::create(g, state(g).blob.proto, bytes, mime);
    resolved_promise(g, blob)
}

unsafe extern "sysv64" fn off_transfer(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let c = classes(g);
    let o = this_off!(g, cf, "transferToImageBitmap");
    let (w, h, kind) = (o.width, o.height, o.kind);
    #[cfg(feature = "webgpu")]
    if kind == 3 {
        let ctx = get_a(g, this, c.s_ctx);
        return super::super::webgpu::canvas::transfer(g, ctx);
    }
    let _ = kind;
    let Some(cx) = ctx_of(g, get_a(g, this, c.s_ctx)) else {
        return domexception_throw_val(g, "InvalidStateError", "OffscreenCanvas.transferToImageBitmap: the canvas has no context");
    };
    let px = {
        let mut s = cx.surface.lock().unwrap();
        let px = s.read_straight(0, 0, w, h);
        s.reset(w, h);
        px
    };
    super::super::image::create_bitmap(g, super::super::image::BitmapData::new(w, h, px))
}

// ---------------------------------------------------------------------------
// The embedder's element half (hidden natives)
// ---------------------------------------------------------------------------

unsafe fn dim(v: u64) -> u32 {
    dec_f64(v).filter(|d| d.is_finite() && *d >= 0.0).map(|d| d.min(u32::MAX as f64) as u32).unwrap_or(0)
}

/// `__rjs_canvas_context(canvas, width, height, options, key?)`
unsafe extern "sysv64" fn h_context(g: *mut c_void, cf: *mut c_void) -> u64 {
    let key = dec_f64(cf_arg(cf, 4)).filter(|k| *k > 0.0).map(|k| k as u64);
    let Ok(options) = read_options(g, cf_arg(cf, 3)) else {
        return UNDEF;
    };
    new_context(g, cf_arg(cf, 0), dim(cf_arg(cf, 1)), dim(cf_arg(cf, 2)), options, key, false)
}

/// `__rjs_canvas_resize(ctx, width, height)`
unsafe extern "sysv64" fn h_resize(g: *mut c_void, cf: *mut c_void) -> u64 {
    let (w, h) = (dim(cf_arg(cf, 1)), dim(cf_arg(cf, 2)));
    let ctx = cf_arg(cf, 0);
    let Some(cx) = ctx_of(g, ctx) else { return throw_type_error(g, "__rjs_canvas_resize: not a context") };
    resize(cx, w, h);
    glue::rjs_define(g, ctx, classes(g).s_styles, new_array(g), 0);
    UNDEF
}

/// Straight RGBA of `ctx`'s bitmap, or a transparent `w`×`h` without one.
unsafe fn element_pixels(g: *mut c_void, ctx: u64, w: u32, h: u32) -> (Vec<u8>, u32, u32) {
    match ctx_of(g, ctx) {
        Some(cx) => {
            let mut s = cx.surface.lock().unwrap();
            let _ = s.readable();
            let (w, h) = (s.width(), s.height());
            (s.read_straight(0, 0, w, h), w, h)
        }
        None => {
            let (w, h) = super::surface::clamp_size(w, h);
            (vec![0; w as usize * h as usize * 4], w, h)
        }
    }
}

/// `__rjs_canvas_data_url(ctx|null, width, height, type, quality)`
unsafe extern "sysv64" fn h_data_url(g: *mut c_void, cf: *mut c_void) -> u64 {
    let (ty, q) = type_and_quality(g, cf_arg(cf, 3), cf_arg(cf, 4));
    let (px, w, h) = element_pixels(g, cf_arg(cf, 0), dim(cf_arg(cf, 1)), dim(cf_arg(cf, 2)));
    js_string(g, &super::encode::data_url(&px, w, h, &ty, q))
}

/// `__rjs_canvas_blob(ctx|null, width, height, type, quality)` → Blob, or
/// null for an empty bitmap (HTML: toBlob's callback then gets null).
unsafe extern "sysv64" fn h_blob(g: *mut c_void, cf: *mut c_void) -> u64 {
    let (ty, q) = type_and_quality(g, cf_arg(cf, 3), cf_arg(cf, 4));
    let (px, w, h) = element_pixels(g, cf_arg(cf, 0), dim(cf_arg(cf, 1)), dim(cf_arg(cf, 2)));
    if w == 0 || h == 0 {
        return NULL;
    }
    let (mime, bytes) = super::encode::encode(&px, w, h, &ty, q);
    super::super::blob::create(g, state(g).blob.proto, bytes, mime)
}

/// `__rjs_canvas_source_hook(fn)`: how the embedder maps its own elements to
/// something drawable. `fn(value)` returns a context, an ImageBitmap (or
/// anything else drawImage takes), `null` for "a valid source with nothing to
/// draw yet", or `undefined` for "not an image source" (a TypeError).
unsafe extern "sysv64" fn h_source_hook(g: *mut c_void, cf: *mut c_void) -> u64 {
    let f = cf_arg(cf, 0);
    let mut hooks = hooks().lock().unwrap();
    if let Some(old) = hooks.remove(&(g as usize)) {
        crate::ffi::redjs_unprotect(g, old);
    }
    if is_cell(f) {
        crate::ffi::redjs_protect(g, f);
        hooks.insert(g as usize, f);
    }
    UNDEF
}

/// `__rjs_canvas_info(ctx)` → `{ backend, width, height, generation, originClean }` (tests, devtools).
unsafe extern "sysv64" fn h_info(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(cx) = ctx_of(g, cf_arg(cf, 0)) else { return NULL };
    let s = cx.surface.lock().unwrap();
    let out = new_object(g, UNDEF);
    define(g, out, "backend", js_string(g, s.backend_name()), 0);
    define(g, out, "width", enc_f64(s.width() as f64), 0);
    define(g, out, "height", enc_f64(s.height() as f64), 0);
    define(g, out, "generation", enc_f64(s.generation() as f64), 0);
    define(g, out, "originClean", if s.origin_clean { TRUE } else { FALSE }, 0);
    out
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

pub(crate) unsafe fn install(g: *mut c_void, event_target_proto: u64) {
    let ctx_cls = make_host_class::<Ctx>("CanvasRenderingContext2D");
    let grad_cls = make_host_class::<Arc<Mutex<Gradient>>>("CanvasGradient");
    let pat_cls = make_host_class::<Arc<Mutex<Pattern>>>("CanvasPattern");
    let off_cls = make_host_class::<Off>("OffscreenCanvas");

    let accessors: &[(&str, HostFn, HostFn)] = &[
        ("globalAlpha", get_global_alpha, set_global_alpha),
        ("globalCompositeOperation", get_composite, set_composite),
        ("imageSmoothingEnabled", get_smoothing, set_smoothing),
        ("imageSmoothingQuality", get_quality, set_quality),
        ("strokeStyle", get_stroke_style, set_stroke_style),
        ("fillStyle", get_fill_style, set_fill_style),
        ("shadowOffsetX", get_shadow_x, set_shadow_x),
        ("shadowOffsetY", get_shadow_y, set_shadow_y),
        ("shadowBlur", get_shadow_blur, set_shadow_blur),
        ("shadowColor", get_shadow_color, set_shadow_color),
        ("filter", get_filter, set_filter),
        ("lineWidth", get_line_width, set_line_width),
        ("lineCap", get_line_cap, set_line_cap),
        ("lineJoin", get_line_join, set_line_join),
        ("miterLimit", get_miter_limit, set_miter_limit),
        ("lineDashOffset", get_line_dash_offset, set_line_dash_offset),
        ("font", get_font, set_font),
        ("textAlign", get_text_align, set_text_align),
        ("textBaseline", get_text_baseline, set_text_baseline),
        ("direction", get_direction, set_direction),
        ("letterSpacing", get_letter_spacing, set_letter_spacing),
        ("fontKerning", get_font_kerning, set_font_kerning),
        ("fontStretch", get_font_stretch, set_font_stretch),
        ("fontVariantCaps", get_font_variant_caps, set_font_variant_caps),
        ("textRendering", get_text_rendering, set_text_rendering),
        ("wordSpacing", get_word_spacing, set_word_spacing),
        ("lang", get_lang, set_lang),
    ];
    let common: &[(&str, u32, HostFn)] = &[
        ("getContextAttributes", 0, m_get_context_attributes),
        ("save", 0, m_save),
        ("restore", 0, m_restore),
        ("reset", 0, m_reset),
        ("isContextLost", 0, m_is_context_lost),
        ("scale", 2, m_scale),
        ("rotate", 1, m_rotate),
        ("translate", 2, m_translate),
        ("transform", 6, m_transform),
        ("getTransform", 0, m_get_transform),
        ("setTransform", 0, m_set_transform),
        ("resetTransform", 0, m_reset_transform),
        ("createLinearGradient", 4, m_linear_gradient),
        ("createRadialGradient", 6, m_radial_gradient),
        ("createConicGradient", 3, m_conic_gradient),
        ("createPattern", 2, m_create_pattern),
        ("clearRect", 4, m_clear_rect),
        ("fillRect", 4, m_fill_rect),
        ("strokeRect", 4, m_stroke_rect),
        ("beginPath", 0, m_begin_path),
        ("fill", 0, m_fill),
        ("stroke", 0, m_stroke),
        ("clip", 0, m_clip),
        ("isPointInPath", 2, m_is_point_in_path),
        ("isPointInStroke", 2, m_is_point_in_stroke),
        ("fillText", 3, m_fill_text),
        ("strokeText", 3, m_fill_text),
        ("measureText", 1, m_measure_text),
        ("drawImage", 3, m_draw_image),
        ("createImageData", 1, m_create_image_data),
        ("getImageData", 4, m_get_image_data),
        ("putImageData", 3, m_put_image_data),
        ("setLineDash", 1, m_set_line_dash),
        ("getLineDash", 0, m_get_line_dash),
        ("closePath", 0, m_close_path),
        ("moveTo", 2, m_move_to),
        ("lineTo", 2, m_line_to),
        ("quadraticCurveTo", 4, m_quad_to),
        ("bezierCurveTo", 6, m_bezier_to),
        ("arcTo", 5, m_arc_to),
        ("rect", 4, m_rect),
        ("roundRect", 4, m_round_rect),
        ("arc", 5, m_arc),
        ("ellipse", 7, m_ellipse),
    ];
    let mut element_methods: Vec<(&str, u32, HostFn)> = common.to_vec();
    element_methods.push(("drawFocusIfNeeded", 1, m_noop));
    element_methods.push(("scrollPathIntoView", 0, m_noop));

    let ctx2d = define_class(g, &ClassSpec {
        name: "CanvasRenderingContext2D",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &element_methods,
        getters: &[("canvas", get_canvas)],
        accessors,
    });
    let offctx = define_class(g, &ClassSpec {
        name: "OffscreenCanvasRenderingContext2D",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: common,
        getters: &[("canvas", get_canvas)],
        accessors,
    });
    let grad = define_class(g, &ClassSpec {
        name: "CanvasGradient",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[("addColorStop", 2, m_add_color_stop)],
        getters: &[],
        accessors: &[],
    });
    let pat = define_class(g, &ClassSpec {
        name: "CanvasPattern",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[("setTransform", 0, m_pattern_set_transform)],
        getters: &[],
        accessors: &[],
    });
    let metrics = define_class(g, &ClassSpec {
        name: "TextMetrics",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[],
        getters: &[],
        accessors: &[],
    });
    let off = define_class(g, &ClassSpec {
        name: "OffscreenCanvas",
        arity: 2,
        construct: Some(off_construct),
        parent_proto: event_target_proto,
        methods: &[
            ("getContext", 1, off_get_context),
            ("convertToBlob", 0, off_convert_to_blob),
            ("transferToImageBitmap", 0, off_transfer),
        ],
        getters: &[],
        accessors: &[("width", off_get_width, off_set_width), ("height", off_get_height, off_set_height)],
    });

    let leaked: &'static Classes = Box::leak(Box::new(Classes {
        ctx_cls,
        ctx2d,
        offctx,
        grad_cls,
        grad,
        pat_cls,
        pat,
        off_cls,
        off,
        metrics,
        s_canvas: atom_priv(g, "redjs#canvas"),
        s_styles: atom_priv(g, "redjs#canvasStyles"),
        s_ctx: atom_priv(g, "redjs#offscreenContext"),
    }));
    registry().lock().unwrap().insert(g as usize, leaked);

    let global = glue::rjs_global(g);
    for (name, arity, f) in [
        ("__rjs_canvas_context", 5u32, h_context as HostFn),
        ("__rjs_canvas_resize", 3, h_resize),
        ("__rjs_canvas_data_url", 5, h_data_url),
        ("__rjs_canvas_blob", 5, h_blob),
        ("__rjs_canvas_source_hook", 1, h_source_hook),
        ("__rjs_canvas_info", 1, h_info),
    ] {
        define(g, global, name, make_fn(g, name, arity, f), 2);
    }
}
