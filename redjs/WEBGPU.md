# Native WebGPU (experimental)

`redjs` feature `webgpu` installs native JavaScriptCore classes backed by
`wgpu`, without a browser or a TypeScript implementation. It is off by default.
Windows uses DirectX 12. Metal/Vulkan dependencies are configured for other
platforms; only Windows has been qualified in this change.

## Run

From the embedding workspace (to inherit its JSC prebuilt and CRT settings):

```powershell
cargo run --manifest-path vendor/redjs/redjs/Cargo.toml --no-default-features --features webgpu --example webgpu
cargo test --manifest-path vendor/redjs/redjs/Cargo.toml --no-default-features --features webgpu --lib webgpu -- --test-threads=1
```

Initialize the ordinary redjs scheduler and Web API layer and keep pumping
`redjs_pump` on the VM thread, as for fetch and timers. No adapter/device is
created during installation. The example requests a standalone native adapter,
executes WGSL, copies GPU output to a readback buffer, and validates the result.
It does not open a window or access an application profile.

## Implemented

- `navigator.gpu.requestAdapter()` and `getPreferredCanvasFormat()`;
  `GPUAdapter.requestDevice()`, adapter info, readonly supported features/limits.
- `GPUDevice`, `GPUQueue`, logical destruction, `lost`, validation/internal/OOM
  error scopes, asynchronous trusted `GPUUncapturedErrorEvent` delivery.
- Buffers: mapped-at-creation, asynchronous read/write maps, non-overlapping
  mapped ranges, detachment on unmap/destroy, buffer uploads and copies.
  `writeBuffer` offsets/sizes are elements for typed arrays, bytes for
  ArrayBuffer/DataView. Mappings use JSC-owned shadow memory; no GPU mapping
  pointer crosses a JS allocation or survives in a JS ArrayBuffer.
- Validated WGSL modules and compilation diagnostics with UTF-16 locations;
  explicit/automatic pipeline layouts, bind group layouts/groups, constants,
  compute pipelines and passes, dispatch and indirect dispatch.
- Textures/views/samplers, synchronous render pipelines, vertex/index buffers,
  blend/depth/stencil/MSAA state, draw/drawIndexed, viewport/scissor, render
  passes, texture uploads and buffer/texture copy commands.
- Single-use command buffers, queue submission and asynchronous work completion.

Bind groups retain their buffer/texture owners. Views retain their texture.
Recorded commands retain these owners through command buffer submission, after
which wgpu keeps native resources alive for in-flight GPU work. JS wrapper GC
therefore cannot remove an in-use resource from logical-device destruction.

Descriptor values are rooted while getters/coercions execute. Private mapped
range edges use direct own properties; application prototype setters cannot
swallow them. Promise workers carry Rust values, return through the VM pump,
and reject delivery to a recycled `ContextId`. Context teardown destroys its
logical clients; native finalizers do not invoke JavaScript.

## Embedder-owned device

`webapi::webgpu::backend::set_host(Some(Arc<dyn GpuHost>))` replaces standalone
adapter discovery for future requests. Return an `Adapter` whose `shared`
context was constructed with:

```rust,ignore
let context = Arc::new(Context::from_shared(adapter, device, queue));
// adapter: Arc<wgpu::Adapter>
// device:  Arc<wgpu::Device>
// queue:   Arc<wgpu::Queue>
```

The device and queue are the existing host handles. Import does not replace
host error/lost callbacks; the owner forwards loss via `context.notify_lost`.
`GPUDevice.destroy()` destroys that client's tracked buffers/textures and marks
its logical device lost. It never calls `wgpu::Device::destroy()`. Cross-client
resource use is rejected even when their physical device is identical.

**Current shared-device restriction:** requested features and limits must match
those of the physical device exactly. Capability-subset virtualization across
all shaders and pipelines is not implemented, so mismatched imports fail
explicitly. JS optional features are currently not advertised. A host requiring
additional physical features needs the next validation/negotiation stage.
This interface is a working shared-device foundation, not yet an untrusted
activity security boundary or a GPUI renderer integration.

## Not implemented yet

- `GPUCanvasContext`, HTML/OffscreenCanvas WebGPU presentation, swapchains.
- `copyExternalImageToTexture`, external video textures and zero-copy video.
- SharedArrayBuffer uploads (rejected until an atomic native copy path exists).
- Async pipeline factories, query sets/timestamps, render bundles, optional
  compressed formats/features, WebGPU CTS qualification.
- Slang/DXIL artifact loading; standard `createShaderModule` accepts WGSL only.
- Full WebIDL/error semantics: e.g. some lifecycle/capability failures currently
  throw a TypeError or InvalidStateError rather than producing an invalid object.

In particular, **do not enable this feature in a shipped UI merely to expose
`navigator.gpu`**. Applications that detect only adapter presence can otherwise
open a WebGPU editor whose canvas cannot be presented yet.

The standard contract is [WebGPU](https://gpuweb.github.io/gpuweb/); the Rust
backend is pinned to `gfx-rs/wgpu` revision
`3b3506ea31154edc04de64debda0e780cba2639c`, matching the GPUI fork used by the
embedder. GPU integration tests require a real adapter; they do not silently
pass against a noop backend.
