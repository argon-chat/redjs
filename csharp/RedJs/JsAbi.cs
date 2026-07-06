using RedJs.Native;

namespace RedJs;

/// <summary>
/// JSVALUE64 NaN-boxing constants + CallFrame offsets, as C# consts so the
/// JIT/ILC folds them into immediates. <see cref="Verify"/> cross-checks every
/// value against the native `redjs_layout` at first context creation — a
/// prebuilt bump that moves anything throws instead of corrupting values.
/// </summary>
public static class JsAbi
{
    public const uint AbiVersion = 1;

    // NaN-boxing (JSCJSValue.h).
    public const ulong NumberTag = 0xfffe_0000_0000_0000;
    public const ulong DoubleEncodeOffset = 1UL << 49;
    public const ulong NotCellMask = NumberTag | 0x2;
    public const ulong Undefined = 0xa;
    public const ulong Null = 0x2;
    public const ulong True = 0x7;
    public const ulong False = 0x6;

    // Host-call CallFrame byte offsets (CallFrame.h slots × 8).
    public const int CfArgcOffset = 32;   // uint argumentCountIncludingThis
    public const int CfThisOffset = 40;   // EncodedJSValue `this`
    public const int CfArgsOffset = 48;   // EncodedJSValue args[0], contiguous
    public const int CfCalleeOffset = 24; // JSObject* callee

    private static int _verified;

    /// <summary>Fail-fast ABI check against the loaded native library.</summary>
    public static unsafe void Verify()
    {
        if (Interlocked.Exchange(ref _verified, 1) != 0) return;

        RjsLayout l;
        NativeMethods.redjs_layout(&l);

        if (l.abi_version != AbiVersion ||
            l.tag_number != NumberTag ||
            l.double_encode_offset != DoubleEncodeOffset ||
            l.not_cell_mask != NotCellMask ||
            l.value_undefined != Undefined ||
            l.value_null != Null ||
            l.value_true != True ||
            l.value_false != False ||
            l.cf_argc_offset != CfArgcOffset ||
            l.cf_this_offset != CfThisOffset ||
            l.cf_args_offset != CfArgsOffset ||
            l.cf_callee_offset != CfCalleeOffset)
        {
            throw new InvalidOperationException(
                $"redjs ABI drift: native layout (abi v{l.abi_version}, argc@{l.cf_argc_offset}, " +
                $"args@{l.cf_args_offset}) does not match the compiled JsAbi constants (v{AbiVersion}). " +
                "Rebuild RedJs against the current packages/jscore prebuilt.");
        }
    }
}
