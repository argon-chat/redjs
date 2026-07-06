using System.Runtime.CompilerServices;

namespace RedJs;

/// <summary>
/// Zero-FFI view of the JSC CallFrame passed to a host function
/// (<c>[UnmanagedCallersOnly] static ulong F(nint global, nint callFrame)</c>).
/// Arguments are read straight out of frame memory using the verified
/// <see cref="JsAbi"/> offsets. Valid only for the duration of the call —
/// never store it.
/// </summary>
public readonly unsafe ref struct JsCallFrame
{
    private readonly byte* _cf;

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public JsCallFrame(nint callFrame) => _cf = (byte*)callFrame;

    /// <summary>Number of arguments the script actually passed (excluding `this`).
    /// May be less than the function's declared arity.</summary>
    public int ArgCount
    {
        [MethodImpl(MethodImplOptions.AggressiveInlining)]
        get => (int)*(uint*)(_cf + JsAbi.CfArgcOffset) - 1;
    }

    public JsValue This
    {
        [MethodImpl(MethodImplOptions.AggressiveInlining)]
        get => new(*(ulong*)(_cf + JsAbi.CfThisOffset));
    }

    /// <summary>Argument <paramref name="i"/>, or undefined when out of range
    /// (JS call semantics).</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public JsValue Arg(int i)
        => (uint)i < (uint)ArgCount ? new JsValue(*(ulong*)(_cf + JsAbi.CfArgsOffset + i * 8)) : JsValue.Undefined;

    /// <summary>Unchecked argument read — only when ArgCount was already validated.</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public JsValue ArgUnchecked(int i) => new(*(ulong*)(_cf + JsAbi.CfArgsOffset + i * 8));
}
