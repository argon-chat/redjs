# Native image probes (Windows x64)

These use Bun frontend `7ca0042d3ba23ed60fd9cd8af1a1ce6ec3f8d074` and
WebKit `4d45755fed4aeef19e95beb90d3d0c884a2812bd`. `native-aot` is opt-in.
Generate the full matching builtin table with the embedder's pinned generator
(vue-native: `scripts/aot/generate-table.ts`), then point
`REDJS_AOT_BUILTIN_TABLE_DIR` at it. An engine-only table cannot consume the
full frontend's builtin IDs.

From `redjs/`, with the pinned frontend on `$frontend` and an isolated `$out`:

```powershell
$env:CARGO_TARGET_DIR = "$out/target"
$env:REDJS_AOT_BUILTIN_TABLE_DIR = "$out/table"
$env:APPDATA = "$out/appdata"
$env:VN_DATA_DIR = "$out/profile"
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
