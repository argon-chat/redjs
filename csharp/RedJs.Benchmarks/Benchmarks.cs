using BenchmarkDotNet.Attributes;
using BenchmarkDotNet.Engines;

namespace RedJs.Benchmarks;

public abstract class PreparedBenchmark
{
    private IEngine? _engine;
    private Func<double> _operation = null!;

    protected void Setup(string engineName, string workloadName)
    {
        Setup(engineName, Workload.All.Single(w => w.Name == workloadName));
    }

    private protected void Setup(string engineName, Workload workload)
    {
        _engine = engineName == "redjs-aot" ? new RedJsAotEngine(workload.Name) : Engines.Create(engineName);
        if (workload.Name == "js-to-cs") _engine.RegisterHostAdd();
        if (workload.Name == "eval")
            _operation = () => _engine.Eval(workload.Source);
        else
        {
            _engine.Prepare(workload.Source);
            _operation = () => _engine.Call(workload.Argument);
        }

        // Fail before measurement if an adapter executes a different operation.
        double result = _operation();
        if (result != workload.Expected)
            throw new InvalidOperationException($"{engineName}/{workload.Name}: expected {workload.Expected}, got {result}");
    }

    protected double Run() => _operation();

    [GlobalCleanup]
    public void Cleanup() => _engine?.Dispose();
}

[MemoryDiagnoser, MedianColumn]
public class ThroughputBenchmarks : PreparedBenchmark
{
    [Params("eval", "cs-to-js", "arithmetic", "fibonacci", "objects", "json")]
    public string Scenario { get; set; } = "eval";

    [GlobalSetup(Target = nameof(Redjs))] public void SetupRedjs() => Setup("redjs", Scenario);
    [GlobalSetup(Target = nameof(ClearScript))] public void SetupClearScript() => Setup("clearscript", Scenario);
    [GlobalSetup(Target = nameof(Jint))] public void SetupJint() => Setup("jint", Scenario);
    [GlobalSetup(Target = nameof(YantraJS))] public void SetupYantra() => Setup("yantra", Scenario);
    [GlobalSetup(Target = nameof(Jurassic))] public void SetupJurassic() => Setup("jurassic", Scenario);
    [GlobalSetup(Target = nameof(NilJS))] public void SetupNil() => Setup("niljs", Scenario);

    [Benchmark(Baseline = true)] public double Redjs() => Run();
    [Benchmark] public double ClearScript() => Run();
    [Benchmark] public double Jint() => Run();
    [Benchmark] public double YantraJS() => Run();
    [Benchmark] public double Jurassic() => Run();
    [Benchmark] public double NilJS() => Run();
}

[MemoryDiagnoser, MedianColumn]
public class HostCallbackBenchmarks : PreparedBenchmark
{
    [GlobalSetup(Target = nameof(Redjs))] public void SetupRedjs() => Setup("redjs", "js-to-cs");
    [GlobalSetup(Target = nameof(ClearScript))] public void SetupClearScript() => Setup("clearscript", "js-to-cs");
    [GlobalSetup(Target = nameof(ClearScriptFast))] public void SetupClearScriptFast() => Setup("clearscript-fast", "js-to-cs");
    [GlobalSetup(Target = nameof(Jint))] public void SetupJint() => Setup("jint", "js-to-cs");
    [GlobalSetup(Target = nameof(YantraJS))] public void SetupYantra() => Setup("yantra", "js-to-cs");
    [GlobalSetup(Target = nameof(Jurassic))] public void SetupJurassic() => Setup("jurassic", "js-to-cs");
    [GlobalSetup(Target = nameof(NilJS))] public void SetupNil() => Setup("niljs", "js-to-cs");

    [Benchmark(Baseline = true, OperationsPerInvoke = 10_000)] public double Redjs() => Run();
    [Benchmark(OperationsPerInvoke = 10_000)] public double ClearScript() => Run();
    [Benchmark(OperationsPerInvoke = 10_000)] public double ClearScriptFast() => Run();
    [Benchmark(OperationsPerInvoke = 10_000)] public double Jint() => Run();
    [Benchmark(OperationsPerInvoke = 10_000)] public double YantraJS() => Run();
    [Benchmark(OperationsPerInvoke = 10_000)] public double Jurassic() => Run();
    [Benchmark(OperationsPerInvoke = 10_000)] public double NilJS() => Run();
}

// One construction + first eval per fresh process. Never repeatedly allocate
// redjs contexts in a throughput loop: its pinned engine cannot tear down a VM.
[SimpleJob(RunStrategy.ColdStart, launchCount: 5, warmupCount: 0, iterationCount: 1, invocationCount: 1)]
[MinColumn, MaxColumn, MedianColumn]
public class ColdStartBenchmarks
{
    private IEngine? _engine;
    private double _result;

    private double Start(string engineName)
    {
        _engine = Engines.Create(engineName);
        return _result = _engine.Eval("6 * 7");
    }

    [Benchmark(Baseline = true)] public double Redjs() => Start("redjs");
    [Benchmark] public double ClearScript() => Start("clearscript");
    [Benchmark] public double Jint() => Start("jint");
    [Benchmark] public double YantraJS() => Start("yantra");
    [Benchmark] public double Jurassic() => Start("jurassic");
    [Benchmark] public double NilJS() => Start("niljs");

    [GlobalCleanup]
    public void Validate()
    {
        if (_result != 42) throw new InvalidOperationException($"Expected 42, got {_result}");
        GC.KeepAlive(_engine); // Destruction is excluded for every engine.
    }
}
