using System.Globalization;
using System.Runtime.InteropServices;
using Jint;
using Microsoft.ClearScript;
using Microsoft.ClearScript.V8;
using Microsoft.ClearScript.V8.FastProxy;
using YantraJS.Core;

namespace RedJs.Benchmarks;

internal interface IEngine : IDisposable
{
    double Eval(string source);
    void RegisterHostAdd();
    void Prepare(string source);
    double Call(double argument);
}

internal static class Engines
{
    internal static readonly string[] Names =
        ["redjs", "clearscript", "clearscript-fast", "jint", "yantra", "jurassic", "niljs"];

    internal static IEngine Create(string name) => name switch
    {
        "redjs" => new RedJsEngine(),
        "redjs-bytecode" => new RedJsBytecodeEngine(false),
        "redjs-interpreter" => new RedJsBytecodeEngine(true),
        "clearscript" => new ClearScriptEngine(false),
        "clearscript-fast" => new ClearScriptEngine(true),
        "jint" => new JintEngine(),
        "yantra" => new YantraEngine(),
        "jurassic" => new JurassicEngine(),
        "niljs" => new NilEngine(),
        _ => throw new ArgumentException($"Unknown engine: {name}")
    };
}

internal sealed class RedJsBytecodeEngine(bool interpreter) : IEngine
{
    private readonly JsContext _context = JsContext.Create(interpreter ? "useJIT=false" : null);
    private CachedScript? _script;
    private JsValue _function;

    public double Eval(string source) => throw new NotSupportedException("Use the prepared bytecode function");
    public unsafe void RegisterHostAdd() => _context.SetGlobalFunction("hostAdd", 2, &RedJsEngine.HostAdd);
    public void Prepare(string source)
    {
        _script = new CachedScript(_context, source, "bench://prepared.js");
        _function = _script.Evaluate();
        _context.Protect(_function);
    }
    public double Call(double argument) => _context.Call(_function, JsValue.Undefined, [JsValue.From(argument)]).AsNumber;
    public void Dispose()
    {
        if (_function.Raw != 0) _context.Unprotect(_function);
        _script?.Dispose();
    }
}

internal sealed class RedJsAotEngine : IEngine
{
    private readonly JsContext _context;
    private readonly uint _entry;
    private JsValue _function;

    internal RedJsAotEngine(string workloadName)
    {
        _entry = NativeImages.LoadAot(Workload.All.Single(w => w.Name == workloadName));
        _context = JsContext.Create();
    }
    public double Eval(string source) => throw new NotSupportedException("Native AOT executes a precompiled image, not dynamic eval");
    public unsafe void RegisterHostAdd() => _context.SetGlobalFunction("hostAdd", 2, &RedJsEngine.HostAdd);
    public void Prepare(string source)
    {
        _context.Set(_context.Global, "__REDJS_BENCH_HOST__", _context.Global);
        NativeImages.Check(_context, NativeImages.Run(_context.Handle, _entry, out ulong result), result);
        if (NativeImages.IsAotActive() != 1) throw new InvalidOperationException("Native AOT was not selected");
        _function = _context.Get(_context.Global, "__redjs_bench");
        _context.Protect(_function);
    }
    public double Call(double argument) => _context.Call(_function, JsValue.Undefined, [JsValue.From(argument)]).AsNumber;
    public void Dispose()
    {
        if (_function.Raw != 0) _context.Unprotect(_function);
    }
}

internal sealed unsafe class RedJsEngine : IEngine
{
    private readonly JsContext _context = JsContext.Create();
    private JsValue _function;

    public double Eval(string source) => _context.Eval(source).AsNumber;
    public void RegisterHostAdd() => _context.SetGlobalFunction("hostAdd", 2, &HostAdd);
    public void Prepare(string source)
    {
        _function = _context.Eval(source);
        _context.Protect(_function);
    }
    public double Call(double argument)
    {
        ReadOnlySpan<JsValue> arguments = [JsValue.From(argument)];
        return _context.Call(_function, JsValue.Undefined, arguments).AsNumber;
    }
    public void Dispose()
    {
        if (_function.Raw != 0) _context.Unprotect(_function);
        // The pinned JSC crashes in VM teardown. The worker owns the VM until exit.
    }

    [UnmanagedCallersOnly]
    internal static ulong HostAdd(nint global, nint callFrame)
    {
        var args = new JsCallFrame(callFrame);
        return JsValue.From(args.Arg(0).AsNumber + args.Arg(1).AsNumber).Raw;
    }
}

internal sealed class ClearScriptEngine(bool fast) : IEngine
{
    private readonly V8ScriptEngine _engine = new();
    private ScriptObject? _function;

    public double Eval(string source) => Convert.ToDouble(_engine.Evaluate(source), CultureInfo.InvariantCulture);
    public void RegisterHostAdd()
    {
        if (fast)
            _engine.AddHostObject("hostAdd", new V8FastHostFunction("hostAdd", 2,
                (bool asConstructor, in V8FastArgs args, in V8FastResult result) =>
                    result.Set(args.GetDouble(0) + args.GetDouble(1))));
        else
            _engine.AddHostObject("hostAdd", new Func<double, double, double>((a, b) => a + b));
    }
    public void Prepare(string source) => _function = (ScriptObject)_engine.Evaluate(source);
    public double Call(double argument) => Convert.ToDouble(_function!.Invoke(false, argument), CultureInfo.InvariantCulture);
    public void Dispose()
    {
        _function?.Dispose();
        _engine.Dispose();
    }
}

internal sealed class JintEngine : IEngine
{
    private readonly Engine _engine = new();
    private Jint.Native.JsValue _function = Jint.Native.JsValue.Undefined;

    public double Eval(string source) => _engine.Evaluate(source).AsNumber();
    public void RegisterHostAdd() => _engine.SetValue("hostAdd", new Func<double, double, double>((a, b) => a + b));
    public void Prepare(string source) => _function = _engine.Evaluate(source);
    public double Call(double argument) => _engine.Invoke(_function, Jint.Native.JsValue.Undefined,
        [Jint.Native.JsNumber.Create(argument)]).AsNumber();
    public void Dispose() => _engine.Dispose();
}

internal sealed class YantraEngine : IEngine
{
    private readonly JSContext _context = new();
    private JSValue _function = JSUndefined.Value;

    public double Eval(string source) => _context.Eval(source).DoubleValue;
    public void RegisterHostAdd() => _context[new JSString("hostAdd")] = new JSFunction(
        (in Arguments args) => new JSNumber(args[0]!.DoubleValue + args[1]!.DoubleValue));
    public void Prepare(string source) => _function = _context.Eval(source);
    public double Call(double argument)
    {
        var args = new Arguments(JSUndefined.Value, new JSNumber(argument));
        return _function.InvokeFunction(in args).DoubleValue;
    }
    public void Dispose() => _context.Dispose();
}

internal sealed class JurassicEngine : IEngine
{
    private readonly Jurassic.ScriptEngine _engine = new();
    private Jurassic.Library.FunctionInstance? _function;

    public double Eval(string source) => Jurassic.TypeConverter.ToNumber(_engine.Evaluate(source));
    public void RegisterHostAdd() => _engine.SetGlobalFunction("hostAdd", new Func<double, double, double>((a, b) => a + b));
    public void Prepare(string source) => _function = (Jurassic.Library.FunctionInstance)_engine.Evaluate(source);
    public double Call(double argument) => Jurassic.TypeConverter.ToNumber(_function!.Call(Jurassic.Undefined.Value, argument));
    public void Dispose() { }
}

internal sealed class NilEngine : IEngine
{
    private readonly NiL.JS.Core.Context _context = new();
    private NiL.JS.BaseLibrary.Function? _function;

    public double Eval(string source) => Convert.ToDouble(_context.Eval(source).Value, CultureInfo.InvariantCulture);
    public void RegisterHostAdd() => _context.DefineVariable("hostAdd").Assign(
        (NiL.JS.Core.JSValue)new Func<double, double, double>((a, b) => a + b));
    public void Prepare(string source) => _function = (NiL.JS.BaseLibrary.Function)_context.Eval(source).Value;
    public double Call(double argument)
    {
        var args = new NiL.JS.Core.Arguments();
        args.Add((NiL.JS.Core.JSValue)argument);
        return Convert.ToDouble(_function!.Call(NiL.JS.Core.JSValue.Undefined, args).Value, CultureInfo.InvariantCulture);
    }
    public void Dispose() { }
}
