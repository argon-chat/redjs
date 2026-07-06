// C# conformance smoke for the RedJs layer — mirrors redjs/tests/smoke.rs
// through the real bindings, exactly the way the player will use them.
// Run JIT:  dotnet run
// Run AOT:  dotnet publish -r win-x64 -c Release -p:PublishAot=true && run the exe

using System.Collections.Concurrent;
using System.Diagnostics;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using RedJs;

internal static unsafe class Program
{
    private static JsContext _ctx = null!;
    private static int _checks;

    private static void Check(bool ok, string what)
    {
        _checks++;
        if (!ok) throw new Exception($"FAILED: {what}");
        Console.WriteLine($"  ok: {what}");
    }

    private static int Main()
    {
        try
        {
            _ctx = JsContext.Create();

            EvalAndValues();
            Strings();
            Properties();
            HostFunctions();
            CallsAndErrors();
            TypedArrays();
            ExternalBuffers();
            Promises();
            HostObjects();
            GcAndStats();

            Console.WriteLine($"\nRedJs C# smoke: OK ({_checks} checks, " +
                (RuntimeFeature.IsDynamicCodeSupported ? "JIT" : "NativeAOT") + ")");
            return 0;
        }
        catch (Exception ex)
        {
            Console.Error.WriteLine($"\nRedJs C# smoke FAILED: {ex}");
            return 1;
        }
    }

    private static void EvalAndValues()
    {
        Console.WriteLine("[eval + values]");
        var v = _ctx.Eval("6 * 7");
        Check(v.IsInt32 && v.AsInt32 == 42, "eval int32");

        v = _ctx.Eval("0.5 + 1");
        Check(!v.IsInt32 && v.IsNumber && v.AsNumber == 1.5, "eval double");

        Check(_ctx.Eval("true").AsBoolean, "eval bool");
        Check(_ctx.Eval("null").IsNull, "eval null");
        Check(_ctx.Eval("undefined").IsUndefined, "eval undefined");
        Check(_ctx.Eval("undefined").IsNullish && _ctx.Eval("null").IsNullish, "IsNullish");

        // Encode side: values into JS through Call.
        var fn = _ctx.Eval("(function(a, b) { return a + b; })");
        var sum = _ctx.Call(fn, JsValue.Undefined, [JsValue.From(40), JsValue.From(2.5)]);
        Check(sum.AsNumber == 42.5, "encode int32+double into call");

        Check(_ctx.KindOf(_ctx.Eval("[1,2]")) == JsValueKind.Array, "KindOf array");
        Check(_ctx.KindOf(_ctx.Eval("new Float32Array(2)")) == JsValueKind.TypedArray, "KindOf typed array");
        Check(_ctx.KindOf(_ctx.Eval("Promise.resolve(1)")) == JsValueKind.Promise, "KindOf promise");
    }

    private static void Strings()
    {
        Console.WriteLine("[strings]");
        var s = _ctx.CreateString("hello");
        Check(_ctx.GetString(s) == "hello", "latin-1 round trip");

        s = _ctx.CreateString("привет 🎮");
        Check(_ctx.GetString(s) == "привет 🎮", "utf-16 round trip");

        var rope = _ctx.Eval("'a'.repeat(1000) + 'b'.repeat(1000)");
        var str = _ctx.GetString(rope);
        Check(str.Length == 2000 && str[0] == 'a' && str[^1] == 'b', "rope resolve");

        Check(_ctx.GetStringCoerced(JsValue.From(42)) == "42", "ToString coercion");
    }

    private static void Properties()
    {
        Console.WriteLine("[properties]");
        var obj = _ctx.CreateObject();
        _ctx.Set(obj, "x", JsValue.From(10));
        _ctx.Define(obj, "y", _ctx.CreateString("val"), attributes: 1);
        Check(_ctx.Get(obj, "x").AsInt32 == 10, "set/get");
        Check(_ctx.GetString(_ctx.Get(obj, "y")) == "val", "define/get");

        _ctx.Define(_ctx.Global, "testObj", obj);
        Check(_ctx.GetStringCoerced(_ctx.Eval("testObj.x + testObj.y")) == "10val", "global + JS sees host props");

        // Descriptor-parse pattern (the GPUDevice path): cached atoms + NumberOr.
        var desc = _ctx.Eval("({ size: 256, usage: 0x28, origin: [1,2,3] })");
        Check(_ctx.Get(desc, "size").UInt64Or(0) == 256, "descriptor size");
        Check(_ctx.Get(desc, "usage").UInt32Or(0) == 0x28, "descriptor usage");
        Check(_ctx.Get(desc, "mipLevel").UInt32Or(7) == 7, "missing prop -> default");
        var origin = _ctx.Get(desc, "origin");
        Check(_ctx.GetIndex(origin, 2).AsInt32 == 3, "indexed read");

        var arr = _ctx.CreateArray(3);
        _ctx.SetIndex(arr, 0, JsValue.From(1));
        _ctx.SetIndex(arr, 2, JsValue.From(3));
        Check(_ctx.ArrayLength(arr) == 3, "array length");
    }

    [UnmanagedCallersOnly]
    private static ulong HostAdd(nint g, nint cf)
    {
        var frame = new JsCallFrame(cf);
        return JsValue.From(frame.Arg(0).NumberOr(0) + frame.Arg(1).NumberOr(0)).Raw;
    }

    [UnmanagedCallersOnly]
    private static ulong HostProbe(nint g, nint cf)
    {
        var frame = new JsCallFrame(cf);
        bool ok = frame.ArgCount == 3
            && frame.Arg(0).AsInt32 == 11
            && frame.Arg(2).AsInt32 == 33
            && frame.Arg(5).IsUndefined
            && frame.This.IsCell;
        return JsValue.From(ok).Raw;
    }

    [UnmanagedCallersOnly]
    private static ulong HostWithData(nint g, nint cf, nint data)
        => JsValue.From((int)data).Raw;

    private static void HostFunctions()
    {
        Console.WriteLine("[host functions]");
        _ctx.SetGlobalFunction("hostAdd", 2, &HostAdd);
        Check(_ctx.Eval("hostAdd(20, 22.5)").AsNumber == 42.5, "C# host fn via trampoline");

        // Hot loop through JIT tiers — validates the SysV trampoline + CallFrame
        // reads under DFG/FTL, and gives a boundary number for this machine.
        var sw = Stopwatch.StartNew();
        var r = _ctx.Eval("let s = 0; for (let i = 0; i < 2000000; i++) s = hostAdd(s, 1); s");
        sw.Stop();
        Check(r.AsNumber == 2_000_000, "2M-call hot loop");
        Console.WriteLine($"  js->C# boundary: {sw.Elapsed.TotalNanoseconds / 2_000_000:f1} ns/call");

        _ctx.SetGlobalFunction("probe", 3, &HostProbe);
        Check(_ctx.Eval("probe.call({o:1}, 11, 22, 33)").AsBoolean, "CallFrame layout contract");

        var f = _ctx.CreateFunction("withData", 0, &HostWithData, 777);
        _ctx.Define(_ctx.Global, "withData", f);
        Check(_ctx.Eval("withData()").AsInt32 == 777, "host fn with data pointer");
    }

    [UnmanagedCallersOnly]
    private static ulong HostThrow(nint g, nint cf)
        => _ctx.ThrowTypeError("host says no");

    private static void CallsAndErrors()
    {
        Console.WriteLine("[calls + errors]");
        var obj = _ctx.Eval("({ n: 40, add(x) { return this.n + x; } })");
        var add = _ctx.Get(obj, "add");
        Check(_ctx.Call(add, obj, [JsValue.From(2)]).AsInt32 == 42, "call with this");

        var ctor = _ctx.Eval("(class P { constructor(x) { this.x = x; } })");
        var p = _ctx.Construct(ctor, [JsValue.From(5)]);
        Check(_ctx.Get(p, "x").AsInt32 == 5, "construct");

        try
        {
            _ctx.Eval("throw new Error('boom')", "game/main.js");
            Check(false, "should have thrown");
        }
        catch (RedJsException ex)
        {
            Check(ex.Message.StartsWith("boom") && ex.JsStack.Contains("game/main.js"),
                "JS exception -> RedJsException with source in stack");
        }

        _ctx.SetGlobalFunction("hostThrow", 0, &HostThrow);
        var caught = _ctx.Eval("(() => { try { hostThrow(); return 'bad'; } catch (e) { return e instanceof TypeError ? e.message : 'wrong'; } })()");
        Check(_ctx.GetString(caught) == "host says no", "C# throw -> JS catch");
        Check(_ctx.Eval("1+1").AsInt32 == 2, "VM clean after host throw");
    }

    private static void TypedArrays()
    {
        Console.WriteLine("[typed arrays]");
        var ta = _ctx.Eval("(() => { const a = new Float32Array(8); for (let i = 0; i < 8; i++) a[i] = i * 1.5; return a; })()");
        Check(_ctx.TryGetTypedArrayInfo(ta, out var info), "typedarray_info");
        Check(info.Kind == JsTypedArrayKind.Float32 && info.Length == 8 && info.ByteLength == 32, "info fields");

        // THE writeBuffer path: read JS memory directly, zero copies.
        var floats = MemoryMarshal.Cast<byte, float>(_ctx.GetTypedArrayBytes(ta));
        Check(floats[4] == 6.0f, "direct span read of JS Float32Array");

        // Write back through the same span; JS sees it.
        floats[0] = 123f;
        Check(_ctx.Eval("ta0 = undefined") is var _ && true, "noop"); // keep simple
        _ctx.Define(_ctx.Global, "hostTa", ta);
        Check((float)_ctx.Eval("hostTa[0]").AsNumber == 123f, "span write visible to JS");

        var mine = _ctx.CreateTypedArray(JsTypedArrayKind.Uint8, 4);
        var bytes = _ctx.GetTypedArrayBytes(mine);
        bytes[0] = 7;
        bytes[3] = 9;
        _ctx.Define(_ctx.Global, "hostBytes", mine);
        Check(_ctx.Eval("hostBytes[0] + hostBytes[3]").AsInt32 == 16, "host-created typed array");
    }

    private static void ExternalBuffers()
    {
        Console.WriteLine("[external buffers]");
        // getMappedRange semantics: JS ArrayBuffer VIEWING host memory.
        var host = (byte*)NativeMemory.Alloc(64);
        new Span<byte>(host, 64).Clear();

        var ab = _ctx.CreateExternalArrayBuffer((nint)host, 64);
        _ctx.Define(_ctx.Global, "mapped", ab);
        _ctx.Eval("new Uint8Array(mapped).set([7, 8, 9], 4)");
        Check(host[4] == 7 && host[6] == 9, "JS write lands in host memory (zero copy)");

        // unmap: detach → views die, host memory stays ours.
        _ctx.DetachArrayBuffer(ab);
        Check(_ctx.Eval("mapped.byteLength").AsInt32 == 0, "detached buffer is dead to JS");
        NativeMemory.Free(host);

        Check(_ctx.TryGetArrayBufferInfo(_ctx.Eval("new ArrayBuffer(128)"), out var abi) && abi.ByteLength == 128, "arraybuffer_info");
    }

    private static void Promises()
    {
        Console.WriteLine("[promises]");
        var promise = _ctx.CreateDeferredPromise(out var resolve, out _);
        _ctx.Define(_ctx.Global, "hostPromise", promise);
        _ctx.Eval("globalThis.observed = 'pending'; hostPromise.then(v => { observed = 'got:' + v; })");

        _ctx.Call(resolve, JsValue.Undefined, [JsValue.From(99)]);
        _ctx.DrainMicrotasks();
        Check(_ctx.GetString(_ctx.Eval("observed")) == "got:99", "deferred resolve + drain");

        Check(_ctx.GetPromiseState(promise, out var result) == JsPromiseState.Fulfilled && result.AsInt32 == 99,
            "promise state poll");

        var p = _ctx.Eval("(async () => (await Promise.resolve(20)) + 22)()");
        _ctx.Protect(p);
        _ctx.DrainMicrotasks();
        Check(_ctx.GetPromiseState(p, out var res) == JsPromiseState.Fulfilled && res.AsInt32 == 42,
            "async fn -> poll (Promise->Task seam)");
        _ctx.Unprotect(p);
    }

    private static readonly ConcurrentQueue<nint> _finalized = new();

    [UnmanagedCallersOnly]
    private static void OnFinalize(nint data) => _finalized.Enqueue(data);

    [UnmanagedCallersOnly]
    private static ulong HostHandleGetter(nint g, nint cf)
    {
        var frame = new JsCallFrame(cf);
        return JsValue.From((int)JsContext.GetHostObjectData(frame.This)).Raw;
    }

    private static void HostObjects()
    {
        Console.WriteLine("[host objects]");
        JsContext.SetHostFinalizer(&OnFinalize);
        var cls = JsContext.CreateHostClass("GPUBuffer\0"u8);

        var proto = _ctx.CreateObject();
        _ctx.Define(proto, "handle", _ctx.CreateFunction("handle", 0, &HostHandleGetter));

        var obj = _ctx.CreateHostObject(cls, 0xBEEF, proto);
        _ctx.Define(_ctx.Global, "gpuBuf", obj);
        Check(JsContext.GetHostObjectData(_ctx.Eval("(() => gpuBuf)()")) == 0xBEEF, "handle round trip through JS");
        Check(_ctx.Eval("gpuBuf.handle()").AsInt32 == 0xBEEF, "prototype method reads handle from this");
        Check(JsContext.GetHostObjectData(_ctx.CreateObject()) == 0, "non-host object -> 0");

        MakeDeadHostObjects(cls);
        for (int i = 0; i < 8 && _finalized.IsEmpty; i++) _ctx.Gc();
        Check(!_finalized.IsEmpty, "finalizer enqueued handles of dead host objects");
    }

    private static void MakeDeadHostObjects(nint cls)
    {
        for (int i = 0; i < 4; i++)
            _ctx.CreateHostObject(cls, 25);
    }

    private static void GcAndStats()
    {
        Console.WriteLine("[gc + stats]");
        var v = _ctx.CreateString("survives");
        _ctx.Protect(v);
        _ctx.Eval("for (let i = 0; i < 1000; i++) ({ junk: 'x'.repeat(100) })");
        _ctx.Gc();
        Check(_ctx.GetString(v) == "survives", "protect keeps value across GC");
        _ctx.Unprotect(v);

        var stats = _ctx.GetHeapStats();
        Check(stats.HeapCapacity > 0 && stats.ObjectCount > 0, "heap stats");

        using (_ctx.Lock())
        {
            Check(_ctx.Eval("2 ** 10").AsInt32 == 1024, "batched under explicit lock");
        }
    }
}
