# jscore — JavaScriptCore embedding layer (ClearScript replacement)

Bun's JavaScriptCore (Windows prebuilt) + a Rust full-control embedding crate
(`redjs`) with a flat C ABI designed for **zero-alloc C# NativeAOT bindings**.
This is the engine seam that replaces ClearScript/V8 in
`packages/player_backend/RedPew.Player`.

Why (measured in `battle_test/`, see its RESULTS.md): ClearScript's managed
marshaling costs ~100× on host calls. JSC via its *internal* binding path does
a js→host call in **2.2 ns** (ties SpiderMonkey, 4× faster than V8) on top of
the best-in-class JIT (fastest compute/fib of every engine tested).

(This line used to add that Bun was "about to land shared-memory
multithreading" in JSC. It is not: oven-sh/WebKit#249 has had no commit since
2026-06-20, says it "may never merge", lists Windows as "Not started", and no
`useJSThreads` option exists in any prebuilt. One VM per thread, under its lock,
is the model — see *Threading & locks*.)

```
packages/jscore/
├─ pins.txt                   # ★ the prebuilt + mimalloc versions (and hashes) — the ONE place they live
├─ setup.ps1                  # fetch/verify/unpack what pins.txt names (gitignored; replaces a stale copy)
├─ build.ps1 [-Test]          # cargo build --release + artifact layout (+ conformance suite)
├─ prebuilt/bun-webkit/       # include/ (internal headers!) + lib/ (static libs), ~0.85 GB
├─ redjs/                     # the Rust crate
│  ├─ build.rs                #   pin check + trampoline codegen + clang-cl glue/mimalloc build + link
│  ├─ cpp/glue.cpp            #   JSC internal-API layer (extern "C" rjs_*)
│  ├─ cpp/mimalloc/           #   oven-sh/mimalloc sources (fetched; JSC allocates through it)
│  ├─ cpp/compat/wtf/…        #   header missing from the prebuilt archive
│  ├─ src/ffi.rs              #   ★ the exported redjs_* C ABI (csbindgen input)
│  ├─ src/api.rs              #   safe Rust API (Context/Value/HostArgs)
│  ├─ src/trampoline.rs       #   SysV→MS host-fn trampoline pool (the C# entry)
│  ├─ src/capi.rs             #   JSC C-API subset used internally
│  └─ tests/smoke.rs          #   ★ conformance suite = the ABI contract, 14 sections
├─ csharp/
│  ├─ codegen/                # cargo build → regenerates NativeMethods.g.cs (csbindgen)
│  ├─ RedJs/                  # ★ the C# binding library (net10.0)
│  │  ├─ NativeMethods.g.cs   #   generated DllImports (61) + structs
│  │  ├─ JsAbi.cs             #   NaN-box + CallFrame consts (verified vs native at init)
│  │  ├─ JsValue.cs           #   zero-FFI value encode/decode (readonly struct ulong)
│  │  ├─ JsCallFrame.cs       #   zero-FFI host-call args (ref struct over the frame)
│  │  ├─ JsContext.cs         #   the ClearScript-replacement surface
│  │  └─ NativeLibraryResolver.cs
│  └─ RedJs.Smoke/            # ★ C# conformance smoke (63 checks) — JIT and AOT
├─ runtimes/win-x64/native/redjs.dll   # built: self-contained, for JIT/debug DllImport
└─ lib/win-x64/redjs.lib               # built: THIN static lib for NativeAOT
```

## Quick start

```powershell
./setup.ps1          # fetch prebuilt + mimalloc at the pins (or -Tarball path\to\bun-webkit-windows-amd64.tar.gz)
./build.ps1 -Test    # build + run the conformance suite

cd csharp/RedJs.Smoke
dotnet run -c Release                                          # C# smoke, JIT via redjs.dll
dotnet publish -r win-x64 -c Release -p:PublishAot=true -p:RedJsRid=win-x64
./bin/Release/net10.0/win-x64/native/RedJs.Smoke.exe           # C# smoke, static-linked AOT
```

Requires stable Rust, `clang-cl` (LLVM) on PATH and an MSVC toolset ≥ 14.44
(the prebuilt's objects need `__std_fs_equivalent`). Everything is verified on
the prebuilt `pins.txt` names — **`autobuild-4d45755`**, the AOT-enabled
Windows archive used by Bun main/canary `7ca0042d3` (2026-11-07). This variant
includes the native AOT compiler/runtime alongside normal JIT; applications
continue using JIT until a typed image frontend/loader is integrated. Stable
Bun 1.4.4 predates AOT. The previous `d6375c72` pin remains the version of the
historical benchmarks below (nehalem CPU floor, no AVX2 required).

**Re-run `./setup.ps1` after pulling a pin change.** It compares what is on
disk with `pins.txt` (the prebuilt names itself: `BUN_WEBKIT_VERSION` in
`include/cmakeconfig.h`, including its AOT variant) and replaces a stale copy; `redjs/build.rs` refuses to
build against anything else, so a checkout that skipped it gets a build error
naming the fix rather than a link of the old engine. `BUN_WEBKIT_DIR` points
the build at a prebuilt elsewhere; it is held to the same pin.

**Measured js→C# host-call boundary** (2M-call hot loop, arg decode + result
encode included): **8.9 ns/call NativeAOT**, 11.2 ns/call Release JIT
(re-run on d6375c72, m24: 7.0 ns/call NativeAOT, 7.2 and 11.4 ns/call JIT in
two runs — read it as "no regression", not as a speed-up). The
~7 ns over the raw Rust trampoline (2.2 ns) is .NET's reverse-P/Invoke
transition — already at parity with V8's raw C++ boundary, versus the ~µs-class
ClearScript marshaling this replaces.

## Design

### Values are raw `EncodedJSValue` bits (u64)

Every JS value crosses the ABI as JSC's 64-bit NaN-boxed value. The constants
(`redjs_layout`) let C# encode/decode **numbers, bools, int32, undefined,
null without any FFI call**:

- int32: `(v & NumberTag) == NumberTag`, payload = low 32 bits
- double: `(v & NumberTag) != 0`, `BitConverter.Int64BitsToDouble(v - DoubleEncodeOffset)`
- undefined/null/true/false: fixed constants (0xa / 0x2 / 0x7 / 0x6)
- cells (strings/objects): `(v & NotCellMask) == 0` → pointer, use FFI

### Host functions: C# code IS the JSC host function (via one trampoline)

This Bun WebKit build compiles host calls with the **System V** convention on
Windows (`JSC_HOST_CALL_ATTRIBUTES = sysv_abi` — that's how they kept the JIT
calling convention portable). .NET can't emit SysV on Windows, so `redjs`
ships a pool of 2048 pre-compiled `extern "sysv64"` Rust trampolines; each
registered C# function takes one slot: `JSC → trampoline_i (SysV) → C# fn
pointer (MS)`. rustc emits the ABI transition; measured cost is a direct call
+ one table load. Rust/Zig host functions can skip the pool entirely
(`redjs_function_create_sysv`).

The C# callback signature is the *native JSC host signature*:

```csharp
[UnmanagedCallersOnly]
static ulong HostFn(nint globalObject, nint callFrame)
{
    // zero FFI: read args straight out of the CallFrame
    uint argcIncThis = *(uint*)(callFrame + Layout.cfArgcOffset);
    ulong arg0 = *(ulong*)(callFrame + Layout.cfArgsOffset);      // + 8*i
    ulong self = *(ulong*)(callFrame + Layout.cfThisOffset);      // `this`
    return EncodeDouble(42.0);                                     // or any JSValue
}
```

`redjs_layout` exports the CallFrame offsets **computed from the actual
headers at build time** (argc@32, this@40, args@48 today) plus `abiVersion` —
a prebuilt bump that moves anything fails loudly.

Arguments are rooted by the CallFrame for the duration of the call. JS may
pass fewer args than arity — bounds-check against argc.

### GC contract

- Values held only on the **native stack** of a thread that's inside the VM
  are safe (conservative stack scan).
- Values stored to **heap** (C# fields, collections) must be pinned:
  `redjs_protect` / `redjs_unprotect`.
- Host-object finalizers (`redjs_set_host_finalizer`) run on GC threads:
  only enqueue handles for later release, never call back into JS.

### Threading & locks

One context = one VM; use it from one thread at a time. Every ABI call takes
the VM's recursive API lock; batch a frame's calls under `redjs_lock` /
`redjs_unlock` to make the inner locks free. Host callbacks already run
locked. There is no shared-memory JS threading to wait for (see the note at
the top); parallel JS means more contexts, each on its own thread.

### Microtasks

Promises/await continuations run when `redjs_drain_microtasks` is called —
the player should drain once per frame after the rAF callback, mirroring the
browser's task model.

## ABI surface (61 exports, prefix `redjs_`)

| group | functions |
|---|---|
| init/vm | `init(options)`, `set_options`, `context_create/release`, `global`, `layout`, `lock/unlock`, `drain_microtasks`, `gc`, `heap_stats`, `report_extra_memory` |
| eval | `eval16`, `eval8` (source URL → stack traces) |
| values | `kind`, `to_number`, `to_string`, `to_boolean`, `strict_equals`, `protect/unprotect` |
| strings | `string8/16` (in), `string_info` (out: engine bytes, Latin-1 or UTF-16, single copy) |
| props | `atom8/16` + `atom_free` (interned names), `get/set/define`, `get_index/set_index`, `has/delete` |
| objects | `object_create(proto)`, `array_create/length`, `construct` |
| functions | `function_create` (C# MS-ABI), `function_create_with_data`, `function_create_sysv`, `call`, `host_slots` |
| errors | `throw`, `error_create`, `error_info` (message+stack), `exception_pending` |
| buffers | `arraybuffer_external` (zero-copy over host memory), `arraybuffer_info/detach`, `typedarray_create/external/wrap/info` |
| promises | `promise_create` (deferred + resolve/reject fns), `promise_info` (status poll) |
| host objects | `hostclass_create`, `hostobject_create` (opaque data ptr + proto), `hostobject_data/set_data`, `set_host_finalizer` |

Status codes: `0` ok · `1` JS exception (out param = exception value) ·
`2` bad arg · `3` trampoline pool exhausted.

The authoritative reference is `redjs/src/ffi.rs` (doc comments per function)
and the conformance suite `redjs/tests/smoke.rs`, which exercises every group
end-to-end **exactly the way the C# bindings will** (including raw CallFrame
reads against `redjs_layout`).

### Web scheduling core — one clock, one pump

The player's frame plumbing (rAF, setTimeout/setInterval, `performance`)
lives NATIVELY inside redjs (`src/web.rs`), not in C#:

- `redjs_web_install(ctx)` registers `performance.now()/timeOrigin`,
  `requestAnimationFrame/cancelAnimationFrame`,
  `setTimeout/setInterval/clearTimeout/clearInterval` as **SysV host
  functions** (~2 ns from JS, no trampoline slots, no reverse-P/Invoke).
- **One monotonic clock** for everything: rAF timestamps, timer deadlines,
  `performance.now()` and `redjs_now()` (host-side reads for the profiler)
  are the same time base. (The ClearScript player has three different clocks
  for these.)
- The host drives a frame with **one FFI call**:
  `redjs_pump(ctx, TIMERS|RAF|MICROTASKS, &stats)` → due timers (deadline
  order; callbacks scheduled during the drain fire next pump; intervals
  reschedule from `now + interval`, min 1 ms) → the rAF batch (snapshot `now`
  as the DOMHighResTimeStamp; re-registrations land in the next frame) → the
  microtask queue. Stats report what fired and the next timer deadline.
- Callback exceptions go to `redjs_set_uncaught_handler` (C# formats them via
  `redjs_error_info`); a throw never aborts the pump.

The player's `ProcessFrame` becomes:

```csharp
ctx.InstallWebGlobals();                     // once at startup
JsContext.SetUncaughtHandler(&OnJsError);    // once

void OnRender(double _) {                    // per frame
    using var _lock = ctx.Lock();
    var stats = ctx.Pump();                  // timers + rAF + microtasks
    navigator.Update(); audio.Update(); canvasContext.Present();
}
```

### Native Web-API layer (the browser-runtime shims)

The runtime shims the player kept in C# (`Scripting/WebApi/*`, `Browser*`)
live INSIDE redjs as **native, standards-shaped classes** (`src/webapi/`),
defined through JSC's own object model: native constructors (with `new.target`
subclassing), prototype methods/getters, private-name internal slots
(invisible even to `getOwnPropertySymbols`), real `instanceof` — no `__brand`
hacks, no JS megashim:

| class | notes |
|---|---|
| `Event` / `CustomEvent` / `EventTarget` | WHATWG dispatch: once, duplicate suppression, `handleEvent` objects, `stopImmediatePropagation`, cancelable/`preventDefault`, on-`type` handler props, listener snapshotting |
| `TextEncoder` / `TextDecoder` | encoder: utf-8 + real `encodeInto`; decoder: EVERY WHATWG label via encoding_rs (windows-125x, utf-16le/be, koi8, shift_jis, …), `fatal`/`ignoreBOM` honored; unknown labels → RangeError; no `{stream:true}` |
| `Blob` | Arc-shared bytes — spec `slice()` is zero-copy; promise `arrayBuffer/bytes/text` |
| `URL` / `URLSearchParams` | backed by the `url` crate (rust-url = the reference WHATWG implementation); searchParams↔href live sync; full iteration protocol (`entries/keys/values/forEach/[Symbol.iterator]`, live-list semantics) |
| `fetch` / `Response` / `Headers` / `WebSocket` | local files (scripts→assets root resolution), `data:` URIs, and http(s)/ws(s) over the native client (`webapi/http.rs`, `webapi/websocket.rs`). **Cookies** (`webapi/cookies.rs`): one RFC 6265bis jar for the process — `Set-Cookie` stored from every hop, `Cookie` sent per hop and on the WebSocket handshake, `credentials` omit/same-origin/include against the page origin `app://redpew` (so `same-origin` never applies to a network URL), `Set-Cookie` never visible to script (`getSetCookie()` is `[]`), `document.cookie` reads `""` and drops writes. The embedder provisions cookies with `cookies::set(HostCookie)` / `remove` (Electron's `session.cookies` shape) and persists them through the `CookieHost` seam. Deviations: SameSite never restricts (the app is first party, one advisory), no third-party blocking or partitioning, a partial public-suffix list |
| `ImageBitmap` / `HTMLImageElement` / `Image` / `createImageBitmap` | decode via the `image` crate (png/jpeg/webp/bmp/gif); `img.src` loads async on the pump tick and fires `load`/`error`; region-crop overload for sprite atlases |
| `document` / `window` | EventTarget-backed; `window === globalThis` (proxy-normalized) |
| `navigator` | a real `Navigator`: NavigatorID (`userAgent` set by the embedder), `hardwareConcurrency`, and `onLine` + trusted `online`/`offline` events on the global in every context. `onLine` is what the embedder's OS notifier says (`navigator::set_on_line` / `redjs_navigator_set_on_line`, any thread; Chromium's meaning: "the OS believes there is a network", not reachability); with no provider it is `true` and says so once on stderr |
| `crypto` | **OS CSPRNG** (`getrandom` → BCryptGenRandom): getRandomValues (spec type/quota checks) + randomUUID v4; `subtle.digest` SHA-1/256/384/512 (RustCrypto) |
| `structuredClone` | HTML StructuredSerialize/Deserialize over a **byte-level serializer** (`webapi/serialize.rs`: frozen wire format, attachment table, no JSC value in the output, so bytes decode in another context on another thread): primitives incl. BigInt and lone surrogates, objects/arrays (holes, key order, getters run once), Date/RegExp/Map/Set/wrappers, the 7 Error types (+ `stack`, `cause`), ArrayBuffer (resizable too) and every view, Blob (by reference), ImageBitmap, ImageData, DOMPoint/DOMMatrix (+ ReadOnly), DOMException/QuotaExceededError, identity and cycles; `{ transfer }` with HTML's validate → serialize → detach order. Everything else → a real `DataCloneError` DOMException; too-deep graphs → RangeError |
| `AbortController` / `AbortSignal` | real EventTarget signals: `abort` event + `onabort`, `reason`, `throwIfAborted()`, statics `AbortSignal.abort()` / `AbortSignal.timeout(ms)` (scheduler-driven TimeoutError) |
| `ImageData` | both constructor forms with spec validation; consumed by the native image pipeline and the 2D canvas stub |
| `BroadcastChannel` | same-context channels: `postMessage` serializes once, then one queued task per other open same-name channel (creation order) deserializes its own copy and fires a trusted `MessageEvent` (`messageerror` if that fails) |
| `atob` / `btoa` / `queueMicrotask` / `setImmediate` | native: base64 in Rust, queueMicrotask enqueues on the VM's REAL microtask queue (QueuedTask/InvokeFunctionJob), setImmediate = 0-delay macrotask sharing the timer registry (`clearImmediate` = `clearTimeout`) |
| **Web Audio** (`AudioContext`, `GainNode`, `StereoPannerNode`, `PannerNode`+listener, `BiquadFilterNode`, `AnalyserNode`, `AudioBufferSourceNode`, `AudioBuffer`, `OscillatorNode`, `ChannelSplitterNode`, `ChannelMergerNode`, `AudioParam` automation, `decodeAudioData`) | REAL native implementation over the **Firewheel** audio-graph engine (cpal output stream, sample-clock `currentTime`); decode via symphonium (wav/flac/mp3/ogg-vorbis; `audio-codecs-extra` adds AAC-LC/MP4/M4A, ALAC, ADPCM, AIFF, CAF, MKV), resampled to the stream rate. Param ramps evaluated per pump tick + node smoothers; an automation scheduled in the task that creates its node ships with the node, so a new node never plays a block at its defaults. A node connected from a live node is live (the spec's connection reference): a playing source holds the whole chain after it, and the chain is collected once the source has ended and nothing else holds it. `GainNode.gain` is an amplitude, as in the spec (firewheel's `Volume::Linear` is squared, so the node is given its square root); a negative gain plays at its magnitude, without phase inversion (one-time advisory). `AnalyserNode` is REAL (bypass monitor ring + Blackman-windowed FFT, `fftSize` capped at 4096). `AudioBufferSourceNode` follows the spec playback algorithm: `start(when, offset, duration)` plays exactly that slice of the buffer (an audio-sprite atlas plays one sprite), `loop` with `loopStart`/`loopEnd` loops only the region (after any lead-in from `offset`), and all three loop attributes apply live; negative `when`/`offset`/`duration` is a `RangeError`. `OscillatorNode` is ours (`webapi/osc.rs`): sine/square/sawtooth/triangle, PolyBLEP band-limited, no `PeriodicWave`. `ChannelSplitterNode` / `ChannelMergerNode` (factories and constructors, 1–32, spec `IndexSizeError` / `InvalidStateError`, `connect(dest, output, input)` by index) are custom Firewheel nodes: splitter output k is port k, merger input j is the port pair (2j, 2j+1) down-mixed to mono in the processor. Deviations: `suspend()` mutes the master instead of pausing the clock; scheduling lands on the pump tick, not the sample; the graph mixes in stereo, so an output wider than two channels keeps channels 0–1 except into a splitter (one-time advisory); `channelCount`/`channelCountMode`/`channelInterpretation` exist only on the splitter and merger; no `OfflineAudioContext`/`AudioWorklet`/`ConvolverNode`/`DelayNode`/`WaveShaperNode`/`DynamicsCompressorNode`; `createMediaElementSource` is a silent pass-through that prints an advisory saying so. ⚠ firewheel 0.10 gotcha: the sampler's `sync_*_event` helpers emit the wrong `ParamData` for `Notify` fields — use `Diff`-generated events (see `webapi/audio.rs`). |
| **`RedStreamSourceNode`** (`ctx.createRedStreamSource(opts)`) — NOT a browser API | Streaming-PCM input: the seam a native producer (WebRTC/LiveKit track, a decoder, a capture bridge) pushes interleaved `f32` frames into so they play out of the Web Audio graph. Backed by Firewheel's `StreamWriterNode` + `fixed_resample`, so the producer's rate is converted to the device's continuously (drift included). Rust side: `redjs::webapi::pcm::sink(stream_id)` → `PcmSink::push_interleaved(&[f32])`, `Send + Sync`, non-blocking, never touches the JS context or the graph. JS side: `node.push(Float32Array)`, `close()`, `streamId`, `queuedSeconds`, `underflowed`/`overflowed` (read-and-clear). Deliberately NOT named `createMediaStreamSource`: there is no `MediaStream` here and the direction is push, not pull. Contract in `webapi/pcm.rs`. |
| `Element` / `HTMLElement` / `SVGElement` / `Node` | real prototype chain off EventTarget (Vue's `instanceof HTMLElement` checks work); `document.createElement` instances are HTMLElements |
| `requestIdleCallback` / `cancelIdleCallback` | native queue fired after the rAF batch each pump; IdleDeadline with a real decreasing ~4 ms budget (hydrate loops progress AND terminate) |
| `matchMedia` / `IntersectionObserver` | inert but spec-shaped (EventTarget-backed MediaQueryList; observer never fires) |

Coverage is verified against the real engine bundle:
`bun run packages/rp-spatial/index.ts packages/player/dist/player.js` →
**0 missing** of 116 detected API usages (the analyzer's HOST_SHIMS list now
describes this runtime; keep the two in sync).

Host integration:
- `redjs_webshim_install(ctx, scriptsRoot, assetsRoot)` (C#:
  `InstallWebShims`) — after `redjs_web_install`.
- Hooks instead of the old C# objects: `redjs_dom_resize`,
  `redjs_dom_set_visibility`, `redjs_dom_content_loaded`,
  `redjs_dom_register_element` (the WebGPU canvas wrapper).
- `redjs_navigator_set_on_line(state)` (1 online, 0 offline, <0 no
  provider) — from the host's OS network-change notifier, any thread; each
  change becomes an `online`/`offline` event at every context's next pump.
- **`redjs_image_pixels(value)`** → direct RGBA8 pointer of an
  ImageBitmap/loaded image — the zero-copy `copyExternalImageToTexture`
  upload path (C#: `TryGetImagePixels` → `ReadOnlySpan<byte>`).

`src/bootstrap.js` now holds ONLY dumb stubs: console formatting (prefers a
host `__hostConsole`), atob/btoa, crypto (non-secure), AbortController,
OffscreenCanvas-2D (over native pixels), BroadcastChannel. (Web Audio has no
JS stub at all: with the `audio` feature off, `AudioContext` simply does not
exist, so `typeof AudioContext !== 'undefined'` tells the truth either way.)

## ClearScript → redjs mapping (from the player's actual usage)

| ClearScript (RedPew.Player today) | redjs |
|---|---|
| `V8ScriptEngine(…flags…)` | `redjs_init(options)` + `redjs_context_create` |
| `engine.Execute(name, code)` | `redjs_eval16(src, url)` — url shows in JS stacks |
| `AddHostObject("navigator", obj)` (reflection binding) | `redjs_hostclass_create` + `redjs_hostobject_create` + prototype of `redjs_function_create` methods, `redjs_define` on global |
| `setTimeout/setInterval` + `TimerScheduler` (C#, stopwatch clock) | native, built into redjs — `redjs_web_install` + `redjs_pump`; spec extra args (`setTimeout(cb, ms, ...args)`) |
| `requestAnimationFrame` + `AnimationFrameScheduler` (ScriptObject marshaling) | native rAF in redjs; registration ≈ 2 ns, fired by the pump |
| `AddPerformanceObject` + `Execute("performance = Performance")` (own clock!) | native `performance.now()` on THE shared clock |
| `GlobalFunctions.NowTime()` stopwatch (third clock) | `redjs_now()` / `JsContext.Now` — same clock as JS |
| `__hostFetch`/`FetchApi` + JS Response wrapper | native `fetch`/`Response` in redjs |
| `Blob`, `HTMLImageElement`, `ImageBitmap`, `CreateImageBitmapApi` (ImageSharp) | native classes in redjs (`image` crate decode) |
| `BrowserHostFns.Utf8Encode/Decode/ImagePixels` | native `TextEncoder`/`TextDecoder`; pixels via `redjs_image_pixels` |
| `DocumentObject`/`WindowObject`/`EventTargetObject` | native `document`/`window`/`EventTarget`; C# pokes via `DomResize`/`DomContentLoaded`/`DomSetVisibility` |
| `copyExternalImageToTexture` → `bmp.GetPixelData()` (C# byte[]) | `TryGetImagePixels` → `ReadOnlySpan<byte>` straight into `wgpuQueueWriteTexture` |
| `BrowserBootstrap.Install` (513-line JS string in C#) | gone — native classes + a small `bootstrap.js` inside redjs |
| custom host fns (`AddHostObject("__x", …)`) | `redjs_function_create` + `redjs_define(global, …)` |
| `AddHostType("GPUBufferUsage", typeof(...))` (const bags) | plain JS object: `object_create` + `define` readonly ints |
| `ScriptObject.GetProperty("size")` + `Convert.ToUInt32` (descriptor parsing, GPUDevice 1224 LOC) | cached `redjs_atom16` + `redjs_get` → **decode u64 in C#, no FFI for numbers** |
| `so[i]` / `GetProperty("length")` | `redjs_get_index` / `redjs_array_length` |
| `ScriptObject.InvokeAsFunction(timestamp)` (rAF) | `redjs_protect`(callback) + `redjs_call(f, undefined, &args)` |
| `ITypedArray<T>.Read(…, span, …)` (copy out) in `writeBuffer` | `redjs_typedarray_info` → **direct pointer into JS memory, zero copy** → `wgpuQueueWriteBuffer` straight from it |
| `IArrayBuffer.GetBytes()` (full copy) | `redjs_arraybuffer_info` → pointer |
| `getMappedRange`: `CreateArrayBuffer(size)` + seed copy + write-back copy on `unmap` | `redjs_arraybuffer_external(wgpu mapped ptr)` → **true zero-copy view**; `unmap` = `redjs_arraybuffer_detach` (spec behavior, deallocator hook fires) |
| `EnableTaskPromiseConversion` (Task→Promise) | `redjs_promise_create` → resolve/reject via `redjs_call` when the Task completes + `drain_microtasks`; JS promise→Task: `redjs_promise_info` poll per frame |
| `JSPromise.Resolve(value)` hack (temp global + eval!) | `redjs_promise_create` + immediate resolve call |
| `IsUndefined(value)` / `Undefined` | `v == layout.valueUndefined` (no FFI) |
| JS exception → `ScriptEngineException` | status 1 + `redjs_error_info` (message + stack as JS strings) |
| `EnableDebugging` (V8 inspector :9222) | ✗ not yet — JSC remote inspector wiring is a later task (Bun has it on Windows; needs a WebSocket transport shim) |
| `EnableDynamicModuleImports` | ✗ classic scripts only for now (the built player.js is a single bundle); module-loader hooks = custom `JSGlobalObject` method table, planned v2 |
| `V8RuntimeConstraints` heap caps | `redjs_init("…JSC options…")` / `redjs_set_options` — full `Options::setOptions` access |

Perf notes vs today's pipeline:
- `writeBuffer(Float32Array)` currently does ITypedArray.Read → C# array →
  fixed → wgpu. With redjs it's one `typedarray_info` call and wgpu reads JS
  memory directly.
- Descriptor parsing (`GetProperty` + `Convert.ChangeType` boxing) becomes
  atom-cached `redjs_get` + branch-free u64 decode — no boxing anywhere.
- Every JS→C# host call drops from ~µs (ClearScript reflection binding under
  AOT) to ~2-4 ns + the C# body.

## NativeAOT (static) vs JIT (dll) consumption

Mirrors the miniaudio pattern (`packages/miniaudio`): **JIT/debug loads
`redjs.dll`, AOT static-links `redjs.lib`** — same `DllImport("redjs")`
externs either way.

JIT/debug: put `runtimes/win-x64/native/redjs.dll` next to the exe (csproj
`None Include` copy, or a NativeLibrary resolver like Miniaudio's).

NativeAOT — in the AOT root csproj (RedPew.Player.csproj):

```xml
<ItemGroup Condition="'$(PublishAot)' == 'true'">
  <DirectPInvoke Include="redjs" />
  <NativeLibrary Include="$(JscoreDir)lib\win-x64\redjs.lib" />
  <!-- redjs.lib is THIN: add the WebKit static libs it was built against -->
  <NativeLibrary Include="$(JscoreDir)prebuilt\bun-webkit\lib\JavaScriptCore.lib" />
  <NativeLibrary Include="$(JscoreDir)prebuilt\bun-webkit\lib\WTF.lib" />
  <NativeLibrary Include="$(JscoreDir)prebuilt\bun-webkit\lib\bmalloc.lib" />
  <NativeLibrary Include="$(JscoreDir)prebuilt\bun-webkit\lib\sicudt.lib" />
  <NativeLibrary Include="$(JscoreDir)prebuilt\bun-webkit\lib\sicuin.lib" />
  <NativeLibrary Include="$(JscoreDir)prebuilt\bun-webkit\lib\sicuuc.lib" />
  <!-- system libs beyond ILC defaults (verified list — RedJs.Smoke.csproj) -->
  <LinkerArg Include="shell32.lib" />
  <LinkerArg Include="ole32.lib" />
  <LinkerArg Include="oleaut32.lib" />
  <LinkerArg Include="shlwapi.lib" />
  <LinkerArg Include="winmm.lib" />
  <LinkerArg Include="dbghelp.lib" />
  <LinkerArg Include="usp10.lib" />
  <LinkerArg Include="synchronization.lib" />
  <LinkerArg Include="userenv.lib" />
  <LinkerArg Include="ws2_32.lib" />
  <LinkerArg Include="rpcrt4.lib" />
  <LinkerArg Include="crypt32.lib" />
  <!-- since d6375c72: JSC registers unwind info for its JIT code (RtlAddGrowableFunctionTable).
       redjs.lib's glue object also carries /DEFAULTLIB:ntdll.lib, so a csproj without
       this line still links. -->
  <LinkerArg Include="ntdll.lib" />
</ItemGroup>

<!-- No identical-COMDAT folding: ILC adds /OPT:REF /OPT:ICF to every non-Debug link, and
     JSC tells a constructor from a non-constructor by comparing native function pointers,
     so folding can turn a real constructor into "not a constructor" (Bun shipped exactly
     that for BigInt; WebKit links /OPT:NOICF). ILC adds its flags inside the
     SetupOSSpecificProps target, AFTER static items — a plain LinkerArg would lose. -->
<Target Name="RedJsNoIcf" AfterTargets="SetupOSSpecificProps" Condition="'$(PublishAot)' == 'true'">
  <ItemGroup>
    <LinkerArg Remove="/OPT:ICF" />
    <LinkerArg Include="/OPT:NOICF" />
  </ItemGroup>
</Target>
```

mimalloc needs no line here: `build.rs` compiles it into `redjs.lib` itself.
The library names are unchanged by the m24 bump (`bmalloc.lib` now holds ~7 KB
of code — libpas is gone — but it is still part of the prebuilt and stays on
the list).

This exact wiring is proven by `csharp/RedJs.Smoke` (publishes and passes all
63 checks as a 55 MB standalone exe on d6375c72 — JSC statically linked, no dll
shipped; it was 50 checks and 42 MB on the libpas engine).
WebKit libs are `/MT`; NativeAOT's link accepts the mix (both smoke paths run).

## C# codegen plan (next step)

Same pipeline as miniaudio (`csharp/codegen` with csbindgen), but richer:

1. `csbindgen` over `redjs/src/ffi.rs` + `redjs/src/glue.rs` →
   `NativeMethods.g.cs` (`DllImport("redjs")`, blittable structs).
2. A hand-written `JsValue` struct (readonly ulong) with the NaN-box
   decode/encode inlined from `RjsLayout` (cached static), `Span`-based
   string readers over `redjs_string_info`, `JsAtom` cache keyed by string
   literal, `JsCallFrame` ref struct reading args by offset.
3. Wrapper generator for host classes (GPUBuffer & co): prototype object +
   `[UnmanagedCallersOnly]` static per method + `hostobject_data` handle
   (index into a C# side table — no GCHandle per object needed).
4. The finalizer callback enqueues handles into a `ConcurrentQueue`, drained
   on the game thread each frame.

## Known issues / limits (v1)

- **`redjs_context_release` fastfail-crashes** in this prebuilt's VM teardown
  (inherited from the fork; Bun never tears down its main VM either). Create
  the context once and keep it for the process lifetime. Re-tested on
  d6375c72 (m24): unchanged — `0xC0000409` on the first release, even for a
  bare context with nothing installed.
- **Float16Array/DataView** can be introspected (`typedarray_info`) but not
  created through the C API enum.
- **No module loader / dynamic `import()`** (classic scripts only) and **no
  remote inspector** yet — both need a custom `JSGlobalObject`/method-table
  layer (v2).
- **Memory — re-measured on d6375c72 (mimalloc), m24.** One VM, `redjs_init`
  + context + `redjs_web_install`, `K32GetProcessMemoryInfo`:

  | stage | working set | private |
  |---|---:|---:|
  | boot | **15.4 MB** | **35.5 MB** |
  | 20 × 20k objects/strings/closures + a Map live | 124.0 MB | 162.4 MB |
  | garbage dropped, full sync GC (`redjs_gc(0)`) | 124.2 MB | 163.4 MB |
  | `redjs_release_memory` | **48.4 MB** | 79.4 MB |
  | a second workload, then 8 s idle, no call | 49.7 MB (flat from +1 s) | 87.8 MB |

  **What changed with the allocator:** a GC frees objects but the pages stay
  resident, and **nothing returns them on its own while the thread is idle** —
  bmalloc's scavenger thread used to (48 → 22 MB after ~4 s); mimalloc gives
  pages back on allocator activity or when asked. So `redjs_release_memory`
  (C# `JsContext.ReleaseMemory` — 2 full GCs + scavenge; the scavenge now
  reaches mimalloc through bmalloc's shim, `mi_collect`) is no longer an
  accelerator but **the** way to shed a workload's peak: call it on loading
  screens, blur and minimize (the player already does on blur). JSC's own
  helper threads call `mi_on_thread_idle` when they park; on the thread that
  runs JS, `redjs_web_pump` does, between waves, at most every 250 ms
  (`web.rs` `idle_sweep`).

  **JSC's timers run from the pump, too.** The incremental sweeper and the
  eden/full GC activity callbacks are RunLoop timers, and this prebuilt's
  RunLoop is the generic one: its timers fire only when something cycles it,
  and until vue-native m26 nothing did — the heap never shrank on its own (the
  Argon client: 280 MB of GC heap capacity for 85 MB live after two idle
  minutes). `redjs_web_pump` now turns it once between waves
  (`rjs_run_loop_cycle`); the same client settles at 96 MB capacity, its
  private working set 655 → 440 MB, frame time unchanged (p50 6.1 ms, p95
  6.7 ms). Cost over that 4-minute run: 55 ms of RunLoop turns in all, 4 ms the
  longest (a collection the timers started); 962 idle sweeps, 0.26 ms on
  average, 8 ms the longest. `redjs_release_memory_and_code` (also drops all
  compiled code) is a measuring tool: what it leaves is the program's data.

  The pre-m24 note, measured on the libpas engine (c9ad5813/2a51deeb) with a
  different staged probe, for the record: ~162 MB private per VM, explained as
  address-space reservation (JIT pool + gigacage + bmalloc arenas) and unmoved
  by every runtime option; ~12 MB WS at boot, ~22–24 MB after a GC. The old
  engine was not re-run with the probe above, so the two sets are not a
  like-for-like comparison.
- The internal-header dependency means **prebuilt bumps are a real upgrade
  task**: edit `pins.txt`, run `setup.ps1`, re-run `build.ps1 -Test`; the
  layout export + conformance suite are the tripwires (its `engine_surface`
  section and `small_structure_heap_boots` test cover what the m24 bump
  changed: ICU + the zstd display names, Temporal, constructor identity under
  the link flags, native unwinding through JIT frames, mimalloc's pagemap). This is the same commitment Bun makes — it's the price of
  the 2 ns boundary.
- battle_test keeps its own copy of the prebuilt (its bench predates this
  package); the canonical location is now `packages/jscore/prebuilt`.
