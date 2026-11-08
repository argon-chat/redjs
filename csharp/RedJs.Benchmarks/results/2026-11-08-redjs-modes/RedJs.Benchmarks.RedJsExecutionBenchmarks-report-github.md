```

BenchmarkDotNet v0.15.8, Windows 11 (10.0.29683.1000)
Unknown processor
.NET SDK 10.0.201
  [Host]   : .NET 10.0.5 (10.0.5, 10.0.526.15411), X64 RyuJIT x86-64-v4
  ShortRun : .NET 10.0.5 (10.0.5, 10.0.526.15411), X64 RyuJIT x86-64-v4

Job=ShortRun  IterationCount=3  LaunchCount=1  
WarmupCount=3  

```
| Method              | Scenario   | Mean         | Error         | StdDev       | Median       | Ratio | RatioSD | Allocated | Alloc Ratio |
|-------------------- |----------- |-------------:|--------------:|-------------:|-------------:|------:|--------:|----------:|------------:|
| **SourceJit**           | **arithmetic** |  **14,084.8 ns** |  **19,918.88 ns** |  **1,091.82 ns** |  **13,669.0 ns** |  **1.00** |    **0.09** |         **-** |          **NA** |
| BytecodeJit         | arithmetic |  13,286.4 ns |   6,048.51 ns |    331.54 ns |  13,160.9 ns |  0.95 |    0.06 |         - |          NA |
| BytecodeInterpreter | arithmetic |  63,799.5 ns |  20,886.29 ns |  1,144.85 ns |  64,126.7 ns |  4.55 |    0.30 |         - |          NA |
| NativeAot           | arithmetic |  11,931.7 ns |     840.50 ns |     46.07 ns |  11,918.1 ns |  0.85 |    0.06 |         - |          NA |
|                     |            |              |               |              |              |       |         |           |             |
| **SourceJit**           | **cs-to-js**   |     **115.9 ns** |      **21.21 ns** |      **1.16 ns** |     **116.1 ns** |  **1.00** |    **0.01** |         **-** |          **NA** |
| BytecodeJit         | cs-to-js   |     115.5 ns |       5.85 ns |      0.32 ns |     115.7 ns |  1.00 |    0.01 |         - |          NA |
| BytecodeInterpreter | cs-to-js   |     111.5 ns |      32.95 ns |      1.81 ns |     110.7 ns |  0.96 |    0.02 |         - |          NA |
| NativeAot           | cs-to-js   |     125.3 ns |      53.53 ns |      2.93 ns |     125.0 ns |  1.08 |    0.02 |         - |          NA |
|                     |            |              |               |              |              |       |         |           |             |
| **SourceJit**           | **fibonacci**  |  **50,719.6 ns** |  **74,022.05 ns** |  **4,057.40 ns** |  **48,708.8 ns** |  **1.00** |    **0.10** |         **-** |          **NA** |
| BytecodeJit         | fibonacci  |  47,219.0 ns |  16,952.34 ns |    929.21 ns |  47,504.7 ns |  0.93 |    0.06 |         - |          NA |
| BytecodeInterpreter | fibonacci  | 401,136.6 ns | 746,297.21 ns | 40,907.07 ns | 401,887.1 ns |  7.94 |    0.88 |         - |          NA |
| NativeAot           | fibonacci  |  77,297.5 ns | 220,148.58 ns | 12,067.09 ns |  72,683.6 ns |  1.53 |    0.23 |         - |          NA |
|                     |            |              |               |              |              |       |         |           |             |
| **SourceJit**           | **json**       |  **32,607.8 ns** |  **23,246.10 ns** |  **1,274.20 ns** |  **32,890.8 ns** |  **1.00** |    **0.05** |         **-** |          **NA** |
| BytecodeJit         | json       |  28,861.7 ns |   7,425.76 ns |    407.03 ns |  29,021.4 ns |  0.89 |    0.03 |         - |          NA |
| BytecodeInterpreter | json       |  37,441.4 ns |   7,133.97 ns |    391.04 ns |  37,503.5 ns |  1.15 |    0.04 |         - |          NA |
| NativeAot           | json       |  20,243.1 ns |   8,973.97 ns |    491.89 ns |  19,984.8 ns |  0.62 |    0.03 |         - |          NA |
|                     |            |              |               |              |              |       |         |           |             |
| **SourceJit**           | **objects**    |  **23,863.1 ns** |  **28,351.94 ns** |  **1,554.07 ns** |  **23,887.3 ns** |  **1.00** |    **0.08** |         **-** |          **NA** |
| BytecodeJit         | objects    |  23,238.2 ns |   6,852.63 ns |    375.62 ns |  23,207.5 ns |  0.98 |    0.06 |         - |          NA |
| BytecodeInterpreter | objects    |  58,510.2 ns |  24,141.32 ns |  1,323.27 ns |  58,499.7 ns |  2.46 |    0.15 |         - |          NA |
| NativeAot           | objects    |   5,493.7 ns |   2,545.03 ns |    139.50 ns |   5,422.6 ns |  0.23 |    0.01 |         - |          NA |
