# redjs

![Platform: Windows x64](https://img.shields.io/badge/platform-Windows%20x64-0078D4)
![Engine: JavaScriptCore](https://img.shields.io/badge/engine-JavaScriptCore-F7DF1E)
![Bindings: Rust and C#](https://img.shields.io/badge/bindings-Rust%20%2F%20C%23-555555)
![Status: experimental](https://img.shields.io/badge/status-experimental-orange)

**JavaScriptCore for native apps.**

redjs embeds Bun's build of JavaScriptCore in Rust, with a C ABI and C#
bindings for .NET 10. Run JavaScript, expose native functions, and give your
scripts familiar web APIs without opening a browser.

```rust
use redjs::api::{Context, Result};

fn main() -> Result<()> {
    let js = Context::new();
    let result = js.eval("[1, 2, 3].map(n => n * 2).join(', ')")?;

    println!("{}", js.to_rust_string(result)?); // 2, 4, 6
    Ok(())
}
```

## What you get

- **JavaScript with JIT.** Evaluate scripts, call functions, and read exceptions
  with their JavaScript stacks.
- **Native integration.** Host functions and objects, promises, typed arrays,
  and zero-copy buffers over host memory.
- **Useful web APIs.** Timers, events, URLs, fetch, WebSocket, streams, crypto,
  and more. The host drives the event loop.
- **Optional extras.** Web Audio, Canvas 2D, IndexedDB, WebGPU, and game-oriented
  helpers for physics, meshes, CSS, and layout.

The current build targets **Windows x64**. It uses JSC's internal headers, so
the engine version is pinned and upgrades need testing.

## Build and try it

You'll need stable Rust with the MSVC target, LLVM's `clang-cl` on `PATH`,
Visual Studio C++ Build Tools (MSVC 14.44 or newer, plus the Windows SDK),
and PowerShell.

This repository currently expects a sibling `redcss` crate at `../redcss`.
Cargo needs that crate's manifest even when its feature is disabled. In the
vue-native workspace, both repositories already live under `vendor/`.

From this repository's root:

```powershell
./setup.ps1
cd redjs
cargo run --release --no-default-features --example mini
```

`setup.ps1` downloads and verifies the native dependencies; allow roughly a
gigabyte for the unpacked JavaScriptCore build. The `mini` probe creates a VM,
evaluates a few expressions, and prints each step.

Versions and hashes live in [pins.txt](pins.txt). Run `setup.ps1` again after
a pin change. To use a prebuilt stored elsewhere, set `BUN_WEBKIT_DIR`; the
build checks it against the same pin.

For the full default build, you'll also need Yoga's C++ sources in
`redjs/cpp/yoga/`. Back in the repository root, run `./build.ps1 -Test` to build
the library, run the Rust suite, and stage the DLL and static library under
`runtimes/` and `lib/`.

## Use it in your app

**Rust:** start with [`Context` and `Value`](redjs/src/api.rs). Disable default
features if you only need the engine and core web APIs. Your host's Cargo
configuration must also use the static CRT settings in
[redjs/.cargo/config.toml](redjs/.cargo/config.toml).

**C#:** reference [csharp/RedJs](csharp/RedJs/RedJs.csproj) and build the native
library first. The basic API looks like this:

```csharp
using RedJs;

var js = JsContext.Create();
Console.WriteLine(js.Eval("6 * 7").AsNumber); // 42
```

The [C# smoke project](csharp/RedJs.Smoke/RedJs.Smoke.csproj) includes both DLL
loading and static linking for .NET NativeAOT. NativeAOT here compiles the
.NET host; JavaScript still uses JSC's normal execution path. Compiling
JavaScript to native images is a separate, experimental
[`native-aot` feature](redjs/examples/aot/README.md).

**Other languages:** the [`redjs_*` C ABI](redjs/src/ffi.rs) exposes the same
engine operations, with per-function documentation in the source.

## Web APIs and the event loop

A fresh context starts with the JavaScript language globals. To add the web
layer:

1. Call `redjs_web_install` for timers, animation frames, and `performance`.
2. Call `redjs_webshim_install` for the web APIs, passing your scripts and
   assets roots.
3. Call `redjs_pump(ctx, 7, stats)` regularly on the VM thread to process
   timers, animation frames, async completions, and microtasks. `stats` can
   be null.

In C#, these are `InstallWebGlobals()`,
`InstallWebShims(scriptsRoot, assetsRoot)`, and `Pump()`.
A render loop can pump once per frame.

The default Cargo features include `audio`, `physics`, `mesh`, `css-host`,
`yoga-host`, `indexeddb`, `svg-images`, and `canvas`. Use
`--no-default-features` for a smaller starting point, then enable what you need.

Some additions are opt-in:

| Feature | Adds |
| --- | --- |
| `storages` | `localStorage` and `sessionStorage`, with host hooks for persistence |
| `clipboard` | `navigator.clipboard`, backed by the host's clipboard |
| `indexeddb-sqlite` | Persistent IndexedDB when the host supplies its storage configuration |
| `net-h2` | Pooled HTTP/1.1 and HTTP/2 for fetch |
| `audio-codecs-extra` | More audio formats, including AAC-LC and ALAC |
| `webgpu` | Experimental native WebGPU; see the [support notes](redjs/WEBGPU.md) |

These APIs cover parts of the web platform. DOM, rendering, permissions, and
OS integration depend on what the host provides. The
[web API modules](redjs/src/webapi/mod.rs) document individual behavior and limits.

## A few things to know

- **Keep contexts for the process lifetime.** The current prebuilt has a
  known crash during VM teardown. C# deliberately has no `Dispose` for a context.
- **Use one thread per context.** Pump on that same thread. Separate contexts
  have separate VMs; values cannot be passed directly between them.
- **Protect values you keep.** A value stored in a field or collection needs
  `protect` / `unprotect` (`Protect` / `Unprotect` in C#). Copying a `Value`
  does not keep the JavaScript object alive.
- **Bundle your scripts.** Evaluation runs classic scripts; there is no module
  loader, dynamic `import()`, or remote inspector yet.

## Benchmarks

The [C# benchmark suite](csharp/RedJs.Benchmarks/README.md) compares redjs with
ClearScript/V8, Jint, YantraJS, Jurassic, and NiL.JS using BenchmarkDotNet.
It covers script execution, calls in both directions, allocations, and first use.
The [redjs modes comparison](csharp/RedJs.Benchmarks/results/2026-11-08-redjs-modes/README.md)
covers warmed JIT, cached bytecode with JIT on or off, and native JavaScript AOT images.
See the [recorded comparison](csharp/RedJs.Benchmarks/results/2026-11-08/README.md)
for results and the conditions they were measured under.

## Going further

- [Rust examples](redjs/examples) — audio, spatial sound, WebGPU, and engine probes.
- [Rust conformance suite](redjs/tests/smoke.rs) and
  [C# smoke tests](csharp/RedJs.Smoke/Program.cs) — working examples of the embedding API.
- [IndexedDB web-platform-tests](redjs/tests/wpt/README.md) — how to run the compatibility suite.
- [Native image probes](redjs/examples/aot/README.md) and
  [AOT memory profiles](redjs/docs/native-aot-memory.md) — the experimental JavaScript AOT path.
