//! Native `ImageBitmap`, `HTMLImageElement` (+ `Image` legacy factory) and
//! `createImageBitmap` (HTML spec subset). Pixels live in Rust memory as
//! RGBA8 (`BitmapData`), owned by the ImageBitmap host object — the C# GPU
//! upload path reads them zero-copy via `redjs_image_pixels`.
//!
//! HTMLImageElement extends EventTarget (real prototype chain, real events):
//! setting `src` decodes on a later task (spec: loading is async) and fires
//! `load`/`error` through the native dispatch (which also honors the
//! onload/onerror handler properties).
//!
//! A `blob:` URL — `src` or a `createImageBitmap` string — resolves through
//! the context's blob URL store, as `fetch()` resolves it: bytes in memory
//! are decoded where they are, a path-backed Blob is read on the blob read
//! threads under the snapshot rule, and an unknown or revoked URL is an
//! `error` (a rejection). `createImageBitmap(file)` of a path-backed File
//! reads it the same way rather than on the UI thread.
//!
//! A raster is decoded upright, its EXIF orientation applied, as a browser
//! draws and measures it (`decode`, which an embedder can call too).

use core::ffi::c_void;

use super::events;
use super::fsutil;
use super::util::*;
use crate::glue;

#[cfg(feature = "webgpu")]
pub(crate) mod external;
#[cfg(feature = "webgpu")]
pub(crate) use external::{external_pixels, ExternalImage};

pub struct BitmapData {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// HTML's origin-clean flag: false for pixels that came from a
    /// cross-origin image fetched without CORS. A canvas that draws such a
    /// bitmap loses its own flag (recorded and advised, not enforced; see
    /// canvas2d `Surface::readable`). A structured clone comes out clean.
    pub origin_clean: bool,
}

impl BitmapData {
    /// Straight RGBA pixels from this origin.
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> BitmapData {
        BitmapData { width, height, rgba, origin_clean: true }
    }
}

unsafe fn this_bitmap<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut BitmapData> {
    host_data::<BitmapData>(g, state(g).bitmap_class, cf_this(cf))
}

/// The ImageBitmap behind `v`, if it is one. A closed (or transferred-away)
/// bitmap reads as 0×0 with no pixels — HTML's [[Detached]].
pub(crate) unsafe fn bitmap_of<'a>(g: *mut c_void, v: u64) -> Option<&'a mut BitmapData> {
    host_data::<BitmapData>(g, state(g).bitmap_class, v)
}

/// Wraps decoded pixels in an ImageBitmap instance.
pub unsafe fn create_bitmap(g: *mut c_void, data: BitmapData) -> u64 {
    let st = state(g);
    host_create(g, st.bitmap_class, st.bitmap.proto, data)
}

#[crate::host_fn]
unsafe fn bitmap_get_width(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_bitmap(g, cf) {
        Some(b) => enc_i32(b.width as i32),
        None => throw_type_error(g, "ImageBitmap.width called on a non-ImageBitmap"),
    }
}

#[crate::host_fn]
unsafe fn bitmap_get_height(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_bitmap(g, cf) {
        Some(b) => enc_i32(b.height as i32),
        None => throw_type_error(g, "ImageBitmap.height called on a non-ImageBitmap"),
    }
}

#[crate::host_fn]
unsafe fn bitmap_close(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Some(b) = this_bitmap(g, cf) {
        b.rgba = Vec::new();
        b.width = 0;
        b.height = 0;
    }
    UNDEF
}

// ---------------------------------------------------------------------------
// Pixel access for other natives (OffscreenCanvas 2D shim, GPU upload)
// ---------------------------------------------------------------------------

/// RGBA pixels + dimensions of any drawable source: ImageBitmap,
/// HTMLImageElement (its loaded bitmap), a canvas (feature `canvas`), or
/// ImageData-shaped {data,width,height}.
pub unsafe fn source_pixels(g: *mut c_void, v: u64) -> Option<(Vec<u8>, u32, u32)> {
    #[cfg(feature = "webgpu")]
    if let Some(result) = super::webgpu::canvas::pixels(g, v) {
        return result.ok();
    }
    let st = state(g);
    if let Some(b) = host_data::<BitmapData>(g, st.bitmap_class, v) {
        return Some((b.rgba.clone(), b.width, b.height));
    }
    // HTMLImageElement: [[bitmap]] slot.
    let bmp = get_a(g, v, st.s_image);
    if is_cell(bmp) {
        if let Some(b) = host_data::<BitmapData>(g, st.bitmap_class, bmp) {
            return Some((b.rgba.clone(), b.width, b.height));
        }
    }
    // A canvas: an OffscreenCanvas, a 2D context, or an embedder's element.
    #[cfg(feature = "canvas")]
    if let Some(px) = super::canvas2d::canvas_pixels(g, v) {
        return Some(px);
    }
    // ImageData-shaped. Root the view while dimensions execute arbitrary
    // getters, then read/copy bytes after every observable conversion.
    let data = get(g, v, "data");
    struct PixelRoot(*mut c_void, u64);
    impl Drop for PixelRoot {
        fn drop(&mut self) {
            if is_cell(self.1) {
                unsafe {
                    crate::ffi::redjs_unprotect(self.0, self.1);
                }
            }
        }
    }
    if is_cell(data) {
        crate::ffi::redjs_protect(g, data);
    }
    let _root = PixelRoot(g, data);
    let w = dec_f64(get(g, v, "width")).unwrap_or(0.0) as u32;
    let h = dec_f64(get(g, v, "height")).unwrap_or(0.0) as u32;
    if let Some(bytes) = bufferish(g, data) {
        if let Some(n) = (w as usize)
            .checked_mul(h as usize)
            .and_then(|n| n.checked_mul(4))
        {
            if w > 0 && h > 0 && bytes.len() >= n {
                return Some((bytes[..n].to_vec(), w, h));
            }
        }
    }
    None
}

/// `__rjs_sourcePixels(src)` → Uint8Array copy (bootstrap's OffscreenCanvas).
#[crate::host_fn]
pub unsafe fn source_pixels_js(g: *mut c_void, cf: *mut c_void) -> u64 {
    match source_pixels(g, cf_arg(cf, 0)) {
        Some((px, _, _)) => make_u8(g, &px),
        None => NULL,
    }
}

// ---------------------------------------------------------------------------
// HTMLImageElement / Image
// ---------------------------------------------------------------------------

#[crate::host_fn]
unsafe fn image_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let st = state(g);
    let obj = ordinary_create(g, cf_new_target(cf), st.image.proto);
    // Legacy Image(w, h) factory semantics: presentational width/height.
    let w = dec_f64(cf_arg(cf, 0)).unwrap_or(0.0) as i32;
    let h = dec_f64(cf_arg(cf, 1)).unwrap_or(0.0) as i32;
    define(g, obj, "width", enc_i32(w.max(0)), 0);
    define(g, obj, "height", enc_i32(h.max(0)), 0);
    define(g, obj, "naturalWidth", enc_i32(0), 0);
    define(g, obj, "naturalHeight", enc_i32(0), 0);
    define(g, obj, "complete", TRUE, 0);
    define(g, obj, "crossOrigin", NULL, 0);
    define(g, obj, "decoding", js_string(g, "auto"), 0);
    define(g, obj, "loading", js_string(g, "eager"), 0);
    define(g, obj, "referrerPolicy", js_string(g, ""), 0);
    glue::rjs_define(g, obj, st.s_image, NULL, 0); // [[bitmap]]
    define(g, obj, "__rjs_src", js_string(g, ""), 2);
    obj
}

#[crate::host_fn]
unsafe fn image_get_src(g: *mut c_void, cf: *mut c_void) -> u64 {
    get(g, cf_this(cf), "__rjs_src")
}

#[crate::host_fn]
unsafe fn image_set_src(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Ok(value) = to_js_string(g, cf_arg(cf, 0)) else {
        return UNDEF;
    };
    let src = read_string(g, value).unwrap_or_default();
    set_image_source(g, cf_this(cf), src)
}

unsafe fn set_image_source(g: *mut c_void, this: u64, src: String) -> u64 {
    let (image_slot, request_slot) = {
        let st = state(g);
        (st.s_image, st.s_image_request)
    };
    let previous = dec_f64(get_a(g, this, request_slot)).unwrap_or(0.);
    let request = enc_f64(if previous >= 9_007_199_254_740_991. {
        1.
    } else {
        previous + 1.
    });
    glue::rjs_define(g, this, request_slot, request, 0);
    define(g, this, "__rjs_src", js_string(g, &src), 2);
    // Release decoded pixels immediately, including when the source is removed.
    // A late completion must neither resurrect them nor fire stale load/error.
    glue::rjs_define(g, this, image_slot, NULL, 0);
    define(g, this, "naturalWidth", enc_i32(0), 0);
    define(g, this, "naturalHeight", enc_i32(0), 0);
    define(
        g,
        this,
        "complete",
        if src.is_empty() { TRUE } else { FALSE },
        0,
    );
    if src.is_empty() {
        return UNDEF;
    }

    crate::ffi::redjs_protect(g, this);
    if is_blob_url(&src) {
        let failed = src.clone();
        blob_bytes(
            g,
            &src,
            Box::new(move |g, r| unsafe {
                if get_a(g, this, state(g).s_image_request) != request {
                    crate::ffi::redjs_unprotect(g, this);
                    return;
                }
                let decoded = match r {
                    Ok(bytes) => decode_rgba(bytes),
                    Err(why) => {
                        eprintln!("[js:image] failed to load '{failed}': {why}");
                        None
                    }
                };
                finish_load(g, this, &failed, request, decoded);
            }),
        );
        return UNDEF;
    }
    crate::web::schedule_native(
        g,
        0.0,
        Box::new(move |g| {
            if get_a(g, this, state(g).s_image_request) != request {
                crate::ffi::redjs_unprotect(g, this);
                return;
            }
            let fetched = fsutil::fetch_url(g, &src);
            let decoded = if fetched.ok {
                decode_typed(&fetched.bytes, Some(&fetched.mime))
            } else {
                None
            };
            finish_load(g, this, &src, request, decoded);
        }),
    );
    UNDEF
}

/// Native Image objects are offscreen image sources, not retained DOM nodes.
/// Their source attributes still follow the DOM disposal contract.
#[crate::host_fn]
unsafe fn image_remove_attribute(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let proto = state(g).image.proto;
    if !proto_chain_has(g, this, proto) {
        return throw_type_error(g, "HTMLImageElement.removeAttribute: illegal invocation");
    }
    let Ok(name) = to_js_string(g, cf_arg(cf, 0)) else {
        return UNDEF;
    };
    let name = read_string(g, name)
        .unwrap_or_default()
        .to_ascii_lowercase();
    match name.as_str() {
        "src" => set_image_source(g, this, String::new()),
        "crossorigin" => {
            define(g, this, "crossOrigin", NULL, 0);
            UNDEF
        }
        _ => UNDEF,
    }
}

/// The end of an `<img>` load: the bitmap and `load`, or `error`. Unprotects
/// the element the setter protected.
unsafe fn finish_load(
    g: *mut c_void,
    this: u64,
    src: &str,
    request: u64,
    decoded: Option<BitmapData>,
) {
    let (image_slot, request_slot) = {
        let st = state(g);
        (st.s_image, st.s_image_request)
    };
    if get_a(g, this, request_slot) != request {
        crate::ffi::redjs_unprotect(g, this);
        return;
    }
    match decoded {
        Some(data) => {
            // `new Image(32, 32)`'s presentational size stays; an unsized
            // image takes its natural one.
            let width = dec_f64(get(g, this, "width")).unwrap_or(0.0);
            let height = dec_f64(get(g, this, "height")).unwrap_or(0.0);
            if get_a(g, this, request_slot) != request {
                crate::ffi::redjs_unprotect(g, this);
                return;
            }
            if width == 0.0 { define(g, this, "width", enc_i32(data.width as i32), 0); }
            if height == 0.0 { define(g, this, "height", enc_i32(data.height as i32), 0); }
            define(g, this, "naturalWidth", enc_i32(data.width as i32), 0);
            define(g, this, "naturalHeight", enc_i32(data.height as i32), 0);
            let bmp = create_bitmap(g, data);
            glue::rjs_define(g, this, image_slot, bmp, 0);
            define(g, this, "complete", TRUE, 0);
            let ev = events::new_trusted_event(g, "load");
            events::dispatch(g, this, ev);
        }
        None => {
            define(g, this, "complete", TRUE, 0);
            eprintln!("[js:image] failed to load '{src}'");
            let ev = events::new_trusted_event(g, "error");
            events::dispatch(g, this, ev);
        }
    }
    crate::ffi::redjs_unprotect(g, this);
}

fn is_blob_url(s: &str) -> bool {
    s.len() >= 5 && s.as_bytes()[..5].eq_ignore_ascii_case(b"blob:")
}

/// What a blob: load hands back on the JS thread: the bytes, or why not.
type BlobBytes = Box<dyn FnOnce(*mut c_void, Result<&[u8], String>) + Send>;

/// The bytes a blob: URL names, delivered to `done` as a task of its own on
/// the JS thread (never synchronously). Memory is used where it is, with no
/// copy; a path-backed Blob is read on the blob read threads under the
/// snapshot rule (`blob::read_async`).
unsafe fn blob_bytes(g: *mut c_void, url: &str, done: BlobBytes) {
    let Some(entry) = super::url::lookup(g, url) else {
        super::blob::queue_task(
            g,
            Box::new(move |g| {
                done(g, Err("it is not in the blob URL store (it was revoked, or never created in this context)".into()))
            }),
        );
        return;
    };
    let range = entry.range;
    if matches!(range.source, super::blob::Source::Disk(_)) {
        super::blob::read_async(
            g,
            range,
            Box::new(move |g, r| match r {
                Ok(bytes) => done(g, Ok(&bytes)),
                Err(e) => done(g, Err(format!("{}: {}", e.name(), e.message()))),
            }),
        );
        return;
    }
    super::blob::queue_task(
        g,
        Box::new(move |g| match range.read_in_place() {
            Ok(bytes) => done(g, Ok(bytes)),
            Err(e) => done(g, Err(format!("{}: {}", e.name(), e.message()))),
        }),
    );
}

#[crate::host_fn]
unsafe fn image_decode_method(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if truthy_flag(get(g, this, "complete")) {
        return resolved_promise(g, UNDEF);
    }
    // Resolve/reject on the load/error events (once).
    let (mut res, mut rej, mut p) = (0u64, 0u64, 0u64);
    if crate::ffi::redjs_promise_create(g, &mut res, &mut rej, &mut p) != 0 {
        return UNDEF;
    }
    // addEventListener(this, 'load', res, {once:true}) via the JS-visible API.
    let add = get(g, this, "addEventListener");
    let once_opts = new_object(g, UNDEF);
    define(g, once_opts, "once", TRUE, 0);
    let load_s = js_string(g, "load");
    let err_s = js_string(g, "error");
    let _ = call(g, add, this, &[load_s, res, once_opts]);
    let _ = call(g, add, this, &[err_s, rej, once_opts]);
    p
}

/// Decode an image's bytes to straight RGBA: a raster format through `image`,
/// SVG through resvg (by its type, or by its first bytes when the type is
/// missing or generic, as a Blob's or a file's often is).
fn decode_rgba(bytes: &[u8]) -> Option<BitmapData> {
    decode_typed(bytes, None)
}

fn decode_typed(bytes: &[u8], mime: Option<&str>) -> Option<BitmapData> {
    decode(bytes, mime)
}

/// An image file's bytes decoded to straight RGBA, synchronously: a raster
/// format through `image`, turned upright by its EXIF orientation (CSS
/// `image-orientation: from-image`, the default every browser draws and
/// measures with), or an SVG rasterized at its intrinsic size. `mime` may be
/// `None`: the raster format is sniffed from the bytes. `None` when the bytes
/// are no image this build decodes.
///
/// For an embedder that holds an image's bytes and needs its pixels now
/// (skathi's `drawImage` of an `<img>` the host loaded).
pub fn decode(bytes: &[u8], mime: Option<&str>) -> Option<BitmapData> {
    use image::ImageDecoder;
    if is_svg(bytes, mime) {
        return svg::rasterize(bytes);
    }
    let mut decoder = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_decoder()
        .ok()?;
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = image::DynamicImage::from_decoder(decoder).ok()?;
    img.apply_orientation(orientation);
    let rgba = img.into_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    Some(BitmapData::new(w, h, rgba.into_raw()))
}

/// Decode a small native icon without allocating its intrinsic SVG dimensions.
/// Input is bounded to 2 MiB; raster images are limited to 4096 per side and
/// 32 MiB of decoder allocation. No upscaling; EXIF orientation is preserved.
/// The returned bitmap is straight RGBA, with neither dimension above `max_side`.
pub fn decode_thumbnail(bytes: &[u8], mime: Option<&str>, max_side: u32) -> Option<BitmapData> {
    use image::ImageDecoder;
    if max_side == 0 || max_side > 512 || bytes.len() > 2 * 1024 * 1024 { return None; }
    if is_svg(bytes, mime) { return svg::rasterize_at(bytes, max_side as f32); }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = image::DynamicImage::from_decoder(decoder).ok()?;
    img.apply_orientation(orientation);
    if img.width() > max_side || img.height() > max_side { img = img.thumbnail(max_side, max_side); }
    let rgba = img.into_rgba8();
    Some(BitmapData::new(rgba.width(), rgba.height(), rgba.into_raw()))
}

/// `image/svg+xml`, or markup that opens an `<svg>` element (after a BOM,
/// whitespace, an XML declaration, comments or a doctype).
fn is_svg(bytes: &[u8], mime: Option<&str>) -> bool {
    if let Some(m) = mime {
        let m = m.split(';').next().unwrap_or("").trim();
        if m.eq_ignore_ascii_case("image/svg+xml") {
            return true;
        }
    }
    let head = &bytes[..bytes.len().min(1024)];
    let head = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    let start = head.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(head.len());
    let head = &head[start..];
    if !head.starts_with(b"<") {
        return false;
    }
    head.windows(4).any(|w| w.eq_ignore_ascii_case(b"<svg"))
}

#[cfg(feature = "svg-images")]
mod svg {
    use super::BitmapData;
    use resvg::{tiny_skia, usvg};
    use std::sync::{Arc, OnceLock};

    /// The largest side a rasterized SVG gets; a bigger one is scaled down to
    /// fit, keeping its aspect ratio.
    const MAX_SIDE: f32 = 8192.0;

    /// System fonts, loaded once and only for an SVG with `<text>` in it: icon
    /// SVGs (paths only) never pay for the font scan.
    fn fonts() -> Arc<usvg::fontdb::Database> {
        static DB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
        Arc::clone(DB.get_or_init(|| {
            let mut db = usvg::fontdb::Database::new();
            db.load_system_fonts();
            Arc::new(db)
        }))
    }

    /// Rasterize at the SVG's intrinsic size (its `width`/`height`, else its
    /// `viewBox`), which is what `naturalWidth`/`naturalHeight` then report.
    pub(super) fn rasterize(bytes: &[u8]) -> Option<BitmapData> { rasterize_at(bytes, MAX_SIDE) }

    pub(super) fn rasterize_at(bytes: &[u8], max_side: f32) -> Option<BitmapData> {
        let mut opt = usvg::Options::default();
        if bytes.windows(5).any(|w| w == b"<text") {
            opt.fontdb = fonts();
        }
        let tree = match usvg::Tree::from_data(bytes, &opt) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[js:image] SVG parse failed: {e}");
                return None;
            }
        };
        let size = tree.size();
        let scale = (max_side / size.width().max(size.height())).min(1.0);
        let w = (size.width() * scale).round().max(1.0) as u32;
        let h = (size.height() * scale).round().max(1.0) as u32;
        let mut pixmap = tiny_skia::Pixmap::new(w, h)?;
        let sx = w as f32 / size.width();
        let sy = h as f32 / size.height();
        resvg::render(&tree, tiny_skia::Transform::from_scale(sx, sy), &mut pixmap.as_mut());
        // tiny-skia is premultiplied; a bitmap here is straight RGBA.
        let mut rgba = pixmap.take();
        for px in rgba.chunks_exact_mut(4) {
            let a = px[3] as u32;
            if a != 0 && a != 255 {
                for c in &mut px[..3] {
                    *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
        Some(BitmapData::new(w, h, rgba))
    }
}

#[cfg(not(feature = "svg-images"))]
mod svg {
    use super::BitmapData;

    pub(super) fn rasterize(bytes: &[u8]) -> Option<BitmapData> { rasterize_at(bytes, 8192.0) }

    pub(super) fn rasterize_at(_bytes: &[u8], _max_side: f32) -> Option<BitmapData> {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            eprintln!("[js:image] advisory: this build has no SVG decoder (redjs feature `svg-images`); SVG images fire `error`");
        });
        None
    }
}

// ---------------------------------------------------------------------------
// createImageBitmap
// ---------------------------------------------------------------------------

fn crop(src: &BitmapData, sx: i64, sy: i64, sw: u32, sh: u32) -> BitmapData {
    let mut out = vec![0u8; (sw * sh * 4) as usize];
    for row in 0..sh as i64 {
        let y = sy + row;
        if y < 0 || y >= src.height as i64 {
            continue;
        }
        for col in 0..sw as i64 {
            let x = sx + col;
            if x < 0 || x >= src.width as i64 {
                continue;
            }
            let si = ((y as u32 * src.width + x as u32) * 4) as usize;
            let di = ((row as u32 * sw + col as u32) * 4) as usize;
            out[di..di + 4].copy_from_slice(&src.rgba[si..si + 4]);
        }
    }
    BitmapData { width: sw, height: sh, rgba: out, origin_clean: src.origin_clean }
}

#[crate::host_fn]
unsafe fn create_image_bitmap(g: *mut c_void, cf: *mut c_void) -> u64 {
    let source = cf_arg(cf, 0);

    // Region overload: (source, sx, sy, sw, sh, options?)
    let region = match (dec_f64(cf_arg(cf, 1)), dec_f64(cf_arg(cf, 2)), dec_f64(cf_arg(cf, 3)), dec_f64(cf_arg(cf, 4))) {
        (Some(sx), Some(sy), Some(sw), Some(sh)) => {
            if sw <= 0.0 || sh <= 0.0 {
                return rejected_promise(g, 2, "createImageBitmap: crop rect width/height must be positive");
            }
            Some((sx as i64, sy as i64, sw as u32, sh as u32))
        }
        _ => None,
    };

    // A path-backed Blob and a blob: URL settle later: the bytes are read off
    // the JS thread (or looked up in the store), then decoded here.
    let deferred: Option<Box<dyn FnOnce(*mut c_void, BlobBytes)>> = match super::blob::data_of(g, source) {
        Some(b) if matches!(b.source, super::blob::Source::Disk(_)) => {
            let range = b.range();
            Some(Box::new(move |g, done: BlobBytes| {
                super::blob::read_async(
                    g,
                    range,
                    Box::new(move |g, r| match r {
                        Ok(bytes) => done(g, Ok(&bytes)),
                        Err(e) => done(g, Err(format!("{}: {}", e.name(), e.message()))),
                    }),
                )
            }))
        }
        Some(_) => None,
        None if glue::rjs_kind(g, source) == 5 => {
            let s = read_string(g, source).unwrap_or_default();
            is_blob_url(&s).then(|| Box::new(move |g, done: BlobBytes| unsafe { blob_bytes(g, &s, done) }) as Box<dyn FnOnce(*mut c_void, BlobBytes)>)
        }
        None => None,
    };
    if let Some(start) = deferred {
        let (mut res, mut rej, mut p) = (0u64, 0u64, 0u64);
        if crate::ffi::redjs_promise_create(g, &mut res, &mut rej, &mut p) != 0 {
            return UNDEF;
        }
        crate::ffi::redjs_protect(g, res);
        crate::ffi::redjs_protect(g, rej);
        start(
            g,
            Box::new(move |g, r| unsafe {
                let decoded = match r {
                    Ok(bytes) => decode_rgba(bytes).ok_or_else(|| "createImageBitmap: image decode failed".to_string()),
                    Err(why) => Err(format!("createImageBitmap: the source could not be read ({why})")),
                };
                match decoded {
                    Ok(full) => {
                        let data = match region {
                            Some((sx, sy, sw, sh)) => crop(&full, sx, sy, sw, sh),
                            None => full,
                        };
                        let bmp = create_bitmap(g, data);
                        let _ = call(g, res, UNDEF, &[bmp]);
                    }
                    Err(msg) => {
                        let err = super::fetch::make_error(g, super::fetch::TYPE_ERR, "", &msg);
                        let _ = call(g, rej, UNDEF, &[err]);
                    }
                }
                crate::ffi::redjs_unprotect(g, res);
                crate::ffi::redjs_unprotect(g, rej);
            }),
        );
        return p;
    }

    let full = match acquire_pixels(g, source) {
        Ok(d) => d,
        Err(msg) => return rejected_promise(g, 1, &msg),
    };
    let data = match region {
        Some((sx, sy, sw, sh)) => crop(&full, sx, sy, sw, sh),
        None => full,
    };
    let bmp = create_bitmap(g, data);
    resolved_promise(g, bmp)
}

unsafe fn acquire_pixels(g: *mut c_void, source: u64) -> Result<BitmapData, String> {
    // Drawable sources first (ImageBitmap / HTMLImageElement / ImageData).
    if let Some((px, w, h)) = source_pixels(g, source) {
        return Ok(BitmapData::new(w, h, px));
    }
    // Blob.
    if let Some(r) = super::blob::bytes_of(g, source, "createImageBitmap") {
        let bytes = r.map_err(|e| super::blob::sync_read_failed("createImageBitmap", e))?;
        return decode_rgba(&bytes).ok_or_else(|| "createImageBitmap: image decode failed".to_string());
    }
    // BufferSource.
    if let Some(bytes) = bufferish(g, source) {
        let owned = bytes.to_vec();
        return decode_rgba(&owned).ok_or_else(|| "createImageBitmap: image decode failed".to_string());
    }
    // Non-standard convenience kept from the old player: a path string.
    if glue::rjs_kind(g, source) == 5 {
        let path = read_string(g, source).unwrap_or_default();
        let fetched = fsutil::fetch_url(g, &path);
        if !fetched.ok {
            return Err(format!("createImageBitmap: not found: {path}"));
        }
        return decode_typed(&fetched.bytes, Some(&fetched.mime)).ok_or_else(|| "createImageBitmap: image decode failed".to_string());
    }
    Err("createImageBitmap: unsupported source".to_string())
}

// ---------------------------------------------------------------------------

pub unsafe fn install(g: *mut c_void, event_target_proto: u64) -> (crate::capi::JSClassRef, Class, Class) {
    let bitmap_cls = make_host_class::<BitmapData>("ImageBitmap");
    let bitmap = define_class(g, &ClassSpec {
        name: "ImageBitmap",
        arity: 0,
        construct: None, // spec: illegal constructor
        parent_proto: 0,
        methods: &[("close", 0, bitmap_close)],
        getters: &[("width", bitmap_get_width), ("height", bitmap_get_height)],
        accessors: &[],
    });

    let image = define_class(g, &ClassSpec {
        name: "HTMLImageElement",
        arity: 0,
        construct: Some(image_construct),
        parent_proto: event_target_proto,
        methods: &[("decode", 0, image_decode_method), ("removeAttribute", 1, image_remove_attribute)],
        getters: &[],
        accessors: &[("src", image_get_src, image_set_src)],
    });

    // `Image` — same constructor & prototype (spec: Image creates an
    // HTMLImageElement); expose as a distinct global.
    let n16: Vec<u16> = "Image".encode_utf16().collect();
    let mut image_ctor2 = UNDEF;
    glue::rjs_function_create_full(
        g,
        n16.as_ptr(),
        n16.len(),
        2,
        ctor_requires_new as *const c_void,
        image_construct as *const c_void,
        &mut image_ctor2,
    );
    define(g, image_ctor2, "prototype", image.proto, 1 | 2 | 4);
    define(g, glue::rjs_global(g), "Image", image_ctor2, 2);

    // createImageBitmap global.
    let n16: Vec<u16> = "createImageBitmap".encode_utf16().collect();
    let mut cib = UNDEF;
    glue::rjs_function_create(g, n16.as_ptr(), n16.len(), 2, create_image_bitmap as *const c_void, &mut cib);
    define(g, glue::rjs_global(g), "createImageBitmap", cib, 2);

    // __rjs_sourcePixels for the OffscreenCanvas JS stub.
    let n16: Vec<u16> = "__rjs_sourcePixels".encode_utf16().collect();
    let mut sp = UNDEF;
    glue::rjs_function_create(g, n16.as_ptr(), n16.len(), 1, source_pixels_js as *const c_void, &mut sp);
    define(g, glue::rjs_global(g), "__rjs_sourcePixels", sp, 2);

    (bitmap_cls, bitmap, image)
}

#[cfg(test)]
mod jsc_tests {
    use crate::webapi::file::testing::*;
    use crate::webapi::testing::*;

    /// A 3×2 PNG, encoded here so the test carries no binary fixture.
    fn png() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(3, 2, image::Rgba([255, 0, 0, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn removing_image_source_releases_pixels_and_fences_queued_loads() {
        let c = file_ctx();
        let data = crate::webapi::fsutil::b64_encode(&png());
        s(&c, &format!("globalThis.imageUrl = 'data:image/png;base64,{data}'; 0"));
        assert_eq!(run(&c, r#"
          const image = new Image(32, 32); let loads=0, errors=0;
          image.onload=()=>loads++;image.onerror=()=>errors++;
          image.src=imageUrl;
          await new Promise(resolve=>setTimeout(resolve,10));
          if(__rjs_sourcePixels(image).length!==24)throw Error('No decoded bitmap');
          image.removeAttribute('SRC');
          if(image.src!==''||image.naturalWidth!==0||image.naturalHeight!==0||!image.complete||__rjs_sourcePixels(image)!==null)throw Error('Source removal retained pixels');
          image.src=imageUrl;image.removeAttribute('src');
          await new Promise(resolve=>setTimeout(resolve,10));
          if(loads!==1||errors!==0||image.naturalWidth!==0)throw Error('Canceled source resurrected');
          image.src=imageUrl;image.src=imageUrl;
          await new Promise(resolve=>setTimeout(resolve,10));
          if(loads!==2||errors!==0||image.naturalWidth!==3)throw Error('Replaced same-URL request was not fenced');
          image.src='';
          return [image.width,image.height,image.complete,__rjs_sourcePixels(image)===null].join(',');
        "#), "32,32,true,true");
    }

    #[test]
    fn source_removal_from_a_dimension_getter_cannot_publish_a_stale_image() {
        let c = file_ctx();
        let data = crate::webapi::fsutil::b64_encode(&png());
        s(&c, &format!("globalThis.imageUrl = 'data:image/png;base64,{data}'; 0"));
        assert_eq!(run(&c, r#"
          const image=new Image();let loads=0;
          image.onload=()=>loads++;
          Object.defineProperty(image,'width',{configurable:true,get(){image.removeAttribute('src');return 0;}});
          image.src=imageUrl;
          await new Promise(resolve=>setTimeout(resolve,10));
          return [loads,image.src,image.naturalWidth,image.complete,__rjs_sourcePixels(image)===null].join(',');
        "#), "0,,0,true,true");
    }

    const LOAD: &str = "const load = (src) => new Promise(r => { const i = new Image(); \
        i.onload = () => r([\x27load\x27, i.naturalWidth, i.naturalHeight, i.complete].join()); \
        i.onerror = () => r(\x27error\x27); i.src = src; });";

    /// `new Image().src = URL.createObjectURL(blob)` fires `load` (m25 review,
    /// finding 1: it went through the local-file loader, which has no blob:
    /// arm, and always fired `error`) — in memory and path-backed, with the
    /// snapshot rule, and `error` for a revoked URL.
    #[test]
    fn an_image_loads_a_blob_url_from_memory_or_from_a_picked_file() {
        let c = file_ctx();
        let bytes = png();
        let t = TempFile::new("pic.png", &bytes);
        t.expose(&c, "picked");
        unsafe {
            let g = c.raw();
            let proto = crate::webapi::util::get(g, crate::webapi::util::get(g, crate::glue::rjs_global(g), "Blob"), "prototype");
            let b = crate::webapi::blob::create(g, proto, bytes.clone(), "image/png".into());
            crate::webapi::util::define(g, crate::glue::rjs_global(g), "mem", b, 0);
        }
        assert_eq!(run(&c, &format!("{LOAD} return await load(URL.createObjectURL(mem));")), "load,3,2,true");
        assert_eq!(run(&c, &format!("{LOAD} return await load(URL.createObjectURL(picked));")), "load,3,2,true");
        assert_eq!(run(&c, &format!("{LOAD} return await load(URL.createObjectURL(picked.slice(0, 10)));")), "error");
        assert_eq!(
            run(&c, &format!("{LOAD} const u = URL.createObjectURL(mem); URL.revokeObjectURL(u); return await load(u);")),
            "error"
        );
        // The load is a task of its own, never synchronous.
        ok(&c, "(() => { const i = new Image(); globalThis.fired = false; i.onload = () => { fired = true; }; \
                 i.src = URL.createObjectURL(mem); return fired === false && i.complete === false; })()");
        // A file that changed since it was picked fails, as text() does.
        s(&c, "globalThis.u = URL.createObjectURL(picked); 0");
        std::fs::write(&t.0, b"no longer the picked file").unwrap();
        assert_eq!(run(&c, &format!("{LOAD} return await load(u);")), "error");
    }

    /// `data:image/svg+xml;charset=utf-8,` + `encodeURIComponent(svg)` — the
    /// Argon client's taskbar icons (client sweep 1, #26) — and the other
    /// spellings of an SVG image: base64, a Blob without a type.
    #[cfg(feature = "svg-images")]
    #[test]
    fn an_svg_image_loads_at_its_intrinsic_size_and_draws() {
        let c = file_ctx();
        let red = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24'><rect width='24' height='24' fill='#ff0000'/></svg>";
        s(&c, &format!("globalThis.RED = {red:?}; 0"));
        assert_eq!(
            run(&c, &format!("{LOAD} return await load('data:image/svg+xml;charset=utf-8,' + encodeURIComponent(RED));")),
            "load,24,24,true"
        );
        assert_eq!(
            run(&c, &format!("{LOAD} return await load('data:image/svg+xml;base64,' + btoa(RED));")),
            "load,24,24,true"
        );
        // width/height win over the viewBox, as for an <img>.
        assert_eq!(
            run(&c, &format!("{LOAD} return await load('data:image/svg+xml,' + encodeURIComponent(\
                 \"<svg xmlns='http://www.w3.org/2000/svg' width='40' height='20' viewBox='0 0 4 2'/>\"));")),
            "load,40,20,true"
        );
        // A Blob with no type is sniffed.
        assert_eq!(
            run(&c, &format!("{LOAD} return await load(URL.createObjectURL(new Blob([RED])));")),
            "load,24,24,true"
        );
        // The pixels are the drawing, straight RGBA, and new Image(w, h) keeps its size.
        assert_eq!(
            run(
                &c,
                "const i = new Image(32, 32); \
                 await new Promise((r, j) => { i.onload = r; i.onerror = j; \
                   i.src = 'data:image/svg+xml;charset=utf-8,' + encodeURIComponent(RED); }); \
                 const px = __rjs_sourcePixels(i); \
                 return [i.width, i.naturalWidth, px.length, px[0], px[1], px[2], px[3]].join();"
            ),
            "32,24,2304,255,0,0,255"
        );
        // Not an SVG at all: `error`, as a browser reports a broken image.
        assert_eq!(
            run(&c, &format!("{LOAD} return await load('data:image/svg+xml,' + encodeURIComponent('<svg'));")),
            "error"
        );
    }

    #[cfg(not(feature = "svg-images"))]
    #[test]
    fn an_svg_image_reports_error_when_the_decoder_is_disabled() {
        let c = file_ctx();
        assert_eq!(run(&c, &format!("{LOAD} return await load('data:image/svg+xml,<svg xmlns=\"http://www.w3.org/2000/svg\"/>');")), "error");
    }

    #[test]
    fn svg_is_recognised_by_type_or_by_its_first_bytes() {
        assert!(super::is_svg(b"anything", Some("image/svg+xml; charset=utf-8")));
        assert!(super::is_svg(b"\xEF\xBB\xBF  <?xml version='1.0'?><!-- x --><svg/>", None));
        assert!(super::is_svg(b"<!DOCTYPE svg><svg xmlns='http://www.w3.org/2000/svg'/>", Some("text/plain")));
        assert!(!super::is_svg(b"\x89PNG\r\n\x1a\n", Some("image/png")));
        assert!(!super::is_svg(b"GIF89a <svg", None), "markup must come first");
    }

    #[test]
    fn create_image_bitmap_takes_a_picked_file_and_a_blob_url() {
        let c = file_ctx();
        let t = TempFile::new("pic.png", &png());
        t.expose(&c, "picked");
        assert_eq!(run(&c, "const b = await createImageBitmap(picked); return [b.width, b.height].join();"), "3,2");
        assert_eq!(run(&c, "const b = await createImageBitmap(picked, 1, 0, 2, 2); return [b.width, b.height].join();"), "2,2");
        assert_eq!(
            run(&c, "const b = await createImageBitmap(URL.createObjectURL(picked)); return [b.width, b.height].join();"),
            "3,2"
        );
        assert!(run(&c, "const u = URL.createObjectURL(picked); URL.revokeObjectURL(u); await createImageBitmap(u);")
            .starts_with("ERROR: TypeError"));
        // The bitmap of a loaded <img> (the canvas-thumbnail idiom).
        assert_eq!(
            run(&c, &format!("{LOAD} const i = new Image(); await new Promise(r => {{ i.onload = r; i.src = URL.createObjectURL(picked); }}); \
                 const b = await createImageBitmap(i); return [b.width, b.height].join();")),
            "3,2"
        );
        std::fs::write(&t.0, b"changed").unwrap();
        let r = run(&c, "await createImageBitmap(picked);");
        assert!(r.starts_with("ERROR: TypeError") && r.contains("NotReadableError"), "{r}");
    }
}

#[cfg(test)]
mod decode_tests {
    use super::{decode, decode_thumbnail};

    #[test]
    fn thumbnails_bound_dimensions_preserve_orientation_and_reject_invalid_inputs() {
        let turned = decode_thumbnail(&jpeg(Some(6)), None, 8).unwrap();
        assert_eq!((turned.width, turned.height, turned.rgba.len()), (4, 8, 128));
        assert!(decode_thumbnail(b"bad", None, 16).is_none());
        assert!(decode_thumbnail(&jpeg(None), None, 0).is_none());
        assert!(decode_thumbnail(&jpeg(None), None, 513).is_none());
        assert!(decode_thumbnail(&vec![0; 2 * 1024 * 1024 + 1], None, 16).is_none());
    }

    #[cfg(feature = "svg-images")]
    #[test]
    fn svg_thumbnail_renders_at_requested_size_instead_of_intrinsic_allocation() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100000" height="50000"><rect width="100000" height="50000" fill="red" fill-opacity="0.5"/></svg>"#;
        let icon = decode_thumbnail(svg, None, 32).unwrap();
        assert_eq!((icon.width, icon.height, icon.rgba.len()), (32, 16, 2048));
        assert_eq!(icon.rgba[0], 255);
        assert!((127..=128).contains(&icon.rgba[3]));
    }

    /// A 32x16 JPEG, red on the left and blue on the right (wide enough that
    /// chroma subsampling keeps the halves apart), with an APP1 Exif segment
    /// carrying `orientation` when given.
    fn jpeg(orientation: Option<u16>) -> Vec<u8> {
        let img = image::RgbImage::from_fn(32, 16, |x, _| if x < 16 { image::Rgb([255, 0, 0]) } else { image::Rgb([0, 0, 255]) });
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
        let jpeg = out.into_inner();
        let Some(o) = orientation else { return jpeg };
        // TIFF, big-endian: one IFD entry, Orientation (0x0112), SHORT, 1.
        let mut tiff = vec![0x4D, 0x4D, 0, 0x2A, 0, 0, 0, 8, 0, 1, 0x01, 0x12, 0, 3, 0, 0, 0, 1];
        tiff.extend_from_slice(&[(o >> 8) as u8, o as u8, 0, 0, 0, 0, 0, 0]);
        let len = (2 + 6 + tiff.len()) as u16;
        let mut app1 = vec![0xFF, 0xE1, (len >> 8) as u8, len as u8];
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&tiff);
        let mut with = jpeg[..2].to_vec();
        with.extend_from_slice(&app1);
        with.extend_from_slice(&jpeg[2..]);
        with
    }

    /// A photo is decoded upright, as a browser draws and measures it: EXIF
    /// orientation 6 (rotate 90 degrees clockwise) turns 32x16 into 16x32, red on top.
    #[test]
    fn a_photo_decodes_upright_by_its_exif_orientation() {
        let plain = decode(&jpeg(None), None).unwrap();
        assert_eq!((plain.width, plain.height), (32, 16));
        let turned = decode(&jpeg(Some(6)), Some("image/jpeg")).unwrap();
        assert_eq!((turned.width, turned.height), (16, 32));
        let at = |x: usize, y: usize| &turned.rgba[(y * 16 + x) * 4..(y * 16 + x) * 4 + 4];
        assert!(at(8, 4)[0] > 200 && at(8, 4)[2] < 60, "{:?}", at(8, 4));
        assert!(at(8, 27)[0] < 60 && at(8, 27)[2] > 200, "{:?}", at(8, 27));
        assert!(turned.origin_clean);
        assert!(decode(b"not an image", None).is_none());
    }
}
