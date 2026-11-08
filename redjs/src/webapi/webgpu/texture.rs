//! Texture resources, render pipelines/passes, and texel-copy commands.
//! No canvas presentation or implicit CPU image/video imports are exposed here.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) struct Texture {
    pub raw: Arc<wgpu::Texture>,
    pub size: wgpu::Extent3d,
    pub usage: wgpu::TextureUsages,
    pub destroyed: AtomicBool,
    mip_levels: u32,
    samples: u32,
    dimension: String,
    format_name: String,
}

/// The native view retains its texture internally, and this owner preserves
/// the outer resource handle used by logical device destruction.
#[derive(Clone)]
pub(super) struct TextureView {
    pub raw: wgpu::TextureView,
    pub _owner: Arc<Texture>,
}

pub(super) struct RenderPass {
    pub raw: Option<wgpu::RenderPass<'static>>,
    pub owner: Arc<Mutex<compute::Encoder>>,
    pub client: Arc<Client>,
}
impl Drop for RenderPass {
    fn drop(&mut self) {
        if let Some(raw) = self.raw.take() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.client.checked(|| drop(raw))
            }));
            self.owner.lock().unwrap_or_else(|e| e.into_inner()).active = false;
        }
    }
}

macro_rules! enum_parser {
    ($name:ident, $ty:ty, $default:literal; $($key:literal => $value:expr),+ $(,)?) => {
        unsafe fn $name(g: *mut c_void, v: u64) -> Result<$ty, ()> {
            let name = text(g, v, $default)?;
            match name.as_str() {
                $($key => Ok($value),)+
                _ => { invalid(g, &format!("Invalid {}: {name}", stringify!($ty)))?; unreachable!() }
            }
        }
    };
}
macro_rules! formats {
    ($m:ident) => { $m! {
        "r8unorm"=>R8Unorm,"r8snorm"=>R8Snorm,"r8uint"=>R8Uint,"r8sint"=>R8Sint,
        "r16uint"=>R16Uint,"r16sint"=>R16Sint,"r16float"=>R16Float,
        "rg8unorm"=>Rg8Unorm,"rg8snorm"=>Rg8Snorm,"rg8uint"=>Rg8Uint,"rg8sint"=>Rg8Sint,
        "r32uint"=>R32Uint,"r32sint"=>R32Sint,"r32float"=>R32Float,
        "rg16uint"=>Rg16Uint,"rg16sint"=>Rg16Sint,"rg16float"=>Rg16Float,
        "rgba8unorm"=>Rgba8Unorm,"rgba8unorm-srgb"=>Rgba8UnormSrgb,"rgba8snorm"=>Rgba8Snorm,"rgba8uint"=>Rgba8Uint,"rgba8sint"=>Rgba8Sint,
        "bgra8unorm"=>Bgra8Unorm,"bgra8unorm-srgb"=>Bgra8UnormSrgb,
        "rgb9e5ufloat"=>Rgb9e5Ufloat,"rgb10a2uint"=>Rgb10a2Uint,"rgb10a2unorm"=>Rgb10a2Unorm,"rg11b10ufloat"=>Rg11b10Ufloat,
        "rg32uint"=>Rg32Uint,"rg32sint"=>Rg32Sint,"rg32float"=>Rg32Float,
        "rgba16uint"=>Rgba16Uint,"rgba16sint"=>Rgba16Sint,"rgba16float"=>Rgba16Float,
        "rgba32uint"=>Rgba32Uint,"rgba32sint"=>Rgba32Sint,"rgba32float"=>Rgba32Float,
        "stencil8"=>Stencil8,"depth16unorm"=>Depth16Unorm,"depth24plus"=>Depth24Plus,"depth24plus-stencil8"=>Depth24PlusStencil8,"depth32float"=>Depth32Float,"depth32float-stencil8"=>Depth32FloatStencil8
    }};
}
macro_rules! parse_formats {
    ($($key:literal=>$variant:ident),*) => {
        pub(super) unsafe fn parse_format(g:*mut c_void,v:u64)->Result<wgpu::TextureFormat,()> {
            let name=text(g,v,"")?;
            match name.as_str() {$($key=>Ok(wgpu::TextureFormat::$variant),)*_=>{invalid(g,&format!("Unsupported GPUTextureFormat: {name}"))?;unreachable!()}}
        }
    };
}
formats!(parse_formats);
enum_parser!(parse_dimension,wgpu::TextureDimension,"2d";"1d"=>wgpu::TextureDimension::D1,"2d"=>wgpu::TextureDimension::D2,"3d"=>wgpu::TextureDimension::D3);
enum_parser!(parse_view_dimension,wgpu::TextureViewDimension,"2d";"1d"=>wgpu::TextureViewDimension::D1,"2d"=>wgpu::TextureViewDimension::D2,"2d-array"=>wgpu::TextureViewDimension::D2Array,"cube"=>wgpu::TextureViewDimension::Cube,"cube-array"=>wgpu::TextureViewDimension::CubeArray,"3d"=>wgpu::TextureViewDimension::D3);
enum_parser!(parse_aspect,wgpu::TextureAspect,"all";"all"=>wgpu::TextureAspect::All,"depth-only"=>wgpu::TextureAspect::DepthOnly,"stencil-only"=>wgpu::TextureAspect::StencilOnly);
enum_parser!(parse_address,wgpu::AddressMode,"clamp-to-edge";"clamp-to-edge"=>wgpu::AddressMode::ClampToEdge,"repeat"=>wgpu::AddressMode::Repeat,"mirror-repeat"=>wgpu::AddressMode::MirrorRepeat);
enum_parser!(parse_filter,wgpu::FilterMode,"nearest";"nearest"=>wgpu::FilterMode::Nearest,"linear"=>wgpu::FilterMode::Linear);
enum_parser!(parse_mipmap,wgpu::MipmapFilterMode,"nearest";"nearest"=>wgpu::MipmapFilterMode::Nearest,"linear"=>wgpu::MipmapFilterMode::Linear);
enum_parser!(parse_compare,wgpu::CompareFunction,"always";"never"=>wgpu::CompareFunction::Never,"less"=>wgpu::CompareFunction::Less,"equal"=>wgpu::CompareFunction::Equal,"less-equal"=>wgpu::CompareFunction::LessEqual,"greater"=>wgpu::CompareFunction::Greater,"not-equal"=>wgpu::CompareFunction::NotEqual,"greater-equal"=>wgpu::CompareFunction::GreaterEqual,"always"=>wgpu::CompareFunction::Always);
enum_parser!(parse_topology,wgpu::PrimitiveTopology,"triangle-list";"point-list"=>wgpu::PrimitiveTopology::PointList,"line-list"=>wgpu::PrimitiveTopology::LineList,"line-strip"=>wgpu::PrimitiveTopology::LineStrip,"triangle-list"=>wgpu::PrimitiveTopology::TriangleList,"triangle-strip"=>wgpu::PrimitiveTopology::TriangleStrip);
enum_parser!(parse_front,wgpu::FrontFace,"ccw";"ccw"=>wgpu::FrontFace::Ccw,"cw"=>wgpu::FrontFace::Cw);
enum_parser!(parse_index,wgpu::IndexFormat,"";"uint16"=>wgpu::IndexFormat::Uint16,"uint32"=>wgpu::IndexFormat::Uint32);
enum_parser!(parse_step,wgpu::VertexStepMode,"vertex";"vertex"=>wgpu::VertexStepMode::Vertex,"instance"=>wgpu::VertexStepMode::Instance);
enum_parser!(parse_vertex_format,wgpu::VertexFormat,"";
"uint8x2"=>wgpu::VertexFormat::Uint8x2,"uint8x4"=>wgpu::VertexFormat::Uint8x4,"sint8x2"=>wgpu::VertexFormat::Sint8x2,"sint8x4"=>wgpu::VertexFormat::Sint8x4,"unorm8x2"=>wgpu::VertexFormat::Unorm8x2,"unorm8x4"=>wgpu::VertexFormat::Unorm8x4,"snorm8x2"=>wgpu::VertexFormat::Snorm8x2,"snorm8x4"=>wgpu::VertexFormat::Snorm8x4,
"uint16x2"=>wgpu::VertexFormat::Uint16x2,"uint16x4"=>wgpu::VertexFormat::Uint16x4,"sint16x2"=>wgpu::VertexFormat::Sint16x2,"sint16x4"=>wgpu::VertexFormat::Sint16x4,"unorm16x2"=>wgpu::VertexFormat::Unorm16x2,"unorm16x4"=>wgpu::VertexFormat::Unorm16x4,"snorm16x2"=>wgpu::VertexFormat::Snorm16x2,"snorm16x4"=>wgpu::VertexFormat::Snorm16x4,"float16x2"=>wgpu::VertexFormat::Float16x2,"float16x4"=>wgpu::VertexFormat::Float16x4,
"float32"=>wgpu::VertexFormat::Float32,"float32x2"=>wgpu::VertexFormat::Float32x2,"float32x3"=>wgpu::VertexFormat::Float32x3,"float32x4"=>wgpu::VertexFormat::Float32x4,
"uint32"=>wgpu::VertexFormat::Uint32,"uint32x2"=>wgpu::VertexFormat::Uint32x2,"uint32x3"=>wgpu::VertexFormat::Uint32x3,"uint32x4"=>wgpu::VertexFormat::Uint32x4,
"sint32"=>wgpu::VertexFormat::Sint32,"sint32x2"=>wgpu::VertexFormat::Sint32x2,"sint32x3"=>wgpu::VertexFormat::Sint32x3,"sint32x4"=>wgpu::VertexFormat::Sint32x4,"unorm10-10-10-2"=>wgpu::VertexFormat::Unorm10_10_10_2);
enum_parser!(parse_blend_factor,wgpu::BlendFactor,"one";"zero"=>wgpu::BlendFactor::Zero,"one"=>wgpu::BlendFactor::One,"src"=>wgpu::BlendFactor::Src,"one-minus-src"=>wgpu::BlendFactor::OneMinusSrc,"src-alpha"=>wgpu::BlendFactor::SrcAlpha,"one-minus-src-alpha"=>wgpu::BlendFactor::OneMinusSrcAlpha,"dst"=>wgpu::BlendFactor::Dst,"one-minus-dst"=>wgpu::BlendFactor::OneMinusDst,"dst-alpha"=>wgpu::BlendFactor::DstAlpha,"one-minus-dst-alpha"=>wgpu::BlendFactor::OneMinusDstAlpha,"src-alpha-saturated"=>wgpu::BlendFactor::SrcAlphaSaturated,"constant"=>wgpu::BlendFactor::Constant,"one-minus-constant"=>wgpu::BlendFactor::OneMinusConstant,"src1"=>wgpu::BlendFactor::Src1,"one-minus-src1"=>wgpu::BlendFactor::OneMinusSrc1,"src1-alpha"=>wgpu::BlendFactor::Src1Alpha,"one-minus-src1-alpha"=>wgpu::BlendFactor::OneMinusSrc1Alpha);
enum_parser!(parse_blend_op,wgpu::BlendOperation,"add";"add"=>wgpu::BlendOperation::Add,"subtract"=>wgpu::BlendOperation::Subtract,"reverse-subtract"=>wgpu::BlendOperation::ReverseSubtract,"min"=>wgpu::BlendOperation::Min,"max"=>wgpu::BlendOperation::Max);
enum_parser!(parse_stencil_op,wgpu::StencilOperation,"keep";"keep"=>wgpu::StencilOperation::Keep,"zero"=>wgpu::StencilOperation::Zero,"replace"=>wgpu::StencilOperation::Replace,"invert"=>wgpu::StencilOperation::Invert,"increment-clamp"=>wgpu::StencilOperation::IncrementClamp,"decrement-clamp"=>wgpu::StencilOperation::DecrementClamp,"increment-wrap"=>wgpu::StencilOperation::IncrementWrap,"decrement-wrap"=>wgpu::StencilOperation::DecrementWrap);
enum_parser!(parse_store,wgpu::StoreOp,"";"store"=>wgpu::StoreOp::Store,"discard"=>wgpu::StoreOp::Discard);

unsafe fn finite(g: *mut c_void, v: u64, default: f64) -> Result<f64, ()> {
    let n = if v == UNDEF {
        default
    } else {
        to_number(g, v)?
    };
    if !n.is_finite() {
        invalid(g, "WebGPU number must be finite")?;
    }
    Ok(n)
}
unsafe fn float(g: *mut c_void, v: u64, default: f32) -> Result<f32, ()> {
    let n = finite(g, v, default as f64)?;
    let value = n as f32;
    if !value.is_finite() {
        invalid(g, "WebGPU float exceeds finite f32 range")?;
    }
    Ok(value)
}
unsafe fn signed(g: *mut c_void, v: u64, default: i32) -> Result<i32, ()> {
    let n = finite(g, v, default as f64)?.trunc();
    if n < i32::MIN as f64 || n > i32::MAX as f64 {
        invalid(g, "WebGPU signed integer out of range")?;
    }
    Ok(n as i32)
}
unsafe fn optional_u32(g: *mut c_void, v: u64) -> Result<Option<u32>, ()> {
    if v == UNDEF {
        Ok(None)
    } else {
        Ok(Some(uint(g, v, 0)?))
    }
}
unsafe fn optional_format(g: *mut c_void, v: u64) -> Result<Option<wgpu::TextureFormat>, ()> {
    if v == UNDEF {
        Ok(None)
    } else {
        Ok(Some(parse_format(g, v)?))
    }
}
unsafe fn required(g: *mut c_void, obj: u64, name: &str) -> Result<Val, ()> {
    let v = field(g, obj, name)?;
    if v.v == UNDEF {
        invalid(g, &format!("Missing required WebGPU member: {name}"))?;
    }
    Ok(v)
}
unsafe fn reject_present(g: *mut c_void, obj: u64, names: &[&str]) -> Result<(), ()> {
    for name in names {
        if field(g, obj, name)?.v != UNDEF {
            invalid(g, &format!("WebGPU member is not implemented: {name}"))?;
        }
    }
    Ok(())
}
unsafe fn is_sequence(g: *mut c_void, v: u64) -> Result<bool, ()> {
    let a = glue::rjs_atom_wellknown(g, 0);
    let mut out = UNDEF;
    let rc = glue::rjs_get(g, v, a, &mut out);
    glue::rjs_atom_free(g, a);
    if rc != 0 {
        glue::rjs_throw(g, out);
        return Err(());
    }
    Ok(out != UNDEF && out != NULL)
}
unsafe fn extent(g: *mut c_void, v: u64) -> Result<wgpu::Extent3d, ()> {
    let v = Val::new(g, v);
    let parts = if is_sequence(g, v.v)? {
        let s = seq(g, v.v)?;
        if s.is_empty() || s.len() > 3 {
            invalid(g, "GPUExtent3D needs one to three entries")?;
        }
        (
            uint(g, s[0].v, 0)?,
            if s.len() > 1 { uint(g, s[1].v, 1)? } else { 1 },
            if s.len() > 2 { uint(g, s[2].v, 1)? } else { 1 },
        )
    } else {
        (
            uint(g, required(g, v.v, "width")?.v, 0)?,
            uint(g, field(g, v.v, "height")?.v, 1)?,
            uint(g, field(g, v.v, "depthOrArrayLayers")?.v, 1)?,
        )
    };
    Ok(wgpu::Extent3d {
        width: parts.0,
        height: parts.1,
        depth_or_array_layers: parts.2,
    })
}
unsafe fn origin(g: *mut c_void, v: u64) -> Result<wgpu::Origin3d, ()> {
    if v == UNDEF {
        return Ok(Default::default());
    }
    let v = Val::new(g, v);
    let (x, y, z) = if is_sequence(g, v.v)? {
        let s = seq(g, v.v)?;
        if s.len() > 3 {
            invalid(g, "GPUOrigin3D needs at most three entries")?;
        }
        (
            if !s.is_empty() {
                uint(g, s[0].v, 0)?
            } else {
                0
            },
            if s.len() > 1 { uint(g, s[1].v, 0)? } else { 0 },
            if s.len() > 2 { uint(g, s[2].v, 0)? } else { 0 },
        )
    } else {
        (
            uint(g, field(g, v.v, "x")?.v, 0)?,
            uint(g, field(g, v.v, "y")?.v, 0)?,
            uint(g, field(g, v.v, "z")?.v, 0)?,
        )
    };
    Ok(wgpu::Origin3d { x, y, z })
}
unsafe fn color(g: *mut c_void, v: u64) -> Result<wgpu::Color, ()> {
    if v == UNDEF {
        return Ok(wgpu::Color::TRANSPARENT);
    }
    let v = Val::new(g, v);
    let [r, green, b, a] = if is_sequence(g, v.v)? {
        let s = seq(g, v.v)?;
        if s.len() != 4 {
            invalid(g, "GPUColor needs four entries")?;
        }
        [
            finite(g, s[0].v, 0.)?,
            finite(g, s[1].v, 0.)?,
            finite(g, s[2].v, 0.)?,
            finite(g, s[3].v, 0.)?,
        ]
    } else {
        [
            finite(g, required(g, v.v, "r")?.v, 0.)?,
            finite(g, required(g, v.v, "g")?.v, 0.)?,
            finite(g, required(g, v.v, "b")?.v, 0.)?,
            finite(g, required(g, v.v, "a")?.v, 0.)?,
        ]
    };
    Ok(wgpu::Color { r, g: green, b, a })
}

unsafe extern "sysv64" fn create_texture(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Device)?;
        let c = client(g, &h)?;
        let d = Val::new(g, cf_arg(cf, 0));
        let label = text(g, field(g, d.v, "label")?.v, "")?;
        let size = extent(g, required(g, d.v, "size")?.v)?;
        let mip_levels = uint(g, field(g, d.v, "mipLevelCount")?.v, 1)?;
        let samples = uint(g, field(g, d.v, "sampleCount")?.v, 1)?;
        let dimension_value = field(g, d.v, "dimension")?;
        let dimension = parse_dimension(g, dimension_value.v)?;
        let dimension_name = text(g, dimension_value.v, "2d")?;
        let format_value = required(g, d.v, "format")?;
        let format = parse_format(g, format_value.v)?;
        let format_name = text(g, format_value.v, "")?;
        let usage_value = uint(g, required(g, d.v, "usage")?.v, 0)?;
        let Some(usage) = wgpu::TextureUsages::from_bits(usage_value) else {
            invalid(g, "Invalid GPUTextureUsage flags")?;
            unreachable!()
        };
        let views = seq(g, field(g, d.v, "viewFormats")?.v)?
            .into_iter()
            .map(|v| parse_format(g, v.v))
            .collect::<Result<Vec<_>, _>>()?;
        let dimensional_limit = match dimension {
            wgpu::TextureDimension::D1 => size.width <= c.limits.max_texture_dimension_1d,
            wgpu::TextureDimension::D2 => {
                size.width <= c.limits.max_texture_dimension_2d
                    && size.height <= c.limits.max_texture_dimension_2d
                    && size.depth_or_array_layers <= c.limits.max_texture_array_layers
            }
            wgpu::TextureDimension::D3 => {
                size.width <= c.limits.max_texture_dimension_3d
                    && size.height <= c.limits.max_texture_dimension_3d
                    && size.depth_or_array_layers <= c.limits.max_texture_dimension_3d
            }
        };
        if !dimensional_limit {
            invalid(g, "GPUTexture exceeds logical device limits")?;
        }
        if !c.features.contains(format.required_features()) {
            invalid(
                g,
                "GPUTexture format requires a feature not enabled on this device",
            )?;
        }
        let guaranteed = format.guaranteed_format_features(c.features);
        if !guaranteed.allowed_usages.contains(usage) {
            invalid(g, "GPUTexture usage is not allowed for this format")?;
        }
        if !guaranteed.flags.sample_count_supported(samples) {
            invalid(
                g,
                "GPUTexture sample count is unavailable on the logical device",
            )?;
        }
        let raw = Arc::new(checked(g, &c, || {
            c.context.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(&label),
                size,
                mip_level_count: mip_levels,
                sample_count: samples,
                dimension,
                format,
                usage,
                view_formats: &views,
            })
        }));
        c.track_texture(&raw);
        let texture = Texture {
            raw,
            size,
            usage,
            destroyed: AtomicBool::new(false),
            mip_levels,
            samples,
            dimension: dimension_name,
            format_name,
        };
        Ok(wrap(
            g,
            Kind::Texture,
            Some(c),
            Resource::Texture(Arc::new(texture)),
        ))
    })
}
unsafe extern "sysv64" fn create_view(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Texture)?;
        let c = client(g, &h)?;
        let Resource::Texture(t) = h.resource else {
            unreachable!()
        };
        let d = Val::new(g, cf_arg(cf, 0));
        let label = text(g, field(g, d.v, "label")?.v, "")?;
        let format = optional_format(g, field(g, d.v, "format")?.v)?;
        let dv = field(g, d.v, "dimension")?;
        let dimension = if dv.v == UNDEF {
            None
        } else {
            Some(parse_view_dimension(g, dv.v)?)
        };
        let uv = field(g, d.v, "usage")?;
        let usage = if uv.v == UNDEF {
            None
        } else {
            let n = uint(g, uv.v, 0)?;
            Some(wgpu::TextureUsages::from_bits(n).ok_or_else(|| {
                throw_type_error(g, "Invalid GPUTextureUsage flags");
            })?)
        };
        let aspect = parse_aspect(g, field(g, d.v, "aspect")?.v)?;
        let base_mip_level = uint(g, field(g, d.v, "baseMipLevel")?.v, 0)?;
        let mip_level_count = optional_u32(g, field(g, d.v, "mipLevelCount")?.v)?;
        let base_array_layer = uint(g, field(g, d.v, "baseArrayLayer")?.v, 0)?;
        let array_layer_count = optional_u32(g, field(g, d.v, "arrayLayerCount")?.v)?;
        let raw = checked(g, &c, || {
            t.raw.create_view(&wgpu::TextureViewDescriptor {
                label: Some(&label),
                format,
                dimension,
                usage,
                aspect,
                base_mip_level,
                mip_level_count,
                base_array_layer,
                array_layer_count,
                ..Default::default()
            })
        });
        Ok(wrap(
            g,
            Kind::TextureView,
            Some(c),
            Resource::TextureView(TextureView { raw, _owner: t }),
        ))
    })
}
unsafe extern "sysv64" fn destroy_texture(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Texture)?;
        let Resource::Texture(t) = h.resource else {
            unreachable!()
        };
        if !t.destroyed.swap(true, Ordering::AcqRel) {
            t.raw.destroy();
        }
        Ok(UNDEF)
    })
}
macro_rules! texture_getter {
    ($fn:ident,$expr:expr) => {
        unsafe extern "sysv64" fn $fn(g: *mut c_void, cf: *mut c_void) -> u64 {
            run(g, || {
                let h = this(g, cf, Kind::Texture)?;
                let Resource::Texture(t) = h.resource else {
                    unreachable!()
                };
                Ok(($expr)(g, &t))
            })
        }
    };
}
texture_getter!(texture_width, |_g, t: &Texture| enc_f64(
    t.size.width as f64
));
texture_getter!(texture_height, |_g, t: &Texture| enc_f64(
    t.size.height as f64
));
texture_getter!(texture_depth, |_g, t: &Texture| enc_f64(
    t.size.depth_or_array_layers as f64
));
texture_getter!(texture_mips, |_g, t: &Texture| enc_f64(t.mip_levels as f64));
texture_getter!(texture_samples, |_g, t: &Texture| enc_f64(t.samples as f64));
texture_getter!(texture_dimension, |g, t: &Texture| js_string(
    g,
    &t.dimension
));
texture_getter!(texture_format, |g, t: &Texture| js_string(
    g,
    &t.format_name
));
texture_getter!(texture_usage, |_g, t: &Texture| enc_f64(
    t.usage.bits() as f64
));

unsafe extern "sysv64" fn create_sampler(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Device)?;
        let c = client(g, &h)?;
        let d = Val::new(g, cf_arg(cf, 0));
        let label = text(g, field(g, d.v, "label")?.v, "")?;
        let address_mode_u = parse_address(g, field(g, d.v, "addressModeU")?.v)?;
        let address_mode_v = parse_address(g, field(g, d.v, "addressModeV")?.v)?;
        let address_mode_w = parse_address(g, field(g, d.v, "addressModeW")?.v)?;
        let mag_filter = parse_filter(g, field(g, d.v, "magFilter")?.v)?;
        let min_filter = parse_filter(g, field(g, d.v, "minFilter")?.v)?;
        let mipmap_filter = parse_mipmap(g, field(g, d.v, "mipmapFilter")?.v)?;
        let lod_min_clamp = float(g, field(g, d.v, "lodMinClamp")?.v, 0.)?;
        let lod_max_clamp = float(g, field(g, d.v, "lodMaxClamp")?.v, 32.)?;
        let cmp = field(g, d.v, "compare")?;
        let compare = if cmp.v == UNDEF {
            None
        } else {
            Some(parse_compare(g, cmp.v)?)
        };
        let anisotropy = uint(g, field(g, d.v, "maxAnisotropy")?.v, 1)?;
        let anisotropy_clamp = u16::try_from(anisotropy).map_err(|_| {
            throw_type_error(g, "maxAnisotropy exceeds unsigned short");
        })?;
        let sampler = checked(g, &c, || {
            c.context.device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some(&label),
                address_mode_u,
                address_mode_v,
                address_mode_w,
                mag_filter,
                min_filter,
                mipmap_filter,
                lod_min_clamp,
                lod_max_clamp,
                compare,
                anisotropy_clamp,
                border_color: None,
            })
        });
        Ok(wrap(g, Kind::Sampler, Some(c), Resource::Sampler(sampler)))
    })
}
unsafe fn blend_component(g: *mut c_void, v: u64) -> Result<wgpu::BlendComponent, ()> {
    let v = Val::new(g, v);
    let src = field(g, v.v, "srcFactor")?;
    let dst = field(g, v.v, "dstFactor")?;
    let src_factor = parse_blend_factor(g, src.v)?;
    let dst_factor = if dst.v == UNDEF {
        wgpu::BlendFactor::Zero
    } else {
        parse_blend_factor(g, dst.v)?
    };
    let operation = parse_blend_op(g, field(g, v.v, "operation")?.v)?;
    Ok(wgpu::BlendComponent {
        src_factor,
        dst_factor,
        operation,
    })
}
unsafe fn stencil_face(g: *mut c_void, v: u64) -> Result<wgpu::StencilFaceState, ()> {
    let v = Val::new(g, v);
    Ok(wgpu::StencilFaceState {
        compare: parse_compare(g, field(g, v.v, "compare")?.v)?,
        fail_op: parse_stencil_op(g, field(g, v.v, "failOp")?.v)?,
        depth_fail_op: parse_stencil_op(g, field(g, v.v, "depthFailOp")?.v)?,
        pass_op: parse_stencil_op(g, field(g, v.v, "passOp")?.v)?,
    })
}
unsafe fn depth_state(g: *mut c_void, v: u64) -> Result<Option<wgpu::DepthStencilState>, ()> {
    if v == UNDEF {
        return Ok(None);
    }
    let v = Val::new(g, v);
    let format = parse_format(g, required(g, v.v, "format")?.v)?;
    let dw = field(g, v.v, "depthWriteEnabled")?;
    let dc = field(g, v.v, "depthCompare")?;
    Ok(Some(wgpu::DepthStencilState {
        format,
        depth_write_enabled: if dw.v == UNDEF {
            None
        } else {
            Some(boolean(g, dw.v))
        },
        depth_compare: if dc.v == UNDEF {
            None
        } else {
            Some(parse_compare(g, dc.v)?)
        },
        stencil: wgpu::StencilState {
            front: stencil_face(g, field(g, v.v, "stencilFront")?.v)?,
            back: stencil_face(g, field(g, v.v, "stencilBack")?.v)?,
            read_mask: uint(g, field(g, v.v, "stencilReadMask")?.v, u32::MAX)?,
            write_mask: uint(g, field(g, v.v, "stencilWriteMask")?.v, u32::MAX)?,
        },
        bias: wgpu::DepthBiasState {
            constant: signed(g, field(g, v.v, "depthBias")?.v, 0)?,
            slope_scale: float(g, field(g, v.v, "depthBiasSlopeScale")?.v, 0.)?,
            clamp: float(g, field(g, v.v, "depthBiasClamp")?.v, 0.)?,
        },
    }))
}
struct VertexLayout {
    stride: u64,
    step: wgpu::VertexStepMode,
    attributes: Vec<wgpu::VertexAttribute>,
}
unsafe extern "sysv64" fn create_render_pipeline(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Device)?;
        let c = client(g, &h)?;
        let d = Val::new(g, cf_arg(cf, 0));
        let label = text(g, field(g, d.v, "label")?.v, "")?;
        let layout = compute::parse_pipeline_layout(g, d.v, &c)?;
        let vertex = required(g, d.v, "vertex")?;
        let shader = owned(g, required(g, vertex.v, "module")?.v, Kind::Shader, &c)?;
        let Resource::Shader(vs) = shader.resource else {
            unreachable!()
        };
        let vp = field(g, vertex.v, "entryPoint")?;
        let vs_entry = if vp.v == UNDEF {
            None
        } else {
            Some(text(g, vp.v, "")?)
        };
        let vs_constants = compute::stage_constants(g, vertex.v)?;
        let mut vertex_owned = Vec::new();
        let mut attribute_count = 0usize;
        for v in seq(g, field(g, vertex.v, "buffers")?.v)? {
            if v.v == NULL {
                vertex_owned.push(None);
                continue;
            }
            let stride = num(g, required(g, v.v, "arrayStride")?.v, 0)?;
            let step = parse_step(g, field(g, v.v, "stepMode")?.v)?;
            let mut attributes = Vec::new();
            for a in seq(g, required(g, v.v, "attributes")?.v)? {
                let location = uint(g, required(g, a.v, "shaderLocation")?.v, 0)?;
                if location >= c.limits.max_vertex_attributes {
                    invalid(g, "shaderLocation exceeds logical device limits")?;
                }
                attributes.push(wgpu::VertexAttribute {
                    format: parse_vertex_format(g, required(g, a.v, "format")?.v)?,
                    offset: num(g, required(g, a.v, "offset")?.v, 0)?,
                    shader_location: location,
                });
            }
            attribute_count += attributes.len();
            if stride > c.limits.max_vertex_buffer_array_stride as u64 {
                invalid(g, "arrayStride exceeds logical device limits")?;
            }
            vertex_owned.push(Some(VertexLayout {
                stride,
                step,
                attributes,
            }));
        }
        if vertex_owned.len() > c.limits.max_vertex_buffers as usize
            || attribute_count > c.limits.max_vertex_attributes as usize
        {
            invalid(g, "Vertex buffers exceed logical device limits")?;
        }
        let vertex_buffers = vertex_owned
            .iter()
            .map(|v| {
                v.as_ref().map(|v| wgpu::VertexBufferLayout {
                    array_stride: v.stride,
                    step_mode: v.step,
                    attributes: &v.attributes,
                })
            })
            .collect::<Vec<_>>();
        let fragment = field(g, d.v, "fragment")?;
        let mut fs_module = None;
        let mut fs_entry = None;
        let mut targets = Vec::new();
        let mut fs_constants = Vec::new();
        if fragment.v != UNDEF {
            let shader = owned(g, required(g, fragment.v, "module")?.v, Kind::Shader, &c)?;
            let Resource::Shader(shader) = shader.resource else {
                unreachable!()
            };
            fs_module = Some(shader);
            let ep = field(g, fragment.v, "entryPoint")?;
            if ep.v != UNDEF {
                fs_entry = Some(text(g, ep.v, "")?);
            }
            fs_constants = compute::stage_constants(g, fragment.v)?;
            for t in seq(g, required(g, fragment.v, "targets")?.v)? {
                if t.v == NULL {
                    targets.push(None);
                    continue;
                }
                let format = parse_format(g, required(g, t.v, "format")?.v)?;
                if !c.features.contains(format.required_features()) {
                    invalid(g, "Color target format requires an unrequested feature")?;
                }
                let b = field(g, t.v, "blend")?;
                let blend = if b.v == UNDEF {
                    None
                } else {
                    Some(wgpu::BlendState {
                        color: blend_component(g, required(g, b.v, "color")?.v)?,
                        alpha: blend_component(g, required(g, b.v, "alpha")?.v)?,
                    })
                };
                if blend.as_ref().is_some_and(|b| {
                    b.color.src_factor.uses_second_blend_source()
                        || b.color.dst_factor.uses_second_blend_source()
                        || b.alpha.src_factor.uses_second_blend_source()
                        || b.alpha.dst_factor.uses_second_blend_source()
                }) && !c.features.contains(wgpu::Features::DUAL_SOURCE_BLENDING)
                {
                    invalid(
                        g,
                        "Dual source blending was not enabled on the logical device",
                    )?;
                }
                let mask = uint(g, field(g, t.v, "writeMask")?.v, 15)?;
                let write_mask = wgpu::ColorWrites::from_bits(mask).ok_or_else(|| {
                    throw_type_error(g, "Invalid GPUColorWrite flags");
                })?;
                targets.push(Some(wgpu::ColorTargetState {
                    format,
                    blend,
                    write_mask,
                }));
            }
        }
        if targets.len() > c.limits.max_color_attachments as usize {
            invalid(g, "Color target count exceeds logical device limits")?;
        }
        let target_bytes: u32 = targets
            .iter()
            .flatten()
            .filter_map(|t| t.format.target_pixel_byte_cost())
            .sum();
        if target_bytes > c.limits.max_color_attachment_bytes_per_sample {
            invalid(
                g,
                "Color target bytes per sample exceed logical device limits",
            )?;
        }
        let p = field(g, d.v, "primitive")?;
        let strip = field(g, p.v, "stripIndexFormat")?;
        let cull = text(g, field(g, p.v, "cullMode")?.v, "none")?;
        let cull_mode = match cull.as_str() {
            "none" => None,
            "front" => Some(wgpu::Face::Front),
            "back" => Some(wgpu::Face::Back),
            _ => {
                invalid(g, "Invalid GPUCullMode")?;
                unreachable!()
            }
        };
        let primitive = wgpu::PrimitiveState {
            topology: parse_topology(g, field(g, p.v, "topology")?.v)?,
            strip_index_format: if strip.v == UNDEF {
                None
            } else {
                Some(parse_index(g, strip.v)?)
            },
            front_face: parse_front(g, field(g, p.v, "frontFace")?.v)?,
            cull_mode,
            unclipped_depth: boolean(g, field(g, p.v, "unclippedDepth")?.v),
            ..Default::default()
        };
        if primitive.unclipped_depth && !c.features.contains(wgpu::Features::DEPTH_CLIP_CONTROL) {
            invalid(g, "unclippedDepth requires depth-clip-control")?;
        }
        let depth_stencil = depth_state(g, field(g, d.v, "depthStencil")?.v)?;
        if depth_stencil
            .as_ref()
            .is_some_and(|d| !c.features.contains(d.format.required_features()))
        {
            invalid(g, "Depth format requires an unrequested feature")?;
        }
        let ms = field(g, d.v, "multisample")?;
        let multisample = wgpu::MultisampleState {
            count: uint(g, field(g, ms.v, "count")?.v, 1)?,
            mask: uint(g, field(g, ms.v, "mask")?.v, u32::MAX)? as u64,
            alpha_to_coverage_enabled: boolean(g, field(g, ms.v, "alphaToCoverageEnabled")?.v),
        };
        for target in targets.iter().flatten() {
            let caps = target.format.guaranteed_format_features(c.features);
            if !caps
                .allowed_usages
                .contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
                || !caps.flags.sample_count_supported(multisample.count)
                || (target.blend.is_some()
                    && !caps
                        .flags
                        .contains(wgpu::TextureFormatFeatureFlags::BLENDABLE))
            {
                invalid(
                    g,
                    "Color target requires unsupported logical device capabilities",
                )?;
            }
        }
        let vs_constants_ref = vs_constants
            .iter()
            .map(|(n, v)| (n.as_str(), *v))
            .collect::<Vec<_>>();
        let fs_constants_ref = fs_constants
            .iter()
            .map(|(n, v)| (n.as_str(), *v))
            .collect::<Vec<_>>();
        let pipeline = checked(g, &c, || {
            c.context
                .device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(&label),
                    layout: layout.as_ref(),
                    vertex: wgpu::VertexState {
                        module: &vs.raw,
                        entry_point: vs_entry.as_deref(),
                        compilation_options: wgpu::PipelineCompilationOptions {
                            constants: &vs_constants_ref,
                            ..Default::default()
                        },
                        buffers: &vertex_buffers,
                    },
                    primitive,
                    depth_stencil,
                    multisample,
                    fragment: fs_module.as_ref().map(|s| wgpu::FragmentState {
                        module: &s.raw,
                        entry_point: fs_entry.as_deref(),
                        compilation_options: wgpu::PipelineCompilationOptions {
                            constants: &fs_constants_ref,
                            ..Default::default()
                        },
                        targets: &targets,
                    }),
                    multiview_mask: None,
                    cache: None,
                })
        });
        Ok(wrap(
            g,
            Kind::RenderPipeline,
            Some(c),
            Resource::RenderPipeline(pipeline),
        ))
    })
}
unsafe extern "sysv64" fn render_layout(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::RenderPipeline)?;
        let c = client(g, &h)?;
        let Resource::RenderPipeline(p) = h.resource else {
            unreachable!()
        };
        let index = uint(g, cf_arg(cf, 0), 0)?;
        let layout = checked(g, &c, || p.get_bind_group_layout(index));
        Ok(wrap(
            g,
            Kind::BindGroupLayout,
            Some(c),
            Resource::BindGroupLayout(layout),
        ))
    })
}

struct ColorAttachment {
    view: TextureView,
    resolve: Option<TextureView>,
    depth_slice: Option<u32>,
    ops: wgpu::Operations<wgpu::Color>,
}
unsafe fn load_color(g: *mut c_void, v: u64) -> Result<wgpu::Operations<wgpu::Color>, ()> {
    let v = Val::new(g, v);
    let op = text(g, required(g, v.v, "loadOp")?.v, "")?;
    let load = match op.as_str() {
        "load" => wgpu::LoadOp::Load,
        "clear" => wgpu::LoadOp::Clear(color(g, field(g, v.v, "clearValue")?.v)?),
        _ => {
            invalid(g, "Invalid GPULoadOp")?;
            unreachable!()
        }
    };
    let store = parse_store(g, required(g, v.v, "storeOp")?.v)?;
    Ok(wgpu::Operations { load, store })
}
unsafe fn depth_ops(g: *mut c_void, v: u64) -> Result<Option<wgpu::Operations<f32>>, ()> {
    if boolean(g, field(g, v, "depthReadOnly")?.v) {
        return Ok(None);
    }
    let op = field(g, v, "depthLoadOp")?;
    if op.v == UNDEF {
        return Ok(None);
    }
    let op = text(g, op.v, "")?;
    let load = match op.as_str() {
        "load" => wgpu::LoadOp::Load,
        "clear" => wgpu::LoadOp::Clear(float(g, required(g, v, "depthClearValue")?.v, 0.)?),
        _ => {
            invalid(g, "Invalid depthLoadOp")?;
            unreachable!()
        }
    };
    Ok(Some(wgpu::Operations {
        load,
        store: parse_store(g, required(g, v, "depthStoreOp")?.v)?,
    }))
}
unsafe fn stencil_ops(g: *mut c_void, v: u64) -> Result<Option<wgpu::Operations<u32>>, ()> {
    if boolean(g, field(g, v, "stencilReadOnly")?.v) {
        return Ok(None);
    }
    let op = field(g, v, "stencilLoadOp")?;
    if op.v == UNDEF {
        return Ok(None);
    }
    let op = text(g, op.v, "")?;
    let load = match op.as_str() {
        "load" => wgpu::LoadOp::Load,
        "clear" => wgpu::LoadOp::Clear(uint(g, field(g, v, "stencilClearValue")?.v, 0)?),
        _ => {
            invalid(g, "Invalid stencilLoadOp")?;
            unreachable!()
        }
    };
    Ok(Some(wgpu::Operations {
        load,
        store: parse_store(g, required(g, v, "stencilStoreOp")?.v)?,
    }))
}
unsafe extern "sysv64" fn begin_render_pass(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Encoder)?;
        let c = client(g, &h)?;
        let Resource::Encoder(owner) = h.resource else {
            unreachable!()
        };
        let d = Val::new(g, cf_arg(cf, 0));
        let label = text(g, field(g, d.v, "label")?.v, "")?;
        reject_present(g, d.v, &["timestampWrites", "occlusionQuerySet"])?;
        let mut colors = Vec::new();
        for a in seq(g, required(g, d.v, "colorAttachments")?.v)? {
            if a.v == NULL {
                colors.push(None);
                continue;
            }
            let view = owned(g, required(g, a.v, "view")?.v, Kind::TextureView, &c)?;
            let Resource::TextureView(view) = view.resource else {
                unreachable!()
            };
            let resolve = field(g, a.v, "resolveTarget")?;
            let resolve = if resolve.v == UNDEF {
                None
            } else {
                let h = owned(g, resolve.v, Kind::TextureView, &c)?;
                let Resource::TextureView(view) = h.resource else {
                    unreachable!()
                };
                Some(view)
            };
            colors.push(Some(ColorAttachment {
                view,
                resolve,
                depth_slice: optional_u32(g, field(g, a.v, "depthSlice")?.v)?,
                ops: load_color(g, a.v)?,
            }));
        }
        if colors.len() > c.limits.max_color_attachments as usize {
            invalid(
                g,
                "Render pass exceeds logical device color attachment limit",
            )?;
        }
        let ds = field(g, d.v, "depthStencilAttachment")?;
        let mut depth_view = None;
        let mut dops = None;
        let mut sops = None;
        if ds.v != UNDEF {
            let h = owned(g, required(g, ds.v, "view")?.v, Kind::TextureView, &c)?;
            let Resource::TextureView(view) = h.resource else {
                unreachable!()
            };
            depth_view = Some(view);
            dops = depth_ops(g, ds.v)?;
            sops = stencil_ops(g, ds.v)?;
        }
        let color_refs = colors
            .iter()
            .map(|a| {
                a.as_ref().map(|a| wgpu::RenderPassColorAttachment {
                    view: &a.view.raw,
                    depth_slice: a.depth_slice,
                    resolve_target: a.resolve.as_ref().map(|v| &v.raw),
                    ops: a.ops,
                })
            })
            .collect::<Vec<_>>();
        let mut resources = Vec::new();
        for color in colors.iter().flatten() {
            resources.push(Resource::TextureView(color.view.clone()));
            if let Some(resolve) = &color.resolve {
                resources.push(Resource::TextureView(resolve.clone()));
            }
        }
        if let Some(depth) = &depth_view {
            resources.push(Resource::TextureView(depth.clone()));
        }
        compute::retain_encoder(&owner, &resources);
        let pass = compute::with_encoder(g, &c, &owner, |encoder| {
            encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some(&label),
                    color_attachments: &color_refs,
                    depth_stencil_attachment: depth_view.as_ref().map(|view| {
                        wgpu::RenderPassDepthStencilAttachment {
                            view: &view.raw,
                            depth_ops: dops,
                            stencil_ops: sops,
                        }
                    }),
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime()
        })?;
        owner.lock().unwrap().active = true;
        Ok(wrap(
            g,
            Kind::RenderPass,
            Some(c.clone()),
            Resource::RenderPass(Arc::new(Mutex::new(RenderPass {
                raw: Some(pass),
                owner,
                client: c,
            }))),
        ))
    })
}
unsafe fn with_pass<T>(
    g: *mut c_void,
    c: &Arc<Client>,
    pass: &Arc<Mutex<RenderPass>>,
    f: impl FnOnce(&mut wgpu::RenderPass<'static>) -> T,
) -> Result<T, ()> {
    let mut pass = pass.lock().unwrap();
    let Some(raw) = pass.raw.as_mut() else {
        drop(pass);
        invalid(g, "Render pass has ended")?;
        unreachable!()
    };
    Ok(checked(g, c, || f(raw)))
}
unsafe fn retain_pass(
    g: *mut c_void,
    pass: &Arc<Mutex<RenderPass>>,
    resources: &[Resource],
) -> Result<(), ()> {
    let owner = {
        let pass = pass.lock().unwrap();
        if pass.raw.is_none() {
            drop(pass);
            return invalid(g, "Render pass has ended");
        }
        pass.owner.clone()
    };
    compute::retain_encoder(&owner, resources);
    Ok(())
}

unsafe fn pass_this(
    g: *mut c_void,
    cf: *mut c_void,
) -> Result<(Arc<Client>, Arc<Mutex<RenderPass>>), ()> {
    let h = this(g, cf, Kind::RenderPass)?;
    let c = client(g, &h)?;
    let Resource::RenderPass(pass) = h.resource else {
        unreachable!()
    };
    Ok((c, pass))
}
unsafe extern "sysv64" fn pass_pipeline(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, pass) = pass_this(g, cf)?;
        let h = owned(g, cf_arg(cf, 0), Kind::RenderPipeline, &c)?;
        let Resource::RenderPipeline(p) = h.resource else {
            unreachable!()
        };
        with_pass(g, &c, &pass, |raw| raw.set_pipeline(&p))?;
        Ok(UNDEF)
    })
}
unsafe extern "sysv64" fn pass_bind_group(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, pass) = pass_this(g, cf)?;
        let index = uint(g, cf_arg(cf, 0), 0)?;
        if index >= c.limits.max_bind_groups {
            invalid(g, "Bind group index exceeds logical device limit")?;
        }
        let group = if cf_arg(cf, 1) == NULL {
            None
        } else {
            let h = owned(g, cf_arg(cf, 1), Kind::BindGroup, &c)?;
            let Resource::BindGroup(b) = h.resource else {
                unreachable!()
            };
            Some(b)
        };
        let offsets = compute::dynamic_offsets(g, cf, 2)?;
        if let Some(group) = &group {
            retain_pass(g, &pass, &[Resource::BindGroup(group.clone())])?;
        }
        with_pass(g, &c, &pass, |raw| {
            raw.set_bind_group(index, group.as_ref().map(|b| &b.raw), &offsets)
        })?;
        Ok(UNDEF)
    })
}
unsafe fn buffer_range(
    g: *mut c_void,
    cf: *mut c_void,
    c: &Arc<Client>,
    buffer_arg: u32,
    offset_arg: u32,
    size_arg: u32,
) -> Result<(Arc<buffer::Buffer>, std::ops::Range<u64>), ()> {
    let h = owned(g, cf_arg(cf, buffer_arg), Kind::Buffer, c)?;
    let Resource::Buffer(b) = h.resource else {
        unreachable!()
    };
    let offset = num(g, cf_arg(cf, offset_arg), 0)?;
    let size = num(g, cf_arg(cf, size_arg), b.size.saturating_sub(offset))?;
    let Some(end) = offset.checked_add(size) else {
        invalid(g, "GPU buffer range overflow")?;
        unreachable!()
    };
    if !b.valid || offset >= end || end > b.raw.size() {
        invalid(g, "GPU buffer range is empty or out of bounds")?;
    }
    Ok((b, offset..end))
}
unsafe extern "sysv64" fn pass_vertex_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, pass) = pass_this(g, cf)?;
        let slot = uint(g, cf_arg(cf, 0), 0)?;
        if slot >= c.limits.max_vertex_buffers {
            invalid(g, "Vertex slot exceeds logical device limit")?;
        }
        if cf_arg(cf, 1) == NULL {
            with_pass(g, &c, &pass, |raw| raw.set_vertex_buffer(slot, None))?;
            return Ok(UNDEF);
        }
        let (b, range) = buffer_range(g, cf, &c, 1, 2, 3)?;
        retain_pass(g, &pass, &[Resource::Buffer(b.clone())])?;
        with_pass(g, &c, &pass, |raw| {
            raw.set_vertex_buffer(slot, b.raw.slice(range))
        })?;
        Ok(UNDEF)
    })
}
unsafe extern "sysv64" fn pass_index_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, pass) = pass_this(g, cf)?;
        let format = parse_index(g, cf_arg(cf, 1))?;
        let (b, range) = buffer_range(g, cf, &c, 0, 2, 3)?;
        retain_pass(g, &pass, &[Resource::Buffer(b.clone())])?;
        with_pass(g, &c, &pass, |raw| {
            raw.set_index_buffer(b.raw.slice(range), format)
        })?;
        Ok(UNDEF)
    })
}
unsafe fn draw_range(g: *mut c_void, start: u32, count: u32) -> Result<std::ops::Range<u32>, ()> {
    let Some(end) = start.checked_add(count) else {
        invalid(g, "GPU draw range overflows unsigned long")?;
        unreachable!()
    };
    Ok(start..end)
}
unsafe extern "sysv64" fn pass_draw(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, pass) = pass_this(g, cf)?;
        let count = uint(g, cf_arg(cf, 0), 0)?;
        let instances = uint(g, cf_arg(cf, 1), 1)?;
        let first = uint(g, cf_arg(cf, 2), 0)?;
        let first_instance = uint(g, cf_arg(cf, 3), 0)?;
        let vertices = draw_range(g, first, count)?;
        let instances = draw_range(g, first_instance, instances)?;
        with_pass(g, &c, &pass, |raw| raw.draw(vertices, instances))?;
        Ok(UNDEF)
    })
}
unsafe extern "sysv64" fn pass_draw_indexed(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, pass) = pass_this(g, cf)?;
        let count = uint(g, cf_arg(cf, 0), 0)?;
        let instances = uint(g, cf_arg(cf, 1), 1)?;
        let first = uint(g, cf_arg(cf, 2), 0)?;
        let base = signed(g, cf_arg(cf, 3), 0)?;
        let first_instance = uint(g, cf_arg(cf, 4), 0)?;
        let indices = draw_range(g, first, count)?;
        let instances = draw_range(g, first_instance, instances)?;
        with_pass(g, &c, &pass, |raw| {
            raw.draw_indexed(indices, base, instances)
        })?;
        Ok(UNDEF)
    })
}
unsafe extern "sysv64" fn pass_viewport(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, pass) = pass_this(g, cf)?;
        let x = float(g, cf_arg(cf, 0), 0.)?;
        let y = float(g, cf_arg(cf, 1), 0.)?;
        let w = float(g, cf_arg(cf, 2), 0.)?;
        let h = float(g, cf_arg(cf, 3), 0.)?;
        let min = float(g, cf_arg(cf, 4), 0.)?;
        let max = float(g, cf_arg(cf, 5), 1.)?;
        with_pass(g, &c, &pass, |raw| raw.set_viewport(x, y, w, h, min, max))?;
        Ok(UNDEF)
    })
}
unsafe extern "sysv64" fn pass_scissor(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, pass) = pass_this(g, cf)?;
        let x = uint(g, cf_arg(cf, 0), 0)?;
        let y = uint(g, cf_arg(cf, 1), 0)?;
        let w = uint(g, cf_arg(cf, 2), 0)?;
        let h = uint(g, cf_arg(cf, 3), 0)?;
        with_pass(g, &c, &pass, |raw| raw.set_scissor_rect(x, y, w, h))?;
        Ok(UNDEF)
    })
}
unsafe extern "sysv64" fn pass_blend(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, pass) = pass_this(g, cf)?;
        let color = color(g, cf_arg(cf, 0))?;
        with_pass(g, &c, &pass, |raw| raw.set_blend_constant(color))?;
        Ok(UNDEF)
    })
}
unsafe extern "sysv64" fn pass_stencil(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, pass) = pass_this(g, cf)?;
        let reference = uint(g, cf_arg(cf, 0), 0)?;
        with_pass(g, &c, &pass, |raw| raw.set_stencil_reference(reference))?;
        Ok(UNDEF)
    })
}
unsafe extern "sysv64" fn end_pass(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, pass) = pass_this(g, cf)?;
        let (mut raw, owner) = {
            let mut p = pass.lock().unwrap();
            (p.raw.take(), p.owner.clone())
        };
        let Some(raw) = raw.take() else {
            invalid(g, "Render pass has already ended")?;
            unreachable!()
        };
        checked(g, &c, || drop(raw));
        owner.lock().unwrap().active = false;
        Ok(UNDEF)
    })
}

struct CopyTexture {
    texture: Arc<Texture>,
    mip: u32,
    origin: wgpu::Origin3d,
    aspect: wgpu::TextureAspect,
}
impl CopyTexture {
    fn descriptor(&self) -> wgpu::TexelCopyTextureInfo<'_> {
        wgpu::TexelCopyTextureInfo {
            texture: &self.texture.raw,
            mip_level: self.mip,
            origin: self.origin,
            aspect: self.aspect,
        }
    }
}
unsafe fn copy_texture(g: *mut c_void, v: u64, c: &Arc<Client>) -> Result<CopyTexture, ()> {
    let v = Val::new(g, v);
    let h = owned(g, required(g, v.v, "texture")?.v, Kind::Texture, c)?;
    let Resource::Texture(texture) = h.resource else {
        unreachable!()
    };
    Ok(CopyTexture {
        texture,
        mip: uint(g, field(g, v.v, "mipLevel")?.v, 0)?,
        origin: origin(g, field(g, v.v, "origin")?.v)?,
        aspect: parse_aspect(g, field(g, v.v, "aspect")?.v)?,
    })
}
unsafe fn copy_layout(g: *mut c_void, v: u64) -> Result<wgpu::TexelCopyBufferLayout, ()> {
    let v = Val::new(g, v);
    Ok(wgpu::TexelCopyBufferLayout {
        offset: num(g, field(g, v.v, "offset")?.v, 0)?,
        bytes_per_row: optional_u32(g, field(g, v.v, "bytesPerRow")?.v)?,
        rows_per_image: optional_u32(g, field(g, v.v, "rowsPerImage")?.v)?,
    })
}
unsafe fn copy_buffer(
    g: *mut c_void,
    v: u64,
    c: &Arc<Client>,
) -> Result<(Arc<buffer::Buffer>, wgpu::TexelCopyBufferLayout), ()> {
    let v = Val::new(g, v);
    let h = owned(g, required(g, v.v, "buffer")?.v, Kind::Buffer, c)?;
    let Resource::Buffer(buffer) = h.resource else {
        unreachable!()
    };
    Ok((buffer, copy_layout(g, v.v)?))
}
unsafe extern "sysv64" fn copy_texture_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Encoder)?;
        let c = client(g, &h)?;
        let Resource::Encoder(encoder) = h.resource else {
            unreachable!()
        };
        let source = copy_texture(g, cf_arg(cf, 0), &c)?;
        let (dest, layout) = copy_buffer(g, cf_arg(cf, 1), &c)?;
        let size = extent(g, cf_arg(cf, 2))?;
        compute::retain_encoder(
            &encoder,
            &[
                Resource::Texture(source.texture.clone()),
                Resource::Buffer(dest.clone()),
            ],
        );
        compute::with_encoder(g, &c, &encoder, |e| {
            e.copy_texture_to_buffer(
                source.descriptor(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &dest.raw,
                    layout,
                },
                size,
            )
        })?;
        Ok(UNDEF)
    })
}
unsafe extern "sysv64" fn copy_buffer_texture(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Encoder)?;
        let c = client(g, &h)?;
        let Resource::Encoder(encoder) = h.resource else {
            unreachable!()
        };
        let (source, layout) = copy_buffer(g, cf_arg(cf, 0), &c)?;
        let dest = copy_texture(g, cf_arg(cf, 1), &c)?;
        let size = extent(g, cf_arg(cf, 2))?;
        compute::retain_encoder(
            &encoder,
            &[
                Resource::Buffer(source.clone()),
                Resource::Texture(dest.texture.clone()),
            ],
        );
        compute::with_encoder(g, &c, &encoder, |e| {
            e.copy_buffer_to_texture(
                wgpu::TexelCopyBufferInfo {
                    buffer: &source.raw,
                    layout,
                },
                dest.descriptor(),
                size,
            )
        })?;
        Ok(UNDEF)
    })
}
unsafe extern "sysv64" fn copy_texture_texture(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Encoder)?;
        let c = client(g, &h)?;
        let Resource::Encoder(encoder) = h.resource else {
            unreachable!()
        };
        let source = copy_texture(g, cf_arg(cf, 0), &c)?;
        let dest = copy_texture(g, cf_arg(cf, 1), &c)?;
        let size = extent(g, cf_arg(cf, 2))?;
        compute::retain_encoder(
            &encoder,
            &[
                Resource::Texture(source.texture.clone()),
                Resource::Texture(dest.texture.clone()),
            ],
        );
        compute::with_encoder(g, &c, &encoder, |e| {
            e.copy_texture_to_texture(source.descriptor(), dest.descriptor(), size)
        })?;
        Ok(UNDEF)
    })
}
unsafe extern "sysv64" fn write_texture(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Queue)?;
        let c = client(g, &h)?;
        let source = Val::new(g, cf_arg(cf, 1));
        let dest = copy_texture(g, cf_arg(cf, 0), &c)?;
        let layout = copy_layout(g, cf_arg(cf, 2))?;
        let size = extent(g, cf_arg(cf, 3))?;
        // Descriptor getters/coercions can modify or detach the source. Take
        // the snapshot only after they have all executed.
        let bytes = buffer::copy_source(g, source.v, 0, None)?;
        checked(g, &c, || {
            c.context
                .queue
                .write_texture(dest.descriptor(), &bytes, layout, size)
        });
        Ok(UNDEF)
    })
}

pub(super) unsafe fn install(g: *mut c_void) {
    class(
        g,
        Kind::Texture,
        "GPUTexture",
        &[
            ("createView", 0, create_view),
            ("destroy", 0, destroy_texture),
        ],
        &[
            ("width", texture_width),
            ("height", texture_height),
            ("depthOrArrayLayers", texture_depth),
            ("mipLevelCount", texture_mips),
            ("sampleCount", texture_samples),
            ("dimension", texture_dimension),
            ("format", texture_format),
            ("usage", texture_usage),
        ],
    );
    class(g, Kind::TextureView, "GPUTextureView", &[], &[]);
    class(g, Kind::Sampler, "GPUSampler", &[], &[]);
    class(
        g,
        Kind::RenderPipeline,
        "GPURenderPipeline",
        &[("getBindGroupLayout", 1, render_layout)],
        &[],
    );
    class(
        g,
        Kind::RenderPass,
        "GPURenderPassEncoder",
        &[
            ("setPipeline", 1, pass_pipeline),
            ("setBindGroup", 2, pass_bind_group),
            ("setVertexBuffer", 2, pass_vertex_buffer),
            ("setIndexBuffer", 2, pass_index_buffer),
            ("draw", 1, pass_draw),
            ("drawIndexed", 1, pass_draw_indexed),
            ("setViewport", 6, pass_viewport),
            ("setScissorRect", 4, pass_scissor),
            ("setBlendConstant", 1, pass_blend),
            ("setStencilReference", 1, pass_stencil),
            ("end", 0, end_pass),
        ],
        &[],
    );
    method(g, Kind::Device, "createTexture", 1, create_texture);
    method(g, Kind::Device, "createSampler", 0, create_sampler);
    method(
        g,
        Kind::Device,
        "createRenderPipeline",
        1,
        create_render_pipeline,
    );
    method(g, Kind::Encoder, "beginRenderPass", 1, begin_render_pass);
    method(
        g,
        Kind::Encoder,
        "copyTextureToBuffer",
        3,
        copy_texture_buffer,
    );
    method(
        g,
        Kind::Encoder,
        "copyBufferToTexture",
        3,
        copy_buffer_texture,
    );
    method(
        g,
        Kind::Encoder,
        "copyTextureToTexture",
        3,
        copy_texture_texture,
    );
    method(g, Kind::Queue, "writeTexture", 4, write_texture);
    constants(
        g,
        "GPUTextureUsage",
        &[
            ("COPY_SRC", 1),
            ("COPY_DST", 2),
            ("TEXTURE_BINDING", 4),
            ("STORAGE_BINDING", 8),
            ("RENDER_ATTACHMENT", 16),
        ],
    );
    constants(
        g,
        "GPUColorWrite",
        &[
            ("RED", 1),
            ("GREEN", 2),
            ("BLUE", 4),
            ("ALPHA", 8),
            ("ALL", 15),
        ],
    );
}
