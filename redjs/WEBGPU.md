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
- `copyExternalImageToTexture` for native/mirror images, ImageData, ImageBitmap,
  2D and WebGPU canvases, with crop/flipY, alpha conversion, mip/layer selection
  and renderable unorm/float destination formats.
- `GPUCanvasContext`: configure/unconfigure/getConfiguration/getCurrentTexture,
  SameObject drawing buffers, exclusive 2D/WebGPU context selection, resize and
  frame expiry. Native `OffscreenCanvas` requires the `canvas` feature; embedders
  can connect HTML canvas through the native element entry points.
- Canvas snapshots for 2D drawImage, ImageBitmap and PNG/JPEG/WebP export, with
  opaque/premultiplied alpha and RGBA/BGRA conversion.

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

## Canvas presentation

A presented canvas is registered by its host node key. The host consumes only
completed frames through `canvas::present`, `present_size` and
`take_presented_changed`. `register_present` / `unregister_present` resume or
pause a retained element without changing its selected context mode.

The current bridge performs a GPU copy into reusable staging buffers, then
asynchronous readback on one lazy worker per physical context. There are at
most three staging leases across queued/in-flight jobs. When these are busy,
each canvas retains only its latest deferred drawing buffer and wakes when a
lease becomes available. Resize/reconfigure/teardown fence late completions
by epoch. No JSC value or callback crosses onto the worker.

Call the normal VM pump through the end of a frame. Presented drawing buffers
expire after the complete rAF batch and its microtasks; submitting commands
that still reference an expired buffer produces a validation error. Headless
OffscreenCanvas retains its buffer until resize, reconfiguration or transfer.
An untouched opaque canvas publishes a constant-black marker, without a CPU
bitmap or GPU drawing-buffer allocation. Explicit pixel/image export can wait
for a GPU snapshot and is capped at 256 MiB; normal presentation never waits
for readback on the UI thread.

Canvas formats currently support `rgba8unorm` and `bgra8unorm`, sRGB, standard
tone mapping, and opaque/premultiplied alpha. Other presentation formats,
wide-gamut/HDR and extended tone mapping fail explicitly. This bridge returns
premultiplied RGBA8 CPU pixels; zero-copy native texture import is a later host
integration.

## External image uploads

`GPUQueue.copyExternalImageToTexture` snapshots native pixels after all JS
argument getters/conversions, retains temporary wrappers through GC, and
rechecks the destination after the embedder source hook. A later bitmap
close, source mutation or canvas resize cannot change an issued upload.
ImageBitmap/ImageData preserve straight RGBA; canvas sources preserve
premultiplied RGBA, including RGB channels above alpha. Matching RGBA8 alpha
modes copy rows directly; BGRA swizzles and alpha/float conversions run natively.

Destination textures require `COPY_DST | RENDER_ATTACHMENT`, one sample and
2D dimension. Supported targets are R/RG/RGBA/BGRA 8-bit unorm, RGBA/BGRA sRGB,
R/RG/RGBA float16/float32 and RGB10A2 unorm. Only sRGB color encoding is
implemented; display-p3 fails explicitly. Source crop and destination mip/layer
ranges are validated independently; flipY reverses the selected source region.
Converted upload buffers are capped at 256 MiB.

Origin-unclean host metadata produces `SecurityError`; it is preserved through
mirror-image hooks and 2D canvas snapshots. This consumes the embedding
runtime's existing origin policy, not a complete browser network/CORS boundary.
VideoFrame, HTML video frame import and GPU-to-GPU zero-copy imports remain
unimplemented. A WebGPU canvas source currently uses an explicit GPU readback.

The conversion contract follows [WebGPU external-image copies](https://gpuweb.github.io/gpuweb/#dom-gpuqueue-copyexternalimagetotexture).

## Not implemented yet

- Native swapchains and zero-copy shared-texture presentation.
- External video textures, VideoFrame/HTML video uploads and zero-copy video.
- SharedArrayBuffer uploads (rejected until an atomic native copy path exists).
- Async pipeline factories, query sets/timestamps, render bundles, optional
  compressed formats/features, WebGPU CTS qualification.
- Slang/DXIL artifact loading; standard `createShaderModule` accepts WGSL only.
- Full WebIDL/error semantics: e.g. some lifecycle/capability failures currently
  throw a TypeError or InvalidStateError rather than producing an invalid object.

In particular, **do not enable this feature in a shipped UI merely to expose
`navigator.gpu`**. Applications that detect only adapter presence can otherwise
open an editor that needs unsupported video uploads or other
missing APIs. Canvas presentation alone does not qualify a complete editor.

The standard contract is [WebGPU](https://gpuweb.github.io/gpuweb/); the Rust
backend is pinned to `gfx-rs/wgpu` revision
`3b3506ea31154edc04de64debda0e780cba2639c`, matching the GPUI fork used by the
embedder. GPU integration tests require a real adapter; they do not silently
pass against a noop backend.
