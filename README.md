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
│  ├─ src/glue.rs             #   raw externs + repr(C) structs (csbindgen input)
│  ├─ src/api.rs              #   safe Rust API (Context/Value/HostArgs)
│  ├─ src/trampoline.rs       #   SysV→MS host-fn trampoline pool (the C# entry)
│  ├─ src/capi.rs             #   JSC C-API subset used internally
│  └─ tests/smoke.rs          #   ★ conformance suite = the ABI contract, 14 sections
├─ runtimes/win-x64/native/redjs.dll   # built: self-contained, for JIT/debug DllImport
└─ lib/win-x64/redjs.lib               # built: THIN static lib for NativeAOT
```

## Quick start

```powershell
./setup.ps1          # once: fetch prebuilt (or -Tarball path\to\bun-webkit-windows-amd64.tar.gz)
./build.ps1 -Test    # build + run the 14-section conformance suite
```

Requires stable Rust and `clang-cl` (LLVM) on PATH. Everything is verified on
this exact pinned prebuilt: `autobuild-c9ad5813fd23bd8b98b0738abc3d037ec716aa92`.

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

## ClearScript → redjs mapping (from the player's actual usage)

| ClearScript (RedPew.Player today) | redjs |
|---|---|
| `V8ScriptEngine(…flags…)` | `redjs_init(options)` + `redjs_context_create` |
| `engine.Execute(name, code)` | `redjs_eval16(src, url)` — url shows in JS stacks |
| `AddHostObject("navigator", obj)` (reflection binding) | `redjs_hostclass_create` + `redjs_hostobject_create` + prototype of `redjs_function_create` methods, `redjs_define` on global |
| `AddHostObject("setTimeout", (ScriptObject cb, …) => …)` | `redjs_function_create` + `redjs_define(global, …)` |
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
  <!-- system libs beyond ILC defaults -->
  <LinkerArg Include="shlwapi.lib winmm.lib dbghelp.lib usp10.lib synchronization.lib" />
</ItemGroup>
```

(Exact system-lib tail may need trimming/extending on unresolved externals;
the full list redjs links is in `redjs/build.rs`. WebKit libs are `/MT` —
NativeAOT's static-CRT link is compatible; the DLL path already proves the
mixed-CRT case runs.)

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
- **Memory**: JSC commits ~162 MB private (28 MB resident) per VM on Windows —
  commit charge, not RAM; env-level options didn't shrink it (measured in
  battle_test). `redjs_init` exposes the full options string if we want to
  dig further; revisit only if commit charge actually matters.
- The internal-header dependency means **prebuilt bumps are a real upgrade
  task**: re-run `build.ps1 -Test`; the layout export + conformance suite are
  the tripwires. This is the same commitment Bun makes — it's the price of
  the 2 ns boundary.
- battle_test keeps its own copy of the prebuilt (its bench predates this
  package); the canonical location is now `packages/jscore/prebuilt`.
