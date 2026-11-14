# redjs

![Platforms: Windows, Linux, macOS](https://img.shields.io/badge/platform-Windows%20%7C%20Linux%20%7C%20macOS-0078D4)
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
- **Optional extras.** Web Audio, Canvas 2D, IndexedDB, and WebGPU.

Build targets are **Windows x64**, **Linux x64 (glibc)**, and **macOS x64 / Apple
Silicon**. Each uses a verified prebuilt from the same JavaScriptCore revision.
The engine's internal headers are version-sensitive, so upgrades need testing.

## Build and try it

On Windows, you'll need Rust 1.97.1 with the MSVC target, LLVM's `clang-cl` on `PATH`,
Visual Studio C++ Build Tools (MSVC 14.44 or newer, plus the Windows SDK),
and PowerShell.

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

Back in the repository root, run `./build.ps1 -Test` to build
the library, run the Rust suite, and stage the DLL and static library under
`runtimes/` and `lib/`.

### Linux and macOS

Use Rust 1.97.1 and Bun 1.4.4. On Linux, install Clang 20, curl, unzip, pkg-config and
OpenSSL headers; Web Audio additionally needs ALSA development headers:

```sh
sudo apt-get update
sudo apt-get install clang-20 lld curl unzip pkg-config libssl-dev libasound2-dev
export CC=clang-20 CXX=clang++-20
```

On macOS, install Xcode command-line tools (`xcode-select --install`); the
macOS 26 SDK / Xcode 26 toolchain supplies the C++23 compiler and standard
library. The pinned engine requires macOS 13+ at runtime. Both Intel and Apple
Silicon use their native architecture. For direct Cargo commands, set
`MACOSX_DEPLOYMENT_TARGET=13.0` (the build script and CI already do this).

```sh
bash ./setup.sh
bash ./build.sh --test
# Additional web APIs, including persistent IndexedDB and HTTP/2:
bash ./build.sh --test --features audio,audio-codecs-extra,svg-images,canvas,indexeddb-sqlite,storages,clipboard,battery,net-h2
```

`setup.sh` verifies archive SHA-256 and extracted source-tree pins. Unix
JavaScriptCore libraries live in `prebuilt/bun-webkit-linux-amd64`,
`prebuilt/bun-webkit-macos-amd64`, or `prebuilt/bun-webkit-macos-arm64`.
The upstream Unix archives omit the portable frontend AOT table sources;
setup obtains those from the verified Windows archive at the same engine
pin and checks that the generated ABI headers match before using them.
It copies sources only. Unix engine bindings already present in the native
archive are not compiled twice.

`build.sh` stages `libredjs.a` under `lib/{linux-x64,osx-x64,osx-arm64}`.
On macOS it also stages `libredjs.dylib` under `runtimes/{rid}/native` for
the C# library resolver. The upstream Linux archive uses executable-only TLS
relocations, so this port supports Rust, static C linking, and C# NativeAOT
on Linux. Building a Linux `.so` requires a separately built PIC engine;
`build.sh --shared` can use one supplied through `BUN_WEBKIT_DIR`.
`cargo build` emits Rust/static libraries; the build scripts explicitly request
shared libraries on Windows and macOS. After building the native library:

```sh
# macOS (shared library):
dotnet run --project csharp/RedJs.Smoke --configuration Release
# Select linux-x64, osx-x64, or osx-arm64 for a .NET NativeAOT smoke build:
dotnet publish csharp/RedJs.Smoke -c Release -r linux-x64 -p:PublishAot=true
```

GitHub Actions runs core and web-feature Rust suites on standard Linux and
macOS runners, plus C# NativeAOT smoke tests (and shared-library smoke tests on
macOS). WebGPU's real-device tests require a GPU, so the hosted test matrix
does not enable them.

## Use it in your app

**Rust:** start with [`Context` and `Value`](redjs/src/api.rs). Disable default
features if you only need the engine and core web APIs. Your host's Cargo
configuration must also use the static CRT settings in
[redjs/.cargo/config.toml](redjs/.cargo/config.toml) on Windows.

Declare raw JSC host callbacks with `#[redjs::host_fn] unsafe fn ...`; the
attribute selects SysV on Windows x64 and the native C ABI on Unix, including
Apple Silicon. Existing `function_sysv` / `redjs_function_create_sysv` names
retain their ABI and accept this platform-correct callback.

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

The default Cargo features include `audio`, `indexeddb`, `svg-images`, `canvas`,
and `webgpu`. Native GPU
acquisition is lazy; see the [WebGPU support notes](redjs/WEBGPU.md). Use
`--no-default-features` for a smaller starting point, then enable what you need.

Some additions are opt-in:

| Feature | Adds |
| --- | --- |
| `storages` | `localStorage` and `sessionStorage`, with host hooks for persistence |
| `clipboard` | `navigator.clipboard`, backed by the host's clipboard |
| `battery` | `navigator.getBattery()` and `BatteryManager`, with native Windows, Linux, and macOS power notifications |
| `indexeddb-sqlite` | Persistent IndexedDB when the host supplies its storage configuration |
| `net-h2` | Pooled HTTP/1.1 and HTTP/2 for fetch |
| `audio-codecs-extra` | More audio formats, including AAC-LC and ALAC |

The optional [Battery Status API](redjs/src/webapi/battery.rs) caches one
Promise and one `BatteryManager` per context. Its read-only `charging`,
`chargingTime`, `dischargingTime`, and `level` attributes support the four
standard change events through `addEventListener` and `on…change` handlers.
Subscriptions start on the first `getBattery()` call; the existing pump
delivers changes without battery polling. Windows uses power-setting
notifications and cannot provide a charging-time estimate (`Infinity` until
full). Linux uses UPower's aggregate display device on the system D-Bus,
including daemon restart notifications. macOS uses IOPowerSources and a
sleeping Core Foundation run loop; its charge estimates are converted from
minutes to seconds. Linux/macOS acquisition runs on a cancellable native
worker, and the initial Promise resolves only after its first snapshot is
pumped. Native callbacks publish no JavaScript values, and cleanup never
joins a worker on the JS thread. No battery, unavailable status / services,
and unsupported platforms use the specification's defaults (`true`, `0`,
`Infinity`, `1`). Linux requires the UPower system service; a lost bus
connection falls back to defaults for that subscription. The native web
installer provides a Window-like application context; there is no Worker installer or Document permissions-policy model.
The feature is off by default in redjs and the generic SDK host; Argon enables
it by default. SDK consumers opt in with `sk-app/battery` (forwarded through
`sk-runtime/battery`); building Argon with `--no-default-features` omits it.

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

## License

redjs is licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at
your option.

It links JavaScriptCore, WTF and bmalloc from Bun's WebKit build, statically by default. Parts of
JavaScriptCore and WTF are licensed under the GNU LGPL 2; the rest of WebKit is BSD. To meet the
LGPL, the patched WebKit source is public at [oven-sh/WebKit](https://github.com/oven-sh/WebKit):
the exact commit is `webkit.version` in [pins.txt](pins.txt). To relink redjs, or a program built
on it, against a modified JavaScriptCore: build that commit, point `BUN_WEBKIT_DIR` at the result
and rebuild (`build.ps1` / `build.sh`). On Windows, `REDJS_ENGINE_DLL=redjs.dll` puts the engine in
a DLL of its own that can be replaced without relinking the program.

Also linked: ICU (Unicode License V3), mimalloc (MIT) and, with the `indexeddb-sqlite` feature,
SQLite3MultipleCiphers (MIT; SQLite is public domain).
