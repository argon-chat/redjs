//! External-image copies: source snapshots, conversion, native queue upload.
use super::*;
use crate::webapi::image::{external_pixels, ExternalImage};

unsafe fn source_origin(g: *mut c_void, value: u64) -> Result<(u32, u32), ()> {
    if value == UNDEF || value == NULL {
        return Ok((0, 0));
    }
    let value = Val::new(g, value);
    if is_sequence(g, value.v)? {
        let values = seq(g, value.v)?;
        if values.len() > 2 {
            invalid(g, "GPUOrigin2D needs at most two entries")?;
        }
        Ok((
            if values.is_empty() {
                0
            } else {
                uint(g, values[0].v, 0)?
            },
            if values.len() < 2 {
                0
            } else {
                uint(g, values[1].v, 0)?
            },
        ))
    } else {
        Ok((
            uint(g, field(g, value.v, "x")?.v, 0)?,
            uint(g, field(g, value.v, "y")?.v, 0)?,
        ))
    }
}

/// The format subset renderable without optional device features. Integer,
/// snorm, compressed and depth formats are never valid external-copy targets.
#[derive(Clone, Copy)]
enum Encoding {
    Unorm8(u8, bool),
    Float16(u8),
    Float32(u8),
    Rgb10a2,
}
impl Encoding {
    fn for_format(format: wgpu::TextureFormat) -> Option<Self> {
        use wgpu::TextureFormat::*;
        Some(match format {
            R8Unorm => Self::Unorm8(1, false),
            Rg8Unorm => Self::Unorm8(2, false),
            Rgba8Unorm | Rgba8UnormSrgb => Self::Unorm8(4, false),
            Bgra8Unorm | Bgra8UnormSrgb => Self::Unorm8(4, true),
            R16Float => Self::Float16(1),
            Rg16Float => Self::Float16(2),
            Rgba16Float => Self::Float16(4),
            R32Float => Self::Float32(1),
            Rg32Float => Self::Float32(2),
            Rgba32Float => Self::Float32(4),
            Rgb10a2Unorm => Self::Rgb10a2,
            _ => return None,
        })
    }
    fn bytes(self) -> u32 {
        match self {
            Self::Unorm8(n, _) => u32::from(n),
            Self::Float16(n) => 2 * u32::from(n),
            Self::Float32(n) => 4 * u32::from(n),
            Self::Rgb10a2 => 4,
        }
    }
    fn push(self, pixel: [f32; 4], out: &mut Vec<u8>) {
        let unorm = |v: f32, max: u32| (v.clamp(0., 1.) * max as f32).round() as u32;
        match self {
            Self::Unorm8(n, bgra) => {
                let mut bytes = pixel.map(|v| unorm(v, 255) as u8);
                if bgra {
                    bytes.swap(0, 2);
                }
                out.extend_from_slice(&bytes[..n as usize]);
            }
            Self::Float16(n) => {
                for value in &pixel[..n as usize] {
                    out.extend_from_slice(&half::f16::from_f32(*value).to_le_bytes());
                }
            }
            Self::Float32(n) => {
                for value in &pixel[..n as usize] {
                    out.extend_from_slice(&value.to_le_bytes());
                }
            }
            Self::Rgb10a2 => {
                let packed = unorm(pixel[0], 1023)
                    | (unorm(pixel[1], 1023) << 10)
                    | (unorm(pixel[2], 1023) << 20)
                    | (unorm(pixel[3], 3) << 30);
                out.extend_from_slice(&packed.to_le_bytes());
            }
        }
    }
}

fn convert(
    source: &ExternalImage,
    origin: (u32, u32),
    size: wgpu::Extent3d,
    flip: bool,
    premultiply: bool,
    encoding: Encoding,
) -> Result<Vec<u8>, String> {
    let expected = (source.width as usize)
        .checked_mul(source.height as usize)
        .and_then(|n| n.checked_mul(4));
    if expected != Some(source.rgba.len()) {
        return Err("External image has malformed pixel data".into());
    }
    let length = (size.width as usize)
        .checked_mul(size.height as usize)
        .and_then(|n| n.checked_mul(encoding.bytes() as usize))
        .ok_or("External image copy size overflow")?;
    if length > 256 * 1024 * 1024 {
        return Err("External image copy exceeds the 256 MiB limit".into());
    }
    let mut bytes = Vec::with_capacity(length);
    for y in 0..size.height {
        // flipY reverses the selected rectangle, not the entire source image.
        let row = origin.1 + if flip { size.height - y - 1 } else { y };
        if let Encoding::Unorm8(4, bgra) = encoding {
            if source.premultiplied == premultiply {
                let start = (row as usize * source.width as usize + origin.0 as usize) * 4;
                let destination = bytes.len();
                bytes.extend_from_slice(&source.rgba[start..start + size.width as usize * 4]);
                if bgra {
                    for pixel in bytes[destination..].chunks_exact_mut(4) {
                        pixel.swap(0, 2);
                    }
                }
                continue;
            }
        }
        for x in 0..size.width {
            let index = (row as usize * source.width as usize + (origin.0 + x) as usize) * 4;
            let mut pixel: [f32; 4] = source.rgba[index..index + 4]
                .try_into()
                .map(|p: [u8; 4]| p.map(|v| f32::from(v) / 255.))
                .unwrap();
            if source.premultiplied != premultiply {
                let alpha = pixel[3];
                for channel in &mut pixel[..3] {
                    *channel = if premultiply {
                        *channel * alpha
                    } else if alpha > 0. {
                        *channel / alpha
                    } else {
                        0.
                    };
                }
            }
            // sRGB texture stores cancel the render-write transfer function.
            // write_texture writes bytes directly, so retain encoded channels.
            encoding.push(pixel, &mut bytes);
        }
    }
    Ok(bytes)
}
fn validate_destination(
    dest: &CopyTexture,
    size: wgpu::Extent3d,
) -> Result<Encoding, &'static str> {
    let t = &dest.texture;
    if t.destroyed.load(Ordering::Acquire) {
        return Err("Destination texture is destroyed");
    }
    if !t
        .usage
        .contains(wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::RENDER_ATTACHMENT)
    {
        return Err("External copies require COPY_DST and RENDER_ATTACHMENT usage");
    }
    if t.dimension != "2d" || t.samples != 1 || dest.aspect != wgpu::TextureAspect::All {
        return Err("External copies require a single-sampled 2D color texture");
    }
    if dest.mip >= t.mip_levels {
        return Err("Destination mip level is out of range");
    }
    let width = t.size.width.checked_shr(dest.mip).unwrap_or(0).max(1);
    let height = t.size.height.checked_shr(dest.mip).unwrap_or(0).max(1);
    if u64::from(dest.origin.x) + u64::from(size.width) > u64::from(width)
        || u64::from(dest.origin.y) + u64::from(size.height) > u64::from(height)
        || u64::from(dest.origin.z) + u64::from(size.depth_or_array_layers)
            > u64::from(t.size.depth_or_array_layers)
    {
        return Err("External copy destination region is out of bounds");
    }
    Encoding::for_format(t.raw.format()).ok_or("Unsupported external copy destination format")
}

#[crate::host_fn]
pub(super) unsafe fn copy(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Queue)?;
        let c = client(g, &h)?;
        let info = Val::new(g, cf_arg(cf, 0));
        let source = required(g, info.v, "source")?;
        let src_origin = source_origin(g, field(g, info.v, "origin")?.v)?;
        let flip = boolean(g, field(g, info.v, "flipY")?.v);
        let dest_info = Val::new(g, cf_arg(cf, 1));
        let dest = copy_texture(g, dest_info.v, &c)?;
        let color_space = text(g, field(g, dest_info.v, "colorSpace")?.v, "srgb")?;
        let premultiply = boolean(g, field(g, dest_info.v, "premultipliedAlpha")?.v);
        let size = extent(g, cf_arg(cf, 2))?;
        if !["srgb", "display-p3"].contains(&color_space.as_str()) {
            invalid(g, "Invalid external image colorSpace")?;
        }
        if color_space != "srgb" {
            return Ok(super::super::super::domexception::throw(
                g,
                "NotSupportedError",
                "External image uploads currently support sRGB only",
            ));
        }
        // Both the source and destination dictionaries can execute arbitrary JS.
        // Resolve and own the actual pixels only once those conversions finish.
        let pixels = external_pixels(g, source.v, 0)?;
        if !pixels.origin_clean {
            return Ok(super::super::super::domexception::throw(
                g,
                "SecurityError",
                "External image source is not origin-clean",
            ));
        }
        if u64::from(src_origin.0) + u64::from(size.width) > u64::from(pixels.width)
            || u64::from(src_origin.1) + u64::from(size.height) > u64::from(pixels.height)
            || size.depth_or_array_layers > 1
        {
            return Ok(super::super::super::domexception::throw(
                g,
                "OperationError",
                "External image copy region is out of bounds",
            ));
        }
        if let Err(e) = c.check() {
            return Ok(super::super::super::domexception::throw(
                g,
                "InvalidStateError",
                &e,
            ));
        }
        let encoding = match validate_destination(&dest, size) {
            Ok(e) => e,
            Err(e) => {
                validation(g, &c, e);
                return Ok(UNDEF);
            }
        };
        if size.width == 0 || size.height == 0 || size.depth_or_array_layers == 0 {
            return Ok(UNDEF);
        }
        let bytes =
            convert(&pixels, src_origin, size, flip, premultiply, encoding).map_err(|e| {
                super::super::super::domexception::throw(g, "OperationError", &e);
            })?;
        checked(g, &c, || {
            c.context.queue.write_texture(
                dest.descriptor(),
                &bytes,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(size.width * encoding.bytes()),
                    rows_per_image: Some(size.height),
                },
                size,
            )
        });
        Ok(UNDEF)
    })
}
