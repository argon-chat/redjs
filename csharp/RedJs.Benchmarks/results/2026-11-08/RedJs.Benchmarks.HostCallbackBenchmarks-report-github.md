```

BenchmarkDotNet v0.15.8, Windows 11 (10.0.29683.1000)
Unknown processor
.NET SDK 10.0.201
  [Host]   : .NET 10.0.5 (10.0.5, 10.0.526.15411), X64 RyuJIT x86-64-v4
  ShortRun : .NET 10.0.5 (10.0.5, 10.0.526.15411), X64 RyuJIT x86-64-v4

Job=ShortRun  IterationCount=3  LaunchCount=1  
WarmupCount=3  

```
| Method          | Mean         | Error        | StdDev      | Median       | Ratio  | RatioSD | Gen0   | Gen1   | Allocated | Alloc Ratio |
|---------------- |-------------:|-------------:|------------:|-------------:|-------:|--------:|-------:|-------:|----------:|------------:|
| Redjs           |     6.491 ns |     1.190 ns |   0.0652 ns |     6.456 ns |   1.00 |    0.01 |      - |      - |         - |          NA |
| ClearScript     | 2,546.134 ns | 2,181.622 ns | 119.5821 ns | 2,523.878 ns | 392.30 |   16.31 | 0.6000 | 0.0031 |   10052 B |          NA |
| ClearScriptFast |   133.761 ns |    69.890 ns |   3.8309 ns |   135.646 ns |  20.61 |    0.54 |      - |      - |         - |          NA |
| Jint            |    98.189 ns |    33.895 ns |   1.8579 ns |    97.244 ns |  15.13 |    0.28 | 0.0043 |      - |      72 B |          NA |
| YantraJS        |    12.447 ns |    10.598 ns |   0.5809 ns |    12.327 ns |   1.92 |    0.08 | 0.0024 | 0.0000 |      40 B |          NA |
| Jurassic        |    42.729 ns |    17.368 ns |   0.9520 ns |    42.658 ns |   6.58 |    0.14 | 0.0091 |      - |     152 B |          NA |
| NilJS           |    70.055 ns |    13.519 ns |   0.7410 ns |    70.031 ns |  10.79 |    0.14 | 0.0147 |      - |     248 B |          NA |
