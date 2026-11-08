//! Owned snapshots for WebGPU external-image uploads. No JS-backed slice escapes.
use super::*;

pub(crate) struct ExternalImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub premultiplied: bool,
    pub origin_clean: bool,
}
impl ExternalImage {
    pub(crate) fn straight(bitmap: &BitmapData) -> Self {
        Self {
            width: bitmap.width,
            height: bitmap.height,
            rgba: bitmap.rgba.clone(),
            premultiplied: false,
            origin_clean: bitmap.origin_clean,
        }
    }
}
struct Root(*mut c_void, u64);
impl Drop for Root {
    fn drop(&mut self) {
        if is_cell(self.1) {
            unsafe {
                crate::ffi::redjs_unprotect(self.0, self.1);
            }
        }
    }
}
pub(crate) unsafe fn fail<T>(g: *mut c_void, name: &str, message: &str) -> Result<T, ()> {
    super::super::domexception::throw(g, name, message);
    Err(())
}

/// Snapshot only after the WebGPU descriptor getters finished. The hook may
/// reenter JS, close a bitmap, resize a canvas or collect the resolved wrapper.
pub(crate) unsafe fn external_pixels(
    g: *mut c_void,
    value: u64,
    depth: u8,
) -> Result<ExternalImage, ()> {
    if !is_cell(value) || depth > 8 {
        return fail(
            g,
            "TypeError",
            "Expected an external image source (source hooks cannot form cycles)",
        );
    }
    crate::ffi::redjs_protect(g, value);
    let _root = Root(g, value);
    if let Some(bitmap) = bitmap_of(g, value) {
        if bitmap.width == 0 || bitmap.height == 0 {
            return fail(g, "InvalidStateError", "ImageBitmap is closed or detached");
        }
        return Ok(ExternalImage::straight(bitmap));
    }
    if let Some((data, width, height)) = super::super::platform::image_data_parts(g, value) {
        crate::ffi::redjs_protect(g, data);
        let _data = Root(g, data);
        let mut info = crate::ffi::RjsTaInfo::default();
        if glue::rjs_typedarray_info(g, data, &mut info) != 0 || info.detached != 0 {
            return fail(g, "InvalidStateError", "ImageData buffer is detached");
        }
        if info.shared != 0 {
            return fail(
                g,
                "NotSupportedError",
                "Shared image uploads require an atomic native copy path",
            );
        }
        let Some(bytes) = bufferish(g, data) else {
            return fail(g, "InvalidStateError", "ImageData buffer is detached");
        };
        let length = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4));
        let Some(length) = length.filter(|n| *n != 0 && *n == bytes.len()) else {
            return fail(
                g,
                "InvalidStateError",
                "ImageData buffer is detached or has an invalid size",
            );
        };
        return Ok(ExternalImage {
            width,
            height,
            rgba: bytes[..length].to_vec(),
            premultiplied: false,
            origin_clean: true,
        });
    }
    let (image_proto, image_slot) = {
        let st = state(g);
        (st.image.proto, st.s_image)
    };
    if proto_chain_has(g, value, image_proto) {
        // Match the native image origin policy, but keep it attached to this
        // snapshot. Do not hold bitmap references while JS getters execute.
        let src = try_get(g, value, "__rjs_src")?;
        let src = read_string(g, src).unwrap_or_default();
        let cors = try_get(g, value, "crossOrigin")?;
        let remote = src.starts_with("http://") || src.starts_with("https://");
        let clean = !remote || (cors != NULL && cors != UNDEF);
        let loaded = get_a(g, value, image_slot);
        let Some(bitmap) = bitmap_of(g, loaded) else {
            return fail(
                g,
                "InvalidStateError",
                "HTMLImageElement has no decoded image",
            );
        };
        if bitmap.width == 0 || bitmap.height == 0 {
            return fail(g, "InvalidStateError", "HTMLImageElement has no pixels");
        }
        let mut pixels = ExternalImage::straight(bitmap);
        pixels.origin_clean &= clean;
        return Ok(pixels);
    }
    #[cfg(feature = "canvas")]
    {
        return super::super::canvas2d::external_pixels(g, value, depth);
    }
    #[cfg(not(feature = "canvas"))]
    {
        fail(g, "TypeError", "Expected ImageBitmap, ImageData or HTMLImageElement; canvas sources require the canvas feature")
    }
}
