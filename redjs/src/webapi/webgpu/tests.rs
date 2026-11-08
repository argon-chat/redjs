//! Real JSC bindings against an actual native GPU. No noop adapter fallback.
//! Each test gets independent JS clients on one physical device, like an
//! embedded activity sharing the renderer's device would.

use super::backend;
use crate::api::Context;
use crate::webapi::testing::{ok, s, throws};
use std::ffi::c_void;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

fn ctx() -> Context {
    let c = Context::new();
    unsafe {
        assert_eq!(crate::ffi::redjs_web_install(c.raw()), 0);
        assert_eq!(
            crate::ffi::redjs_webshim_install(c.raw(), core::ptr::null(), 0, core::ptr::null(), 0),
            0
        );
    }
    c
}

struct SharedHost(Arc<backend::Context>);
impl backend::GpuHost for SharedHost {
    fn request_adapter(&self, _options: backend::AdapterOptions) -> backend::AdapterFuture {
        let shared = self.0.clone();
        Box::pin(async move {
            Ok(Some(backend::Adapter {
                raw: shared.adapter.clone(),
                shared: Some(shared),
            }))
        })
    }
}

fn physical() -> Arc<backend::Context> {
    static GPU: OnceLock<Arc<backend::Context>> = OnceLock::new();
    GPU.get_or_init(|| {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = if cfg!(target_os = "windows") {
            wgpu::Backends::DX12
        } else {
            wgpu::Backends::PRIMARY
        };
        let instance = wgpu::Instance::new(descriptor);
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .expect("WebGPU integration tests require a real native GPU adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("redjs WebGPU integration tests"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            ..Default::default()
        }))
        .expect("WebGPU integration tests could not create their native GPU device");
        Arc::new(backend::Context::from_shared(
            Arc::new(adapter),
            Arc::new(device),
            Arc::new(queue),
        ))
    })
    .clone()
}

struct HostGuard {
    _serial: MutexGuard<'static, ()>,
    gpu: Arc<backend::Context>,
}
impl HostGuard {
    fn new() -> Self {
        static SERIAL: Mutex<()> = Mutex::new(());
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let gpu = physical();
        backend::set_host(Some(Arc::new(SharedHost(gpu.clone()))));
        Self {
            _serial: serial,
            gpu,
        }
    }
}
impl Drop for HostGuard {
    fn drop(&mut self) {
        backend::set_host(None);
    }
}

unsafe fn pump(c: &Context) {
    crate::glue::rjs_lock(c.raw());
    crate::ffi::redjs_pump(
        c.raw(),
        crate::web::PUMP_TIMERS | crate::web::PUMP_MICROTASKS,
        core::ptr::null_mut(),
    );
    crate::glue::rjs_unlock(c.raw());
}

fn run(c: &Context, body: &str) -> String {
    s(
        c,
        &format!(
            r#"globalThis.__out = '<pending>';
        (async () => {{ {body} }})().then(
          v => {{ globalThis.__out = String(v); }},
          e => {{ globalThis.__out = 'ERROR: ' + (e && e.name) + ': ' + ((e && e.message) || e) + '\n' + ((e && e.stack) || ''); }}
        ); 'started'"#
        ),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        unsafe { pump(c) };
        let result = s(c, "globalThis.__out");
        if result != "<pending>" {
            return result;
        }
        assert!(
            Instant::now() < deadline,
            "WebGPU JS promise did not settle within 15 seconds: {body}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

const SETUP: &str = r#"
    globalThis.adapter = await navigator.gpu.requestAdapter();
    if (!adapter) throw new Error('Native adapter is unavailable');
    globalThis.d = await adapter.requestDevice();
    globalThis.readBuffer = async (source, size, device = d) => {
        const result = device.createBuffer({size, usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ});
        const encoder = device.createCommandEncoder();
        encoder.copyBufferToBuffer(source, 0, result, 0, size);
        device.queue.submit([encoder.finish()]);
        await result.mapAsync(GPUMapMode.READ);
        const bytes = new Uint8Array(result.getMappedRange()).slice();
        result.unmap(); result.destroy();
        return bytes;
    };
    globalThis.computeValues = async (device, values) => {
        const size = values.length * 4;
        const input = device.createBuffer({size, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST});
        const output = device.createBuffer({size, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_SRC});
        device.queue.writeBuffer(input, 0, new Uint32Array(values));
        const shader = device.createShaderModule({code: `
          @group(0) @binding(0) var<storage, read> input: array<u32>;
          @group(0) @binding(1) var<storage, read_write> output: array<u32>;
          @compute @workgroup_size(1) fn main(@builtin(global_invocation_id) id: vec3u) {
            output[id.x] = input[id.x] * 7u + 3u;
          }`});
        const pipeline = device.createComputePipeline({layout: 'auto', compute: {module: shader, entryPoint: 'main'}});
        const bindGroup = device.createBindGroup({layout: pipeline.getBindGroupLayout(0), entries: [
          {binding: 0, resource: {buffer: input}}, {binding: 1, resource: {buffer: output}}
        ]});
        const encoder = device.createCommandEncoder();
        const pass = encoder.beginComputePass();
        pass.setPipeline(pipeline); pass.setBindGroup(0, bindGroup);
        pass.dispatchWorkgroups(values.length); pass.end();
        device.queue.submit([encoder.finish()]);
        const bytes = await readBuffer(output, size, device);
        input.destroy(); output.destroy();
        return Array.from(new Uint32Array(bytes.buffer));
    };
"#;

fn run_gpu(c: &Context, body: &str) -> String {
    run(c, &format!("{SETUP}\n{body}"))
}

unsafe extern "sysv64" fn force_gc(g: *mut c_void, _cf: *mut c_void) -> u64 {
    crate::ffi::redjs_gc(g, 0);
    super::UNDEF
}

#[test]
fn webgpu_native_classes_brands_and_capabilities() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_gpu(
            &c,
            r#"
        for (const name of ['GPU','GPUAdapter','GPUDevice','GPUQueue','GPUBuffer','GPUShaderModule',
                            'GPUCommandEncoder','GPUComputePassEncoder','GPUCommandBuffer','GPUTexture']) {
          if (typeof globalThis[name] !== 'function') throw new Error(name + ' missing');
          let refused = false; try { new globalThis[name](); } catch (e) { refused = e instanceof TypeError; }
          if (!refused) throw new Error(name + ' constructor was callable');
        }
        const b = d.createBuffer({size:16, usage:GPUBufferUsage.COPY_DST});
        const checks = {
          'GPU brand': navigator.gpu instanceof GPU,
          'adapter brand': adapter instanceof GPUAdapter,
          'device brand': d instanceof GPUDevice,
          'queue brand': d.queue instanceof GPUQueue,
          'buffer brand': b instanceof GPUBuffer,
          'buffer tag': Object.prototype.toString.call(b) === '[object GPUBuffer]',
          'native method': /native code/.test(GPUDevice.prototype.createBuffer.toString()),
          'feature brand': d.features instanceof GPUSupportedFeatures && adapter.features instanceof GPUSupportedFeatures,
          'feature contents': d.features.size === 0 && d.features.has('shader-f16') === false,
          'readonly features': typeof d.features.add === 'undefined' && typeof d.features.delete === 'undefined' && typeof d.features.clear === 'undefined',
          'feature SameObject': d.features === d.features && adapter.features === adapter.features,
          'limit SameObject': d.limits === d.limits && adapter.limits === adapter.limits,
          'info SameObject': adapter.info === adapter.info,
          'limit brands': d.limits instanceof GPUSupportedLimits && adapter.limits instanceof GPUSupportedLimits,
          'info brand': adapter.info instanceof GPUAdapterInfo,
          'limit values': d.limits.maxBufferSize >= 16 && adapter.limits.maxBufferSize >= d.limits.maxBufferSize,
          'feature iteration': Array.from(d.features).length === 0 && d.features.values().next().done && d.features.keys().next().done && d.features.entries().next().done,
          'canvas format': ['rgba8unorm','bgra8unorm'].includes(navigator.gpu.getPreferredCanvasFormat())
        };
        for (const [name,valid] of Object.entries(checks)) if (!valid) throw new Error('WebGPU capability assertion: ' + name);
        let eachCount=0; d.features.forEach(() => {eachCount++;});
        if(eachCount!==0) throw new Error('Empty feature iteration invoked callback');
        const result=true;
        b.destroy(); d.destroy(); return result;
    "#
        ),
        "true"
    );
    assert!(throws(
        &c,
        "GPUDevice.prototype.createBuffer.call({}, {size:4, usage:8})"
    )
    .contains("WebGPU"));
}

#[test]
fn webgpu_compute_dispatches_real_wgsl_and_reads_gpu_result() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(run_gpu(&c, "const result = await computeValues(d, [0,1,2,31,65535]); d.destroy(); return result.join(',');"), "3,10,17,220,458748");
}

#[test]
fn webgpu_explicit_pipeline_and_binding_layouts() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_gpu(
            &c,
            r#"
        const b = d.createBuffer({size:16, usage:GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_SRC});
        const layout = d.createBindGroupLayout({entries:[{binding:0, visibility:GPUShaderStage.COMPUTE, buffer:{type:'storage'}}]});
        const pipelineLayout = d.createPipelineLayout({bindGroupLayouts:[layout]});
        const module = d.createShaderModule({code:'@group(0) @binding(0) var<storage,read_write> out:array<u32>; @compute @workgroup_size(1) fn main(@builtin(global_invocation_id) id:vec3u){out[id.x]=100u+id.x;}'});
        const pipeline = d.createComputePipeline({layout:pipelineLayout,compute:{module,entryPoint:'main'}});
        const group = d.createBindGroup({layout,entries:[{binding:0,resource:{buffer:b,offset:0,size:16}}]});
        const encoder = d.createCommandEncoder(); const pass = encoder.beginComputePass();
        pass.setPipeline(pipeline); pass.setBindGroup(0,group,[]); pass.dispatchWorkgroups(4); pass.end();
        d.queue.submit([encoder.finish()]);
        const bytes = await readBuffer(b,16); b.destroy(); d.destroy();
        return Array.from(new Uint32Array(bytes.buffer)).join(',');
    "#
        ),
        "100,101,102,103"
    );
}

#[test]
fn webgpu_mapped_at_creation_upload_and_writebuffer_element_offsets() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_gpu(
            &c,
            r#"
        const initial = d.createBuffer({size:16,usage:GPUBufferUsage.COPY_SRC,mappedAtCreation:true});
        const mapped = initial.getMappedRange(); new Uint32Array(mapped).set([5,6,7,8]);
        initial.unmap(); if (mapped.byteLength !== 0) throw new Error('unmap did not detach upload memory');
        const first = Array.from(new Uint32Array((await readBuffer(initial,16)).buffer));
        const source = new Uint32Array([11,22,33,44,55,66]);
        const output = d.createBuffer({size:16,usage:GPUBufferUsage.COPY_DST|GPUBufferUsage.COPY_SRC});
        d.queue.writeBuffer(output,0,source.subarray(1,5),1,2);
        d.queue.writeBuffer(output,8,source.buffer,16,4);
        d.queue.writeBuffer(output,12,new DataView(source.buffer,4,16),4,4);
        const second = Array.from(new Uint32Array((await readBuffer(output,16)).buffer));
        initial.destroy(); output.destroy(); d.destroy(); return first.join(',')+';'+second.join(',');
    "#
        ),
        "5,6,7,8;33,44,55,33"
    );
}

#[test]
fn webgpu_map_ranges_refuse_overlap_and_detach_on_unmap_and_destroy() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_gpu(
            &c,
            r#"
        const b = d.createBuffer({size:32,usage:GPUBufferUsage.MAP_WRITE|GPUBufferUsage.COPY_SRC});
        await b.mapAsync(GPUMapMode.WRITE,8,16);
        const a = b.getMappedRange(8,8); const z = b.getMappedRange(16,8);
        let refused = false; try { b.getMappedRange(8,4); } catch(e) { refused = e.name === 'OperationError'; }
        if (!refused || b.mapState !== 'mapped') throw new Error('mapping overlap was accepted');
        b.unmap(); if (a.byteLength !== 0 || z.byteLength !== 0 || b.mapState !== 'unmapped') throw new Error('unmap kept a range alive');
        await b.mapAsync(GPUMapMode.WRITE); const finalRange = b.getMappedRange();
        b.destroy(); const detached = finalRange.byteLength === 0 && b.mapState === 'unmapped';
        d.destroy(); return detached;
    "#
        ),
        "true"
    );
}

#[test]
fn webgpu_read_mapping_cannot_write_back_to_gpu_memory() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_gpu(
            &c,
            r#"
        const source = d.createBuffer({size:8,usage:GPUBufferUsage.COPY_SRC|GPUBufferUsage.COPY_DST});
        const result = d.createBuffer({size:8,usage:GPUBufferUsage.COPY_DST|GPUBufferUsage.MAP_READ});
        d.queue.writeBuffer(source,0,new Uint32Array([12345,67890]));
        const encoder=d.createCommandEncoder(); encoder.copyBufferToBuffer(source,0,result,0,8); d.queue.submit([encoder.finish()]);
        await result.mapAsync(GPUMapMode.READ); new Uint32Array(result.getMappedRange()).fill(0); result.unmap();
        await result.mapAsync(GPUMapMode.READ); const words=Array.from(new Uint32Array(result.getMappedRange())); result.unmap();
        source.destroy(); result.destroy(); d.destroy(); return words.join(',');
    "#
        ),
        "12345,67890"
    );
}

#[test]
fn webgpu_rejects_double_mapping_and_aborts_pending_mapping() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_gpu(
            &c,
            r#"
        const b=d.createBuffer({size:16,usage:GPUBufferUsage.MAP_READ|GPUBufferUsage.COPY_DST});
        const first=b.mapAsync(GPUMapMode.READ);
        let secondRefused=false; try { await b.mapAsync(GPUMapMode.READ); } catch(e) { secondRefused=true; }
        await first; b.unmap();
        const pending=b.mapAsync(GPUMapMode.READ); b.unmap();
        let aborted=false; try { await pending; } catch(e) { aborted=true; }
        const result=secondRefused&&aborted&&b.mapState==='unmapped'; b.destroy(); d.destroy(); return result;
    "#
        ),
        "true"
    );
}

#[test]
fn webgpu_encoder_pass_state_and_command_submission_are_single_use() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_gpu(
            &c,
            r#"
        const encoder=d.createCommandEncoder(); const pass=encoder.beginComputePass();
        let activeRefused=false; try { encoder.finish(); } catch(e) { activeRefused=true; }
        pass.end(); const commands=encoder.finish();
        let finishRefused=false; try { encoder.finish(); } catch(e) { finishRefused=true; }
        d.queue.submit([commands]);
        d.pushErrorScope("validation"); d.queue.submit([commands]);
        const submitRefused=(await d.popErrorScope()) instanceof GPUValidationError;
        await d.queue.onSubmittedWorkDone(); d.destroy(); return activeRefused&&finishRefused&&submitRefused;
    "#
        ),
        "true"
    );
}

#[test]
fn webgpu_cross_logical_device_resources_are_rejected_on_shared_physical_device() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_gpu(
            &c,
            r#"
        const second=await (await navigator.gpu.requestAdapter()).requestDevice();
        const foreign=d.createBuffer({size:16,usage:GPUBufferUsage.COPY_DST});
        let refused=false; try { second.queue.writeBuffer(foreign,0,new Uint32Array([1])); } catch(e) { refused=e instanceof TypeError; }
        const own=second.createBuffer({size:4,usage:GPUBufferUsage.COPY_SRC|GPUBufferUsage.COPY_DST});
        second.queue.writeBuffer(own,0,new Uint32Array([91]));
        const result=Array.from(new Uint32Array((await readBuffer(own,4,second)).buffer))[0];
        foreign.destroy(); own.destroy(); d.destroy(); second.destroy(); return refused&&result===91;
    "#
        ),
        "true"
    );
}

#[test]
fn webgpu_error_scopes_capture_real_shader_and_usage_validation() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_gpu(
            &c,
            r#"
        d.pushErrorScope('validation');
        d.createShaderModule({code:'this is deliberately invalid WGSL'});
        const shaderError=await d.popErrorScope();
        d.pushErrorScope('validation');
        d.createBuffer({size:16,usage:GPUBufferUsage.MAP_READ|GPUBufferUsage.STORAGE});
        const bufferError=await d.popErrorScope();
        d.pushErrorScope('validation'); d.createBuffer({size:4,usage:GPUBufferUsage.COPY_DST});
        const clean=await d.popErrorScope();
        const result=shaderError instanceof GPUValidationError && bufferError instanceof GPUValidationError &&
          typeof shaderError.message==='string' && shaderError.message.length>0 && clean===null;
        d.destroy(); return result;
    "#
        ),
        "true"
    );
}

#[test]
fn webgpu_requested_limits_and_unknown_features_reject_without_destroying_host() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run(
            &c,
            r#"
        const a=await navigator.gpu.requestAdapter();
        let badLimits=false; try { await a.requestDevice({requiredLimits:{maxBufferSize:a.limits.maxBufferSize+4}}); } catch(e) { badLimits=true; }
        const b=await navigator.gpu.requestAdapter();
        let badFeature=false; try { await b.requestDevice({requiredFeatures:['not-a-webgpu-feature']}); } catch(e) { badFeature=true; }
        const d=await (await navigator.gpu.requestAdapter()).requestDevice({requiredFeatures:[]});
        const alive=d.features.size===0; d.destroy(); return badLimits&&badFeature&&alive;
    "#
        ),
        "true"
    );
}

#[test]
fn webgpu_write_texture_rows_round_trip_through_native_gpu_copy() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_gpu(
            &c,
            r#"
        const texture=d.createTexture({size:[2,2,1],format:'rgba8unorm',usage:GPUTextureUsage.COPY_DST|GPUTextureUsage.COPY_SRC});
        const pixels=new Uint8Array([255,0,0,255, 0,255,0,255, 0,0,255,255, 255,255,0,255]);
        d.queue.writeTexture({texture},pixels,{bytesPerRow:8,rowsPerImage:2},[2,2,1]);
        const read=d.createBuffer({size:512,usage:GPUBufferUsage.COPY_DST|GPUBufferUsage.MAP_READ});
        const encoder=d.createCommandEncoder();
        encoder.copyTextureToBuffer({texture},{buffer:read,bytesPerRow:256,rowsPerImage:2},[2,2,1]);
        d.queue.submit([encoder.finish()]); await read.mapAsync(GPUMapMode.READ);
        const bytes=new Uint8Array(read.getMappedRange());
        const result=Array.from(bytes.slice(0,8)).concat(Array.from(bytes.slice(256,264))).join(',');
        read.unmap(); read.destroy(); texture.destroy(); d.destroy(); return result;
    "#
        ),
        "255,0,0,255,0,255,0,255,0,0,255,255,255,255,0,255"
    );
}

#[test]
fn webgpu_renders_triangle_into_texture_and_reads_expected_pixel() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_gpu(
            &c,
            r#"
        const shader=d.createShaderModule({code:`
          @vertex fn vs(@builtin(vertex_index) id:u32)->@builtin(position) vec4f {
            var points=array<vec2f,3>(vec2f(-1,-1),vec2f(3,-1),vec2f(-1,3));
            return vec4f(points[id],0,1);
          }
          @fragment fn fs()->@location(0) vec4f {return vec4f(1,0.25,0,1);}`});
        const pipeline=d.createRenderPipeline({layout:'auto',vertex:{module:shader,entryPoint:'vs'},
          fragment:{module:shader,entryPoint:'fs',targets:[{format:'rgba8unorm'}]},primitive:{topology:'triangle-list'}});
        const texture=d.createTexture({size:[4,4,1],format:'rgba8unorm',usage:GPUTextureUsage.RENDER_ATTACHMENT|GPUTextureUsage.COPY_SRC});
        const read=d.createBuffer({size:1024,usage:GPUBufferUsage.COPY_DST|GPUBufferUsage.MAP_READ});
        const encoder=d.createCommandEncoder();
        const pass=encoder.beginRenderPass({colorAttachments:[{view:texture.createView(),loadOp:'clear',storeOp:'store',clearValue:{r:0,g:0,b:1,a:1}}]});
        pass.setPipeline(pipeline); pass.draw(3); pass.end();
        encoder.copyTextureToBuffer({texture},{buffer:read,bytesPerRow:256,rowsPerImage:4},[4,4,1]);
        d.queue.submit([encoder.finish()]); await d.queue.onSubmittedWorkDone(); await read.mapAsync(GPUMapMode.READ);
        const bytes=new Uint8Array(read.getMappedRange()); const i=256*2+4*2;
        const result=bytes[i]===255 && Math.abs(bytes[i+1]-64)<=1 && bytes[i+2]===0 && bytes[i+3]===255;
        read.unmap(); read.destroy(); texture.destroy(); d.destroy(); return result;
    "#
        ),
        "true"
    );
}

#[test]
fn webgpu_destroy_is_logical_and_resolves_device_lost_without_breaking_other_clients() {
    let host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_gpu(
            &c,
            r#"
        const second=await (await navigator.gpu.requestAdapter()).requestDevice();
        const mapped=d.createBuffer({size:16,usage:GPUBufferUsage.COPY_SRC,mappedAtCreation:true});
        const range=mapped.getMappedRange();
        const lost=d.lost; d.destroy(); const info=await lost;
        if(range.byteLength!==0) throw new Error("device.destroy did not detach mapped range");
        const values=await computeValues(second,[9,10]);
        const result=info.reason==='destroyed'&&values.join(',')==='66,73'; second.destroy(); return result;
    "#
        ),
        "true"
    );
    assert!(
        host.gpu.loss().is_none(),
        "Logical JS destruction reported physical GPU loss"
    );
    // A direct native allocation after all JS clients were destroyed detects
    // accidentally calling wgpu::Device::destroy on the shared host device.
    let buffer = host.gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("host still alive"),
        size: 4,
        usage: wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    host.gpu.queue.write_buffer(&buffer, 0, &[1, 2, 3, 4]);
    buffer.destroy();
}

#[test]
fn webgpu_descriptor_getters_and_pending_operations_survive_gc() {
    let _host = HostGuard::new();
    let c = ctx();
    unsafe {
        let f = super::make_fn(c.raw(), "__forceGpuGc", 0, force_gc);
        super::define(
            c.raw(),
            crate::glue::rjs_global(c.raw()),
            "__forceGpuGc",
            f,
            2,
        );
    }
    assert_eq!(
        run_gpu(
            &c,
            r#"
        const b=d.createBuffer({get size(){__forceGpuGc(); return 16;},get usage(){__forceGpuGc(); return GPUBufferUsage.COPY_SRC|GPUBufferUsage.COPY_DST;}});
        d.queue.writeBuffer(b,0,new Uint32Array([41,42,43,44]),{valueOf(){__forceGpuGc();return 0;}},4);
        const pending=readBuffer(b,16); __forceGpuGc(); const bytes=await pending; __forceGpuGc();
        const result=Array.from(new Uint32Array(bytes.buffer)).join(','); b.destroy(); d.destroy(); return result;
    "#
        ),
        "41,42,43,44"
    );
}

#[test]
fn webgpu_context_teardown_discards_late_async_completions() {
    let host = HostGuard::new();
    struct DelayedHost(Arc<backend::Context>);
    impl backend::GpuHost for DelayedHost {
        fn request_adapter(&self, _options: backend::AdapterOptions) -> backend::AdapterFuture {
            let shared = self.0.clone();
            Box::pin(async move {
                std::thread::sleep(Duration::from_millis(80));
                Ok(Some(backend::Adapter {
                    raw: shared.adapter.clone(),
                    shared: Some(shared),
                }))
            })
        }
    }
    backend::set_host(Some(Arc::new(DelayedHost(host.gpu.clone()))));
    let old = ctx();
    s(&old,"navigator.gpu.requestAdapter().then(() => {globalThis.__staleGpuCompletion = true;}); 'started'");
    // Ensure the request captured the delayed host before releasing the VM.
    std::thread::sleep(Duration::from_millis(5));
    drop(old);
    let fresh = ctx();
    let deadline = Instant::now() + Duration::from_millis(160);
    while Instant::now() < deadline {
        unsafe { pump(&fresh) };
        std::thread::sleep(Duration::from_millis(2));
    }
    ok(
        &fresh,
        "typeof globalThis.__staleGpuCompletion === 'undefined'",
    );
    backend::set_host(Some(Arc::new(SharedHost(host.gpu.clone()))));
    assert_eq!(
        run_gpu(
            &fresh,
            "const result=await computeValues(d,[2]); d.destroy(); return result[0];"
        ),
        "17"
    );
}

fn install_gc(c: &Context) {
    unsafe {
        let f = super::make_fn(c.raw(), "__forceGpuGc", 0, force_gc);
        super::define(
            c.raw(),
            crate::glue::rjs_global(c.raw()),
            "__forceGpuGc",
            f,
            2,
        );
    }
}

#[test]
fn webgpu_mapped_ranges_keep_private_gc_edges_under_poisoned_prototypes() {
    let _host = HostGuard::new();
    let c = ctx();
    install_gc(&c);
    assert_eq!(
        run_gpu(
            &c,
            r#"
        const b=d.createBuffer({size:16,usage:GPUBufferUsage.COPY_SRC,mappedAtCreation:true});
        const oldBuffer=Object.getOwnPropertyDescriptor(Uint8Array.prototype,'buffer');
        const oldZero=Object.getOwnPropertyDescriptor(Array.prototype,'0');
        let getterHits=0, setterHits=0;
        Object.defineProperty(Uint8Array.prototype,'buffer',{configurable:true,get(){getterHits++; __forceGpuGc(); return new ArrayBuffer(1);}});
        Object.defineProperty(Array.prototype,'0',{configurable:true,set(value){setterHits++;}});
        try {
          let range=b.getMappedRange();
          if(!(range instanceof ArrayBuffer)||range.byteLength!==16) throw new Error('Mapped range used poisoned buffer getter');
          new Uint32Array(range).set([91,92,93,94]); range=null;
          // No application reference to the mapping survives this job. Its
          // private native edge must keep it alive for unmap's write-back.
          await new Promise(resolve=>setTimeout(resolve,0)); __forceGpuGc(); b.unmap();
        } finally {
          if(oldBuffer) Object.defineProperty(Uint8Array.prototype,'buffer',oldBuffer); else delete Uint8Array.prototype.buffer;
          if(oldZero) Object.defineProperty(Array.prototype,'0',oldZero); else delete Array.prototype[0];
        }
        if(getterHits!==0||setterHits!==0) throw new Error('Mapping touched application prototype accessors');
        const bytes=await readBuffer(b,16); const result=Array.from(new Uint32Array(bytes.buffer)).join(',');
        b.destroy(); d.destroy(); return result;
    "#
        ),
        "91,92,93,94"
    );
}

#[test]
fn webgpu_uncaptured_errors_are_trusted_async_events_and_scoped_errors_do_not_leak() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_gpu(
            &c,
            r#"
        let count=0;
        const eventPromise=new Promise(resolve=>d.addEventListener('uncapturederror',event=>{count++; resolve(event);}));
        d.createBuffer({size:16,usage:GPUBufferUsage.MAP_READ|GPUBufferUsage.STORAGE});
        if(count!==0) throw new Error('Uncaptured error was dispatched synchronously');
        const event=await eventPromise;
        if(!(event instanceof GPUUncapturedErrorEvent)||!(event instanceof Event)||!event.isTrusted||event.target!==d||
           !(event.error instanceof GPUError)||!(event.error instanceof GPUValidationError)||!event.error.message.length)
          throw new Error('Incorrect uncaptured error event or error interface');
        d.pushErrorScope('validation'); d.createShaderModule({code:'deliberately invalid WGSL'});
        let settled=false; const scopePromise=d.popErrorScope().then(error=>{settled=true;return error;});
        if(settled) throw new Error('Error scope promise settled synchronously');
        const scoped=await scopePromise;
        await new Promise(resolve=>setTimeout(resolve,0));
        if(!(scoped instanceof GPUValidationError)||count!==1) throw new Error('Scoped error escaped as an uncaptured event');
        const manual=new GPUValidationError('manual');
        const manualEvent=new GPUUncapturedErrorEvent('uncapturederror',{error:manual});
        const result=manual instanceof GPUError&&manualEvent.error===manual&&!manualEvent.isTrusted;
        d.destroy(); return result;
    "#
        ),
        "true"
    );
}

fn native_buffer_clone(c: &Context, name: &str) -> wgpu::Buffer {
    let value = c.eval(name).expect("GPU buffer expression must evaluate");
    let handle = unsafe { super::handle(c.raw(), value.0, super::Kind::Buffer) }
        .expect("Expected GPUBuffer");
    let super::Resource::Buffer(buffer) = handle.resource else {
        unreachable!()
    };
    // Deliberately retain only wgpu's native handle, not the outer Arc that
    // Client uses to track ownership. A missing owner edge remains observable.
    buffer.raw.as_ref().clone()
}
fn native_texture_clone(c: &Context, name: &str) -> wgpu::Texture {
    let value = c.eval(name).expect("GPU texture expression must evaluate");
    let handle = unsafe { super::handle(c.raw(), value.0, super::Kind::Texture) }
        .expect("Expected GPUTexture");
    let super::Resource::Texture(texture) = handle.resource else {
        unreachable!()
    };
    texture.raw.as_ref().clone()
}

/// Release the wrapper's native Box<Handle> exactly as finalize_box does.
/// Clearing its private pointer prevents a later real GC finalizer from
/// dropping the same box twice. These wrappers are never invoked again.
fn finalize_wrapper(c: &Context, name: &str) -> Arc<backend::Client> {
    let value = c.eval(name).expect("Resource wrapper must evaluate");
    unsafe {
        struct ApiLock(*mut c_void);
        impl Drop for ApiLock {
            fn drop(&mut self) {
                unsafe {
                    crate::glue::rjs_unlock(self.0);
                }
            }
        }
        crate::glue::rjs_lock(c.raw());
        let _lock = ApiLock(c.raw());
        let object = value.0 as usize as crate::capi::JSObjectRef;
        let pointer = crate::capi::JSObjectGetPrivate(object) as *mut super::Handle;
        assert!(
            !pointer.is_null(),
            "Expected an unfinalized native resource wrapper"
        );
        assert!(matches!(
            (*pointer).kind,
            super::Kind::Buffer | super::Kind::Texture
        ));
        let client = (*pointer).client.as_ref().unwrap().clone();
        assert!(crate::capi::JSObjectSetPrivate(
            object,
            core::ptr::null_mut()
        ));
        drop(Box::from_raw(pointer));
        client
    }
}

#[test]
fn webgpu_command_bind_group_and_texture_view_owners_survive_wrapper_finalization() {
    let host = HostGuard::new();
    let c = ctx();
    install_gc(&c);
    assert_eq!(
        run_gpu(
            &c,
            r#"
        globalThis.copyDestination=d.createBuffer({size:16,usage:GPUBufferUsage.MAP_READ|GPUBufferUsage.COPY_DST});
        const upload=d.createBuffer({size:16,usage:GPUBufferUsage.COPY_SRC,mappedAtCreation:true}); upload.unmap();
        const encoder=d.createCommandEncoder(); encoder.copyBufferToBuffer(upload,0,copyDestination,0,16);
        globalThis.keptCommands=encoder.finish();
        globalThis.boundBuffer=d.createBuffer({size:16,usage:GPUBufferUsage.STORAGE|GPUBufferUsage.COPY_DST});
        const layout=d.createBindGroupLayout({entries:[{binding:0,visibility:GPUShaderStage.COMPUTE,buffer:{type:'storage'}}]});
        globalThis.keptGroup=d.createBindGroup({layout,entries:[{binding:0,resource:{buffer:boundBuffer}}]});
        globalThis.viewTexture=d.createTexture({size:[2,2,1],format:'rgba8unorm',usage:GPUTextureUsage.TEXTURE_BINDING|GPUTextureUsage.COPY_DST});
        globalThis.keptView=viewTexture.createView();
        return true;
    "#
        ),
        "true"
    );
    let copy = native_buffer_clone(&c, "copyDestination");
    let bound = native_buffer_clone(&c, "boundBuffer");
    let texture = native_texture_clone(&c, "viewTexture");
    // Exercise exactly the native ownership release done by each JSC
    // finalizer, deterministically. Conservative stack scanning can keep a
    // wrapper alive after forced GC, which would make this test inconclusive.
    let client = finalize_wrapper(&c, "copyDestination");
    let bound_client = finalize_wrapper(&c, "boundBuffer");
    let texture_client = finalize_wrapper(&c, "viewTexture");
    assert_eq!(client.id, bound_client.id);
    assert_eq!(client.id, texture_client.id);
    client.destroy();

    let scope = host
        .gpu
        .device
        .push_error_scope(wgpu::ErrorFilter::Validation);
    let (tx, rx) = std::sync::mpsc::channel();
    copy.map_async(wgpu::MapMode::Read, 0..16, move |result| {
        let _ = tx.send(result);
    });
    let _ = host.gpu.device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: Some(Duration::from_secs(2)),
    });
    let mapping = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("Native map callback must complete");
    let _ = pollster::block_on(scope.pop());
    assert!(
        mapping.is_err(),
        "Command buffer lost the logical ownership edge; GPUBuffer survived device.destroy"
    );

    let scope = host
        .gpu
        .device
        .push_error_scope(wgpu::ErrorFilter::Validation);
    host.gpu.queue.write_buffer(&bound, 0, &[1, 2, 3, 4]);
    assert!(
        pollster::block_on(scope.pop()).is_some(),
        "Bind group lost its buffer ownership edge; buffer survived device.destroy"
    );

    let scope = host
        .gpu
        .device
        .push_error_scope(wgpu::ErrorFilter::Validation);
    host.gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &[255; 16],
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(8),
            rows_per_image: Some(2),
        },
        wgpu::Extent3d {
            width: 2,
            height: 2,
            depth_or_array_layers: 1,
        },
    );
    assert!(
        pollster::block_on(scope.pop()).is_some(),
        "Texture view lost its texture ownership edge; texture survived device.destroy"
    );
}

#[test]
fn webgpu_texture_upload_snapshots_after_descriptor_getters() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_gpu(
            &c,
            r#"
   const t=d.createTexture({size:[1,1],format:'rgba8unorm',usage:GPUTextureUsage.COPY_DST|GPUTextureUsage.COPY_SRC});
   const bytes=new Uint8Array([1,2,3,255]);
   d.queue.writeTexture({get texture(){bytes[0]=7;return t;}},bytes,{get bytesPerRow(){bytes[1]=8;return 4;}},{get width(){bytes[2]=9;return 1;},height:1});
   const b=d.createBuffer({size:256,usage:GPUBufferUsage.MAP_READ|GPUBufferUsage.COPY_DST});
   const e=d.createCommandEncoder();e.copyTextureToBuffer({texture:t},{buffer:b,bytesPerRow:256},[1,1]);d.queue.submit([e.finish()]);
   await b.mapAsync(GPUMapMode.READ);const result=Array.from(new Uint8Array(b.getMappedRange()).slice(0,4)).join(',');b.unmap();b.destroy();t.destroy();d.destroy();return result;
 "#
        ),
        "7,8,9,255"
    );
}
