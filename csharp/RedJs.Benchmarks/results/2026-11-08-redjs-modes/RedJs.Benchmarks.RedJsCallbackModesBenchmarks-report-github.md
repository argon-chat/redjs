```

BenchmarkDotNet v0.15.8, Windows 11 (10.0.29683.1000)
Unknown processor
.NET SDK 10.0.201
  [Host]   : .NET 10.0.5 (10.0.5, 10.0.526.15411), X64 RyuJIT x86-64-v4
  ShortRun : .NET 10.0.5 (10.0.5, 10.0.526.15411), X64 RyuJIT x86-64-v4

Job=ShortRun  IterationCount=3  LaunchCount=1  
WarmupCount=3  

```
| Method              | Mean      | Error    | StdDev    | Median    | Ratio | RatioSD | Allocated | Alloc Ratio |
|-------------------- |----------:|---------:|----------:|----------:|------:|--------:|----------:|------------:|
| SourceJit           |  6.691 ns | 2.433 ns | 0.1333 ns |  6.750 ns |  1.00 |    0.02 |         - |          NA |
| BytecodeJit         |  6.078 ns | 1.452 ns | 0.0796 ns |  6.124 ns |  0.91 |    0.02 |         - |          NA |
| BytecodeInterpreter | 14.065 ns | 9.966 ns | 0.5463 ns | 13.802 ns |  2.10 |    0.08 |         - |          NA |
| NativeAot           |  7.644 ns | 1.764 ns | 0.0967 ns |  7.602 ns |  1.14 |    0.02 |         - |          NA |
