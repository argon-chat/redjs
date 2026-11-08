```

BenchmarkDotNet v0.15.8, Windows 11 (10.0.29683.1000)
Unknown processor
.NET SDK 10.0.201
  [Host]     : .NET 10.0.5 (10.0.5, 10.0.526.15411), X64 RyuJIT x86-64-v4
  Job-GENPFV : .NET 10.0.5 (10.0.5, 10.0.526.15411), X64 RyuJIT x86-64-v4

InvocationCount=1  IterationCount=1  LaunchCount=5  
RunStrategy=ColdStart  UnrollFactor=1  WarmupCount=0  

```
| Method      | Mean      | Error     | StdDev     | Median    | Min       | Max       | Ratio | RatioSD |
|------------ |----------:|----------:|-----------:|----------:|----------:|----------:|------:|--------:|
| Redjs       |  77.92 ms | 508.84 ms | 132.144 ms |  18.84 ms |  17.40 ms | 314.29 ms |  3.38 |    6.00 |
| ClearScript |  89.73 ms | 384.10 ms |  99.749 ms |  47.61 ms |  40.44 ms | 268.02 ms |  3.89 |    4.74 |
| Jint        | 151.90 ms |  18.52 ms |   4.810 ms | 152.12 ms | 145.74 ms | 156.99 ms |  6.58 |    3.16 |
| YantraJS    | 112.06 ms |  56.65 ms |  14.712 ms | 108.86 ms |  93.88 ms | 128.22 ms |  4.86 |    2.41 |
| Jurassic    |  38.59 ms |  28.45 ms |   7.390 ms |  40.19 ms |  31.13 ms |  48.73 ms |  1.67 |    0.86 |
| NilJS       | 118.43 ms |  23.93 ms |   6.215 ms | 119.76 ms | 108.61 ms | 125.77 ms |  5.13 |    2.47 |
