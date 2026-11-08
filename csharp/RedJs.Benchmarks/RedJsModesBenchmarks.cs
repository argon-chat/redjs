using BenchmarkDotNet.Attributes;

namespace RedJs.Benchmarks;

[MemoryDiagnoser, MedianColumn]
public class RedJsExecutionBenchmarks : PreparedBenchmark
{
    [Params("cs-to-js", "arithmetic", "fibonacci", "objects", "json")]
    public string Scenario { get; set; } = "arithmetic";

    [GlobalSetup(Target = nameof(SourceJit))] public void SetupJit() => Setup("redjs", Scenario);
    [GlobalSetup(Target = nameof(BytecodeJit))] public void SetupBytecode() => Setup("redjs-bytecode", Scenario);
    [GlobalSetup(Target = nameof(BytecodeInterpreter))] public void SetupInterpreter() => Setup("redjs-interpreter", Scenario);
    [GlobalSetup(Target = nameof(NativeAot))] public void SetupAot() => Setup("redjs-aot", Scenario);

    [Benchmark(Baseline = true)] public double SourceJit() => Run();
    [Benchmark] public double BytecodeJit() => Run();
    [Benchmark] public double BytecodeInterpreter() => Run();
    [Benchmark] public double NativeAot() => Run();
}

[MemoryDiagnoser, MedianColumn]
public class RedJsCallbackModesBenchmarks : PreparedBenchmark
{
    [GlobalSetup(Target = nameof(SourceJit))] public void SetupJit() => Setup("redjs", "js-to-cs");
    [GlobalSetup(Target = nameof(BytecodeJit))] public void SetupBytecode() => Setup("redjs-bytecode", "js-to-cs");
    [GlobalSetup(Target = nameof(BytecodeInterpreter))] public void SetupInterpreter() => Setup("redjs-interpreter", "js-to-cs");
    [GlobalSetup(Target = nameof(NativeAot))] public void SetupAot() => Setup("redjs-aot", "js-to-cs");

    [Benchmark(Baseline = true, OperationsPerInvoke = 10_000)] public double SourceJit() => Run();
    [Benchmark(OperationsPerInvoke = 10_000)] public double BytecodeJit() => Run();
    [Benchmark(OperationsPerInvoke = 10_000)] public double BytecodeInterpreter() => Run();
    [Benchmark(OperationsPerInvoke = 10_000)] public double NativeAot() => Run();
}
