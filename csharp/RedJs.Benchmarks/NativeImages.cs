using System.Reflection;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using Microsoft.Win32.SafeHandles;

namespace RedJs.Benchmarks;

internal static unsafe class NativeImages
{
    static NativeImages() => NativeLibrary.SetDllImportResolver(typeof(NativeImages).Assembly, Resolve);

    private static nint Resolve(string name, Assembly assembly, DllImportSearchPath? searchPath)
    {
        if (name != "redjs") return 0;
        string path = Environment.GetEnvironmentVariable("REDJS_NATIVE_LIBRARY")
            ?? Path.Combine(AppContext.BaseDirectory, "runtimes/win-x64/native/redjs.dll");
        return NativeLibrary.Load(path);
    }

    internal static string AotSource(Workload workload) =>
        "(function(host) { 'use strict'; var hostAdd = host.hostAdd;\n"
        + "host.__redjs_bench = " + workload.Source + ";\n})(__REDJS_BENCH_HOST__);\n";

    internal static void WriteAotSources(string directory)
    {
        Directory.CreateDirectory(directory);
        foreach (var workload in Workload.All.Where(w => w.Name is not "eval" and not "cold-start"))
            File.WriteAllText(Path.Combine(directory, workload.Name + ".js"), AotSource(workload), new UTF8Encoding(false));
    }

    internal static uint LoadAot(Workload workload)
    {
        string library = Path.Combine(AppContext.BaseDirectory, "runtimes/win-x64/native-aot/redjs.dll");
        if (!File.Exists(library)) throw new FileNotFoundException("Build with ./build.ps1 -NativeAot -Frontend <bun.exe> -BuiltinTable <table>", library);
        Environment.SetEnvironmentVariable("REDJS_NATIVE_LIBRARY", library);

        nuint size = BuildInfo(null, 0);
        if (size == 0) throw new InvalidOperationException("Native library has no native-aot feature");
        byte[] infoBytes = new byte[checked((int)size)];
        fixed (byte* data = infoBytes) BuildInfo(data, size);
        using var info = JsonDocument.Parse(infoBytes);
        string path = Path.Combine(AppContext.BaseDirectory, "native-images", workload.Name + ".image");
        using var manifest = JsonDocument.Parse(File.ReadAllBytes(path + ".json"));
        var metadata = manifest.RootElement;
        if (metadata.GetProperty("schema").GetString() != "redjs-benchmark-native-v1")
            throw new InvalidDataException("Unrecognized benchmark image manifest");
        foreach (string key in new[] { "frontend", "engine", "tableId", "compileOptions" })
            if (metadata.GetProperty("build").GetProperty(key).GetString() != info.RootElement.GetProperty(key).GetString())
                throw new InvalidDataException($"AOT {key} differs from this native library");
        string sourceHash = Convert.ToHexStringLower(SHA256.HashData(Encoding.UTF8.GetBytes(AotSource(workload))));
        if (metadata.GetProperty("sourceSha256").GetString() != sourceHash)
            throw new InvalidDataException("AOT workload source differs; rebuild its image");

        // Validate and load the SAME file object. Retained duplicate handles
        // preserve deny-write sharing after this stream and caller handle close.
        using var file = CreateFile(path, 0x80000000u | 0x20000000u, 1 | 4, 0, 3, 0x80, 0);
        if (file.IsInvalid) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
        using var stream = new FileStream(file, FileAccess.Read);
        string hash = Convert.ToHexStringLower(SHA256.HashData(stream));
        if (metadata.GetProperty("imageSha256").GetString() != hash)
            throw new InvalidDataException("AOT image digest differs from its trusted local manifest");
        byte[] identity = Convert.FromHexString(metadata.GetProperty("identity").GetString()!);
        if (identity.Length != 16) throw new InvalidDataException("Invalid AOT identity");
        fixed (byte* id = identity)
            if (LoadHandle(file.DangerousGetHandle(), id, 0) != 1)
                throw new InvalidOperationException("AOT loader rejected the verified native image");
        if (IsAotActive() != 1) throw new InvalidOperationException("AOT backend is not active");
        uint entry = metadata.GetProperty("entry").GetUInt32();
        if (entry == 0) throw new InvalidDataException("Invalid AOT entry");
        return entry;
    }

    internal static void Check(JsContext context, int status, ulong value)
    {
        if (status == 0) return;
        if (status == 1) throw new InvalidOperationException(context.GetStringCoerced(new JsValue(value)));
        throw new InvalidOperationException($"Compiled-script operation returned status {status}");
    }

    [DllImport("redjs", EntryPoint = "redjs_bytecode_generate", CallingConvention = CallingConvention.Cdecl)]
    internal static extern int Generate(nint context, byte* source, nuint length, byte* url, nuint urlLength,
        uint depth, out nint handle, out nint bytes, out nuint byteLength, out ulong result);
    [DllImport("redjs", EntryPoint = "redjs_bytecode_free", CallingConvention = CallingConvention.Cdecl)]
    internal static extern void Free(nint handle);
    [DllImport("redjs", EntryPoint = "redjs_eval8_cached", CallingConvention = CallingConvention.Cdecl)]
    internal static extern int EvalCached(nint context, byte* source, nuint length, byte* url, nuint urlLength,
        nint bytes, nuint byteLength, out uint used, out ulong result);
    [DllImport("redjs", EntryPoint = "redjs_aot_build_info", CallingConvention = CallingConvention.Cdecl)]
    private static extern nuint BuildInfo(byte* destination, nuint capacity);
    [DllImport("redjs", EntryPoint = "redjs_aot_load_handle", CallingConvention = CallingConvention.Cdecl)]
    private static extern uint LoadHandle(nint file, byte* identity, uint profile);
    [DllImport("redjs", EntryPoint = "redjs_aot_is_active", CallingConvention = CallingConvention.Cdecl)]
    internal static extern int IsAotActive();
    [DllImport("redjs", EntryPoint = "redjs_aot_run", CallingConvention = CallingConvention.Cdecl)]
    internal static extern int Run(nint context, uint entry, out ulong result);
    [DllImport("kernel32.dll", EntryPoint = "CreateFileW", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern SafeFileHandle CreateFile(string path, uint access, uint share, nint security,
        uint disposition, uint attributes, nint template);
}

internal sealed unsafe class CachedScript : IDisposable
{
    private readonly JsContext _context;
    private readonly byte[] _source;
    private readonly byte[] _url;
    private nint _handle;
    private readonly nint _bytes;
    private readonly nuint _length;

    internal CachedScript(JsContext context, string source, string url)
    {
        _context = context;
        _source = Encoding.UTF8.GetBytes(source);
        _url = Encoding.UTF8.GetBytes(url);
        fixed (byte* src = _source, file = _url)
        {
            int status = NativeImages.Generate(context.Handle, src, (nuint)_source.Length,
                file, (nuint)_url.Length, uint.MaxValue, out _handle, out _bytes, out _length, out ulong result);
            if (status != 0)
            {
                Dispose();
                NativeImages.Check(context, status, result);
            }
        }
        if (_handle == 0 || _bytes == 0 || _length == 0)
            throw new InvalidOperationException("No bytecode was generated");
    }

    internal JsValue Evaluate()
    {
        ObjectDisposedException.ThrowIf(_handle == 0, this);
        fixed (byte* src = _source, file = _url)
        {
            int status = NativeImages.EvalCached(_context.Handle, src, (nuint)_source.Length,
                file, (nuint)_url.Length, _bytes, _length, out uint used, out ulong result);
            NativeImages.Check(_context, status, result);
            if (used != 1) throw new InvalidOperationException("JSC rejected the bytecode cache; source fallback is not a benchmark result");
            return new JsValue(result);
        }
    }

    public void Dispose()
    {
        if (_handle != 0) NativeImages.Free(_handle);
        _handle = 0;
    }
}
