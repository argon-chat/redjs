//! Immutable video-frame storage shared by WebGPU imports and queue copies.
use super::*;
use redjs::webapi::image::external_pixels;
use std::sync::Weak;
pub use wgpu;

/// A decoder snapshot, independently retained across playback, seek and source changes.
/// Return a single-sample BGRA/RGBA unorm texture with TEXTURE_BINDING usage.
pub trait NativeFrame: Send + Sync {
    fn texture(&self, context: &backend::Context) -> Result<wgpu::Texture, String>;
}

#[derive(Clone)]
enum Storage {
    Pixels(Arc<[u8]>),
    Native(Arc<dyn NativeFrame>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Rgba,
    Rgbx,
    Bgra,
    Bgrx,
}

impl PixelFormat {
    fn name(self) -> &'static str {
        match self {
            Self::Rgba => "RGBA",
            Self::Rgbx => "RGBX",
            Self::Bgra => "BGRA",
            Self::Bgrx => "BGRX",
        }
    }
}

#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub timestamp: i64,
    format: PixelFormat,
    storage: Storage,
}

impl Frame {
    pub fn packed(
        width: u32,
        height: u32,
        timestamp: i64,
        mut bytes: Vec<u8>,
        format: PixelFormat,
    ) -> Option<Self> {
        let length = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        if length == 0 || bytes.len() != length {
            return None;
        }
        if matches!(format, PixelFormat::Rgbx | PixelFormat::Bgrx) {
            for pixel in bytes.chunks_exact_mut(4) {
                pixel[3] = 255;
            }
        }
        Some(Self {
            width,
            height,
            timestamp,
            format,
            storage: Storage::Pixels(bytes.into()),
        })
    }

    pub fn native(
        width: u32,
        height: u32,
        timestamp: i64,
        format: PixelFormat,
        source: Arc<dyn NativeFrame>,
    ) -> Self {
        Self {
            width,
            height,
            timestamp,
            storage: Storage::Native(source),
            format,
        }
    }

    pub fn texture(&self, context: &backend::Context) -> Result<wgpu::Texture, String> {
        let bytes = match &self.storage {
            Storage::Pixels(bytes) => bytes,
            Storage::Native(native) => return native.texture(context),
        };
        let size = wgpu::Extent3d {
            width: self.width,
            height: self.height,
            depth_or_array_layers: 1,
        };
        let texture = context.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("VideoFrame pixels"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: if matches!(self.format, PixelFormat::Bgra | PixelFormat::Bgrx) {
                wgpu::TextureFormat::Bgra8Unorm
            } else {
                wgpu::TextureFormat::Rgba8Unorm
            },
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        context.queue.write_texture(
            texture.as_image_copy(),
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.width * 4),
                rows_per_image: Some(self.height),
            },
            size,
        );
        Ok(texture)
    }
}

pub enum SourceError {
    NoFrame,
    Origin,
    Unavailable(String),
}
/// None identifies an object other than a host video element.
pub type ElementSource = unsafe fn(*mut c_void, u64) -> Option<Result<Frame, SourceError>>;

pub fn set_element_source(g: *mut c_void, source: ElementSource) {
    states()
        .lock()
        .unwrap()
        .entry(g as usize)
        .or_default()
        .video_source = Some(source);
}

pub(super) struct Imported {
    pub raw: wgpu::ExternalTexture,
    pub expired: AtomicBool,
    _plane: wgpu::Texture,
}
impl Imported {
    fn expire(&self) {
        if !self.expired.swap(true, Ordering::AcqRel) {
            self.raw.destroy();
        }
    }
}
pub(super) struct FrameHandle {
    data: Mutex<Option<Frame>>,
    imports: Mutex<Vec<Weak<Imported>>>,
    timestamp: i64,
    duration: Option<u64>,
}
fn expire(list: Vec<Weak<Imported>>) {
    for value in list.into_iter().filter_map(|v| v.upgrade()) {
        value.expire();
    }
}
pub(super) unsafe fn expire_elements(g: *mut c_void) {
    let list = states()
        .lock()
        .unwrap()
        .get_mut(&(g as usize))
        .map(|s| std::mem::take(&mut s.video_imports));
    if let Some(list) = list {
        expire(list);
    }
}
unsafe fn fail<T>(g: *mut c_void, name: &str, msg: &str) -> Result<T, ()> {
    redjs::webapi::domexception::throw(g, name, msg);
    Err(())
}

pub(super) struct Source {
    pub frame: Frame,
    handle: Option<Arc<FrameHandle>>,
}
pub(super) unsafe fn source(g: *mut c_void, object: u64) -> Result<Option<Source>, ()> {
    if let Some(h) = try_handle(g, object, Kind::VideoFrame) {
        let Resource::VideoFrame(handle) = h.resource else {
            unreachable!()
        };
        let data = handle.data.lock().unwrap().clone();
        let Some(frame) = data else {
            return fail(g, "InvalidStateError", "VideoFrame is closed");
        };
        return Ok(Some(Source {
            frame,
            handle: Some(handle),
        }));
    }
    let resolver = states()
        .lock()
        .unwrap()
        .get(&(g as usize))
        .and_then(|s| s.video_source);
    match resolver.and_then(|resolve| resolve(g, object)) {
        None => Ok(None),
        Some(Ok(frame)) => Ok(Some(Source {
            frame,
            handle: None,
        })),
        Some(Err(SourceError::NoFrame)) => {
            fail(g, "InvalidStateError", "The video has no current frame")
        }
        Some(Err(SourceError::Origin)) => fail(g, "SecurityError", "The video is not origin-clean"),
        Some(Err(SourceError::Unavailable(e))) => fail(g, "OperationError", &e),
    }
}

unsafe fn frame_handle(g: *mut c_void, cf: *mut c_void) -> Result<Arc<FrameHandle>, ()> {
    let h = this(g, cf, Kind::VideoFrame)?;
    let Resource::VideoFrame(handle) = h.resource else {
        unreachable!()
    };
    Ok(handle)
}
unsafe fn wrap_frame(g: *mut c_void, frame: Frame, duration: Option<u64>) -> u64 {
    let handle = FrameHandle {
        timestamp: frame.timestamp,
        data: Mutex::new(Some(frame)),
        imports: Mutex::default(),
        duration,
    };
    wrap(
        g,
        Kind::VideoFrame,
        None,
        Resource::VideoFrame(Arc::new(handle)),
    )
}
unsafe fn packed_input(
    g: *mut c_void,
    source: u64,
    init: u64,
    format: PixelFormat,
    timestamp: i64,
) -> Result<Frame, ()> {
    let width = uint(g, field(g, init, "codedWidth")?.v, 0)?;
    let height = uint(g, field(g, init, "codedHeight")?.v, 0)?;
    let row = u64::from(width) * 4;
    if width == 0
        || height == 0
        || !row
            .checked_mul(u64::from(height))
            .is_some_and(|n| n <= 256 * 1024 * 1024)
    {
        return fail(g, "TypeError", "Invalid or oversized VideoFrame dimensions");
    }
    let layout = seq(g, field(g, init, "layout")?.v)?;
    let (offset, stride) = match layout.as_slice() {
        [] => (0, row),
        [plane] => (
            num(g, field(g, plane.v, "offset")?.v, 0)?,
            num(g, field(g, plane.v, "stride")?.v, row)?,
        ),
        _ => return fail(g, "TypeError", "A packed VideoFrame has one plane"),
    };
    let end = stride
        .checked_mul(u64::from(height - 1))
        .and_then(|n| n.checked_add(offset))
        .and_then(|n| n.checked_add(row));
    let mut ta = redjs::ffi::RjsTaInfo::default();
    let mut ab = redjs::ffi::RjsAbInfo::default();
    let (ptr, len, detached, shared) = if glue::rjs_typedarray_info(g, source, &mut ta) == 0 {
        (ta.data, u64::from(ta.byte_length), ta.detached, ta.shared)
    } else if glue::rjs_arraybuffer_info(g, source, &mut ab) == 0 {
        (ab.data, ab.byte_length, ab.detached, ab.shared)
    } else {
        return fail(g, "TypeError", "Expected a VideoFrame BufferSource");
    };
    if shared != 0 {
        return fail(
            g,
            "NotSupportedError",
            "Shared VideoFrame buffers are not supported",
        );
    }
    if detached != 0
        || ptr.is_null()
        || stride < row
        || !end.is_some_and(|e| e <= len && e <= isize::MAX as u64)
    {
        return fail(g, "TypeError", "VideoFrame plane exceeds its buffer");
    }
    // All dictionary conversions have finished. No JS can detach the input during this copy.
    let mut bytes = Vec::with_capacity((row * u64::from(height)) as usize);
    for y in 0..height {
        let start = (offset + u64::from(y) * stride) as usize;
        bytes.extend_from_slice(std::slice::from_raw_parts(
            ptr.cast::<u8>().add(start),
            row as usize,
        ));
    }
    Ok(Frame::packed(width, height, timestamp, bytes, format).unwrap())
}

#[redjs::host_fn]
pub(super) unsafe fn construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let object = Val::new(g, cf_arg(cf, 0));
        let init = Val::new(g, cf_arg(cf, 1));
        let ts = field(g, init.v, "timestamp")?;
        let timestamp = if ts.v == UNDEF {
            None
        } else {
            let t = to_number(g, ts.v)?;
            if !t.is_finite() || t < i64::MIN as f64 || t >= -(i64::MIN as f64) {
                return fail(g, "TypeError", "Invalid VideoFrame timestamp");
            }
            Some(t as i64)
        };
        let dur = field(g, init.v, "duration")?;
        let mut duration = if dur.v == UNDEF {
            None
        } else {
            Some(num(g, dur.v, 0)?)
        };
        let format = field(g, init.v, "format")?;
        let mut frame = if format.v != UNDEF {
            let format = match text(g, format.v, "")?.as_str() {
                "RGBA" => PixelFormat::Rgba,
                "BGRA" => PixelFormat::Bgra,
                "RGBX" => PixelFormat::Rgbx,
                "BGRX" => PixelFormat::Bgrx,
                _ => {
                    return fail(
                        g,
                        "NotSupportedError",
                        "Expected a packed RGBA, RGBX, BGRA or BGRX frame",
                    )
                }
            };
            let Some(t) = timestamp else {
                return fail(g, "TypeError", "VideoFrame requires a timestamp");
            };
            packed_input(g, object.v, init.v, format, t)?
        } else if let Some(s) = source(g, object.v)? {
            duration = duration.or_else(|| s.handle.as_ref()?.duration);
            s.frame
        } else {
            let Some(t) = timestamp else {
                return fail(g, "TypeError", "VideoFrame requires a timestamp");
            };
            let mut pixels = external_pixels(g, object.v, 0)?;
            if !pixels.origin_clean {
                return fail(g, "SecurityError", "VideoFrame source is not origin-clean");
            }
            if pixels.premultiplied {
                for p in pixels.rgba.chunks_exact_mut(4) {
                    let a = u32::from(p[3]);
                    for v in &mut p[..3] {
                        *v = if a == 0 {
                            0
                        } else {
                            ((u32::from(*v) * 255 + a / 2) / a).min(255) as u8
                        };
                    }
                }
            }
            let Some(frame) = Frame::packed(
                pixels.width,
                pixels.height,
                t,
                pixels.rgba,
                PixelFormat::Rgba,
            ) else {
                return fail(g, "InvalidStateError", "Empty video frame");
            };
            frame
        };
        if let Some(t) = timestamp {
            frame.timestamp = t;
        }
        Ok(wrap_frame(g, frame, duration))
    })
}
#[redjs::host_fn]
unsafe fn clone_frame(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let handle = frame_handle(g, cf)?;
        let frame = handle.data.lock().unwrap().clone();
        let Some(frame) = frame else {
            return fail(g, "InvalidStateError", "VideoFrame is closed");
        };
        Ok(wrap_frame(g, frame, handle.duration))
    })
}
#[redjs::host_fn]
unsafe fn close(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let handle = frame_handle(g, cf)?;
        handle.data.lock().unwrap().take();
        expire(std::mem::take(&mut *handle.imports.lock().unwrap()));
        Ok(UNDEF)
    })
}
macro_rules! frame_getter {
    ($name:ident, $f:expr) => {
        #[redjs::host_fn]
        unsafe fn $name(g: *mut c_void, cf: *mut c_void) -> u64 {
            run(g, || {
                let h = frame_handle(g, cf)?;
                Ok(($f)(g, &h))
            })
        }
    };
}
frame_getter!(width, |_, h: &FrameHandle| enc_f64(
    h.data.lock().unwrap().as_ref().map_or(0, |f| f.width) as f64
));
frame_getter!(height, |_, h: &FrameHandle| enc_f64(
    h.data.lock().unwrap().as_ref().map_or(0, |f| f.height) as f64
));
frame_getter!(timestamp, |_, h: &FrameHandle| enc_f64(h.timestamp as f64));
frame_getter!(duration, |_, h: &FrameHandle| h
    .duration
    .map_or(NULL, |v| enc_f64(v as f64)));
frame_getter!(format, |g, h: &FrameHandle| {
    let format = h.data.lock().unwrap().as_ref().map(|f| f.format);
    format.map_or(NULL, |f| js_string(g, f.name()))
});

pub(super) unsafe fn install(g: *mut c_void) {
    class(g, Kind::ExternalTexture, "GPUExternalTexture", &[], &[]);
    class(
        g,
        Kind::VideoFrame,
        "VideoFrame",
        &[("close", 0, close), ("clone", 0, clone_frame)],
        &[
            ("codedWidth", width),
            ("codedHeight", height),
            ("displayWidth", width),
            ("displayHeight", height),
            ("timestamp", timestamp),
            ("duration", duration),
            ("format", format),
        ],
    );
}

#[redjs::host_fn]
pub(super) unsafe fn import(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Device)?;
        let c = client(g, &h)?;
        let descriptor = Val::new(g, cf_arg(cf, 0));
        let label = text(g, field(g, descriptor.v, "label")?.v, "")?;
        let color = text(g, field(g, descriptor.v, "colorSpace")?.v, "srgb")?;
        let object = field(g, descriptor.v, "source")?;
        let Some(source) = source(g, object.v)? else {
            return fail(g, "TypeError", "Expected a video element or VideoFrame");
        };
        if color != "srgb" {
            return fail(
                g,
                if color == "display-p3" {
                    "NotSupportedError"
                } else {
                    "TypeError"
                },
                "Video imports support sRGB",
            );
        }
        if !c
            .context
            .device
            .features()
            .contains(Features::EXTERNAL_TEXTURE)
        {
            return fail(
                g,
                "NotSupportedError",
                "This backend requires copyExternalImageToTexture for video",
            );
        }
        if let Err(e) = c.check() {
            return fail(g, "InvalidStateError", &e);
        }
        let plane = checked(g, &c, || source.frame.texture(&c.context)).map_err(|e| {
            redjs::webapi::domexception::throw(g, "OperationError", &e);
        })?;
        let raw = checked(g, &c, || {
            c.context.device.create_external_texture(
                &wgpu::ExternalTextureDescriptor {
                    label: Some(&label),
                    format: wgpu::ExternalTextureFormat::Rgba,
                    width: source.frame.width,
                    height: source.frame.height,
                    yuv_conversion_matrix: [0.; 16],
                    gamut_conversion_matrix: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
                    src_transfer_function: Default::default(),
                    dst_transfer_function: Default::default(),
                    sample_transform: [1., 0., 0., 1., 0., 0.],
                    load_transform: [1., 0., 0., 1., 0., 0.],
                },
                &[&plane.create_view(&Default::default())],
            )
        });
        let imported = Arc::new(Imported {
            raw,
            expired: AtomicBool::new(false),
            _plane: plane,
        });
        if let Some(handle) = source.handle {
            let mut list = handle.imports.lock().unwrap();
            list.retain(|t| t.strong_count() != 0);
            list.push(Arc::downgrade(&imported));
        } else {
            states()
                .lock()
                .unwrap()
                .get_mut(&(g as usize))
                .unwrap()
                .video_imports
                .push(Arc::downgrade(&imported));
        }
        Ok(wrap_labeled(
            g,
            Kind::ExternalTexture,
            Some(c),
            Resource::ExternalTexture(imported),
            &label,
        ))
    })
}
