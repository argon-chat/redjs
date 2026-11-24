//! Convert a decoded video plane into the destination mip/layer on the GPU.
use super::*;
use wgpu::util::DeviceExt;

const BLIT: &str = r#"
struct Params { src: vec2u, dst: vec2u, height: u32, flip: u32, premultiply: u32, srgb: u32 };
@group(0) @binding(0) var plane: texture_2d<f32>;
@group(0) @binding(1) var<uniform> p: Params;
@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4f {
    let xy = array(vec2f(-1,-1), vec2f(3,-1), vec2f(-1,3));
    return vec4f(xy[i], 0, 1);
}
@fragment fn fs(@builtin(position) pos: vec4f) -> @location(0) vec4f {
    var xy = vec2u(pos.xy) - p.dst;
    if p.flip != 0 { xy.y = p.height - 1u - xy.y; }
    let color = textureLoad(plane, p.src + xy, 0);
    var rgb = color.rgb;
    if p.premultiply != 0 { rgb *= color.a; }
    if p.srgb != 0 {
        rgb = select(pow((rgb + 0.055) / 1.055, vec3f(2.4)), rgb / 12.92, rgb <= vec3f(0.04045));
    }
    return vec4f(rgb, color.a);
}
"#;

pub(super) fn copy(
    client: &Client,
    source: &wgpu::Texture,
    dest: &CopyTexture,
    origin: (u32, u32),
    size: wgpu::Extent3d,
    flip: bool,
    premultiply: bool,
) {
    let device = &client.context.device;
    let format = dest.texture.raw.format();
    let pipeline = client
        .video_blits
        .lock()
        .unwrap()
        .entry(format)
        .or_insert_with(|| {
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("Video copy"),
                source: wgpu::ShaderSource::Wgsl(BLIT.into()),
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("Video copy"),
                layout: None,
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        })
        .clone();
    let params: Vec<_> = [
        origin.0,
        origin.1,
        dest.origin.x,
        dest.origin.y,
        size.height,
        u32::from(flip),
        u32::from(premultiply),
        u32::from(format.is_srgb()),
    ]
    .into_iter()
    .flat_map(u32::to_ne_bytes)
    .collect();
    let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("Video copy rectangle"),
        contents: &params,
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let source_view = source.create_view(&Default::default());
    let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&source_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: uniform.as_entire_binding(),
            },
        ],
    });
    let view = dest.texture.raw.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2),
        base_mip_level: dest.mip,
        mip_level_count: Some(1),
        base_array_layer: dest.origin.z,
        array_layer_count: Some(1),
        ..Default::default()
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Video copy"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.set_scissor_rect(dest.origin.x, dest.origin.y, size.width, size.height);
        pass.draw(0..3, 0..1);
    }
    client.context.queue.submit([encoder.finish()]);
}
