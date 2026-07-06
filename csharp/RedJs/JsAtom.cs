namespace RedJs;

/// <summary>
/// An interned property name (JSC <c>Identifier</c>). Create once per name via
/// <see cref="JsContext.Atom(string)"/> (which caches) and reuse for every
/// property access — this is what makes descriptor parsing cheap.
/// Atoms belong to their context's VM and normally live for the process.
/// </summary>
public readonly struct JsAtom
{
    internal readonly nint Handle;

    internal JsAtom(nint handle) => Handle = handle;

    public bool IsValid => Handle != 0;
}
