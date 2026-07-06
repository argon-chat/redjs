using System.Runtime.CompilerServices;

namespace RedJs;

/// <summary>
/// A JavaScript value: raw JSC <c>EncodedJSValue</c> bits. Numbers, booleans,
/// undefined and null encode/decode entirely in managed code (no FFI, no
/// boxing). Cells (strings, objects, functions) are pointers — use
/// <see cref="JsContext"/> to work with them.
///
/// GC contract: a JsValue held only in locals/args of code running inside the
/// VM is safe (JSC scans the native stack conservatively). Storing one in a
/// field/collection requires <see cref="JsContext.Protect"/>.
/// </summary>
public readonly struct JsValue : IEquatable<JsValue>
{
    public readonly ulong Raw;

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public JsValue(ulong raw) => Raw = raw;

    public static JsValue Undefined
    {
        [MethodImpl(MethodImplOptions.AggressiveInlining)]
        get => new(JsAbi.Undefined);
    }

    public static JsValue Null
    {
        [MethodImpl(MethodImplOptions.AggressiveInlining)]
        get => new(JsAbi.Null);
    }

    public static JsValue True
    {
        [MethodImpl(MethodImplOptions.AggressiveInlining)]
        get => new(JsAbi.True);
    }

    public static JsValue False
    {
        [MethodImpl(MethodImplOptions.AggressiveInlining)]
        get => new(JsAbi.False);
    }

    // ---- encode ------------------------------------------------------------

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static JsValue From(bool b) => new(b ? JsAbi.True : JsAbi.False);

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static JsValue From(int i) => new(JsAbi.NumberTag | (uint)i);

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static JsValue From(uint u) => u <= int.MaxValue ? From((int)u) : From((double)u);

    /// <summary>Encodes a double, preferring the int32 representation when
    /// exact (JSC fast paths and property keys want int32).</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static JsValue From(double d)
    {
        int i = (int)d;
        if (i == d && !(i == 0 && double.IsNegative(d)))
            return From(i);
        return new JsValue(unchecked((ulong)BitConverter.DoubleToInt64Bits(d) + JsAbi.DoubleEncodeOffset));
    }

    // ---- classify ----------------------------------------------------------

    public bool IsUndefined
    {
        [MethodImpl(MethodImplOptions.AggressiveInlining)]
        get => Raw == JsAbi.Undefined;
    }

    public bool IsNull
    {
        [MethodImpl(MethodImplOptions.AggressiveInlining)]
        get => Raw == JsAbi.Null;
    }

    /// <summary>undefined or null — the "missing" checks the player does everywhere.</summary>
    public bool IsNullish
    {
        [MethodImpl(MethodImplOptions.AggressiveInlining)]
        get => (Raw & ~0x8UL) == JsAbi.Null; // ValueNull == ValueUndefined & ~UndefinedTag
    }

    public bool IsBoolean
    {
        [MethodImpl(MethodImplOptions.AggressiveInlining)]
        get => (Raw & ~1UL) == JsAbi.False;
    }

    public bool IsInt32
    {
        [MethodImpl(MethodImplOptions.AggressiveInlining)]
        get => (Raw & JsAbi.NumberTag) == JsAbi.NumberTag;
    }

    public bool IsNumber
    {
        [MethodImpl(MethodImplOptions.AggressiveInlining)]
        get => (Raw & JsAbi.NumberTag) != 0;
    }

    /// <summary>True for heap values: strings, objects, functions, symbols…</summary>
    public bool IsCell
    {
        [MethodImpl(MethodImplOptions.AggressiveInlining)]
        get => Raw != 0 && (Raw & JsAbi.NotCellMask) == 0;
    }

    // ---- decode (no coercion) ----------------------------------------------

    /// <summary>Raw int32 payload; only valid when <see cref="IsInt32"/>.</summary>
    public int AsInt32
    {
        [MethodImpl(MethodImplOptions.AggressiveInlining)]
        get => unchecked((int)(uint)Raw);
    }

    public bool AsBoolean
    {
        [MethodImpl(MethodImplOptions.AggressiveInlining)]
        get => Raw == JsAbi.True;
    }

    /// <summary>Number payload (int32 or double); NaN when not a number.
    /// Use <see cref="TryGetNumber"/> to distinguish.</summary>
    public double AsNumber
    {
        [MethodImpl(MethodImplOptions.AggressiveInlining)]
        get
        {
            if (IsInt32) return AsInt32;
            if (IsNumber) return BitConverter.Int64BitsToDouble(unchecked((long)(Raw - JsAbi.DoubleEncodeOffset)));
            return double.NaN;
        }
    }

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public bool TryGetNumber(out double value)
    {
        if (IsInt32) { value = AsInt32; return true; }
        if (IsNumber) { value = BitConverter.Int64BitsToDouble(unchecked((long)(Raw - JsAbi.DoubleEncodeOffset))); return true; }
        value = 0;
        return false;
    }

    /// <summary>Number with a default for undefined/null/non-number — the
    /// `GetValueOr` pattern from descriptor parsing.</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public double NumberOr(double fallback) => TryGetNumber(out var v) ? v : fallback;

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public int Int32Or(int fallback) => IsInt32 ? AsInt32 : TryGetNumber(out var v) ? (int)v : fallback;

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public uint UInt32Or(uint fallback) => TryGetNumber(out var v) ? (uint)v : fallback;

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public ulong UInt64Or(ulong fallback) => TryGetNumber(out var v) ? (ulong)v : fallback;

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public bool BoolOr(bool fallback) => IsBoolean ? AsBoolean : fallback;

    // ---- plumbing ------------------------------------------------------------

    public bool Equals(JsValue other) => Raw == other.Raw;
    public override bool Equals(object? obj) => obj is JsValue v && v.Raw == Raw;
    public override int GetHashCode() => Raw.GetHashCode();
    public static bool operator ==(JsValue a, JsValue b) => a.Raw == b.Raw;
    public static bool operator !=(JsValue a, JsValue b) => a.Raw != b.Raw;

    public override string ToString() =>
        IsUndefined ? "undefined"
        : IsNull ? "null"
        : IsBoolean ? (AsBoolean ? "true" : "false")
        : IsInt32 ? AsInt32.ToString()
        : IsNumber ? AsNumber.ToString()
        : $"cell:0x{Raw:x}";
}
