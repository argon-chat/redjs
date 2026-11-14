# Native image probes (Windows x64)

These use Bun frontend `7ca0042d3ba23ed60fd9cd8af1a1ce6ec3f8d074` and
WebKit `4d45755fed4aeef19e95beb90d3d0c884a2812bd`. `native-aot` is opt-in.
Generate the full matching builtin table with the embedder's pinned generator
(skathi: `scripts/aot/generate-table.ts`), then point
`REDJS_AOT_BUILTIN_TABLE_DIR` at it. An engine-only table cannot consume the
full frontend's builtin IDs.

The compiler and loader also build on Linux x64 and macOS x64/arm64. Native
images are specific to their OS and architecture; Unix `COMPILE_OPTIONS`
includes both, and a host must validate the matching manifest before loading.
The Rust path loader uses an open file descriptor on Unix (including non-UTF-8
paths), duplicates it for the engine, and maps the initial view read-only.
Keep that inode immutable while the image is in use. Windows keeps its existing
UTF-16 path / retained-handle API and write-sharing policy.

From `redjs/`, with the pinned frontend on `$frontend` and an isolated `$out`:

```powershell
$env:CARGO_TARGET_DIR = "$out/target"
$env:REDJS_AOT_BUILTIN_TABLE_DIR = "$out/table"
$env:APPDATA = "$out/appdata"
$env:SK_DATA_DIR = "$out/profile"
& $frontend compile examples/aot/web.ts --no-builtin-modules "--aot-out=$out/web.ir" --aot-threads=1 --aot-testing=NoExecutable --outfile "$out/unused.exe"
cargo run --example aot-image --no-default-features --features native-aot -- compile "$out/web.ir" "$out/web.image"
cargo run --example aot-image --no-default-features --features native-aot -- run-web "$out/web.image"
```

Compile and load run in separate processes. The loader checks entry bounds,
uses a custom JSC realm and invokes real Rust host callbacks. The web probe
asserts Promise/timer/event delivery, recursion, URL, known engine builtins,
structuredClone of compiled objects, and GC while compiled JS is on the stack.
It never evaluates a source copy of the application as a fallback.

A native image is trusted executable code. The embedder must validate engine,
frontend, builtin-table digest, compile options and its host/bundle contract;
passing the image's own identity alone is not sufficient ABI validation.

## Hosted realm startup and builtin protection

The embedding host is registered before creating a VM. Web APIs (including
native Performance/Crypto interfaces) are installed before `hostDidCreateRealm`
finishes builtin locking and watchpoint setup. Engine native helper bindings
are retained in the registered host set. The host registration does not permit
compiled application code to replace protected builtins.

`host-startup.js` checks that distinction with the same source in separate
native and JIT processes. It retains the web/callback/GC checks above and writes
`Math.abs` dynamically through the opaque host input. The native run must reject
the executed write and preserve the original; JIT must accept and restore it.
An unrelated indirect-eval refusal cannot satisfy this test.

```powershell
& $frontend compile examples/aot/host-startup.js --no-builtin-modules "--aot-out=$out/host.ir" --aot-threads=1 --aot-testing=NoExecutable --outfile "$out/unused.exe"
cargo run --example aot-image --no-default-features --features native-aot -- compile "$out/host.ir" "$out/host.image"
cargo run --example aot-image --no-default-features --features native-aot -- run-web-host-startup "$out/host.image" compact
cargo run --example aot-image --no-default-features --features native-aot -- run-web-host-startup "$out/host.image" default
cargo run --example aot-image --no-default-features --features native-aot -- run-web-host-startup-jit examples/aot/host-startup.js
```

Install the native scheduling clock with `redjs_web_install` before calling
`redjs_webshim_install`. Out-of-order installation returns BadArg before
publishing partial Web API state; it does not fabricate a replacement clock.

Custom realms also apply `refuseEvaluationUnlessAdmitted` from the engine. The
closed host-startup fixture refuses JavaScript eval/Function, while its trusted
host Program probe returns 42. Images whose compiler-generated flags admit
unknown JavaScript retain that policy; they must not be described as closed
or fully trimmed solely because their entry runs as native code.
