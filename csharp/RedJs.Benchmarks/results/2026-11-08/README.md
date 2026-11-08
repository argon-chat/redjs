# Recorded comparison: 2026-11-08

Means from BenchmarkDotNet 0.15.8 on an AMD Ryzen 9 9950X (32 logical processors),
Windows 11 10.0.29683.1000, .NET SDK 10.0.201 / runtime 10.0.5, x64 RyuJIT.
All C# hosts use the .NET JIT. This run does not compare NativeAOT hosts.

The 43 steady-state cases use ShortRun: one launch, three warmups, and three
measured iterations. Cold start uses five launches with one measured invocation
each and no warmup. All setup/result checks passed. Lower elapsed time is better.

| Scenario | Unit | redjs | ClearScript/V8 | Jint | YantraJS | Jurassic | NiL.JS |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Evaluate source | ns/eval | 521.04 | 6,680.67 | 617.54 | 89.29 | 28,345.97 | 1,198.87 |
| C# to JS | ns/call | 112.48 | 889.49 | 80.44 | 117.65 | 29.52 | 75.75 |
| JS to C# | ns/callback | 6.49 | 2,546.13 | 98.19 | 12.45 | 42.73 | 70.06 |
| Arithmetic (10,000 iterations) | us/run | 13.55 | 105.80 | 424.35 | 291.11 | 229.91 | 245.01 |
| fib(20) | us/run | 44.05 | 305.57 | 3,624.08 | 1,614.59 | 961.34 | 2,307.51 |
| 1,000 objects | us/run | 25.33 | 5.34 | 139.31 | 94.52 | 318.69 | 365.10 |
| JSON, 128 records | us/run | 43.38 | 47.47 | 104.63 | 187.29 | 247.78 | 863.09 |
| First engine + eval | ms/start | 77.92 | 89.73 | 151.90 | 112.06 | 38.59 | 118.43 |

ClearScript's FastProxy variant measured **133.76 ns/callback** for JS to C#.
The ClearScript column above uses its ordinary delegate binding. Full callback
reports include both variants and managed allocations.

These are development measurements from a shared machine. BenchmarkDotNet
could not query WMI processor details or select its usual power plan in the
sandbox; the CPU name was read from the Windows registry. System load and the
existing power plan were not controlled. ShortRun has only three measured
samples: consult the full reports' error and standard deviation columns before
interpreting small differences. The suite README describes a longer run.

## Full reports

- [Execution and C# to JS](RedJs.Benchmarks.ThroughputBenchmarks-report-github.md)
  ([raw JSON](RedJs.Benchmarks.ThroughputBenchmarks-report-full.json)).
- [JS to C# callbacks](RedJs.Benchmarks.HostCallbackBenchmarks-report-github.md)
  ([raw JSON](RedJs.Benchmarks.HostCallbackBenchmarks-report-full.json)).
- [First use](RedJs.Benchmarks.ColdStartBenchmarks-report-github.md)
  ([raw JSON](RedJs.Benchmarks.ColdStartBenchmarks-report-full.json)).

`Allocated` covers managed memory only; it excludes JSC and V8 native heaps.
The callback unit includes JS loop work and numeric argument/result conversion.

## Reproduce

From `csharp/RedJs.Benchmarks/`, after `./build.ps1`:

```powershell
dotnet run -c Release --no-build -- --filter '*ThroughputBenchmarks*' '*HostCallbackBenchmarks*' --job short
dotnet run -c Release --no-build -- --filter '*ColdStartBenchmarks*'
```

Dependency versions are in [the project](../../RedJs.Benchmarks.csproj).
The native library was built in Release with default features disabled and no
web globals installed. JavaScriptCore pin: `4d45755fed4aeef19e95beb90d3d0c884a2812bd`.
Base repository revision: `5f2c2a738e3fb631967bfc1eecebeab3098a0c59`; this benchmark suite was an uncommitted
addition during the run.

Native DLL SHA-256: `cc3aa31501f6aca90ed179af313b89ab617fa48991af5e5cfc28af141b1e4126`.
