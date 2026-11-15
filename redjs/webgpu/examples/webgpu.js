globalThis.__webgpuResult = 'pending';
(async () => {
  const adapter = await navigator.gpu.requestAdapter({powerPreference: 'high-performance'});
  if (!adapter) throw new Error('No native GPU adapter');
  const device = await adapter.requestDevice();
  try {
    const input = new Uint32Array([1, 2, 3, 4]);
    const storage = device.createBuffer({size: input.byteLength, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST | GPUBufferUsage.COPY_SRC});
    const readback = device.createBuffer({size: input.byteLength, usage: GPUBufferUsage.MAP_READ | GPUBufferUsage.COPY_DST});
    device.queue.writeBuffer(storage, 0, input);
    const shader = device.createShaderModule({code: `
      @group(0) @binding(0) var<storage, read_write> values: array<u32>;
      @compute @workgroup_size(1) fn main(@builtin(global_invocation_id) id: vec3u) {
        values[id.x] = values[id.x] * 7u + 3u;
      }`});
    const pipeline = device.createComputePipeline({layout: 'auto', compute: {module: shader, entryPoint: 'main'}});
    const group = device.createBindGroup({layout: pipeline.getBindGroupLayout(0), entries: [{binding: 0, resource: {buffer: storage}}]});
    const encoder = device.createCommandEncoder();
    const pass = encoder.beginComputePass();
    pass.setPipeline(pipeline); pass.setBindGroup(0, group); pass.dispatchWorkgroups(input.length); pass.end();
    encoder.copyBufferToBuffer(storage, 0, readback, 0, input.byteLength);
    device.queue.submit([encoder.finish()]);
    await readback.mapAsync(GPUMapMode.READ);
    const actual = Array.from(new Uint32Array(readback.getMappedRange()));
    readback.unmap(); readback.destroy(); storage.destroy();
    if (actual.join(',') !== '10,17,24,31') throw new Error('Unexpected GPU result: ' + actual);
    return 'PASS ' + JSON.stringify({adapter: adapter.info.description, result: actual});
  } finally { device.destroy(); }
})().then(value => { globalThis.__webgpuResult = value; }, error => { globalThis.__webgpuResult = 'FAIL ' + error.name + ': ' + error.message; });
