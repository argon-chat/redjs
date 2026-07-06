# jscore — JavaScriptCore embedding layer (ClearScript replacement)

Bun's JavaScriptCore (Windows prebuilt) + a Rust full-control embedding crate
(`redjs`) with a flat C ABI designed for **zero-alloc C# NativeAOT bindings**.
This is the engine seam that replaces ClearScript/V8 in
`packages/player_backend/RedPew.Player`.

Why (measured in `battle_test/`, see its RESULTS.md): ClearScript's managed
marshaling costs ~100× on host calls. JSC via its *internal* binding path does
a js→host call in **2.2 ns** (ties SpiderMonkey, 4× faster than V8) on top of
the best-in-class JIT (fastest compute/fib of every engine tested), and Bun is
about to land shared-memory multithreading in it.

```
packages/jscore/
├─ setup.ps1                  # fetch/unpack the pinned bun-webkit prebuilt (~1.1 GB, gitignored)
├─ build.ps1 [-Test]          # cargo build --release + artifact layout (+ conformance suite)
├─ prebuilt/bun-webkit/       # include/ (internal headers!) + lib/ (static libs)
├─ redjs/                     # the Rust crate
│  ├─ build.rs                #   trampoline codegen + clang-cl glue build + link
│  ├─ cpp/glue.cpp            #   JSC internal-API layer (extern "C" rjs_*)
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
│  └─ RedJs.Smoke/            # ★ C# conformance smoke (50 checks) — JIT and AOT
├─ runtimes/win-x64/native/redjs.dll   # built: self-contained, for JIT/debug DllImport
└─ lib/win-x64/redjs.lib               # built: THIN static lib for NativeAOT
```

## Quick start

```powershell
./setup.ps1          # once: fetch prebuilt (or -Tarball path\to\bun-webkit-windows-amd64.tar.gz)
./build.ps1 -Test    # build + run the 14-section conformance suite

cd csharp/RedJs.Smoke
dotnet run -c Release                                          # C# smoke, JIT via redjs.dll
dotnet publish -r win-x64 -c Release -p:PublishAot=true -p:RedJsRid=win-x64
./bin/Release/net10.0/win-x64/native/RedJs.Smoke.exe           # C# smoke, static-linked AOT
```

Requires stable Rust and `clang-cl` (LLVM) on PATH. Everything is verified on
this exact pinned prebuilt: `autobuild-c9ad5813fd23bd8b98b0738abc3d037ec716aa92`.

**Measured js→C# host-call boundary** (2M-call hot loop, arg decode + result
encode included): **8.9 ns/call NativeAOT**, 11.2 ns/call Release JIT. The
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
locked. (JSC multi-threading lands with Bun's shared-memory work; the
lock-per-context model stays correct.)

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
| `TextEncoder` / `TextDecoder` | UTF-8; wrong labels throw RangeError per spec |
| `Blob` | Arc-shared bytes — spec `slice()` is zero-copy; promise `arrayBuffer/bytes/text` |
| `URL` / `URLSearchParams` | backed by the `url` crate (rust-url = the reference WHATWG implementation); searchParams↔href live sync; full iteration protocol (`entries/keys/values/forEach/[Symbol.iterator]`, live-list semantics) |
| `fetch` / `Response` / `Headers` | local files (scripts→assets root resolution) + `data:` URIs; http(s) → network-error response |
| `ImageBitmap` / `HTMLImageElement` / `Image` / `createImageBitmap` | decode via the `image` crate (png/jpeg/webp/bmp/gif); `img.src` loads async on the pump tick and fires `load`/`error`; region-crop overload for sprite atlases |
| `document` / `window` | EventTarget-backed; `window === globalThis` (proxy-normalized) |
| `crypto` | **OS CSPRNG** (`getrandom` → BCryptGenRandom): getRandomValues (spec type/quota checks) + randomUUID v4; no `subtle` |
| `structuredClone` | native deep clone: plain objects/arrays, typed arrays/ArrayBuffers, Date/RegExp/Map/Set, Blob/ImageBitmap, cycles; functions → DataCloneError |
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
- **`redjs_image_pixels(value)`** → direct RGBA8 pointer of an
  ImageBitmap/loaded image — the zero-copy `copyExternalImageToTexture`
  upload path (C#: `TryGetImagePixels` → `ReadOnlySpan<byte>`).

`src/bootstrap.js` now holds ONLY dumb stubs: console formatting (prefers a
host `__hostConsole`), atob/btoa, crypto (non-secure), AbortController,
OffscreenCanvas-2D (over native pixels), silent Web Audio, BroadcastChannel.

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
</ItemGroup>
```

This exact wiring is proven by `csharp/RedJs.Smoke` (publishes and passes all
50 checks as a 42 MB standalone exe — JSC statically linked, no dll shipped).
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
  the context once and keep it for the process lifetime.
- **Float16Array/DataView** can be introspected (`typedarray_info`) but not
  created through the C API enum.
- **No module loader / dynamic `import()`** (classic scripts only) and **no
  remote inspector** yet — both need a custom `JSGlobalObject`/method-table
  layer (v2).
- **Memory (tuned, verdict: a non-problem)**: the ~162 MB private per VM is
  **address-space RESERVATION** (JIT fixed-VM pool + gigacage + bmalloc
  arenas), fixed at context creation and **unmoved by every runtime option**
  (`forceMiniVMMode`, `jitMemoryReservationSize`, `useGigacage`, `forceRAMSize`,
  JIT-tier toggles — all measured via a staged WS/private probe). Only
  `useJIT=false` lowers it, which defeats the purpose. **Working set (real
  RAM) is the honest number: ~12 MB at boot, ~22–24 MB after a GC even
  post-heavy-workload.** bmalloc's scavenger returns free pages to the OS on
  its own after ~4 s idle (48 → 22 MB WS observed); `redjs_release_memory`
  (C# `JsContext.ReleaseMemory` — 2 full GCs + scavenge) forces it immediately
  for loading screens / window-minimize (the player calls it on blur).
- The internal-header dependency means **prebuilt bumps are a real upgrade
  task**: re-run `build.ps1 -Test`; the layout export + conformance suite are
  the tripwires. This is the same commitment Bun makes — it's the price of
  the 2 ns boundary.
- battle_test keeps its own copy of the prebuilt (its bench predates this
  package); the canonical location is now `packages/jscore/prebuilt`.
