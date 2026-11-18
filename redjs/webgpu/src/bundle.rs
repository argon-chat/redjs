//! Render bundles. Commands are recorded with owned resources and replayed
//! into a native bundle encoder at finish(), whose lifetime borrows them.
use super::*;
use std::ops::Range;

enum Command {
    Pipeline(wgpu::RenderPipeline),
    BindGroup(u32, Option<wgpu::BindGroup>, Vec<u32>),
    Vertex(u32, Option<(Arc<buffer::Buffer>, Range<u64>)>),
    Index(Arc<buffer::Buffer>, wgpu::IndexFormat, Range<u64>),
    Draw(Range<u32>, Range<u32>),
    DrawIndexed(Range<u32>, i32, Range<u32>),
    DrawIndirect(Arc<buffer::Buffer>, u64),
    DrawIndexedIndirect(Arc<buffer::Buffer>, u64),
    Immediates(u32, Vec<u8>),
}

pub(super) struct Encoder {
    color_formats: Vec<Option<wgpu::TextureFormat>>,
    depth_stencil: Option<wgpu::RenderBundleDepthStencil>,
    sample_count: u32,
    commands: Vec<Command>,
    resources: Vec<Resource>,
    // wgpu bundles carry no debug groups; their balance is still validated.
    debug_depth: u32,
    unbalanced: bool,
    finished: bool,
}

pub(super) struct Bundle {
    pub raw: Option<wgpu::RenderBundle>,
    pub resources: Vec<Resource>,
    pub valid: bool,
}

#[redjs::host_fn]
unsafe fn create(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Device)?;
        let c = client(g, &h)?;
        let d = Val::new(g, cf_arg(cf, 0));
        let label = text(g, field(g, d.v, "label")?.v, "")?;
        let formats = field(g, d.v, "colorFormats")?;
        if formats.v == UNDEF {
            invalid(
                g,
                "GPURenderBundleEncoderDescriptor.colorFormats is required",
            )?;
        }
        let mut color_formats = Vec::new();
        for f in seq(g, formats.v)? {
            color_formats.push(if f.v == NULL || f.v == UNDEF {
                None
            } else {
                Some(texture::parse_format(g, f.v)?)
            });
        }
        let depth = field(g, d.v, "depthStencilFormat")?;
        let depth_stencil = if depth.v == UNDEF {
            None
        } else {
            Some(wgpu::RenderBundleDepthStencil {
                format: texture::parse_format(g, depth.v)?,
                depth_read_only: boolean(g, field(g, d.v, "depthReadOnly")?.v),
                stencil_read_only: boolean(g, field(g, d.v, "stencilReadOnly")?.v),
            })
        };
        let sample_count = uint(g, field(g, d.v, "sampleCount")?.v, 1)?;
        if color_formats.len() > c.limits.max_color_attachments as usize {
            validation(
                g,
                &c,
                "Render bundle color formats exceed maxColorAttachments",
            );
        }
        Ok(wrap_labeled(
            g,
            Kind::BundleEncoder,
            Some(c),
            Resource::BundleEncoder(Arc::new(Mutex::new(Encoder {
                color_formats,
                depth_stencil,
                sample_count,
                commands: Vec::new(),
                resources: Vec::new(),
                debug_depth: 0,
                unbalanced: false,
                finished: false,
            }))),
            &label,
        ))
    })
}

unsafe fn encoder_this(
    g: *mut c_void,
    cf: *mut c_void,
) -> Result<(Arc<Client>, Arc<Mutex<Encoder>>), ()> {
    let h = this(g, cf, Kind::BundleEncoder)?;
    let c = client(g, &h)?;
    let Resource::BundleEncoder(e) = h.resource else {
        unreachable!()
    };
    Ok((c, e))
}

unsafe fn record(
    g: *mut c_void,
    c: &Arc<Client>,
    encoder: &Arc<Mutex<Encoder>>,
    command: Command,
    resources: &[Resource],
) {
    let mut e = encoder.lock().unwrap();
    if e.finished {
        drop(e);
        validation(g, c, "GPURenderBundleEncoder is already finished");
        return;
    }
    e.commands.push(command);
    e.resources.extend_from_slice(resources);
}

#[redjs::host_fn]
unsafe fn set_pipeline(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, e) = encoder_this(g, cf)?;
        let h = owned(g, cf_arg(cf, 0), Kind::RenderPipeline, &c)?;
        let Resource::RenderPipeline(p) = h.resource else {
            unreachable!()
        };
        record(g, &c, &e, Command::Pipeline(p), &[]);
        Ok(UNDEF)
    })
}

#[redjs::host_fn]
unsafe fn set_bind_group(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, e) = encoder_this(g, cf)?;
        let index = uint(g, cf_arg(cf, 0), 0)?;
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
        let retained: Vec<_> = group.iter().cloned().map(Resource::BindGroup).collect();
        record(
            g,
            &c,
            &e,
            Command::BindGroup(index, group.map(|b| b.raw), offsets),
            &retained,
        );
        Ok(UNDEF)
    })
}

#[redjs::host_fn]
unsafe fn set_vertex_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, e) = encoder_this(g, cf)?;
        let slot = uint(g, cf_arg(cf, 0), 0)?;
        if cf_arg(cf, 1) == NULL {
            record(g, &c, &e, Command::Vertex(slot, None), &[]);
            return Ok(UNDEF);
        }
        let (b, range) = texture::buffer_range(g, cf, &c, 1, 2, 3)?;
        let retained = [Resource::Buffer(b.clone())];
        record(
            g,
            &c,
            &e,
            Command::Vertex(slot, Some((b, range))),
            &retained,
        );
        Ok(UNDEF)
    })
}

#[redjs::host_fn]
unsafe fn set_index_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, e) = encoder_this(g, cf)?;
        let format = texture::parse_index(g, cf_arg(cf, 1))?;
        let (b, range) = texture::buffer_range(g, cf, &c, 0, 2, 3)?;
        let retained = [Resource::Buffer(b.clone())];
        record(g, &c, &e, Command::Index(b, format, range), &retained);
        Ok(UNDEF)
    })
}

#[redjs::host_fn]
unsafe fn draw(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, e) = encoder_this(g, cf)?;
        let count = uint(g, cf_arg(cf, 0), 0)?;
        let instances = uint(g, cf_arg(cf, 1), 1)?;
        let first = uint(g, cf_arg(cf, 2), 0)?;
        let first_instance = uint(g, cf_arg(cf, 3), 0)?;
        let vertices = texture::draw_range(g, first, count)?;
        let instances = texture::draw_range(g, first_instance, instances)?;
        record(g, &c, &e, Command::Draw(vertices, instances), &[]);
        Ok(UNDEF)
    })
}

#[redjs::host_fn]
unsafe fn draw_indexed(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, e) = encoder_this(g, cf)?;
        let count = uint(g, cf_arg(cf, 0), 0)?;
        let instances = uint(g, cf_arg(cf, 1), 1)?;
        let first = uint(g, cf_arg(cf, 2), 0)?;
        let base = texture::signed(g, cf_arg(cf, 3), 0)?;
        let first_instance = uint(g, cf_arg(cf, 4), 0)?;
        let indices = texture::draw_range(g, first, count)?;
        let instances = texture::draw_range(g, first_instance, instances)?;
        record(
            g,
            &c,
            &e,
            Command::DrawIndexed(indices, base, instances),
            &[],
        );
        Ok(UNDEF)
    })
}

#[redjs::host_fn]
unsafe fn draw_indirect(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, e) = encoder_this(g, cf)?;
        let (b, offset) = texture::indirect_buffer(g, cf, &c)?;
        let retained = [Resource::Buffer(b.clone())];
        record(g, &c, &e, Command::DrawIndirect(b, offset), &retained);
        Ok(UNDEF)
    })
}

#[redjs::host_fn]
unsafe fn draw_indexed_indirect(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, e) = encoder_this(g, cf)?;
        let (b, offset) = texture::indirect_buffer(g, cf, &c)?;
        let retained = [Resource::Buffer(b.clone())];
        record(
            g,
            &c,
            &e,
            Command::DrawIndexedIndirect(b, offset),
            &retained,
        );
        Ok(UNDEF)
    })
}

#[redjs::host_fn]
unsafe fn set_immediates(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, e) = encoder_this(g, cf)?;
        if let Some((offset, bytes)) = compute::immediates(g, cf, &c)? {
            record(g, &c, &e, Command::Immediates(offset, bytes), &[]);
        }
        Ok(UNDEF)
    })
}

#[redjs::host_fn]
unsafe fn push_debug_group(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, e) = encoder_this(g, cf)?;
        text(g, cf_arg(cf, 0), "undefined")?;
        let mut e = e.lock().unwrap();
        if e.finished {
            drop(e);
            validation(g, &c, "GPURenderBundleEncoder is already finished");
        } else {
            e.debug_depth += 1;
        }
        Ok(UNDEF)
    })
}

#[redjs::host_fn]
unsafe fn pop_debug_group(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, e) = encoder_this(g, cf)?;
        let mut e = e.lock().unwrap();
        if e.finished {
            drop(e);
            validation(g, &c, "GPURenderBundleEncoder is already finished");
        } else if e.debug_depth == 0 {
            e.unbalanced = true;
        } else {
            e.debug_depth -= 1;
        }
        Ok(UNDEF)
    })
}

#[redjs::host_fn]
unsafe fn insert_debug_marker(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, e) = encoder_this(g, cf)?;
        text(g, cf_arg(cf, 0), "undefined")?;
        if e.lock().unwrap().finished {
            validation(g, &c, "GPURenderBundleEncoder is already finished");
        }
        Ok(UNDEF)
    })
}

#[redjs::host_fn]
unsafe fn finish(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, e) = encoder_this(g, cf)?;
        let d = Val::new(g, cf_arg(cf, 0));
        let label = text(g, field(g, d.v, "label")?.v, "")?;
        let (formats, depth_stencil, sample_count, commands, resources, balanced, finished) = {
            let mut e = e.lock().unwrap();
            let finished = std::mem::replace(&mut e.finished, true);
            (
                std::mem::take(&mut e.color_formats),
                e.depth_stencil.clone(),
                e.sample_count,
                std::mem::take(&mut e.commands),
                std::mem::take(&mut e.resources),
                !e.unbalanced && e.debug_depth == 0,
                finished,
            )
        };
        if finished {
            validation(g, &c, "GPURenderBundleEncoder is already finished");
        } else if !balanced {
            validation(
                g,
                &c,
                "GPURenderBundleEncoder debug groups are not balanced",
            );
        }
        // wgpu treats a bundle validation failure as fatal and panics; WebGPU
        // makes it a validation error and an invalid bundle.
        let replay = std::panic::AssertUnwindSafe(|| {
            let mut encoder = c.context.device.create_render_bundle_encoder(
                &wgpu::RenderBundleEncoderDescriptor {
                    label: Some(&label),
                    color_formats: &formats,
                    depth_stencil,
                    sample_count,
                    multiview: None,
                },
            );
            for command in &commands {
                match command {
                    Command::Pipeline(p) => encoder.set_pipeline(p),
                    Command::BindGroup(index, group, offsets) => {
                        encoder.set_bind_group(*index, group.as_ref(), offsets)
                    }
                    Command::Vertex(slot, None) => {
                        encoder.set_vertex_buffer(*slot, None::<wgpu::BufferSlice<'_>>)
                    }
                    Command::Vertex(slot, Some((b, range))) => {
                        encoder.set_vertex_buffer(*slot, b.raw.slice(range.clone()))
                    }
                    Command::Index(b, format, range) => {
                        encoder.set_index_buffer(b.raw.slice(range.clone()), *format)
                    }
                    Command::Draw(vertices, instances) => {
                        encoder.draw(vertices.clone(), instances.clone())
                    }
                    Command::DrawIndexed(indices, base, instances) => {
                        encoder.draw_indexed(indices.clone(), *base, instances.clone())
                    }
                    Command::DrawIndirect(b, offset) => encoder.draw_indirect(&b.raw, *offset),
                    Command::DrawIndexedIndirect(b, offset) => {
                        encoder.draw_indexed_indirect(&b.raw, *offset)
                    }
                    Command::Immediates(offset, bytes) => encoder.set_immediates(*offset, bytes),
                }
            }
            encoder.finish(&wgpu::RenderBundleDescriptor {
                label: Some(&label),
            })
        });
        let raw = match checked(g, &c, || std::panic::catch_unwind(replay)) {
            Ok(raw) => Some(raw),
            Err(payload) => {
                let message = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("Render bundle validation failed");
                validation(
                    g,
                    &c,
                    &message.split_whitespace().collect::<Vec<_>>().join(" "),
                );
                None
            }
        };
        let valid = raw.is_some() && balanced && !finished;
        Ok(wrap_labeled(
            g,
            Kind::Bundle,
            Some(c),
            Resource::Bundle(Arc::new(Bundle {
                raw,
                resources,
                valid,
            })),
            &label,
        ))
    })
}

#[redjs::host_fn]
unsafe fn execute_bundles(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, pass) = texture::pass_this(g, cf)?;
        let mut bundles = Vec::new();
        for v in seq(g, cf_arg(cf, 0))? {
            let h = owned(g, v.v, Kind::Bundle, &c)?;
            let Resource::Bundle(b) = h.resource else {
                unreachable!()
            };
            bundles.push(b);
        }
        if bundles.iter().any(|b| !b.valid) {
            validation(g, &c, "executeBundles was given an invalid GPURenderBundle");
            return Ok(UNDEF);
        }
        let retained: Vec<_> = bundles.iter().cloned().map(Resource::Bundle).collect();
        texture::retain_pass(g, &pass, &retained)?;
        texture::with_pass(g, &c, &pass, |raw| {
            raw.execute_bundles(bundles.iter().filter_map(|b| b.raw.as_ref()))
        })?;
        Ok(UNDEF)
    })
}

pub(super) unsafe fn install(g: *mut c_void) {
    class(
        g,
        Kind::BundleEncoder,
        "GPURenderBundleEncoder",
        &[
            ("setPipeline", 1, set_pipeline),
            ("setBindGroup", 2, set_bind_group),
            ("setVertexBuffer", 2, set_vertex_buffer),
            ("setIndexBuffer", 2, set_index_buffer),
            ("setImmediates", 2, set_immediates),
            ("draw", 1, draw),
            ("drawIndexed", 1, draw_indexed),
            ("drawIndirect", 2, draw_indirect),
            ("drawIndexedIndirect", 2, draw_indexed_indirect),
            ("pushDebugGroup", 1, push_debug_group),
            ("popDebugGroup", 0, pop_debug_group),
            ("insertDebugMarker", 1, insert_debug_marker),
            ("finish", 0, finish),
        ],
        &[],
    );
    class(g, Kind::Bundle, "GPURenderBundle", &[], &[]);
    method(g, Kind::Device, "createRenderBundleEncoder", 1, create);
    method(g, Kind::RenderPass, "executeBundles", 1, execute_bundles);
}
