//! Native shader, binding, command and compute interfaces.
use super::*;
use std::num::NonZeroU64;
use std::sync::mpsc;
use std::time::Duration;

#[derive(Clone)]
pub(super) struct Shader {
    pub raw: wgpu::ShaderModule,
    pub source: Arc<str>,
}

/// Retains resource ownership while native wgpu handles are stored in commands.
pub(super) struct Encoder {
    pub raw: Option<wgpu::CommandEncoder>,
    pub active: bool,
    pub resources: Vec<Resource>,
}

pub(super) struct CommandBuffer {
    pub raw: Option<wgpu::CommandBuffer>,
    pub resources: Vec<Resource>,
}

#[derive(Clone)]
pub(super) struct BindGroup {
    pub raw: wgpu::BindGroup,
    pub _resources: Arc<Vec<Resource>>,
}

pub(super) fn retain_encoder(encoder: &Arc<Mutex<Encoder>>, resources: &[Resource]) {
    let mut encoder = encoder.lock().unwrap();
    if encoder.raw.is_some() {
        encoder.resources.extend_from_slice(resources);
    }
}

pub(super) struct ComputePass {
    raw: Option<wgpu::ComputePass<'static>>,
    owner: Arc<Mutex<Encoder>>,
    client: Arc<Client>,
}

impl Drop for ComputePass {
    fn drop(&mut self) {
        if let Some(raw) = self.raw.take() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.client.checked(|| drop(raw))
            }));
            self.owner.lock().unwrap_or_else(|e| e.into_inner()).active = false;
        }
    }
}

/// The JS pass owns its encoder through an Arc, but the encoder is unavailable
/// for recording/finish until end() or pass collection. No Rust lifetime is
/// transmuted: wgpu's safe forget_lifetime retains its native runtime lock.
pub(super) unsafe fn with_encoder<T>(
    g: *mut c_void,
    c: &Arc<Client>,
    encoder: &Arc<Mutex<Encoder>>,
    f: impl FnOnce(&mut wgpu::CommandEncoder) -> T,
) -> Result<T, ()> {
    {
        let e = encoder.lock().unwrap();
        if e.active || e.raw.is_none() {
            drop(e);
            return invalid(g, "GPUCommandEncoder is finished or has an active pass")
                .map(|_| unreachable!());
        }
    }
    Ok(checked(g, c, || {
        let mut e = encoder.lock().unwrap();
        f(e.raw.as_mut().unwrap())
    }))
}

unsafe fn with_pass<T>(
    g: *mut c_void,
    c: &Arc<Client>,
    pass: &Arc<Mutex<ComputePass>>,
    f: impl FnOnce(&mut wgpu::ComputePass<'static>) -> T,
) -> Result<T, ()> {
    if pass.lock().unwrap().raw.is_none() {
        return invalid(g, "GPUComputePassEncoder has ended").map(|_| unreachable!());
    }
    Ok(checked(g, c, || {
        let mut p = pass.lock().unwrap();
        f(p.raw.as_mut().unwrap())
    }))
}

pub(super) unsafe fn parse_pipeline_layout(
    g: *mut c_void,
    descriptor: u64,
    c: &Arc<Client>,
) -> Result<Option<wgpu::PipelineLayout>, ()> {
    let layout = field(g, descriptor, "layout")?;
    if glue::rjs_kind(g, layout.v) == 5 {
        if text(g, layout.v, "")? == "auto" {
            return Ok(None);
        }
        invalid(g, "GPUPipelineLayout must be a layout or 'auto'")?;
    }
    let h = owned(g, layout.v, Kind::PipelineLayout, c)?;
    let Resource::PipelineLayout(raw) = h.resource else {
        unreachable!()
    };
    Ok(Some(raw))
}

/// The Uint32Array overload indexes elements, not its byte backing store.
/// Convert all arguments before locking the pass: getters may reenter JS.
pub(super) unsafe fn dynamic_offsets(
    g: *mut c_void,
    cf: *mut c_void,
    startarg: u32,
) -> Result<Vec<u32>, ()> {
    let value = cf_arg(cf, startarg);
    if value == UNDEF {
        return Ok(Vec::new());
    }
    let values = seq(g, value)?;
    let mut offsets = Vec::with_capacity(values.len());
    for value in values {
        offsets.push(uint(g, value.v, 0)?);
    }
    if cf_argc(cf) > startarg + 1 {
        let mut info = crate::ffi::RjsTaInfo::default();
        if glue::rjs_typedarray_info(g, value, &mut info) != 0
            || info.kind != 7
            || info.detached != 0
        {
            invalid(g, "Dynamic offset range overload requires a Uint32Array")?;
        }
        let start = num(g, cf_arg(cf, startarg + 1), 0)?;
        let length = num(
            g,
            cf_arg(cf, startarg + 2),
            offsets.len() as u64 - start.min(offsets.len() as u64),
        )?;
        let Some(end) = start.checked_add(length) else {
            invalid(g, "Dynamic offset range exceeds the array")?;
            unreachable!()
        };
        if end > offsets.len() as u64 {
            invalid(g, "Dynamic offset range exceeds the array")?;
        }
        offsets = offsets[start as usize..end as usize].to_vec();
    }
    Ok(offsets)
}

pub(super) unsafe fn stage_constants(g: *mut c_void, stage: u64) -> Result<Vec<(String, f64)>, ()> {
    let constants = field(g, stage, "constants")?;
    if constants.v == UNDEF {
        return Ok(Vec::new());
    }
    let mut keys = UNDEF;
    if glue::rjs_own_keys(g, constants.v, &mut keys) != 0 {
        glue::rjs_throw(g, keys);
        return Err(());
    }
    let keys = Val::new(g, keys);
    let mut out = Vec::new();
    for key in seq(g, keys.v)? {
        let key = text(g, key.v, "")?;
        let value = field(g, constants.v, &key)?;
        let number = to_number(g, value.v)?;
        if !number.is_finite() {
            invalid(g, "Pipeline constants must be finite numbers")?;
        }
        out.push((key, number));
    }
    Ok(out)
}

unsafe extern "sysv64" fn create_shader(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Device)?;
        let c = client(g, &h)?;
        let descriptor = Val::new(g, cf_arg(cf, 0));
        let source = field(g, descriptor.v, "code")?;
        if source.v == UNDEF {
            invalid(g, "GPUShaderModuleDescriptor.code is required")?;
        }
        let source: Arc<str> = Arc::from(text(g, source.v, "")?);
        let label = text(g, field(g, descriptor.v, "label")?.v, "")?;
        let raw = checked(g, &c, || {
            c.context
                .device
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some(&label),
                    source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(&source)),
                })
        });
        Ok(wrap(
            g,
            Kind::Shader,
            Some(c),
            Resource::Shader(Shader { raw, source }),
        ))
    })
}

fn utf16_location(source: &str, location: wgpu::SourceLocation) -> (u64, u64, u64, u64) {
    // Slang/WGSL diagnostics from wgpu use UTF-8 byte spans; WebGPU reports
    // UTF-16 code units. Clamp malformed driver spans without slicing a codepoint.
    let boundary = |offset: usize| {
        let mut n = offset.min(source.len());
        while !source.is_char_boundary(n) {
            n -= 1;
        }
        n
    };
    let start = boundary(location.offset as usize);
    let end = boundary((location.offset as usize).saturating_add(location.length as usize));
    let prefix = &source[..start];
    let line_start = prefix.rfind('\n').map_or(0, |p| p + 1);
    (
        1 + prefix.bytes().filter(|&b| b == b'\n').count() as u64,
        1 + source[line_start..start].encode_utf16().count() as u64,
        prefix.encode_utf16().count() as u64,
        source[start..end.max(start)].encode_utf16().count() as u64,
    )
}

unsafe fn compilation_info(g: *mut c_void, info: wgpu::CompilationInfo, source: &str) -> u64 {
    let messages = Val::new(g, new_array(g));
    for (index, message) in info.messages.into_iter().enumerate() {
        let object = Val::new(g, new_object(g, UNDEF));
        define(
            g,
            object.v,
            "message",
            js_string(g, &message.message),
            1 | 2,
        );
        let kind = match message.message_type {
            wgpu::CompilationMessageType::Error => "error",
            wgpu::CompilationMessageType::Warning => "warning",
            wgpu::CompilationMessageType::Info => "info",
        };
        define(g, object.v, "type", js_string(g, kind), 1 | 2);
        let (line, column, offset, length) = message
            .location
            .map_or((0, 0, 0, 0), |l| utf16_location(source, l));
        for (key, value) in [
            ("lineNum", line),
            ("linePos", column),
            ("offset", offset),
            ("length", length),
        ] {
            define(g, object.v, key, enc_f64(value as f64), 1 | 2);
        }
        set_i(g, messages.v, index as u32, object.v);
    }
    let object = Val::new(g, new_object(g, UNDEF));
    define(g, object.v, "messages", messages.v, 1 | 2);
    object.v
}

unsafe extern "sysv64" fn get_compilation_info(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Shader)?;
        let Resource::Shader(shader) = h.resource else {
            unreachable!()
        };
        let source = shader.source.clone();
        Ok(async_op(
            g,
            vec![cf_this(cf)],
            async move { Ok(shader.raw.get_compilation_info().await) },
            move |g, info| compilation_info(g, info, &source),
        ))
    })
}

unsafe fn view_dimension(g: *mut c_void, value: u64) -> Result<wgpu::TextureViewDimension, ()> {
    Ok(match text(g, value, "2d")?.as_str() {
        "1d" => wgpu::TextureViewDimension::D1,
        "2d" => wgpu::TextureViewDimension::D2,
        "2d-array" => wgpu::TextureViewDimension::D2Array,
        "cube" => wgpu::TextureViewDimension::Cube,
        "cube-array" => wgpu::TextureViewDimension::CubeArray,
        "3d" => wgpu::TextureViewDimension::D3,
        _ => {
            invalid(g, "Invalid GPUTextureViewDimension")?;
            unreachable!()
        }
    })
}

unsafe fn required(g: *mut c_void, object: u64, name: &str) -> Result<Val, ()> {
    let value = field(g, object, name)?;
    if value.v == UNDEF {
        invalid(
            g,
            &format!("Required WebGPU descriptor field '{name}' is missing"),
        )?;
    }
    Ok(value)
}

unsafe fn binding_type(g: *mut c_void, entry: u64) -> Result<wgpu::BindingType, ()> {
    let buffer = field(g, entry, "buffer")?;
    let sampler = field(g, entry, "sampler")?;
    let texture = field(g, entry, "texture")?;
    let storage = field(g, entry, "storageTexture")?;
    let count = [buffer.v, sampler.v, texture.v, storage.v]
        .into_iter()
        .filter(|&v| v != UNDEF)
        .count();
    if count != 1 {
        invalid(
            g,
            "Bind group layout entry requires exactly one supported resource layout",
        )?;
    }
    if buffer.v != UNDEF {
        let ty = match text(g, field(g, buffer.v, "type")?.v, "uniform")?.as_str() {
            "uniform" => wgpu::BufferBindingType::Uniform,
            "storage" => wgpu::BufferBindingType::Storage { read_only: false },
            "read-only-storage" => wgpu::BufferBindingType::Storage { read_only: true },
            _ => {
                invalid(g, "Invalid GPUBufferBindingType")?;
                unreachable!()
            }
        };
        let size = num(g, field(g, buffer.v, "minBindingSize")?.v, 0)?;
        return Ok(wgpu::BindingType::Buffer {
            ty,
            has_dynamic_offset: boolean(g, field(g, buffer.v, "hasDynamicOffset")?.v),
            min_binding_size: NonZeroU64::new(size),
        });
    }
    if sampler.v != UNDEF {
        let ty = match text(g, field(g, sampler.v, "type")?.v, "filtering")?.as_str() {
            "filtering" => wgpu::SamplerBindingType::Filtering,
            "non-filtering" => wgpu::SamplerBindingType::NonFiltering,
            "comparison" => wgpu::SamplerBindingType::Comparison,
            _ => {
                invalid(g, "Invalid GPUSamplerBindingType")?;
                unreachable!()
            }
        };
        return Ok(wgpu::BindingType::Sampler(ty));
    }
    if texture.v != UNDEF {
        let sample_type = match text(g, field(g, texture.v, "sampleType")?.v, "float")?.as_str() {
            "float" => wgpu::TextureSampleType::Float { filterable: true },
            "unfilterable-float" => wgpu::TextureSampleType::Float { filterable: false },
            "depth" => wgpu::TextureSampleType::Depth,
            "sint" => wgpu::TextureSampleType::Sint,
            "uint" => wgpu::TextureSampleType::Uint,
            _ => {
                invalid(g, "Invalid GPUTextureSampleType")?;
                unreachable!()
            }
        };
        return Ok(wgpu::BindingType::Texture {
            sample_type,
            view_dimension: view_dimension(g, field(g, texture.v, "viewDimension")?.v)?,
            multisampled: boolean(g, field(g, texture.v, "multisampled")?.v),
        });
    }
    let access = match text(g, field(g, storage.v, "access")?.v, "write-only")?.as_str() {
        "write-only" => wgpu::StorageTextureAccess::WriteOnly,
        "read-only" => wgpu::StorageTextureAccess::ReadOnly,
        "read-write" => wgpu::StorageTextureAccess::ReadWrite,
        _ => {
            invalid(g, "Invalid GPUStorageTextureAccess")?;
            unreachable!()
        }
    };
    let format = texture::parse_format(g, field(g, storage.v, "format")?.v)?;
    Ok(wgpu::BindingType::StorageTexture {
        access,
        format,
        view_dimension: view_dimension(g, field(g, storage.v, "viewDimension")?.v)?,
    })
}

unsafe extern "sysv64" fn create_bind_group_layout(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Device)?;
        let c = client(g, &h)?;
        let descriptor = Val::new(g, cf_arg(cf, 0));
        let label = text(g, field(g, descriptor.v, "label")?.v, "")?;
        let mut entries = Vec::new();
        for entry in seq(g, required(g, descriptor.v, "entries")?.v)? {
            let binding = uint(g, required(g, entry.v, "binding")?.v, 0)?;
            let bits = uint(g, required(g, entry.v, "visibility")?.v, 0)?;
            if bits & !7 != 0 {
                invalid(
                    g,
                    "GPUShaderStage only accepts VERTEX, FRAGMENT and COMPUTE",
                )?;
            }
            let Some(visibility) = wgpu::ShaderStages::from_bits(bits) else {
                invalid(g, "Invalid GPUShaderStage flags")?;
                unreachable!()
            };
            entries.push(wgpu::BindGroupLayoutEntry {
                binding,
                visibility,
                ty: binding_type(g, entry.v)?,
                count: None,
            });
        }
        let raw = checked(g, &c, || {
            c.context
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some(&label),
                    entries: &entries,
                })
        });
        Ok(wrap(
            g,
            Kind::BindGroupLayout,
            Some(c),
            Resource::BindGroupLayout(raw),
        ))
    })
}

unsafe extern "sysv64" fn create_pipeline_layout(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Device)?;
        let c = client(g, &h)?;
        let descriptor = Val::new(g, cf_arg(cf, 0));
        let label = text(g, field(g, descriptor.v, "label")?.v, "")?;
        let mut layouts = Vec::new();
        for value in seq(g, required(g, descriptor.v, "bindGroupLayouts")?.v)? {
            if value.v == NULL {
                layouts.push(None);
                continue;
            }
            let h = owned(g, value.v, Kind::BindGroupLayout, &c)?;
            let Resource::BindGroupLayout(raw) = h.resource else {
                unreachable!()
            };
            layouts.push(Some(raw));
        }
        let refs: Vec<_> = layouts.iter().map(Option::as_ref).collect();
        let raw = checked(g, &c, || {
            c.context
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some(&label),
                    bind_group_layouts: &refs,
                    immediate_size: 0,
                })
        });
        Ok(wrap(
            g,
            Kind::PipelineLayout,
            Some(c),
            Resource::PipelineLayout(raw),
        ))
    })
}

enum Binding {
    Buffer(Arc<buffer::Buffer>, u64, Option<NonZeroU64>),
    Texture(texture::TextureView),
    Sampler(wgpu::Sampler),
}
impl Binding {
    fn retained(&self) -> Resource {
        match self {
            Self::Buffer(buffer, _, _) => Resource::Buffer(buffer.clone()),
            Self::Texture(view) => Resource::TextureView(view.clone()),
            Self::Sampler(sampler) => Resource::Sampler(sampler.clone()),
        }
    }
    fn resource(&self) -> wgpu::BindingResource<'_> {
        match self {
            Self::Buffer(b, offset, size) => wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                buffer: &b.raw,
                offset: *offset,
                size: *size,
            }),
            Self::Texture(v) => wgpu::BindingResource::TextureView(&v.raw),
            Self::Sampler(s) => wgpu::BindingResource::Sampler(s),
        }
    }
}

unsafe fn binding(g: *mut c_void, value: u64, c: &Arc<Client>) -> Result<Binding, ()> {
    // Check host brands without invoking user-visible getters or relying on
    // instanceof (whose prototype is writable by application code).
    let hosts: Vec<_> = {
        let s = states().lock().unwrap();
        let c = s[&(g as usize)].classes.as_ref().unwrap();
        [Kind::TextureView, Kind::Sampler]
            .iter()
            .map(|k| c.hosts[k] as capi::JSClassRef)
            .collect()
    };
    let resource = hosts
        .into_iter()
        .find_map(|host| host_data::<Handle>(g, host, value).cloned());
    if let Some(h) = resource {
        if h.client.as_ref().map(|x| x.id) != Some(c.id) {
            invalid(g, "Bind group resource belongs to a different device")?;
        }
        match h.resource {
            Resource::TextureView(v) => return Ok(Binding::Texture(v)),
            Resource::Sampler(s) => return Ok(Binding::Sampler(s)),
            _ => {
                invalid(g, "Unsupported bind group resource interface")?;
            }
        }
    }
    let b = field(g, value, "buffer")?;
    let h = owned(g, b.v, Kind::Buffer, c)?;
    let Resource::Buffer(buffer) = h.resource else {
        unreachable!()
    };
    let offset = num(g, field(g, value, "offset")?.v, 0)?;
    let size_value = field(g, value, "size")?;
    let size = if size_value.v == UNDEF {
        None
    } else {
        let number = num(g, size_value.v, 0)?;
        let Some(size) = NonZeroU64::new(number) else {
            invalid(g, "GPUBufferBinding.size must be positive")?;
            unreachable!()
        };
        Some(size)
    };
    Ok(Binding::Buffer(buffer, offset, size))
}

unsafe extern "sysv64" fn create_bind_group(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Device)?;
        let c = client(g, &h)?;
        let descriptor = Val::new(g, cf_arg(cf, 0));
        let label = text(g, field(g, descriptor.v, "label")?.v, "")?;
        let layout = required(g, descriptor.v, "layout")?;
        let h = owned(g, layout.v, Kind::BindGroupLayout, &c)?;
        let Resource::BindGroupLayout(layout) = h.resource else {
            unreachable!()
        };
        let mut values = Vec::new();
        for value in seq(g, required(g, descriptor.v, "entries")?.v)? {
            let index = uint(g, required(g, value.v, "binding")?.v, 0)?;
            let res = required(g, value.v, "resource")?;
            values.push((index, binding(g, res.v, &c)?));
        }
        let entries: Vec<_> = values
            .iter()
            .map(|(binding, value)| wgpu::BindGroupEntry {
                binding: *binding,
                resource: value.resource(),
            })
            .collect();
        let raw = checked(g, &c, || {
            c.context
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some(&label),
                    layout: &layout,
                    entries: &entries,
                })
        });
        let resources = Arc::new(
            values
                .iter()
                .map(|(_, binding)| binding.retained())
                .collect(),
        );
        Ok(wrap(
            g,
            Kind::BindGroup,
            Some(c),
            Resource::BindGroup(BindGroup {
                raw,
                _resources: resources,
            }),
        ))
    })
}

unsafe extern "sysv64" fn create_compute_pipeline(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Device)?;
        let c = client(g, &h)?;
        let descriptor = Val::new(g, cf_arg(cf, 0));
        let label = text(g, field(g, descriptor.v, "label")?.v, "")?;
        let layout = parse_pipeline_layout(g, descriptor.v, &c)?;
        let stage = required(g, descriptor.v, "compute")?;
        let module = field(g, stage.v, "module")?;
        let h = owned(g, module.v, Kind::Shader, &c)?;
        let Resource::Shader(shader) = h.resource else {
            unreachable!()
        };
        let entry_value = field(g, stage.v, "entryPoint")?;
        let entry = if entry_value.v == UNDEF {
            None
        } else {
            Some(text(g, entry_value.v, "")?)
        };
        let constants = stage_constants(g, stage.v)?;
        let constant_refs: Vec<_> = constants.iter().map(|(n, v)| (n.as_str(), *v)).collect();
        let raw = checked(g, &c, || {
            c.context
                .device
                .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(&label),
                    layout: layout.as_ref(),
                    module: &shader.raw,
                    entry_point: entry.as_deref(),
                    compilation_options: wgpu::PipelineCompilationOptions {
                        constants: &constant_refs,
                        zero_initialize_workgroup_memory: true,
                    },
                    cache: None,
                })
        });
        Ok(wrap(
            g,
            Kind::ComputePipeline,
            Some(c),
            Resource::ComputePipeline(raw),
        ))
    })
}

unsafe extern "sysv64" fn compute_bind_group_layout(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::ComputePipeline)?;
        let c = client(g, &h)?;
        let Resource::ComputePipeline(pipeline) = h.resource else {
            unreachable!()
        };
        let index = uint(g, cf_arg(cf, 0), 0)?;
        let layout = checked(g, &c, || pipeline.get_bind_group_layout(index));
        Ok(wrap(
            g,
            Kind::BindGroupLayout,
            Some(c),
            Resource::BindGroupLayout(layout),
        ))
    })
}

unsafe extern "sysv64" fn create_encoder(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Device)?;
        let c = client(g, &h)?;
        let descriptor = Val::new(g, cf_arg(cf, 0));
        let label = text(g, field(g, descriptor.v, "label")?.v, "")?;
        let raw = checked(g, &c, || {
            c.context
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some(&label),
                })
        });
        Ok(wrap(
            g,
            Kind::Encoder,
            Some(c),
            Resource::Encoder(Arc::new(Mutex::new(Encoder {
                raw: Some(raw),
                active: false,
                resources: Vec::new(),
            }))),
        ))
    })
}

unsafe extern "sysv64" fn begin_compute_pass(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Encoder)?;
        let c = client(g, &h)?;
        let Resource::Encoder(owner) = h.resource else {
            unreachable!()
        };
        let descriptor = Val::new(g, cf_arg(cf, 0));
        let label = text(g, field(g, descriptor.v, "label")?.v, "")?;
        if field(g, descriptor.v, "timestampWrites")?.v != UNDEF {
            super::super::domexception::throw(
                g,
                "NotSupportedError",
                "Timestamp queries are not implemented",
            );
            return Err(());
        }
        let raw = with_encoder(g, &c, &owner, |encoder| {
            encoder
                .begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some(&label),
                    timestamp_writes: None,
                })
                .forget_lifetime()
        })?;
        owner.lock().unwrap().active = true;
        let pass = ComputePass {
            raw: Some(raw),
            owner,
            client: c.clone(),
        };
        Ok(wrap(
            g,
            Kind::ComputePass,
            Some(c),
            Resource::ComputePass(Arc::new(Mutex::new(pass))),
        ))
    })
}

unsafe extern "sysv64" fn copy_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Encoder)?;
        let c = client(g, &h)?;
        let Resource::Encoder(encoder) = h.resource else {
            unreachable!()
        };
        let source = owned(g, cf_arg(cf, 0), Kind::Buffer, &c)?;
        let source_offset = num(g, cf_arg(cf, 1), 0)?;
        let destination = owned(g, cf_arg(cf, 2), Kind::Buffer, &c)?;
        let destination_offset = num(g, cf_arg(cf, 3), 0)?;
        let size = num(g, cf_arg(cf, 4), 0)?;
        let Resource::Buffer(source) = source.resource else {
            unreachable!()
        };
        let Resource::Buffer(destination) = destination.resource else {
            unreachable!()
        };
        retain_encoder(
            &encoder,
            &[
                Resource::Buffer(source.clone()),
                Resource::Buffer(destination.clone()),
            ],
        );
        with_encoder(g, &c, &encoder, |e| {
            e.copy_buffer_to_buffer(
                &source.raw,
                source_offset,
                &destination.raw,
                destination_offset,
                size,
            )
        })?;
        Ok(UNDEF)
    })
}

unsafe extern "sysv64" fn clear_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Encoder)?;
        let c = client(g, &h)?;
        let Resource::Encoder(encoder) = h.resource else {
            unreachable!()
        };
        let buffer = owned(g, cf_arg(cf, 0), Kind::Buffer, &c)?;
        let Resource::Buffer(buffer) = buffer.resource else {
            unreachable!()
        };
        let offset = num(g, cf_arg(cf, 1), 0)?;
        let size = if cf_arg(cf, 2) == UNDEF {
            None
        } else {
            Some(num(g, cf_arg(cf, 2), 0)?)
        };
        retain_encoder(&encoder, &[Resource::Buffer(buffer.clone())]);
        with_encoder(g, &c, &encoder, |e| {
            e.clear_buffer(&buffer.raw, offset, size)
        })?;
        Ok(UNDEF)
    })
}

unsafe extern "sysv64" fn finish(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Encoder)?;
        let c = client(g, &h)?;
        let Resource::Encoder(encoder) = h.resource else {
            unreachable!()
        };
        // The descriptor only contains a label; still run conversion before
        // consuming the encoder, so an accessor exception leaves it usable.
        let descriptor = Val::new(g, cf_arg(cf, 0));
        let _label = text(g, field(g, descriptor.v, "label")?.v, "")?;
        let (raw, resources) = {
            let mut e = encoder.lock().unwrap();
            if e.active || e.raw.is_none() {
                drop(e);
                invalid(
                    g,
                    "GPUCommandEncoder cannot finish while a pass is active or after finish",
                )?;
                unreachable!()
            }
            (e.raw.take().unwrap(), std::mem::take(&mut e.resources))
        };
        let raw = checked(g, &c, || raw.finish());
        Ok(wrap(
            g,
            Kind::CommandBuffer,
            Some(c),
            Resource::CommandBuffer(Arc::new(Mutex::new(CommandBuffer {
                raw: Some(raw),
                resources,
            }))),
        ))
    })
}

unsafe extern "sysv64" fn pass_set_pipeline(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::ComputePass)?;
        let c = client(g, &h)?;
        let Resource::ComputePass(pass) = h.resource else {
            unreachable!()
        };
        let h = owned(g, cf_arg(cf, 0), Kind::ComputePipeline, &c)?;
        let Resource::ComputePipeline(pipeline) = h.resource else {
            unreachable!()
        };
        with_pass(g, &c, &pass, |p| p.set_pipeline(&pipeline))?;
        Ok(UNDEF)
    })
}

unsafe extern "sysv64" fn pass_set_bind_group(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::ComputePass)?;
        let c = client(g, &h)?;
        let Resource::ComputePass(pass) = h.resource else {
            unreachable!()
        };
        let index = uint(g, cf_arg(cf, 0), 0)?;
        let group = if cf_arg(cf, 1) == NULL {
            None
        } else {
            let h = owned(g, cf_arg(cf, 1), Kind::BindGroup, &c)?;
            let Resource::BindGroup(raw) = h.resource else {
                unreachable!()
            };
            Some(raw)
        };
        let offsets = dynamic_offsets(g, cf, 2)?;
        if let Some(group) = &group {
            let owner = pass.lock().unwrap().owner.clone();
            retain_encoder(&owner, &[Resource::BindGroup(group.clone())]);
        }
        with_pass(g, &c, &pass, |p| {
            p.set_bind_group(index, group.as_ref().map(|group| &group.raw), &offsets)
        })?;
        Ok(UNDEF)
    })
}

unsafe extern "sysv64" fn dispatch(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::ComputePass)?;
        let c = client(g, &h)?;
        let Resource::ComputePass(pass) = h.resource else {
            unreachable!()
        };
        let x = uint(g, cf_arg(cf, 0), 0)?;
        if x > c.limits.max_compute_workgroups_per_dimension {
            validation(g, &c, "dispatchWorkgroups exceeds the logical device limit");
            return Ok(UNDEF);
        }
        let y = uint(g, cf_arg(cf, 1), 1)?;
        let z = uint(g, cf_arg(cf, 2), 1)?;
        if y > c.limits.max_compute_workgroups_per_dimension
            || z > c.limits.max_compute_workgroups_per_dimension
        {
            validation(g, &c, "dispatchWorkgroups exceeds the logical device limit");
            return Ok(UNDEF);
        }
        with_pass(g, &c, &pass, |p| p.dispatch_workgroups(x, y, z))?;
        Ok(UNDEF)
    })
}

unsafe extern "sysv64" fn dispatch_indirect(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::ComputePass)?;
        let c = client(g, &h)?;
        let Resource::ComputePass(pass) = h.resource else {
            unreachable!()
        };
        let h = owned(g, cf_arg(cf, 0), Kind::Buffer, &c)?;
        let Resource::Buffer(buffer) = h.resource else {
            unreachable!()
        };
        let offset = num(g, cf_arg(cf, 1), 0)?;
        let owner = pass.lock().unwrap().owner.clone();
        retain_encoder(&owner, &[Resource::Buffer(buffer.clone())]);
        with_pass(g, &c, &pass, |p| {
            p.dispatch_workgroups_indirect(&buffer.raw, offset)
        })?;
        Ok(UNDEF)
    })
}

unsafe extern "sysv64" fn end(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::ComputePass)?;
        let c = client(g, &h)?;
        let Resource::ComputePass(pass) = h.resource else {
            unreachable!()
        };
        let (raw, owner) = {
            let mut p = pass.lock().unwrap();
            let Some(raw) = p.raw.take() else {
                drop(p);
                invalid(g, "GPUComputePassEncoder has already ended")?;
                unreachable!()
            };
            (raw, p.owner.clone())
        };
        checked(g, &c, || drop(raw));
        owner.lock().unwrap().active = false;
        Ok(UNDEF)
    })
}

unsafe extern "sysv64" fn submit(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Queue)?;
        let c = client(g, &h)?;
        let mut commands = Vec::new();
        for value in seq(g, cf_arg(cf, 0))? {
            let h = owned(g, value.v, Kind::CommandBuffer, &c)?;
            let Resource::CommandBuffer(raw) = h.resource else {
                unreachable!()
            };
            // Validate the whole input before consuming any one buffer. A
            // duplicate must not partially submit or consume other entries.
            if commands.iter().any(|other| Arc::ptr_eq(other, &raw))
                || raw.lock().unwrap().raw.is_none()
            {
                validation(g, &c, "A command buffer was submitted twice");
                return Ok(UNDEF);
            }
            commands.push(raw);
        }
        if let Err(e) = c.check() {
            return Ok(super::super::domexception::throw(
                g,
                "InvalidStateError",
                &e,
            ));
        }
        // Iteration/getters above may reenter submit() or destroy resources.
        // Revalidate the complete batch after all JavaScript has finished.
        if commands.iter().any(|command| {
            let command = command.lock().unwrap();
            command.raw.is_none() || !command.resources.iter().all(resource_alive)
        }) {
            validation(
                g,
                &c,
                "Command buffer was consumed or references a destroyed resource",
            );
            return Ok(UNDEF);
        }
        let mut native_commands = Vec::with_capacity(commands.len());
        let mut resources = Vec::new();
        for command in commands {
            let mut command = command.lock().unwrap();
            native_commands.push(command.raw.take().unwrap());
            resources.append(&mut command.resources);
        }
        checked(g, &c, || c.context.queue.submit(native_commands));
        // The queue now retains the native resources for work in flight. There
        // is no remaining JS command object capable of exposing these owners.
        drop(resources);
        Ok(UNDEF)
    })
}

unsafe extern "sysv64" fn on_submitted_work_done(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Queue)?;
        let c = client(g, &h)?;
        let (tx, rx) = mpsc::sync_channel(1);
        checked(g, &c, || {
            c.context.queue.on_submitted_work_done(move || {
                let _ = tx.send(());
            })
        });
        let context = c.context.clone();
        Ok(async_op(
            g,
            vec![cf_this(cf)],
            async move {
                context
                    .device
                    .poll(wgpu::PollType::Wait {
                        submission_index: None,
                        timeout: Some(Duration::from_secs(30)),
                    })
                    .map_err(|e| e.to_string())?;
                rx.recv_timeout(Duration::from_secs(30))
                    .map_err(|e| e.to_string())?;
                Ok(())
            },
            |_g, ()| UNDEF,
        ))
    })
}

pub(super) unsafe fn install(g: *mut c_void) {
    class(
        g,
        Kind::Shader,
        "GPUShaderModule",
        &[("getCompilationInfo", 0, get_compilation_info)],
        &[],
    );
    class(g, Kind::BindGroupLayout, "GPUBindGroupLayout", &[], &[]);
    class(g, Kind::PipelineLayout, "GPUPipelineLayout", &[], &[]);
    class(g, Kind::BindGroup, "GPUBindGroup", &[], &[]);
    class(
        g,
        Kind::ComputePipeline,
        "GPUComputePipeline",
        &[("getBindGroupLayout", 1, compute_bind_group_layout)],
        &[],
    );
    class(g, Kind::CommandBuffer, "GPUCommandBuffer", &[], &[]);
    class(
        g,
        Kind::Encoder,
        "GPUCommandEncoder",
        &[
            ("beginComputePass", 0, begin_compute_pass),
            ("copyBufferToBuffer", 5, copy_buffer),
            ("clearBuffer", 1, clear_buffer),
            ("finish", 0, finish),
        ],
        &[],
    );
    class(
        g,
        Kind::ComputePass,
        "GPUComputePassEncoder",
        &[
            ("setPipeline", 1, pass_set_pipeline),
            ("setBindGroup", 2, pass_set_bind_group),
            ("dispatchWorkgroups", 1, dispatch),
            ("dispatchWorkgroupsIndirect", 2, dispatch_indirect),
            ("end", 0, end),
        ],
        &[],
    );
    for (name, arity, function) in [
        ("createShaderModule", 1, create_shader as HostFn),
        ("createBindGroupLayout", 1, create_bind_group_layout),
        ("createPipelineLayout", 1, create_pipeline_layout),
        ("createBindGroup", 1, create_bind_group),
        ("createComputePipeline", 1, create_compute_pipeline),
        ("createCommandEncoder", 0, create_encoder),
    ] {
        method(g, Kind::Device, name, arity, function);
    }
    method(g, Kind::Queue, "submit", 1, submit);
    method(
        g,
        Kind::Queue,
        "onSubmittedWorkDone",
        0,
        on_submitted_work_done,
    );
    constants(
        g,
        "GPUShaderStage",
        &[("VERTEX", 1), ("FRAGMENT", 2), ("COMPUTE", 4)],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utf8_diagnostic_spans_become_utf16() {
        let source = "// 🦀\n//é\ninvalid";
        let offset = source.find("invalid").unwrap();
        let mapped = utf16_location(
            source,
            wgpu::SourceLocation {
                line_number: 99,
                line_position: 99,
                offset: offset as u32,
                length: 7,
            },
        );
        assert_eq!(
            mapped,
            (3, 1, source[..offset].encode_utf16().count() as u64, 7)
        );
        // A driver span in the middle of a UTF-8 sequence must not panic.
        let mapped = utf16_location(
            "🦀x",
            wgpu::SourceLocation {
                line_number: 1,
                line_position: 2,
                offset: 1,
                length: 3,
            },
        );
        assert_eq!(mapped, (1, 1, 0, 2));
    }
}
