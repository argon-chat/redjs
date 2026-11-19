use super::HostGuard;
use crate::backend;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn submit_work(gpu: &backend::Context, pipeline: &wgpu::ComputePipeline, group: &wgpu::BindGroup) {
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_compute_pass(&Default::default());
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, group, &[]);
        pass.dispatch_workgroups(64, 1, 1);
    }
    gpu.queue.submit([encoder.finish()]);
}

#[test]
fn waiters_on_many_threads_survive_submissions_retired_under_them() {
    let host = HostGuard::new();
    let gpu = host.gpu.clone();
    let module = gpu
        .device
        .create_shader_module(wgpu::ShaderModuleDescriptor {
            label: None,
            source: wgpu::ShaderSource::Wgsl(
                "@group(0) @binding(0) var<storage, read_write> out: array<u32>;
             @compute @workgroup_size(64) fn main(@builtin(global_invocation_id) id: vec3u) {
               var x = id.x;
               for (var i = 0u; i < 4000u; i++) { x = x * 1664525u + 1013904223u; }
               out[id.x] = x;
             }"
                .into(),
            ),
        });
    let pipeline = gpu
        .device
        .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: None,
            layout: None,
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
    let out = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: 4 * 64 * 64,
        usage: wgpu::BufferUsages::STORAGE,
        mapped_at_creation: false,
    });
    let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: out.as_entire_binding(),
        }],
    });
    let stop = Arc::new(AtomicBool::new(false));
    let waiters: Vec<_> = (0..16)
        .map(|_| {
            let gpu = gpu.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut pause = Duration::ZERO;
                while !stop.load(Ordering::Relaxed) {
                    let _ = backend::pump(&gpu.device, &mut pause);
                    if pause >= Duration::from_millis(1) {
                        pause = Duration::ZERO;
                    }
                }
            })
        })
        .collect();
    let until = Instant::now() + Duration::from_secs(2);
    let mut round = 0u32;
    while Instant::now() < until {
        if round % 4 == 0 {
            submit_work(&gpu, &pipeline, &group);
        } else {
            gpu.queue.submit([gpu
                .device
                .create_command_encoder(&Default::default())
                .finish()]);
        }
        round += 1;
    }
    stop.store(true, Ordering::Relaxed);
    let panicked = waiters.into_iter().filter_map(|w| w.join().err()).count();
    assert_eq!(panicked, 0);
}
