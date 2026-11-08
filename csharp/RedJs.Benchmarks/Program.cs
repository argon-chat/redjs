using BenchmarkDotNet.Configs;
using BenchmarkDotNet.Exporters.Json;
using BenchmarkDotNet.Running;
using RedJs.Benchmarks;

if (args is ["--write-aot-sources", var directory])
{
    NativeImages.WriteAotSources(directory);
    return 0;
}

var config = DefaultConfig.Instance.AddExporter(JsonExporter.Full);
var summaries = BenchmarkSwitcher.FromAssembly(typeof(Program).Assembly).Run(args, config);
return summaries.Any(summary => summary.HasCriticalValidationErrors
    || summary.Reports.Any(report => !report.Success)) ? 1 : 0;
