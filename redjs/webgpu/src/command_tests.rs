//! Render bundles, indirect draws, queries, debug groups, immediates, labels,
//! asynchronous pipelines and capability reporting on a real GPU.
use super::*;

const HELPERS: &str = r#"
  globalThis.scope = async (f, filter = 'validation') => { d.pushErrorScope(filter); await f(); return await d.popErrorScope(); };
  globalThis.readTexture = async (texture, width, height) => {
    const buffer = d.createBuffer({size: 256 * height, usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ});
    const encoder = d.createCommandEncoder();
    encoder.copyTextureToBuffer({texture}, {buffer, bytesPerRow: 256}, [width, height]);
    d.queue.submit([encoder.finish()]);
    await buffer.mapAsync(GPUMapMode.READ);
    const raw = new Uint8Array(buffer.getMappedRange()); const out = [];
    for (let y = 0; y < height; y++) out.push(...raw.slice(y * 256, y * 256 + width * 4));
    buffer.unmap(); buffer.destroy(); return out;
  };
  globalThis.readQueries = async (set, count, device = d) => {
    const resolved = device.createBuffer({size: 8 * count, usage: GPUBufferUsage.QUERY_RESOLVE | GPUBufferUsage.COPY_SRC});
    const read = device.createBuffer({size: 8 * count, usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ});
    const encoder = device.createCommandEncoder();
    encoder.resolveQuerySet(set, 0, count, resolved, 0);
    encoder.copyBufferToBuffer(resolved, 0, read, 0, 8 * count);
    device.queue.submit([encoder.finish()]);
    await read.mapAsync(GPUMapMode.READ);
    const values = Array.from(new BigUint64Array(read.getMappedRange().slice(0)));
    read.unmap(); read.destroy(); resolved.destroy(); return values;
  };
  globalThis.shader = d.createShaderModule({code: `
    struct U { color: vec4f }
    @group(0) @binding(0) var<uniform> u: U;
    @vertex fn vs(@location(0) p: vec2f) -> @builtin(position) vec4f { return vec4f(p, 0, 1); }
    @fragment fn fs() -> @location(0) vec4f { return u.color; }`});
  globalThis.pipeline = d.createRenderPipeline({layout: 'auto',
    vertex: {module: shader, entryPoint: 'vs', buffers: [{arrayStride: 8, attributes: [{shaderLocation: 0, offset: 0, format: 'float32x2'}]}]},
    fragment: {module: shader, entryPoint: 'fs', targets: [{format: 'rgba8unorm'}]}});
  const buffer = (usage, data) => { const b = d.createBuffer({size: data.byteLength, usage: usage | GPUBufferUsage.COPY_DST}); d.queue.writeBuffer(b, 0, data); return b; };
  globalThis.vertices = buffer(GPUBufferUsage.VERTEX, new Float32Array([-1,-1, 0,-1, -1,1, 0,-1, 0,1, -1,1, 0,-1, 1,-1, 0,1]));
  globalThis.indices = buffer(GPUBufferUsage.INDEX, new Uint16Array([0, 1, 2, 0]));
  const group = color => d.createBindGroup({layout: pipeline.getBindGroupLayout(0),
    entries: [{binding: 0, resource: {buffer: buffer(GPUBufferUsage.UNIFORM, new Float32Array(color))}}]});
  globalThis.red = group([1, 0, 0, 1]); globalThis.green = group([0, 1, 0, 1]);
  const args = new Uint32Array(10);
  args.set([6, 1, 0, 0], 0);
  args.set([3, 1, 0, 6, 0], 4);
  globalThis.indirect = buffer(GPUBufferUsage.INDIRECT, args);
  globalThis.renderWith = async (record, descriptor = {}) => {
    const texture = d.createTexture({size: [8, 8], format: 'rgba8unorm', usage: GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_SRC});
    const encoder = d.createCommandEncoder();
    const pass = encoder.beginRenderPass({colorAttachments: [{view: texture.createView(), loadOp: 'clear', storeOp: 'store', clearValue: [0, 0, 1, 1]}], ...descriptor});
    record(pass); pass.end(); d.queue.submit([encoder.finish()]);
    const pixels = await readTexture(texture, 8, 8); texture.destroy(); return pixels;
  };
  globalThis.direct = p => {
    p.setPipeline(pipeline); p.setVertexBuffer(0, vertices);
    p.setBindGroup(0, red); p.draw(6);
    p.setIndexBuffer(indices, 'uint16'); p.setBindGroup(0, green); p.drawIndexed(3, 1, 0, 6);
  };
  globalThis.viaIndirect = p => {
    p.setPipeline(pipeline); p.setVertexBuffer(0, vertices);
    p.setBindGroup(0, red); p.drawIndirect(indirect, 0);
    p.setIndexBuffer(indices, 'uint16'); p.setBindGroup(0, green); p.drawIndexedIndirect(indirect, 16);
  };
"#;

fn commands(c: &Context, body: &str) -> String {
    run_gpu(c, &format!("{HELPERS}\n{body}"))
}

#[test]
fn webgpu_render_bundles_and_indirect_draws_match_direct_draws() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        commands(
            &c,
            r#"
      const expected = await renderWith(direct);
      const pixel = (x, y) => expected.slice((y * 8 + x) * 4, (y * 8 + x) * 4 + 4).join();
      if (pixel(1, 3) !== '255,0,0,255' || pixel(4, 7) !== '0,255,0,255' || pixel(7, 0) !== '0,0,255,255')
        throw new Error('Direct draws did not produce the reference image: ' + [pixel(1, 3), pixel(4, 7), pixel(7, 0)]);
      const record = draw => { const b = d.createRenderBundleEncoder({colorFormats: ['rgba8unorm'], label: 'bundle'}); draw(b); return b.finish({label: 'done'}); };
      const bundle = record(direct), indirectBundle = record(viaIndirect);
      const bundled = await renderWith(p => p.executeBundles([bundle]));
      const replayed = await renderWith(p => p.executeBundles([bundle]));
      const indirectDraws = await renderWith(viaIndirect);
      const indirectBundled = await renderWith(p => p.executeBundles([indirectBundle]));
      const same = image => image.join() === expected.join();
      const reused = await scope(() => renderWith(p => p.executeBundles([bundle, indirectBundle])));
      return [bundle instanceof GPURenderBundle, bundle.label, same(bundled), same(replayed), same(indirectDraws), same(indirectBundled), reused].join();
    "#
        ),
        "true,done,true,true,true,true,"
    );
}

#[test]
fn webgpu_occlusion_queries_count_passing_samples() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        commands(
            &c,
            r#"
      const set = d.createQuerySet({type: 'occlusion', count: 2, label: 'occ'});
      await renderWith(p => {
        p.beginOcclusionQuery(0); direct(p); p.endOcclusionQuery();
        p.beginOcclusionQuery(1); p.endOcclusionQuery();
      }, {occlusionQuerySet: set});
      const [drawn, empty] = await readQueries(set, 2);
      const outside = await scope(() => renderWith(p => { p.beginOcclusionQuery(0); p.endOcclusionQuery(); }));
      set.destroy();
      const destroyed = await scope(() => renderWith(() => {}, {occlusionQuerySet: set}));
      return [set.type, set.count, drawn > 0n, empty === 0n, outside instanceof GPUValidationError, destroyed instanceof GPUValidationError].join();
    "#
        ),
        "occlusion,2,true,true,true,true"
    );
}

#[test]
fn webgpu_timestamp_queries_are_monotonic_when_supported() {
    let _host = HostGuard::new();
    let c = ctx();
    let result = commands(
        &c,
        r#"
      let refused = false;
      try { d.createQuerySet({type: 'timestamp', count: 2}); } catch (e) { refused = e instanceof TypeError; }
      if (!refused) throw new Error('A device without timestamp-query created a timestamp query set');
      if (!adapter.features.has('timestamp-query')) {
        let rejected = false;
        try { await adapter.requestDevice({requiredFeatures: ['timestamp-query']}); } catch (e) { rejected = true; }
        return 'unsupported:' + rejected;
      }
      const td = await adapter.requestDevice({requiredFeatures: ['timestamp-query']});
      const set = td.createQuerySet({type: 'timestamp', count: 4});
      const module = td.createShaderModule({code: `@group(0) @binding(0) var<storage, read_write> o: array<u32>;
        @compute @workgroup_size(64) fn main(@builtin(global_invocation_id) id: vec3u) { var x = id.x; for (var i = 0u; i < 256u; i++) { x = x * 1664525u + 1013904223u; } o[id.x] = x; }`});
      const busy = td.createComputePipeline({layout: 'auto', compute: {module, entryPoint: 'main'}});
      const out = td.createBuffer({size: 4 * 64 * 64, usage: GPUBufferUsage.STORAGE});
      const target = td.createTexture({size: [4, 4], format: 'rgba8unorm', usage: GPUTextureUsage.RENDER_ATTACHMENT});
      const encoder = td.createCommandEncoder();
      const compute = encoder.beginComputePass({timestampWrites: {querySet: set, beginningOfPassWriteIndex: 0, endOfPassWriteIndex: 1}});
      compute.setPipeline(busy);
      compute.setBindGroup(0, td.createBindGroup({layout: busy.getBindGroupLayout(0), entries: [{binding: 0, resource: {buffer: out}}]}));
      compute.dispatchWorkgroups(64); compute.end();
      const render = encoder.beginRenderPass({colorAttachments: [{view: target.createView(), loadOp: 'clear', storeOp: 'store'}],
        timestampWrites: {querySet: set, beginningOfPassWriteIndex: 2, endOfPassWriteIndex: 3}});
      render.end();
      td.queue.submit([encoder.finish()]);
      const t = await readQueries(set, 4, td);
      td.pushErrorScope('validation');
      const bad = td.createCommandEncoder();
      bad.beginComputePass({timestampWrites: {querySet: set}}).end(); bad.finish();
      const noIndex = await td.popErrorScope();
      td.destroy();
      if (!(noIndex instanceof GPUValidationError)) throw new Error('timestampWrites without an index was accepted');
      if (!(t[0] > 0n && t[1] >= t[0] && t[3] >= t[2])) throw new Error('Timestamps are not monotonic: ' + t);
      return 'monotonic';
    "#,
    );
    assert!(
        result == "monotonic" || result == "unsupported:true",
        "{result}"
    );
    eprintln!("timestamp queries: {result}");
}

#[test]
fn webgpu_debug_groups_must_balance() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        commands(
            &c,
            r#"
      const target = d.createTexture({size: [4, 4], format: 'rgba8unorm', usage: GPUTextureUsage.RENDER_ATTACHMENT});
      const renderPass = e => e.beginRenderPass({colorAttachments: [{view: target.createView(), loadOp: 'clear', storeOp: 'store'}]});
      const balanced = await scope(() => {
        const e = d.createCommandEncoder(); e.pushDebugGroup('frame'); e.insertDebugMarker('start');
        const c = e.beginComputePass(); c.pushDebugGroup('compute'); c.insertDebugMarker('m'); c.popDebugGroup(); c.end();
        const r = renderPass(e); r.pushDebugGroup('render'); r.popDebugGroup(); r.end();
        e.popDebugGroup(); d.queue.submit([e.finish()]);
      });
      const extraPop = await scope(() => { const e = d.createCommandEncoder(); e.popDebugGroup(); e.finish(); });
      const unclosed = await scope(() => { const e = d.createCommandEncoder(); e.pushDebugGroup('x'); e.finish(); });
      const computePop = await scope(() => { const e = d.createCommandEncoder(); const c = e.beginComputePass(); c.popDebugGroup(); c.end(); e.finish(); });
      const renderUnclosed = await scope(() => { const e = d.createCommandEncoder(); const r = renderPass(e); r.pushDebugGroup('x'); r.end(); e.finish(); });
      const bundle = f => scope(() => { const b = d.createRenderBundleEncoder({colorFormats: ['rgba8unorm']}); f(b); b.finish(); });
      const bundleBalanced = await bundle(b => { b.pushDebugGroup('a'); b.insertDebugMarker('m'); b.popDebugGroup(); });
      const bundlePop = await bundle(b => b.popDebugGroup());
      const bundleUnclosed = await bundle(b => b.pushDebugGroup('a'));
      target.destroy();
      const error = e => e instanceof GPUValidationError;
      return [balanced, error(extraPop), error(unclosed), error(computePop), error(renderUnclosed), bundleBalanced, error(bundlePop), error(bundleUnclosed)].join();
    "#
        ),
        ",true,true,true,true,,true,true"
    );
}

#[test]
fn webgpu_labels_round_trip() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        commands(
            &c,
            r#"
      const b = d.createBuffer({size: 4, usage: GPUBufferUsage.COPY_DST, label: 'buf'});
      const t = d.createTexture({size: [1, 1], format: 'rgba8unorm', usage: GPUTextureUsage.TEXTURE_BINDING, label: 'tex'});
      const checks = {
        buffer: b.label === 'buf', texture: t.label === 'tex', view: t.createView({label: 'view'}).label === 'view',
        sampler: d.createSampler().label === '', shader: shader.label === '',
        module: d.createShaderModule({code: '', label: 'mod'}).label === 'mod',
        pipeline: d.createRenderPipeline({layout: 'auto', label: 'pipe', vertex: {module: shader, buffers: [{arrayStride: 8, attributes: [{shaderLocation: 0, offset: 0, format: 'float32x2'}]}]}, fragment: {module: shader, targets: [{format: 'rgba8unorm'}]}}).label === 'pipe',
        encoder: d.createCommandEncoder({label: 'enc'}).label === 'enc',
        commandBuffer: d.createCommandEncoder().finish({label: 'cb'}).label === 'cb',
        querySet: d.createQuerySet({type: 'occlusion', count: 1, label: 'q'}).label === 'q',
        bundleEncoder: d.createRenderBundleEncoder({colorFormats: ['rgba8unorm'], label: 'rb'}).label === 'rb',
        device: d.label === '' && d.queue.label === '',
      };
      b.label = 'renamed'; checks.renamed = b.label === 'renamed';
      b.label = 42; checks.stringified = b.label === '42';
      const labelled = await adapter.requestDevice({label: 'dev', defaultQueue: {label: 'q0'}});
      checks.requested = labelled.label === 'dev' && labelled.queue.label === 'q0';
      labelled.destroy();
      let threw = false;
      try { Object.getOwnPropertyDescriptor(GPUBuffer.prototype, 'label').get.call({}); } catch (e) { threw = e instanceof TypeError; }
      checks.brand = threw && !('label' in adapter) && 'label' in GPUQuerySet.prototype;
      const failed = Object.entries(checks).filter(([, ok]) => !ok).map(([name]) => name);
      return failed.length ? 'failed: ' + failed : 'ok';
    "#
        ),
        "ok"
    );
}

#[test]
fn webgpu_async_pipelines_resolve_and_reject() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        commands(
            &c,
            r#"
      let uncaptured = 0; d.addEventListener('uncapturederror', () => uncaptured++);
      const good = d.createShaderModule({code: '@group(0) @binding(0) var<storage, read_write> o: array<u32>; @compute @workgroup_size(1) fn main() { o[0] = 7u; }'});
      const pending = d.createComputePipelineAsync({layout: 'auto', compute: {module: good, entryPoint: 'main'}, label: 'cp'});
      const compute = await pending;
      const out = d.createBuffer({size: 4, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_SRC});
      const e = d.createCommandEncoder(); const p = e.beginComputePass(); p.setPipeline(compute);
      p.setBindGroup(0, d.createBindGroup({layout: compute.getBindGroupLayout(0), entries: [{binding: 0, resource: {buffer: out}}]}));
      p.dispatchWorkgroups(1); p.end(); d.queue.submit([e.finish()]);
      const ran = new Uint32Array((await readBuffer(out, 4)).buffer)[0] === 7;
      const render = await d.createRenderPipelineAsync({layout: 'auto',
        vertex: {module: shader, buffers: [{arrayStride: 8, attributes: [{shaderLocation: 0, offset: 0, format: 'float32x2'}]}]},
        fragment: {module: shader, targets: [{format: 'rgba8unorm'}]}});
      d.pushErrorScope('validation'); const bad = d.createShaderModule({code: 'fn broken( {'}); await d.popErrorScope();
      let rejected = null;
      try { await d.createComputePipelineAsync({layout: 'auto', compute: {module: bad, entryPoint: 'main'}}); } catch (e) { rejected = e; }
      let renderRejected = null;
      try { await d.createRenderPipelineAsync({layout: 'auto', vertex: {module: shader, entryPoint: 'missing'}}); } catch (e) { renderRejected = e; }
      let conversion = null; const malformed = d.createComputePipelineAsync({layout: 'auto'});
      try { await malformed; } catch (e) { conversion = e; }
      await new Promise(resolve => setTimeout(resolve, 10));
      const constructed = new GPUPipelineError('m', {reason: 'internal'});
      return [compute instanceof GPUComputePipeline, compute.label, ran, render instanceof GPURenderPipeline,
        rejected instanceof GPUPipelineError, rejected instanceof DOMException, rejected.name, rejected.reason,
        renderRejected instanceof GPUPipelineError && renderRejected.reason, malformed instanceof Promise && conversion instanceof TypeError,
        uncaptured, constructed.reason, constructed.message].join();
    "#
        ),
        "true,cp,true,true,true,true,GPUPipelineError,validation,validation,true,0,internal,m"
    );
}

#[test]
fn webgpu_onuncapturederror_fires_for_uncaptured_validation_errors() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        run_gpu(
            &c,
            r#"
      const initial = d.onuncapturederror === null;
      const fired = new Promise(resolve => { d.onuncapturederror = event => resolve(event); });
      d.createBuffer({size: 16, usage: GPUBufferUsage.MAP_READ | GPUBufferUsage.STORAGE});
      const event = await fired;
      d.onuncapturederror = 'not a handler';
      const cleared = d.onuncapturederror === null;
      d.destroy();
      return [initial, event instanceof GPUUncapturedErrorEvent, event.error instanceof GPUValidationError, event.target === d, cleared].join();
    "#
        ),
        "true,true,true,true,true"
    );
}

#[test]
fn webgpu_reports_language_features_adapter_info_limits_and_transient_attachments() {
    let _host = HostGuard::new();
    let c = ctx();
    assert_eq!(
        commands(
            &c,
            r#"
      const language = navigator.gpu.wgslLanguageFeatures;
      const texture = d.createTexture({size: [4, 4], format: 'rgba8unorm', usage: GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.TRANSIENT_ATTACHMENT});
      const pass = (loadOp, storeOp) => scope(() => {
        const e = d.createCommandEncoder();
        e.beginRenderPass({colorAttachments: [{view: texture.createView(), loadOp, storeOp}]}).end(); d.queue.submit([e.finish()]);
      });
      const checks = {
        language: language instanceof WGSLLanguageFeatures && language === navigator.gpu.wgslLanguageFeatures && language.has('pointer_composite_access') && !language.has('not_a_feature'),
        info: d.adapterInfo instanceof GPUAdapterInfo && d.adapterInfo === d.adapterInfo && d.adapterInfo.description === adapter.info.description,
        subgroups: d.adapterInfo.subgroupMinSize === 4 && d.adapterInfo.subgroupMaxSize === 128,
        limits: d.limits.maxBindGroupsPlusVertexBuffers === 24 && d.limits.maxStorageBuffersInFragmentStage === d.limits.maxStorageBuffersPerShaderStage
          && d.limits.maxStorageTexturesInVertexStage === d.limits.maxStorageTexturesPerShaderStage,
        immediateLimit: d.limits.maxImmediateSize === Math.min(64, adapter.limits.maxImmediateSize),
        transientFlag: GPUTextureUsage.TRANSIENT_ATTACHMENT === 32 && texture.usage === 48,
        bindingDimension: texture.textureBindingViewDimension === undefined && 'textureBindingViewDimension' in GPUTexture.prototype,
        transientDiscard: (await pass('clear', 'discard')) === null,
        transientStore: (await pass('clear', 'store')) instanceof GPUValidationError,
        transientUsage: (await scope(() => d.createTexture({size: [4, 4], format: 'rgba8unorm', usage: GPUTextureUsage.TRANSIENT_ATTACHMENT | GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_SRC}))) instanceof GPUValidationError,
      };
      const failed = Object.entries(checks).filter(([, ok]) => !ok).map(([name]) => name);
      return failed.length ? 'failed: ' + failed : 'ok';
    "#
        ),
        "ok"
    );
}

#[test]
fn webgpu_immediates_reach_the_shader_within_the_limit() {
    let _host = HostGuard::new();
    let c = ctx();
    let result = commands(
        &c,
        r#"
      if (d.limits.maxImmediateSize < 16) return 'unsupported:' + d.limits.maxImmediateSize;
      // naga's HLSL backend only accepts struct-typed immediates.
      const s = d.createShaderModule({code: `struct C { color: vec4f } var<immediate> c: C;
        @vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4f {
          var p = array<vec2f, 3>(vec2f(-1, -1), vec2f(3, -1), vec2f(-1, 3)); return vec4f(p[i], 0, 1); }
        @fragment fn fs() -> @location(0) vec4f { return c.color; }`});
      const layout = d.createPipelineLayout({bindGroupLayouts: [], immediateSize: 16});
      let fill;
      const created = await scope(() => { fill = d.createRenderPipeline({layout, vertex: {module: s}, fragment: {module: s, targets: [{format: 'rgba8unorm'}]}}); }, 'internal');
      if (created) throw new Error(created.message);
      const pixels = await renderWith(p => { p.setPipeline(fill); p.setImmediates(0, new Float32Array([0, 0, 0, 1, 0, 1, 0, 1]), 4, 4); p.draw(3); });
      const bundle = d.createRenderBundleEncoder({colorFormats: ['rgba8unorm']});
      bundle.setPipeline(fill); bundle.setImmediates(0, new Float32Array([1, 0, 0, 1])); bundle.draw(3);
      const replayed = await renderWith(p => p.executeBundles([bundle.finish()]));
      const misaligned = await scope(() => renderWith(p => p.setImmediates(2, new Uint8Array(4))));
      const oversized = await scope(() => renderWith(p => p.setImmediates(0, new Uint8Array(d.limits.maxImmediateSize + 4))));
      const bigLayout = await scope(() => d.createPipelineLayout({bindGroupLayouts: [], immediateSize: d.limits.maxImmediateSize + 4}));
      return [pixels.slice(0, 4).join(), replayed.slice(0, 4).join(), misaligned instanceof GPUValidationError,
        oversized instanceof GPUValidationError, bigLayout instanceof GPUValidationError].join();
    "#,
    );
    assert!(
        result == "0,255,0,255,255,0,0,255,true,true,true" || result.starts_with("unsupported:"),
        "{result}"
    );
    eprintln!("immediates: {result}");
}
