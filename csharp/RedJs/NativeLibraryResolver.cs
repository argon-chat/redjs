using System.Reflection;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;

namespace RedJs.Native;

// Maps the DllImport name "redjs" to runtimes/{rid}/native/redjs.dll next to
// the app (JIT/debug path). Under NativeAOT the DirectPInvoke static link
// bypasses this entirely. Mirrors Miniaudio's resolver.
internal static class NativeLibraryResolver
{
    private const string LibName = "redjs";
    private static int _registered;

#pragma warning disable CA2255
    [ModuleInitializer]
    internal static void Register()
#pragma warning restore CA2255
    {
        if (Interlocked.Exchange(ref _registered, 1) != 0) return;
        NativeLibrary.SetDllImportResolver(typeof(NativeLibraryResolver).Assembly, Resolve);
    }

    private static IntPtr Resolve(string libraryName, Assembly assembly, DllImportSearchPath? searchPath)
    {
        if (!string.Equals(libraryName, LibName, StringComparison.Ordinal))
            return IntPtr.Zero;

        string fileName = RuntimeInformation.IsOSPlatform(OSPlatform.Windows) ? "redjs.dll"
            : RuntimeInformation.IsOSPlatform(OSPlatform.OSX) ? "libredjs.dylib"
            : "libredjs.so";
        string rid = Rid();

        string withRid = Path.Combine(AppContext.BaseDirectory, "runtimes", rid, "native", fileName);
        if (File.Exists(withRid) && NativeLibrary.TryLoad(withRid, out var h1)) return h1;

        string flat = Path.Combine(AppContext.BaseDirectory, fileName);
        if (File.Exists(flat) && NativeLibrary.TryLoad(flat, out var h2)) return h2;

        return NativeLibrary.TryLoad(fileName, assembly, searchPath, out var h) ? h : IntPtr.Zero;
    }

    private static string Rid()
    {
        string os = RuntimeInformation.IsOSPlatform(OSPlatform.Windows) ? "win"
            : RuntimeInformation.IsOSPlatform(OSPlatform.OSX) ? "osx"
            : "linux";
        string arch = RuntimeInformation.ProcessArchitecture switch
        {
            Architecture.X64 => "x64",
            Architecture.Arm64 => "arm64",
            _ => RuntimeInformation.ProcessArchitecture.ToString().ToLowerInvariant(),
        };
        return $"{os}-{arch}";
    }
}
