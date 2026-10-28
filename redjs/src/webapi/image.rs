//! Native `ImageBitmap`, `HTMLImageElement` (+ `Image` legacy factory) and
//! `createImageBitmap` (HTML spec subset). Pixels live in Rust memory as
//! RGBA8 (`BitmapData`), owned by the ImageBitmap host object — the C# GPU
//! upload path reads them zero-copy via `redjs_image_pixels`.
//!
//! HTMLImageElement extends EventTarget (real prototype chain, real events):
//! setting `src` decodes on the next pump tick (spec: loading is async) and
//! fires `load`/`error` through the native dispatch (which also honors the
//! onload/onerror handler properties).

use core::ffi::c_void;

use super::events;
use super::fsutil;
use super::util::*;
use crate::glue;

pub struct BitmapData {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
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

unsafe extern "sysv64" fn bitmap_get_width(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_bitmap(g, cf) {
        Some(b) => enc_i32(b.width as i32),
        None => throw_type_error(g, "ImageBitmap.width called on a non-ImageBitmap"),
    }
}

unsafe extern "sysv64" fn bitmap_get_height(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_bitmap(g, cf) {
        Some(b) => enc_i32(b.height as i32),
        None => throw_type_error(g, "ImageBitmap.height called on a non-ImageBitmap"),
    }
}

unsafe extern "sysv64" fn bitmap_close(g: *mut c_void, cf: *mut c_void) -> u64 {
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
/// HTMLImageElement (its loaded bitmap), or ImageData-shaped {data,width,height}.
pub unsafe fn source_pixels(g: *mut c_void, v: u64) -> Option<(Vec<u8>, u32, u32)> {
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
    // ImageData-shaped.
    let data = get(g, v, "data");
    if let Some(bytes) = bufferish(g, data) {
        let w = dec_f64(get(g, v, "width")).unwrap_or(0.0) as u32;
        let h = dec_f64(get(g, v, "height")).unwrap_or(0.0) as u32;
        if w > 0 && h > 0 && bytes.len() >= (w * h * 4) as usize {
            return Some((bytes[..(w * h * 4) as usize].to_vec(), w, h));
        }
    }
    None
}

/// `__rjs_sourcePixels(src)` → Uint8Array copy (bootstrap's OffscreenCanvas).
pub unsafe extern "sysv64" fn source_pixels_js(g: *mut c_void, cf: *mut c_void) -> u64 {
    match source_pixels(g, cf_arg(cf, 0)) {
        Some((px, _, _)) => make_u8(g, &px),
        None => NULL,
    }
}

// ---------------------------------------------------------------------------
// HTMLImageElement / Image
// ---------------------------------------------------------------------------

unsafe extern "sysv64" fn image_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
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

unsafe extern "sysv64" fn image_get_src(g: *mut c_void, cf: *mut c_void) -> u64 {
    get(g, cf_this(cf), "__rjs_src")
}

unsafe extern "sysv64" fn image_set_src(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let src = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    define(g, this, "__rjs_src", js_string(g, &src), 2);
    if src.is_empty() {
        return UNDEF;
    }
    define(g, this, "complete", FALSE, 0);

    // Spec: loading is async — decode on the next pump tick, then dispatch
    // load/error through the real EventTarget machinery.
    crate::ffi::redjs_protect(g, this);
    crate::web::schedule_native(g, 0.0, Box::new(move |g| {
        let st = state(g);
        let fetched = fsutil::fetch_url(g, &src);
        let decoded = if fetched.ok { decode_rgba(&fetched.bytes) } else { None };
        match decoded {
            Some(data) => {
                define(g, this, "width", enc_i32(data.width as i32), 0);
                define(g, this, "height", enc_i32(data.height as i32), 0);
                define(g, this, "naturalWidth", enc_i32(data.width as i32), 0);
                define(g, this, "naturalHeight", enc_i32(data.height as i32), 0);
                let bmp = create_bitmap(g, data);
                glue::rjs_define(g, this, st.s_image, bmp, 0);
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
    }));
    UNDEF
}

unsafe extern "sysv64" fn image_decode_method(g: *mut c_void, cf: *mut c_void) -> u64 {
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

fn decode_rgba(bytes: &[u8]) -> Option<BitmapData> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    Some(BitmapData { width: w, height: h, rgba: rgba.into_raw() })
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
    BitmapData { width: sw, height: sh, rgba: out }
}

unsafe extern "sysv64" fn create_image_bitmap(g: *mut c_void, cf: *mut c_void) -> u64 {
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
        return Ok(BitmapData { width: w, height: h, rgba: px });
    }
    // Blob.
    if let Some(bytes) = super::blob::bytes_of(g, source) {
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
        return decode_rgba(&fetched.bytes).ok_or_else(|| "createImageBitmap: image decode failed".to_string());
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
        methods: &[("decode", 0, image_decode_method)],
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
