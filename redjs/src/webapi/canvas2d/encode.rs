//! `toDataURL` / `toBlob` / `convertToBlob`: a bitmap to an image file.
//!
//! HTML: `type` is matched case-insensitively; an unsupported one means PNG;
//! `quality` (0..=1) matters to lossy formats only and anything else means
//! the default. Formats without alpha get the pixels composited onto opaque
//! black first.

use image::ImageEncoder;

/// The type this produces for `requested`, and its bytes, for straight RGBA.
pub fn encode(rgba: &[u8], width: u32, height: u32, requested: &str, quality: Option<f64>) -> (String, Vec<u8>) {
    let ty = requested.trim().to_ascii_lowercase();
    let q = quality.filter(|q| (0.0..=1.0).contains(q));
    let mut out = Vec::new();
    let ok = match ty.as_str() {
        "image/jpeg" => {
            let q = (q.unwrap_or(0.92) * 100.0).round().clamp(1.0, 100.0) as u8;
            let rgb: Vec<u8> = rgba
                .chunks_exact(4)
                .flat_map(|p| {
                    let a = p[3] as u32;
                    [p[0], p[1], p[2]].map(|c| ((c as u32 * a + 127) / 255) as u8)
                })
                .collect();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, q)
                .write_image(&rgb, width, height, image::ExtendedColorType::Rgb8)
                .is_ok()
        }
        "image/webp" => {
            if q.is_some() {
                super::advise(
                    "canvas:webp-lossless",
                    "canvas: image/webp is encoded LOSSLESS here (quality is ignored), so the file is larger \
                     than a browser's lossy WebP at that quality"
                        .to_string(),
                );
            }
            image::codecs::webp::WebPEncoder::new_lossless(&mut out)
                .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
                .is_ok()
        }
        _ => false,
    };
    if ok {
        return (ty, out);
    }
    out.clear();
    let _ = image::codecs::png::PngEncoder::new(&mut out).write_image(rgba, width, height, image::ExtendedColorType::Rgba8);
    ("image/png".to_string(), out)
}

/// `toDataURL`: `"data:,"` for an empty bitmap, as HTML has it.
pub fn data_url(rgba: &[u8], width: u32, height: u32, requested: &str, quality: Option<f64>) -> String {
    if width == 0 || height == 0 {
        return "data:,".to_string();
    }
    let (ty, bytes) = encode(rgba, width, height, requested, quality);
    format!("data:{ty};base64,{}", super::super::fsutil::b64_encode(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_is_the_default_and_jpeg_and_webp_are_real() {
        let px = [255u8, 0, 0, 255].repeat(4);
        let (ty, png) = encode(&px, 2, 2, "image/bmp", None);
        assert_eq!(ty, "image/png");
        assert_eq!(&png[1..4], b"PNG");
        let (ty, jpg) = encode(&px, 2, 2, "IMAGE/JPEG", Some(0.5));
        assert_eq!(ty, "image/jpeg");
        assert_eq!(&jpg[..2], &[0xff, 0xd8]);
        let (ty, webp) = encode(&px, 2, 2, "image/webp", None);
        assert_eq!(ty, "image/webp");
        assert_eq!(&webp[8..12], b"WEBP");
        let back = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(back.into_raw(), px);
    }

    #[test]
    fn an_empty_bitmap_is_data_comma() {
        assert_eq!(data_url(&[], 0, 5, "image/png", None), "data:,");
        assert!(data_url(&[0; 4], 1, 1, "", None).starts_with("data:image/png;base64,"));
    }
}
