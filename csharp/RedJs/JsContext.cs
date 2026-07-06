using System.Runtime.CompilerServices;
using System.Text;
using RedJs.Native;

namespace RedJs;

/// <summary>
/// One JavaScriptCore VM + global object. The ClearScript-replacement seam:
/// eval, properties via cached atoms, host functions with zero-FFI argument
/// access, zero-copy typed arrays / external ArrayBuffers, promises,
/// exceptions with JS stacks, GC control.
///
/// Threading: use from one thread at a time (every native call takes the
/// VM's recursive lock; hold <see cref="LockScope"/> around a frame to make
/// the inner locks free). Contexts live for the process lifetime — the
/// current prebuilt crashes in VM teardown, so there is deliberately no
/// Dispose.
/// </summary>
public sealed unsafe class JsContext
{
    private readonly void* _ctx;
    private readonly Dictionary<string, JsAtom> _atoms = new();

    public nint Handle => (nint)_ctx;
    public JsValue Global { get; }

    private JsContext(void* ctx)
    {
        _ctx = ctx;
        Global = new JsValue(NativeMethods.redjs_global(ctx));
    }

    /// <summary>Creates a VM. <paramref name="options"/> is a JSC options
    /// list ("useJIT=true,…"), applied process-wide before the first VM.</summary>
    public static JsContext Create(string? options = null)
    {
        if (!string.IsNullOrEmpty(options))
        {
            var bytes = Encoding.UTF8.GetBytes(options + '\0');
            fixed (byte* p = bytes)
            {
                if (NativeMethods.redjs_init(p) != 0)
                    throw new RedJsEngineException($"redjs_init(\"{options}\")", 2);
            }
        }

        var ctx = NativeMethods.redjs_context_create();
        if (ctx == null)
            throw new RedJsEngineException("redjs_context_create", 3);
        JsAbi.Verify();
        return new JsContext(ctx);
    }

    // ---------------------------------------------------------------------
    // Eval
    // ---------------------------------------------------------------------

    /// <summary>Evaluates a classic script. <paramref name="sourceUrl"/> names
    /// it in JS stack traces.</summary>
    public JsValue Eval(ReadOnlySpan<char> code, string sourceUrl = "")
    {
        ulong result;
        int rc;
        fixed (char* src = code)
        fixed (char* url = sourceUrl)
        {
            rc = NativeMethods.redjs_eval16(_ctx, (ushort*)src, (nuint)code.Length, (ushort*)url, (nuint)sourceUrl.Length, &result);
        }
        return Check(rc, result, "eval");
    }

    // ---------------------------------------------------------------------
    // Values / strings
    // ---------------------------------------------------------------------

    public JsValueKind KindOf(JsValue v) => (JsValueKind)NativeMethods.redjs_kind(_ctx, v.Raw);

    /// <summary>ToNumber with JS coercion (may run valueOf / throw).</summary>
    public double ToNumber(JsValue v)
    {
        if (v.TryGetNumber(out var fast)) return fast;
        double d;
        Check(NativeMethods.redjs_to_number(_ctx, v.Raw, &d), 0, "toNumber");
        return d;
    }

    public bool ToBoolean(JsValue v)
        => v.IsBoolean ? v.AsBoolean : NativeMethods.redjs_to_boolean(_ctx, v.Raw) != 0;

    public bool StrictEquals(JsValue a, JsValue b)
        => a.Raw == b.Raw || NativeMethods.redjs_strict_equals(_ctx, a.Raw, b.Raw) != 0;

    public JsValue CreateString(ReadOnlySpan<char> s)
    {
        ulong result;
        fixed (char* p = s)
        {
            Check(NativeMethods.redjs_string16(_ctx, (ushort*)p, (nuint)s.Length, &result), 0, "string16");
        }
        return new JsValue(result);
    }

    /// <summary>Reads a JS string value into a managed string (single copy).
    /// The value must already be a string — use <see cref="ToJsString"/> for coercion.</summary>
    public string GetString(JsValue stringValue)
    {
        RjsStr info;
        var rc = NativeMethods.redjs_string_info(_ctx, stringValue.Raw, &info);
        if (rc != 0) ThrowStatus(rc, 0, "string_info");
        if (info.len == 0) return string.Empty;

        if (info.is8bit != 0)
        {
            // Latin-1 → UTF-16 widening.
            return string.Create((int)info.len, (nint)info.ptr, static (dst, src) =>
            {
                var bytes = new ReadOnlySpan<byte>((void*)src, dst.Length);
                for (int i = 0; i < dst.Length; i++) dst[i] = (char)bytes[i];
            });
        }
        return new string((char*)info.ptr, 0, (int)info.len);
    }

    /// <summary>ToString coercion returning the JS string value.</summary>
    public JsValue ToJsString(JsValue v)
    {
        ulong result;
        return Check(NativeMethods.redjs_to_string(_ctx, v.Raw, &result), result, "toString");
    }

    /// <summary>ToString coercion straight to a managed string.</summary>
    public string GetStringCoerced(JsValue v) => GetString(ToJsString(v));

    /// <summary>Pins a value so it can be stored in fields/collections.</summary>
    public void Protect(JsValue v) => NativeMethods.redjs_protect(_ctx, v.Raw);

    public void Unprotect(JsValue v) => NativeMethods.redjs_unprotect(_ctx, v.Raw);

    // ---------------------------------------------------------------------
    // Atoms + properties
    // ---------------------------------------------------------------------

    /// <summary>Interns a property name (cached per context).</summary>
    public JsAtom Atom(string name)
    {
        if (_atoms.TryGetValue(name, out var cached)) return cached;
        void* h;
        fixed (char* p = name)
        {
            h = NativeMethods.redjs_atom16(_ctx, (ushort*)p, (nuint)name.Length);
        }
        if (h == null) throw new RedJsEngineException($"atom(\"{name}\")", 2);
        var atom = new JsAtom((nint)h);
        _atoms[name] = atom;
        return atom;
    }

    /// <summary>base[name] with full JS semantics (proto chain, getters).</summary>
    public JsValue Get(JsValue baseValue, JsAtom name)
    {
        ulong result;
        return Check(NativeMethods.redjs_get(_ctx, baseValue.Raw, (void*)name.Handle, &result), result, "get");
    }

    public JsValue Get(JsValue baseValue, string name) => Get(baseValue, Atom(name));

    /// <summary>obj[name] = v with full JS semantics (setters).</summary>
    public void Set(JsValue obj, JsAtom name, JsValue v)
        => Check(NativeMethods.redjs_set(_ctx, obj.Raw, (void*)name.Handle, v.Raw), 0, "set");

    public void Set(JsValue obj, string name, JsValue v) => Set(obj, Atom(name), v);

    /// <summary>Defines an own property directly (no setters). Attribute bits:
    /// 1 readonly, 2 dontenum, 4 dontdelete.</summary>
    public void Define(JsValue obj, JsAtom name, JsValue v, uint attributes = 0)
        => Check(NativeMethods.redjs_define(_ctx, obj.Raw, (void*)name.Handle, v.Raw, attributes), 0, "define");

    public void Define(JsValue obj, string name, JsValue v, uint attributes = 0)
        => Define(obj, Atom(name), v, attributes);

    public JsValue GetIndex(JsValue baseValue, uint index)
    {
        ulong result;
        return Check(NativeMethods.redjs_get_index(_ctx, baseValue.Raw, index, &result), result, "get_index");
    }

    public void SetIndex(JsValue obj, uint index, JsValue v)
        => Check(NativeMethods.redjs_set_index(_ctx, obj.Raw, index, v.Raw), 0, "set_index");

    public bool Has(JsValue obj, JsAtom name)
    {
        var rc = NativeMethods.redjs_has(_ctx, obj.Raw, (void*)name.Handle);
        if (rc < 0) throw new RedJsEngineException("has", rc);
        return rc == 1;
    }

    public bool Delete(JsValue obj, JsAtom name)
    {
        var rc = NativeMethods.redjs_delete(_ctx, obj.Raw, (void*)name.Handle);
        if (rc < 0) throw new RedJsEngineException("delete", rc);
        return rc == 1;
    }

    // ---------------------------------------------------------------------
    // Objects / arrays / functions
    // ---------------------------------------------------------------------

    /// <summary>Creates {}. Pass an object as <paramref name="prototype"/> to
    /// use it, <see cref="JsValue.Null"/> for a null-proto object.</summary>
    public JsValue CreateObject(JsValue prototype = default)
    {
        ulong result;
        var proto = prototype.Raw == 0 ? JsValue.Undefined : prototype;
        Check(NativeMethods.redjs_object_create(_ctx, proto.Raw, &result), 0, "object_create");
        return new JsValue(result);
    }

    public JsValue CreateArray(uint length = 0)
    {
        ulong result;
        return Check(NativeMethods.redjs_array_create(_ctx, length, &result), result, "array_create");
    }

    public uint ArrayLength(JsValue array)
    {
        uint len;
        Check(NativeMethods.redjs_array_length(_ctx, array.Raw, &len), 0, "array_length");
        return len;
    }

    /// <summary>Registers an <c>[UnmanagedCallersOnly] static ulong F(nint global,
    /// nint callFrame)</c> as a JS function. Decode arguments with
    /// <see cref="JsCallFrame"/>. Consumes one trampoline slot for the process
    /// lifetime — register once, reuse the value.</summary>
    public JsValue CreateFunction(string name, uint arity, delegate* unmanaged<nint, nint, ulong> fn)
    {
        ulong result;
        int rc;
        fixed (char* p = name)
        {
            rc = NativeMethods.redjs_function_create(_ctx, (ushort*)p, (nuint)name.Length, arity, fn, &result);
        }
        return Check(rc, result, "function_create");
    }

    /// <summary>Variant whose callback also receives an opaque data pointer:
    /// <c>ulong F(nint global, nint callFrame, nint data)</c>.</summary>
    public JsValue CreateFunction(string name, uint arity, delegate* unmanaged<nint, nint, nint, ulong> fn, nint data)
    {
        ulong result;
        int rc;
        fixed (char* p = name)
        {
            rc = NativeMethods.redjs_function_create_with_data(_ctx, (ushort*)p, (nuint)name.Length, arity, fn, (void*)data, &result);
        }
        return Check(rc, result, "function_create_with_data");
    }

    /// <summary>Registers a host function directly on the global object.</summary>
    public void SetGlobalFunction(string name, uint arity, delegate* unmanaged<nint, nint, ulong> fn)
        => Define(Global, name, CreateFunction(name, arity, fn));

    public JsValue Call(JsValue function, JsValue thisValue, ReadOnlySpan<JsValue> args)
    {
        ulong result;
        int rc;
        fixed (JsValue* p = args)
        {
            rc = NativeMethods.redjs_call(_ctx, function.Raw, thisValue.Raw, (ulong*)p, (uint)args.Length, &result);
        }
        return Check(rc, result, "call");
    }

    public JsValue Call(JsValue function, JsValue thisValue) => Call(function, thisValue, []);

    public JsValue Construct(JsValue constructor, ReadOnlySpan<JsValue> args)
    {
        ulong result;
        int rc;
        fixed (JsValue* p = args)
        {
            rc = NativeMethods.redjs_construct(_ctx, constructor.Raw, (ulong*)p, (uint)args.Length, &result);
        }
        return Check(rc, result, "construct");
    }

    // ---------------------------------------------------------------------
    // Errors
    // ---------------------------------------------------------------------

    /// <summary>Creates an Error object. kind: 0 Error, 1 TypeError,
    /// 2 RangeError, 3 SyntaxError.</summary>
    public JsValue CreateError(uint kind, ReadOnlySpan<char> message)
    {
        ulong result;
        fixed (char* p = message)
        {
            Check(NativeMethods.redjs_error_create(_ctx, kind, (ushort*)p, (nuint)message.Length, &result), 0, "error_create");
        }
        return new JsValue(result);
    }

    /// <summary>Throws into the VM. Use inside a host function right before
    /// returning (the return value is then ignored).</summary>
    public void Throw(JsValue error) => NativeMethods.redjs_throw(_ctx, error.Raw);

    /// <summary>Host-function helper: throw a TypeError and produce the return value.</summary>
    public ulong ThrowTypeError(string message)
    {
        Throw(CreateError(1, message));
        return JsAbi.Undefined;
    }

    /// <summary>Extracts (message, stack) from an exception value.</summary>
    public (string Message, string Stack) GetErrorInfo(JsValue error)
    {
        ulong msg, stack;
        NativeMethods.redjs_error_info(_ctx, error.Raw, &msg, &stack);
        return (GetString(new JsValue(msg)), GetString(new JsValue(stack)));
    }

    // ---------------------------------------------------------------------
    // Typed arrays / ArrayBuffers
    // ---------------------------------------------------------------------

    /// <summary>One-call introspection of a typed-array view. Returns false if
    /// the value is not a view. <c>info.data</c> points INTO JS memory — read
    /// or write it directly (valid while the value is alive and not detached).</summary>
    public bool TryGetTypedArrayInfo(JsValue v, out JsTypedArrayInfo info)
    {
        RjsTaInfo raw;
        if (NativeMethods.redjs_typedarray_info(_ctx, v.Raw, &raw) != 0)
        {
            info = default;
            return false;
        }
        info = new JsTypedArrayInfo((nint)raw.data, raw.length, raw.byte_length, raw.byte_offset, (JsTypedArrayKind)raw.kind, raw.detached != 0, raw.shared != 0);
        return true;
    }

    /// <summary>The typed array's bytes as a span over JS memory (empty when
    /// not a view or detached). Zero copy — the writeBuffer path.</summary>
    public Span<byte> GetTypedArrayBytes(JsValue v)
        => TryGetTypedArrayInfo(v, out var i) && !i.Detached
            ? new Span<byte>((void*)i.Data, (int)i.ByteLength)
            : default;

    public bool TryGetArrayBufferInfo(JsValue v, out JsArrayBufferInfo info)
    {
        RjsAbInfo raw;
        if (NativeMethods.redjs_arraybuffer_info(_ctx, v.Raw, &raw) != 0)
        {
            info = default;
            return false;
        }
        info = new JsArrayBufferInfo((nint)raw.data, raw.byte_length, raw.detached != 0, raw.shared != 0);
        return true;
    }

    /// <summary>Creates an ArrayBuffer that VIEWS host memory (zero copy —
    /// the getMappedRange path). With <paramref name="deallocator"/> null the
    /// host keeps ownership and must <see cref="DetachArrayBuffer"/> before
    /// freeing the memory.</summary>
    public JsValue CreateExternalArrayBuffer(nint bytes, nuint byteLength,
        delegate* unmanaged<void*, void*, void> deallocator = null, nint deallocatorContext = 0)
    {
        ulong result;
        var rc = NativeMethods.redjs_arraybuffer_external(_ctx, (void*)bytes, byteLength, deallocator, (void*)deallocatorContext, &result);
        return Check(rc, result, "arraybuffer_external");
    }

    /// <summary>Detaches the buffer: JS views become zero-length, the
    /// deallocator (if any) fires. The unmap() semantics.</summary>
    public void DetachArrayBuffer(JsValue arrayBuffer)
        => Check(NativeMethods.redjs_arraybuffer_detach(_ctx, arrayBuffer.Raw), 0, "arraybuffer_detach");

    public JsValue CreateTypedArray(JsTypedArrayKind kind, nuint length)
    {
        ulong result;
        return Check(NativeMethods.redjs_typedarray_create(_ctx, (uint)kind, length, &result), result, "typedarray_create");
    }

    public JsValue CreateExternalTypedArray(JsTypedArrayKind kind, nint bytes, nuint byteLength,
        delegate* unmanaged<void*, void*, void> deallocator = null, nint deallocatorContext = 0)
    {
        ulong result;
        var rc = NativeMethods.redjs_typedarray_external(_ctx, (uint)kind, (void*)bytes, byteLength, deallocator, (void*)deallocatorContext, &result);
        return Check(rc, result, "typedarray_external");
    }

    /// <summary>new TypedArray(buffer, byteOffset, length).</summary>
    public JsValue WrapTypedArray(JsTypedArrayKind kind, JsValue arrayBuffer, nuint byteOffset, nuint length)
    {
        ulong result;
        var rc = NativeMethods.redjs_typedarray_wrap(_ctx, (uint)kind, arrayBuffer.Raw, byteOffset, length, &result);
        return Check(rc, result, "typedarray_wrap");
    }

    // ---------------------------------------------------------------------
    // Promises / microtasks
    // ---------------------------------------------------------------------

    /// <summary>Creates a deferred promise; settle it by calling
    /// <paramref name="resolve"/>/<paramref name="reject"/> (plain JS
    /// functions) and then <see cref="DrainMicrotasks"/>. The Task→Promise seam.</summary>
    public JsValue CreateDeferredPromise(out JsValue resolve, out JsValue reject)
    {
        ulong r, j, p;
        Check(NativeMethods.redjs_promise_create(_ctx, &r, &j, &p), 0, "promise_create");
        resolve = new JsValue(r);
        reject = new JsValue(j);
        return new JsValue(p);
    }

    /// <summary>Polls a promise: 0 pending, 1 fulfilled, 2 rejected. The
    /// Promise→Task seam (poll per frame instead of callbacks).</summary>
    public JsPromiseState GetPromiseState(JsValue promise, out JsValue result)
    {
        uint status;
        ulong res;
        Check(NativeMethods.redjs_promise_info(_ctx, promise.Raw, &status, &res), 0, "promise_info");
        result = new JsValue(res);
        return (JsPromiseState)status;
    }

    /// <summary>Runs queued promise reactions / await continuations. Call once
    /// per frame after the rAF callback (mirrors the browser task model).</summary>
    public void DrainMicrotasks() => NativeMethods.redjs_drain_microtasks(_ctx);

    // ---------------------------------------------------------------------
    // Host objects
    // ---------------------------------------------------------------------

    /// <summary>Creates a host-object class (one per wrapper family, e.g.
    /// "GPUBuffer"). <paramref name="utf8Name"/> must be a NUL-terminated
    /// UTF-8 literal (e.g. <c>"GPUBuffer\0"u8</c>).</summary>
    public static nint CreateHostClass(ReadOnlySpan<byte> utf8Name)
    {
        fixed (byte* p = utf8Name)
        {
            var cls = NativeMethods.redjs_hostclass_create(p);
            if (cls == null) throw new RedJsEngineException("hostclass_create", 2);
            return (nint)cls;
        }
    }

    /// <summary>Sets the process-global host-object finalizer
    /// (<c>void F(nint data)</c>). ⚠ Runs on GC threads — only enqueue.</summary>
    public static void SetHostFinalizer(delegate* unmanaged<nint, void> finalizer)
        => NativeMethods.redjs_set_host_finalizer(finalizer);

    /// <summary>Creates a JS object of <paramref name="hostClass"/> carrying an
    /// opaque handle (e.g. an index into a C# side table).</summary>
    public JsValue CreateHostObject(nint hostClass, nint data, JsValue prototype = default)
    {
        ulong result;
        var rc = NativeMethods.redjs_hostobject_create(_ctx, (void*)hostClass, (void*)data, prototype.Raw, &result);
        return Check(rc, result, "hostobject_create");
    }

    /// <summary>Reads the handle back (0 when not a host object). The single
    /// FFI call in a wrapper method dispatch.</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static nint GetHostObjectData(JsValue v) => (nint)NativeMethods.redjs_hostobject_data(v.Raw);

    // ---------------------------------------------------------------------
    // Web scheduling core (performance / rAF / timers)
    // ---------------------------------------------------------------------

    /// <summary>Installs NATIVE <c>performance</c> (now/timeOrigin),
    /// requestAnimationFrame/cancelAnimationFrame and setTimeout/setInterval/
    /// clearTimeout/clearInterval globals. They, <see cref="Now"/> and pump
    /// timestamps all share one monotonic clock — a single time base for the
    /// whole frame.</summary>
    public void InstallWebGlobals()
    {
        var rc = NativeMethods.redjs_web_install(_ctx);
        if (rc != 0) throw new RedJsEngineException("web_install", rc);
    }

    /// <summary>Milliseconds on the scheduler clock — exactly what JS sees
    /// from <c>performance.now()</c>. Use for host-side frame timing.</summary>
    public static double Now => NativeMethods.redjs_now();

    /// <summary>Runs one frame of the web scheduler: due timers, the rAF
    /// batch (timestamp = snapshot now), then microtasks. THE per-frame call
    /// — replaces ProcessTimers + ProcessFrame + drain in the old runtime.
    /// Callback exceptions go to the uncaught handler; the pump never throws.</summary>
    public JsPumpStats Pump(JsPumpFlags flags = JsPumpFlags.All)
    {
        RjsPumpStats s;
        NativeMethods.redjs_pump(_ctx, (uint)flags, &s);
        return new JsPumpStats(s.now, s.timers_fired, s.raf_fired, s.raf_pending, s.timers_active, s.next_timer_deadline);
    }

    /// <summary>Process-global handler for exceptions thrown by pumped
    /// callbacks: <c>void F(nint ctx, ulong exception)</c>. Called with the
    /// VM lock held — extract info via <see cref="GetErrorInfo"/>, don't
    /// re-enter JS.</summary>
    public static void SetUncaughtHandler(delegate* unmanaged<nint, ulong, void> handler)
        => NativeMethods.redjs_set_uncaught_handler(handler);

    // ---------------------------------------------------------------------
    // Native Web-API layer (browser runtime shims)
    // ---------------------------------------------------------------------

    /// <summary>Installs the native web API: Event/CustomEvent/EventTarget,
    /// TextEncoder/TextDecoder, Blob, URL/URLSearchParams, fetch/Response/
    /// Headers (local files + data: URIs), ImageBitmap/HTMLImageElement/
    /// Image/createImageBitmap, document/window, plus small JS stubs
    /// (OffscreenCanvas-2D, silent Web Audio, atob/btoa…). Replaces the
    /// player's C# WebApi/Browser* classes. Call after
    /// <see cref="InstallWebGlobals"/>. Idempotent.</summary>
    public void InstallWebShims(string scriptsRoot, string assetsRoot)
    {
        fixed (char* s = scriptsRoot)
        fixed (char* a = assetsRoot)
        {
            var rc = NativeMethods.redjs_webshim_install(_ctx, (ushort*)s, (nuint)scriptsRoot.Length, (ushort*)a, (nuint)assetsRoot.Length);
            if (rc != 0) throw new RedJsEngineException("webshim_install", rc);
        }
    }

    /// <summary>Updates window.innerWidth/innerHeight/devicePixelRatio and
    /// dispatches a trusted `resize` event. dpr &lt;= 0 keeps the current value.</summary>
    public void DomResize(int width, int height, double devicePixelRatio = 0)
        => NativeMethods.redjs_dom_resize(_ctx, width, height, devicePixelRatio);

    /// <summary>document.hidden/visibilityState + `visibilitychange` event.</summary>
    public void DomSetVisibility(bool hidden)
        => NativeMethods.redjs_dom_set_visibility(_ctx, hidden ? 1 : 0);

    /// <summary>Fires DOMContentLoaded + window `load` (call once after the
    /// entry script has run).</summary>
    public void DomContentLoaded() => NativeMethods.redjs_dom_content_loaded(_ctx);

    /// <summary>Registers a value for document.getElementById(id) — the host
    /// canvas wrapper. The value is protected by the registry.</summary>
    public void DomRegisterElement(string id, JsValue element)
    {
        Protect(element);
        fixed (char* p = id)
        {
            NativeMethods.redjs_dom_register_element(_ctx, (ushort*)p, (nuint)id.Length, element.Raw);
        }
    }

    /// <summary>RGBA8 pixels of an ImageBitmap / loaded HTMLImageElement —
    /// direct span into engine memory for zero-copy GPU upload
    /// (copyExternalImageToTexture). Valid while the value stays alive.</summary>
    public bool TryGetImagePixels(JsValue image, out ReadOnlySpan<byte> pixels, out uint width, out uint height)
    {
        byte* ptr;
        nuint len;
        uint w, h;
        if (NativeMethods.redjs_image_pixels(_ctx, image.Raw, &ptr, &len, &w, &h) != 0)
        {
            pixels = default;
            width = height = 0;
            return false;
        }
        pixels = new ReadOnlySpan<byte>(ptr, (int)len);
        width = w;
        height = h;
        return true;
    }

    // ---------------------------------------------------------------------
    // VM control
    // ---------------------------------------------------------------------

    /// <summary>mode: 0 full sync, 1 eden sync, 2 async request.</summary>
    public void Gc(int mode = 0) => NativeMethods.redjs_gc(_ctx, mode);

    public JsHeapStats GetHeapStats()
    {
        RjsHeapStats s;
        NativeMethods.redjs_heap_stats(_ctx, &s);
        return new JsHeapStats(s.heap_size, s.heap_capacity, s.extra_memory, s.object_count, s.protected_object_count, s.global_object_count);
    }

    public void ReportExtraMemory(ulong bytes) => NativeMethods.redjs_report_extra_memory(_ctx, bytes);

    /// <summary>Holds the VM API lock for a batch of calls (e.g. one frame) so
    /// every inner per-call lock is a free recursive acquire.</summary>
    public LockScope Lock() => new(_ctx);

    public readonly struct LockScope : IDisposable
    {
        private readonly void* _ctx;

        internal LockScope(void* ctx)
        {
            _ctx = ctx;
            NativeMethods.redjs_lock(ctx);
        }

        public void Dispose() => NativeMethods.redjs_unlock(_ctx);
    }

    // ---------------------------------------------------------------------

    private JsValue Check(int rc, ulong result, string operation)
    {
        if (rc == 0) return new JsValue(result);
        ThrowStatus(rc, result, operation);
        return default; // unreachable
    }

    private void ThrowStatus(int rc, ulong exceptionValue, string operation)
    {
        if (rc == 1)
        {
            var (message, stack) = GetErrorInfo(new JsValue(exceptionValue));
            throw new RedJsException(message, stack);
        }
        throw new RedJsEngineException(operation, rc);
    }
}

public enum JsValueKind : uint
{
    Undefined = 0,
    Null = 1,
    Boolean = 2,
    Int32 = 3,
    Double = 4,
    String = 5,
    Symbol = 6,
    BigInt = 7,
    Object = 8,
    Function = 9,
    Array = 10,
    TypedArray = 11,
    ArrayBuffer = 12,
    Promise = 13,
}

public enum JsTypedArrayKind : uint
{
    None = 0,
    Int8 = 1,
    Uint8 = 2,
    Uint8Clamped = 3,
    Int16 = 4,
    Uint16 = 5,
    Int32 = 6,
    Uint32 = 7,
    Float32 = 8,
    Float64 = 9,
    BigInt64 = 10,
    BigUint64 = 11,
    DataView = 12,
    Float16 = 13,
}

public enum JsPromiseState : uint
{
    Pending = 0,
    Fulfilled = 1,
    Rejected = 2,
}

public readonly record struct JsTypedArrayInfo(nint Data, uint Length, uint ByteLength, uint ByteOffset, JsTypedArrayKind Kind, bool Detached, bool Shared);

public readonly record struct JsArrayBufferInfo(nint Data, ulong ByteLength, bool Detached, bool Shared);

public readonly record struct JsHeapStats(ulong HeapSize, ulong HeapCapacity, ulong ExtraMemory, ulong ObjectCount, ulong ProtectedObjectCount, ulong GlobalObjectCount);

[Flags]
public enum JsPumpFlags : uint
{
    Timers = 1,
    AnimationFrames = 2,
    Microtasks = 4,
    All = Timers | AnimationFrames | Microtasks,
}

/// <param name="Now">The pump's snapshot time (ms, performance.now() clock).</param>
/// <param name="NextTimerDeadline">-1 when no timers are scheduled.</param>
public readonly record struct JsPumpStats(double Now, uint TimersFired, uint RafFired, uint RafPending, uint TimersActive, double NextTimerDeadline);
