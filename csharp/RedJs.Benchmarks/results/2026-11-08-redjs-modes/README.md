# redjs execution modes: 2026-11-08

BenchmarkDotNet 0.15.8, ShortRun: one launch, three warmups, three measured
iterations per case. All 24 cases passed their result and backend/cache checks.
Machine: AMD Ryzen 9 9950X (32 logical processors), Windows 11 10.0.29683.1000,
.NET SDK 10.0.201 / runtime 10.0.5, x64 RyuJIT. C# uses the normal .NET JIT.

These are means from this run. The baseline is the fresh `SourceJit` result
in the same suite. The older cross-engine comparison is a separate run.

| Scenario | Unit | Source JIT | Bytecode + JIT | Bytecode interpreter | Native AOT |
| --- | --- | ---: | ---: | ---: | ---: |
| C# to JS | ns/call | 115.92 | 115.50 | 111.52 | 125.35 |
| JS to C# | ns/callback | 6.69 | 6.08 | 14.07 | 7.64 |
| Arithmetic (10,000 iterations) | us/run | 14.08 | 13.29 | 63.80 | 11.93 |
| fib(20) | us/run | 50.72 | 47.22 | 401.14 | 77.30 |
| 1,000 objects | us/run | 23.86 | 23.24 | 58.51 | 5.49 |
| JSON, 128 records | us/run | 32.61 | 28.86 | 37.44 | 20.24 |

- **Source JIT:** function prepared from JS source and warmed up.
- **Bytecode + JIT:** recursive bytecode generated and accepted in setup;
  calls use the loaded function, with JIT warmup enabled.
- **Bytecode interpreter:** the same cache path with `useJIT=false`.
- **Native AOT:** functions compiled into a JSC native image before the run.
  The image is verified and loaded before VM creation; no source eval fallback.

Compilation, bytecode decoding, image hashing/loading, and function lookup are
outside the measured region. These are hot execution and call benchmarks,
not cache-generation or image-startup benchmarks. AOT uses the same JS function
bodies with an opaque host-object wrapper to install them into the native realm;
no extra TypeScript type annotations are added. Callbacks use the same C#
function pointer in all four modes.

The JSON/Markdown reports include deviation and confidence intervals. With only
three measured samples, small differences between warmed Source JIT and Bytecode
JIT can be noise; both ultimately run JIT code. Managed allocations exclude the
JSC native heap. This shared-machine sandbox could not query WMI processor info
or change the Windows power plan; CPU details came from the registry, and the
existing system load/power plan were not controlled.

## Full reports

- [Execution and C# to JS](RedJs.Benchmarks.RedJsExecutionBenchmarks-report-github.md)
  ([raw JSON](RedJs.Benchmarks.RedJsExecutionBenchmarks-report-full.json)).
- [JS to C#](RedJs.Benchmarks.RedJsCallbackModesBenchmarks-report-github.md)
  ([raw JSON](RedJs.Benchmarks.RedJsCallbackModesBenchmarks-report-full.json)).
- [Native-image provenance](images.json): source/image hashes, identities,
  entries, frontend, engine, builtin table, and compiler options.

## Reproduce

From `csharp/RedJs.Benchmarks/`, with the pinned toolchain paths:

```powershell
./build.ps1 -NativeAot -Frontend <pinned-bun.exe> -BuiltinTable <generated-table-directory>
dotnet run -c Release --no-build -- --filter '*RedJsExecutionBenchmarks*' '*RedJsCallbackModesBenchmarks*' --job short
```

Frontend: `7ca0042d3ba23ed60fd9cd8af1a1ce6ec3f8d074`.

Engine: `4d45755fed4aeef19e95beb90d3d0c884a2812bd`.

Builtin table: `4750b3540882158b35c1f7cc946edc1acb18561345bc70f9151fb913e59268b8`.

JIT library SHA-256: `aa047a486fd5e90d1bb706c4e75e84d009d71a03324d040750f8c294d8d6b5ef`.

AOT library SHA-256: `3d6722ad5483c56b100f4ea2eb961ff47717f049e877971e2ca4a0764f9f24d0`.
