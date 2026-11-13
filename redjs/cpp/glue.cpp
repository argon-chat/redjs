// redjs C++ glue: the parts of JSC that are only reachable through the
// *internal* C++ API (fast JSFunction host bindings, CallFrame/JSValue ABI
// export, direct typed-array/promise/heap access, Options).
//
// Everything here is `extern "C"` with the `rjs_` prefix; the Rust crate
// re-exports it (plus pure C-API calls) as the stable `redjs_` ABI.
//
// Compiled with clang-cl /std:c++23preview against the Bun WebKit prebuilt's
// internal headers; cmakeconfig.h is force-included by build.rs so this TU
// matches the prebuilt's ABI exactly (see build.rs).
//
// Threading: every entry takes the VM's API lock (recursive, cheap when
// already held). Host callbacks run with the lock already held by JSC, so
// nested rjs_* calls from inside a callback are fine.
//
// Value lifetime contract (important for the C# side): an EncodedJSValue held
// in native stack/registers is protected by JSC's conservative stack scan
// while this thread is inside the VM. Storing a value into heap memory (C#
// object fields, Rust collections) requires rjs_protect / a strong handle.

#include <JavaScriptCore/JavaScript.h>
#include <JavaScriptCore/APICast.h>
#include <JavaScriptCore/CallData.h>
#include <JavaScriptCore/DeferredWorkTimer.h>
#include <JavaScriptCore/Completion.h>
#include <JavaScriptCore/CachedBytecode.h>
#include <JavaScriptCore/CachedTypes.h>
#include <JavaScriptCore/CodeCache.h>
#include <JavaScriptCore/JSCBytecodeCacheVersion.h>
#include <JavaScriptCore/ParserError.h>
#include <JavaScriptCore/SourceCodeKey.h>
#include <JavaScriptCore/Strong.h>
#include <JavaScriptCore/UnlinkedProgramCodeBlock.h>
#include <JavaScriptCore/Error.h>
#include <JavaScriptCore/ExceptionHelpers.h>
#include <JavaScriptCore/ExceptionScope.h>
#include <JavaScriptCore/JSCast.h>
#include <JavaScriptCore/Heap.h>
#include <JavaScriptCore/Identifier.h>
#include <JavaScriptCore/InitializeThreading.h>
#include <JavaScriptCore/JSArray.h>
#include <JavaScriptCore/JSArrayBuffer.h>
#include <JavaScriptCore/JSArrayBufferView.h>
#include <JavaScriptCore/JSCInlines.h>
#include <JavaScriptCore/JSCJSValue.h>
#include <JavaScriptCore/JSFunction.h>
#include <JavaScriptCore/JSFunctionInlines.h>
#include <JavaScriptCore/FunctionExecutable.h>
#include <JavaScriptCore/NativeExecutable.h>
#include <JavaScriptCore/JSGlobalObject.h>
#include <JavaScriptCore/JSLock.h>
#include <JavaScriptCore/JSPromise.h>
#include <JavaScriptCore/MicrotaskQueue.h>
#include <JavaScriptCore/JSString.h>
#include <JavaScriptCore/ObjectConstructor.h>
#include <JavaScriptCore/Options.h>
#include <JavaScriptCore/SourceCode.h>
#include <JavaScriptCore/SourceOrigin.h>
#include <JavaScriptCore/ThrowScope.h>
#include <JavaScriptCore/TypedArrayType.h>
#include <JavaScriptCore/VM.h>
#include <wtf/NakedPtr.h>
#include <wtf/text/WTFString.h>
#include <wtf/text/StringBuilder.h>
#include <cstdlib>
#include <cstring>
#include <unicode/udata.h>

// JSC registers unwind info for its JIT pool, LLInt and vmEntry with
// `RtlAddGrowableFunctionTable` (oven-sh/WebKit #315/#442), which lives in
// ntdll.lib. build.rs puts ntdll on the list for every cargo link; this
// directive puts it in the objects, so a consumer that links the THIN
// redjs.lib by hand (a C# host's NativeAOT csproj) gets it without having to
// learn that it changed.
#if defined(_WIN32)
#pragma comment(lib, "ntdll")
#endif

using namespace JSC;

namespace {

constexpr int32_t RJS_OK = 0;
constexpr int32_t RJS_EXCEPTION = 1;
constexpr int32_t RJS_BAD_ARG = 2;

inline JSGlobalObject* G(void* g) { return static_cast<JSGlobalObject*>(g); }

// Bun's WebKit fork removed CatchScope.h; VM::exception()/clearException()
// are private. ExceptionScope (protected ctor, public exception() +
// tryClearException()) gives us the sanctioned access path — this is the
// release-mode CatchScope in all but name.
class RjsCatchScope final : public ExceptionScope {
public:
    ALWAYS_INLINE RjsCatchScope(VM& vm)
        : ExceptionScope(vm)
    {
    }
};

// Returns true and stores the exception value into *out if the VM holds a
// pending exception (clearing it unless it's a termination request). `out`
// may be null (exception swallowed).
inline bool takeException(VM& vm, uint64_t* out)
{
    RjsCatchScope scope(vm);
    if (Exception* e = scope.exception()) {
        if (out)
            *out = JSValue::encode(e->value());
        (void)scope.tryClearException();
        return true;
    }
    return false;
}

// Our stable typed-array kind ids (mirrored in ffi.rs / C#).
enum RjsTaKind : uint32_t {
    RJS_TA_NONE = 0,
    RJS_TA_I8 = 1,
    RJS_TA_U8 = 2,
    RJS_TA_U8C = 3,
    RJS_TA_I16 = 4,
    RJS_TA_U16 = 5,
    RJS_TA_I32 = 6,
    RJS_TA_U32 = 7,
    RJS_TA_F32 = 8,
    RJS_TA_F64 = 9,
    RJS_TA_BIGI64 = 10,
    RJS_TA_BIGU64 = 11,
    RJS_TA_DATAVIEW = 12,
    RJS_TA_F16 = 13,
};

uint32_t mapTypedArrayType(TypedArrayType t)
{
    switch (t) {
    case TypeInt8: return RJS_TA_I8;
    case TypeUint8: return RJS_TA_U8;
    case TypeUint8Clamped: return RJS_TA_U8C;
    case TypeInt16: return RJS_TA_I16;
    case TypeUint16: return RJS_TA_U16;
    case TypeInt32: return RJS_TA_I32;
    case TypeUint32: return RJS_TA_U32;
    case TypeFloat32: return RJS_TA_F32;
    case TypeFloat64: return RJS_TA_F64;
    case TypeBigInt64: return RJS_TA_BIGI64;
    case TypeBigUint64: return RJS_TA_BIGU64;
    case TypeDataView: return RJS_TA_DATAVIEW;
    case TypeFloat16: return RJS_TA_F16;
    default: return RJS_TA_NONE;
    }
}

// Our stable property-attribute bits -> JSC's.
unsigned mapAttributes(uint32_t a)
{
    unsigned r = 0;
    if (a & 1) r |= PropertyAttribute::ReadOnly;
    if (a & 2) r |= PropertyAttribute::DontEnum;
    if (a & 4) r |= PropertyAttribute::DontDelete;
    return r;
}

} // namespace

// ---------------------------------------------------------------------------
// ABI layout export: everything the C# side needs to decode JSValues and read
// host-call CallFrames with zero FFI calls. Filled from the real headers this
// TU is compiled against, so it can never drift from the prebuilt.
// ---------------------------------------------------------------------------

struct RjsLayout {
    uint32_t abiVersion;
    // Byte offsets from the CallFrame pointer passed to host functions.
    uint32_t cfArgcOffset;   // u32 argumentCountIncludingThis (payload half)
    uint32_t cfThisOffset;   // EncodedJSValue `this`
    uint32_t cfArgsOffset;   // EncodedJSValue args[0] (contiguous upward)
    uint32_t cfCalleeOffset; // JSObject* callee
    uint32_t reserved0;
    // JSVALUE64 NaN-boxing constants.
    uint64_t tagNumber;          // JSValue::NumberTag
    uint64_t doubleEncodeOffset; // JSValue::DoubleEncodeOffset
    uint64_t notCellMask;        // JSValue::NotCellMask
    uint64_t valueUndefined;
    uint64_t valueNull;
    uint64_t valueTrue;
    uint64_t valueFalse;
};

extern "C" void rjs_layout(RjsLayout* out)
{
    out->abiVersion = 1;
    out->cfArgcOffset = static_cast<uint32_t>(CallFrameSlot::argumentCountIncludingThis) * sizeof(Register);
    out->cfThisOffset = static_cast<uint32_t>(CallFrameSlot::thisArgument) * sizeof(Register);
    out->cfArgsOffset = static_cast<uint32_t>(CallFrameSlot::firstArgument) * sizeof(Register);
    out->cfCalleeOffset = static_cast<uint32_t>(CallFrameSlot::callee) * sizeof(Register);
    out->reserved0 = 0;
    out->tagNumber = static_cast<uint64_t>(JSValue::NumberTag);
    out->doubleEncodeOffset = static_cast<uint64_t>(JSValue::DoubleEncodeOffset);
    out->notCellMask = static_cast<uint64_t>(JSValue::NotCellMask);
    out->valueUndefined = JSValue::encode(jsUndefined());
    out->valueNull = JSValue::encode(jsNull());
    out->valueTrue = JSValue::encode(jsBoolean(true));
    out->valueFalse = JSValue::encode(jsBoolean(false));
}

// ---------------------------------------------------------------------------
// Init / lifecycle
// ---------------------------------------------------------------------------

// The bun-webkit static ICU expects the common data to be handed over
// explicitly (u_init otherwise fails with U_FILE_ACCESS_ERROR and every
// Intl/toLocale* API breaks). Same wiring Bun does at startup.
//
// Version-agnostic on purpose: ICU renames every export by its major version
// (`udata_setCommonData` is `udata_setCommonData_78` at link time) and names
// the data blob the same way (`icudt78_dat`). Both names come from the
// prebuilt's own headers — <unicode/udata.h> carries the renaming and
// U_ICUDATA_ENTRY_POINT expands to the blob's symbol — so an ICU bump in the
// prebuilt is a recompile, not an edit.
#if !defined(__APPLE__)
extern "C" const char U_ICUDATA_ENTRY_POINT[];
#endif

extern "C" int32_t rjs_init(const char* options)
{
#if !defined(__APPLE__)
    static bool icuReady = false;
    if (!icuReady) {
        icuReady = true;
        UErrorCode err = U_ZERO_ERROR;
        udata_setCommonData(U_ICUDATA_ENTRY_POINT, &err);
        if (U_FAILURE(err))
            fprintf(stderr, "[redjs] udata_setCommonData failed: %s (%d)\n", u_errorName(err), static_cast<int>(err));
    }
#endif
    JSC::initialize();
    if (options && *options) {
        if (!Options::setOptions(options))
            return RJS_BAD_ARG;
    }
    return RJS_OK;
}

extern "C" int32_t rjs_set_options(const char* options)
{
    if (!options)
        return RJS_BAD_ARG;
    return Options::setOptions(options) ? RJS_OK : RJS_BAD_ARG;
}

extern "C" uint64_t rjs_global(void* g)
{
    return JSValue::encode(JSValue(G(g)));
}

extern "C" void rjs_lock(void* g)
{
    G(g)->vm().apiLock().lock();
}

extern "C" void rjs_unlock(void* g)
{
    G(g)->vm().apiLock().unlock();
}

extern "C" void rjs_drain_microtasks(void* g)
{
    VM& vm = G(g)->vm();
    JSLockHolder lock(vm);
    // Async engine work (WebAssembly.instantiate compilation, Atomics.waitAsync
    // …) resolves its promises through the VM's DeferredWorkTimer, which is
    // normally serviced by a host run loop we don't run — pump it here so
    // those promises settle on the frame tick, then drain the microtasks it
    // queued.
    vm.deferredWorkTimer->doWork(vm);
    vm.drainMicrotasks();
}

// 1 if JS is on this VM's stack right now (any VMEntryScope is live) — the
// "JS execution context stack is empty" test HTML's microtask checkpoint
// hinges on. `web::checkpoint` runs its hooks only when this is 0.
extern "C" int32_t rjs_in_js(void* g)
{
    VM& vm = G(g)->vm();
    JSLockHolder lock(vm);
    return vm.isEntered() ? 1 : 0;
}

extern "C" void rjs_gc(void* g, int32_t mode)
{
    VM& vm = G(g)->vm();
    JSLockHolder lock(vm);
    switch (mode) {
    case 1: vm.heap.collectNow(Sync, CollectionScope::Eden); break;
    case 2: vm.heap.collectAsync(CollectionScope::Full); break;
    default: vm.heap.collectNow(Sync, CollectionScope::Full); break;
    }
}

struct RjsHeapStats {
    uint64_t heapSize;
    uint64_t heapCapacity;
    uint64_t extraMemory;
    uint64_t objectCount;
    uint64_t protectedObjectCount;
    uint64_t globalObjectCount;
};

extern "C" void rjs_heap_stats(void* g, RjsHeapStats* out)
{
    VM& vm = G(g)->vm();
    JSLockHolder lock(vm);
    out->heapSize = vm.heap.size();
    out->heapCapacity = vm.heap.capacity();
    out->extraMemory = vm.heap.extraMemorySize();
    out->objectCount = vm.heap.objectCount();
    out->protectedObjectCount = vm.heap.protectedObjectCount();
    out->globalObjectCount = vm.heap.globalObjectCount();
}

extern "C" void rjs_report_extra_memory(void* g, uint64_t bytes)
{
    VM& vm = G(g)->vm();
    JSLockHolder lock(vm);
    vm.heap.deprecatedReportExtraMemory(static_cast<size_t>(bytes));
}

// ---------------------------------------------------------------------------
// Eval
// ---------------------------------------------------------------------------

static int32_t evalCommon(JSGlobalObject* g, const WTF::String& code, const WTF::String& file, uint64_t* out)
{
    VM& vm = g->vm();
    JSLockHolder lock(vm);
    auto source = makeSource(code, SourceOrigin(), SourceTaintedOrigin::Untainted, file);
    NakedPtr<Exception> exc;
    JSValue result = evaluate(g, source, jsUndefined(), exc);
    if (exc) {
        if (out)
            *out = JSValue::encode(exc->value());
        return RJS_EXCEPTION;
    }
    if (out)
        *out = JSValue::encode(result);
    return RJS_OK;
}

extern "C" int32_t rjs_eval16(void* g, const char16_t* src, size_t len, const char16_t* url, size_t urlLen, uint64_t* out)
{
    WTF::String code(std::span<const char16_t> { src, len });
    WTF::String file = urlLen ? WTF::String(std::span<const char16_t> { url, urlLen }) : WTF::String();
    return evalCommon(G(g), code, file, out);
}

extern "C" int32_t rjs_eval8(void* g, const char* src, size_t len, const char* url, size_t urlLen, uint64_t* out)
{
    WTF::String code = WTF::String::fromUTF8(std::span<const char8_t> { reinterpret_cast<const char8_t*>(src), len });
    WTF::String file = urlLen
        ? WTF::String::fromUTF8(std::span<const char8_t> { reinterpret_cast<const char8_t*>(url), urlLen })
        : WTF::String();
    return evalCommon(G(g), code, file, out);
}

// A cache is an optimisation of this classic-script source, never another
// program. The Rust layer admits only caches generated here or explicitly
// imported from a trusted local artifact after identity/integrity checks.
class RjsCachedSourceProvider final : public StringSourceProvider {
public:
    static Ref<RjsCachedSourceProvider> create(const WTF::String& code, WTF::String file, Ref<CachedBytecode>&& bytecode)
    {
        return adoptRef(*new RjsCachedSourceProvider(code, WTF::move(file), WTF::move(bytecode)));
    }

    RefPtr<CachedBytecode> cachedBytecode() const final
    {
        m_wasRead = true;
        return m_bytecode.ptr();
    }
    void didGenerateUnlinkedCodeBlock(VM&, const SourceCodeKey&, UnlinkedCodeBlock*) const final
    {
        m_wasGenerated = true;
    }
    bool wasUsed() const { return m_wasRead && !m_wasGenerated && Options::useCodeCache(); }

private:
    RjsCachedSourceProvider(const WTF::String& code, WTF::String file, Ref<CachedBytecode>&& bytecode)
        : StringSourceProvider(code, SourceOrigin(), SourceTaintedOrigin::Untainted, WTF::move(file), TextPosition(), SourceProviderSourceType::Program)
        , m_bytecode(WTF::move(bytecode))
    {
    }

    Ref<CachedBytecode> m_bytecode;
    mutable bool m_wasRead { false };
    mutable bool m_wasGenerated { false };
};

extern "C" uint32_t rjs_bytecode_cache_version()
{
    return computeJSCBytecodeCacheVersion();
}

// Snapshot all effective options instead of guessing which JSC flag a future
// engine revision will add to its bytecode format. The caller hashes the text.
extern "C" size_t rjs_bytecode_cache_options(void* g, uint8_t* destination, size_t capacity)
{
    JSLockHolder lock(G(g)->vm());
    WTF::StringBuilder builder;
    Options::dumpAllOptionsInALine(builder);
    auto options = builder.toString().utf8();
    if (destination && capacity >= options.length())
        std::memcpy(destination, options.data(), options.length());
    return options.length();
}

// The handle owns the encoded payload until rjs_bytecode_free. It contains no
// borrowed Rust memory; the bytes can be copied before releasing the handle.
extern "C" int32_t rjs_bytecode_generate(void* global, const char* src, size_t len, const char* url, size_t urlLen,
    uint32_t functionDepth, void** handle, const uint8_t** bytes, size_t* byteLength, uint64_t* out)
{
    if (!global || !handle || !bytes || !byteLength || (!src && len) || (!url && urlLen))
        return RJS_BAD_ARG;
    *handle = nullptr;
    *bytes = nullptr;
    *byteLength = 0;
    auto* g = G(global);
    VM& vm = g->vm();
    JSLockHolder lock(vm);
    auto code = WTF::String::fromUTF8(std::span<const char8_t> { reinterpret_cast<const char8_t*>(src), len });
    auto file = urlLen ? WTF::String::fromUTF8(std::span<const char8_t> { reinterpret_cast<const char8_t*>(url), urlLen }) : WTF::String();
    auto source = makeSource(code, SourceOrigin(), SourceTaintedOrigin::Untainted, file);
    ParserError parserError;
    auto* block = recursivelyGenerateUnlinkedCodeBlockForProgram(vm, source, NoLexicallyScopedFeatures,
        JSParserScriptMode::Classic, { }, parserError, EvalContextType::None, functionDepth, OptimizeBytecode::No);
    if (parserError.isValid()) {
        if (out)
            *out = JSValue::encode(parserError.toErrorObject(g, source));
        return RJS_EXCEPTION;
    }
    if (!block)
        return RJS_BAD_ARG;
    Strong<UnlinkedProgramCodeBlock> rootedBlock(vm, block);
    auto key = sourceCodeKeyForSerializedProgram(vm, source);
    auto bytecode = encodeCodeBlock(vm, key, block, nullptr, BytecodeCacheUpdatable::No);
    if (!bytecode)
        return RJS_BAD_ARG;
    *bytes = bytecode->span().data();
    *byteLength = bytecode->size();
    *handle = bytecode.leakRef();
    return RJS_OK;
}

extern "C" void rjs_bytecode_free(void* handle)
{
    if (handle)
        static_cast<CachedBytecode*>(handle)->deref();
}

extern "C" int32_t rjs_eval8_cached(void* global, const char* src, size_t len, const char* url, size_t urlLen,
    const uint8_t* bytes, size_t byteLength, uint32_t* used, uint64_t* out)
{
    if (!global || !used || (!src && len) || (!url && urlLen) || (!bytes && byteLength))
        return RJS_BAD_ARG;
    *used = 0;
    auto* g = G(global);
    VM& vm = g->vm();
    JSLockHolder lock(vm);
    auto code = WTF::String::fromUTF8(std::span<const char8_t> { reinterpret_cast<const char8_t*>(src), len });
    auto file = urlLen ? WTF::String::fromUTF8(std::span<const char8_t> { reinterpret_cast<const char8_t*>(url), urlLen }) : WTF::String();
    if (!byteLength)
        return evalCommon(g, code, file, out);

    // JSC may decode nested functions after eval returns, or keep pointers into
    // its decoder. Give CachedBytecode ownership, never promise persistence of
    // a caller's slice. Its destructor follows the last decoder's lifetime.
    auto* copy = static_cast<uint8_t*>(std::malloc(byteLength));
    if (!copy)
        return evalCommon(g, code, file, out);
    std::memcpy(copy, bytes, byteLength);
    auto bytecode = CachedBytecode::create(std::span<uint8_t> { copy, byteLength },
        [] (const void* address) { std::free(const_cast<void*>(address)); }, { });
    // Bun omits source text from its cache. Its provider-less
    // isCachedBytecodeStillValid() intentionally rejects such payloads. The
    // ordinary CodeCache decoder receives this runtime provider, validates the
    // format/source key, and falls back to parsing before any execution.
    auto provider = RjsCachedSourceProvider::create(code, WTF::move(file), WTF::move(bytecode));
    SourceCode cachedSource(provider.copyRef());
    NakedPtr<Exception> exception;
    auto result = evaluate(g, cachedSource, jsUndefined(), exception);
    *used = provider->wasUsed() ? 1 : 0;
    // An exception is an executed program's result. Never fall back here: it
    // would repeat any effects performed before the throw.
    if (out)
        *out = JSValue::encode(exception ? exception->value() : result);
    return exception ? RJS_EXCEPTION : RJS_OK;
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

// Mirrored in ffi.rs / C#.
enum RjsKind : uint32_t {
    RJS_KIND_UNDEFINED = 0,
    RJS_KIND_NULL = 1,
    RJS_KIND_BOOL = 2,
    RJS_KIND_INT32 = 3,
    RJS_KIND_DOUBLE = 4,
    RJS_KIND_STRING = 5,
    RJS_KIND_SYMBOL = 6,
    RJS_KIND_BIGINT = 7,
    RJS_KIND_OBJECT = 8,
    RJS_KIND_FUNCTION = 9,
    RJS_KIND_ARRAY = 10,
    RJS_KIND_TYPEDARRAY = 11,
    RJS_KIND_ARRAYBUFFER = 12,
    RJS_KIND_PROMISE = 13,
};

extern "C" uint32_t rjs_kind(void* g, uint64_t v)
{
    VM& vm = G(g)->vm();
    JSLockHolder lock(vm);
    JSValue val = JSValue::decode(v);
    if (val.isUndefined() || !v) return RJS_KIND_UNDEFINED;
    if (val.isNull()) return RJS_KIND_NULL;
    if (val.isBoolean()) return RJS_KIND_BOOL;
    if (val.isInt32()) return RJS_KIND_INT32;
    if (val.isNumber()) return RJS_KIND_DOUBLE;
    if (val.isString()) return RJS_KIND_STRING;
    if (val.isSymbol()) return RJS_KIND_SYMBOL;
    if (val.isBigInt()) return RJS_KIND_BIGINT;
    if (!val.isObject()) return RJS_KIND_OBJECT; // other cells: treat as object
    if (dynamicDowncast<JSArrayBufferView>(val)) return RJS_KIND_TYPEDARRAY;
    if (dynamicDowncast<JSArrayBuffer>(val)) return RJS_KIND_ARRAYBUFFER;
    if (dynamicDowncast<JSPromise>(val)) return RJS_KIND_PROMISE;
    if (isJSArray(val)) return RJS_KIND_ARRAY;
    if (val.isCallable()) return RJS_KIND_FUNCTION;
    return RJS_KIND_OBJECT;
}

extern "C" int32_t rjs_to_number(void* g, uint64_t v, double* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    double d = JSValue::decode(v).toNumber(go);
    uint64_t exc = 0;
    if (takeException(vm, &exc))
        return RJS_EXCEPTION;
    *out = d;
    return RJS_OK;
}

// ToNumber that hands back what it threw (rjs_to_number swallows it), for the
// WebIDL conversions whose exceptions must reach the caller unchanged.
extern "C" int32_t rjs_to_number_x(void* g, uint64_t v, double* out, uint64_t* exc)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    double d = JSValue::decode(v).toNumber(go);
    if (takeException(vm, exc))
        return RJS_EXCEPTION;
    *out = d;
    return RJS_OK;
}

extern "C" int32_t rjs_to_string(void* g, uint64_t v, uint64_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSString* s = JSValue::decode(v).toString(go);
    if (takeException(vm, out))
        return RJS_EXCEPTION;
    *out = JSValue::encode(JSValue(s));
    return RJS_OK;
}

extern "C" int32_t rjs_to_boolean(void* g, uint64_t v)
{
    JSGlobalObject* go = G(g);
    JSLockHolder lock(go->vm());
    return JSValue::decode(v).toBoolean(go) ? 1 : 0;
}

extern "C" int32_t rjs_strict_equals(void* g, uint64_t a, uint64_t b)
{
    JSGlobalObject* go = G(g);
    JSLockHolder lock(go->vm());
    return JSValue::strictEqual(go, JSValue::decode(a), JSValue::decode(b)) ? 1 : 0;
}

// ---------------------------------------------------------------------------
// Strings
// ---------------------------------------------------------------------------

extern "C" int32_t rjs_string16(void* g, const char16_t* p, size_t len, uint64_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    if (!len) {
        *out = JSValue::encode(jsEmptyString(vm));
        return RJS_OK;
    }
    *out = JSValue::encode(jsString(vm, WTF::String(std::span<const char16_t> { p, len })));
    return RJS_OK;
}

extern "C" int32_t rjs_string8(void* g, const char* p, size_t len, uint64_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    if (!len) {
        *out = JSValue::encode(jsEmptyString(vm));
        return RJS_OK;
    }
    auto s = WTF::String::fromUTF8(std::span<const char8_t> { reinterpret_cast<const char8_t*>(p), len });
    if (s.isNull())
        return RJS_BAD_ARG; // invalid UTF-8
    *out = JSValue::encode(jsString(vm, s));
    return RJS_OK;
}

struct RjsStr {
    const void* ptr;
    uint32_t len;
    uint32_t is8bit; // 1 => Latin-1 bytes, 0 => UTF-16 code units
};

extern "C" int32_t rjs_string_info(void* g, uint64_t v, RjsStr* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue val = JSValue::decode(v);
    if (!val.isString())
        return RJS_BAD_ARG;
    auto held = asString(val)->value(go); // resolves ropes; result owned by the JSString cell
    if (takeException(vm, nullptr))
        return RJS_EXCEPTION;
    const WTF::String& w = held;
    auto* impl = w.impl();
    if (!impl || !impl->length()) {
        out->ptr = nullptr;
        out->len = 0;
        out->is8bit = 1;
        return RJS_OK;
    }
    out->len = impl->length();
    if (impl->is8Bit()) {
        out->is8bit = 1;
        out->ptr = impl->span8().data();
    } else {
        out->is8bit = 0;
        out->ptr = impl->span16().data();
    }
    return RJS_OK;
}

// ---------------------------------------------------------------------------
// Atoms (interned property names)
// ---------------------------------------------------------------------------

extern "C" void* rjs_atom16(void* g, const char16_t* p, size_t len)
{
    VM& vm = G(g)->vm();
    JSLockHolder lock(vm);
    return new Identifier(Identifier::fromString(vm, WTF::String(std::span<const char16_t> { p, len })));
}

extern "C" void* rjs_atom8(void* g, const char* p, size_t len)
{
    VM& vm = G(g)->vm();
    JSLockHolder lock(vm);
    auto s = WTF::String::fromUTF8(std::span<const char8_t> { reinterpret_cast<const char8_t*>(p), len });
    if (s.isNull())
        return nullptr;
    return new Identifier(Identifier::fromString(vm, s));
}

extern "C" void rjs_atom_free(void* g, void* atom)
{
    // Identifier destruction derefs an ATOM StringImpl, which must be removed
    // from the owning VM's atom table — and WTF resolves that table through a
    // thread-local that JSLock swaps to the VM's on acquire. Freeing without
    // the lock hits the wrong table and fastfails in AtomStringImpl::remove.
    VM& vm = G(g)->vm();
    JSLockHolder lock(vm);
    delete static_cast<Identifier*>(atom);
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

extern "C" int32_t rjs_get(void* g, uint64_t base, void* atom, uint64_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue r = JSValue::decode(base).get(go, *static_cast<Identifier*>(atom));
    if (takeException(vm, out))
        return RJS_EXCEPTION;
    *out = JSValue::encode(r);
    return RJS_OK;
}

// A read that throws comes back as undefined, as rjs_get callers that ignore the status see it.
extern "C" int32_t rjs_get_many(void* g, uint64_t base, void* const* atoms, size_t n, uint64_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue b = JSValue::decode(base);
    if (!b.isObject())
        return RJS_BAD_ARG;
    for (size_t i = 0; i < n; ++i) {
        JSValue r = b.get(go, *static_cast<Identifier*>(atoms[i]));
        out[i] = takeException(vm, nullptr) ? JSValue::encode(jsUndefined()) : JSValue::encode(r);
    }
    return RJS_OK;
}

extern "C" int32_t rjs_define_many(void* g, uint64_t obj, void* const* atoms, const uint64_t* vals, size_t n, uint32_t attrs)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue base = JSValue::decode(obj);
    if (!base.isObject())
        return RJS_BAD_ARG;
    JSObject* o = asObject(base);
    unsigned a = mapAttributes(attrs);
    for (size_t i = 0; i < n; ++i)
        o->putDirect(vm, *static_cast<Identifier*>(atoms[i]), JSValue::decode(vals[i]), a);
    return RJS_OK;
}

extern "C" int32_t rjs_set(void* g, uint64_t obj, void* atom, uint64_t val)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue base = JSValue::decode(obj);
    if (!base.isObject())
        return RJS_BAD_ARG;
    JSObject* o = asObject(base);
    PutPropertySlot slot(base, false);
    o->methodTable()->put(o, go, *static_cast<Identifier*>(atom), JSValue::decode(val), slot);
    if (takeException(vm, nullptr))
        return RJS_EXCEPTION;
    return RJS_OK;
}

extern "C" int32_t rjs_define(void* g, uint64_t obj, void* atom, uint64_t val, uint32_t attrs)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue base = JSValue::decode(obj);
    if (!base.isObject())
        return RJS_BAD_ARG;
    asObject(base)->putDirect(vm, *static_cast<Identifier*>(atom), JSValue::decode(val), mapAttributes(attrs));
    return RJS_OK;
}

extern "C" int32_t rjs_get_index(void* g, uint64_t base, uint32_t index, uint64_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue r = JSValue::decode(base).get(go, index);
    if (takeException(vm, out))
        return RJS_EXCEPTION;
    *out = JSValue::encode(r);
    return RJS_OK;
}

extern "C" int32_t rjs_set_index(void* g, uint64_t obj, uint32_t index, uint64_t val)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue base = JSValue::decode(obj);
    if (!base.isObject())
        return RJS_BAD_ARG;
    JSObject* o = asObject(base);
    o->methodTable()->putByIndex(o, go, index, JSValue::decode(val), false);
    if (takeException(vm, nullptr))
        return RJS_EXCEPTION;
    return RJS_OK;
}

extern "C" int32_t rjs_has(void* g, uint64_t obj, void* atom)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue base = JSValue::decode(obj);
    if (!base.isObject())
        return -2;
    bool r = asObject(base)->hasProperty(go, *static_cast<Identifier*>(atom));
    if (takeException(vm, nullptr))
        return -1;
    return r ? 1 : 0;
}

extern "C" int32_t rjs_delete(void* g, uint64_t obj, void* atom)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue base = JSValue::decode(obj);
    if (!base.isObject())
        return -2;
    bool r = asObject(base)->deleteProperty(go, *static_cast<Identifier*>(atom));
    if (takeException(vm, nullptr))
        return -1;
    return r ? 1 : 0;
}

// ---------------------------------------------------------------------------
// Objects / arrays / functions / calls
// ---------------------------------------------------------------------------

extern "C" int32_t rjs_object_create(void* g, uint64_t proto, uint64_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue protoVal = JSValue::decode(proto);
    JSObject* o;
    if (protoVal.isObject())
        o = constructEmptyObject(go, asObject(protoVal));
    else if (protoVal.isNull())
        // Object.create(null). The (globalObject, prototype) overload asks the
        // structure cache for a prototype it then dereferences — a null one
        // fastfails; the realm keeps a structure for exactly this.
        o = constructEmptyObject(vm, go->nullPrototypeObjectStructure());
    else
        o = constructEmptyObject(go);
    *out = JSValue::encode(JSValue(o));
    return RJS_OK;
}

extern "C" int32_t rjs_array_create(void* g, uint32_t len, uint64_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSArray* a = constructEmptyArray(go, nullptr, len);
    if (takeException(vm, out))
        return RJS_EXCEPTION;
    *out = JSValue::encode(JSValue(a));
    return RJS_OK;
}

extern "C" int32_t rjs_array_length(void* g, uint64_t v, uint32_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue val = JSValue::decode(v);
    if (isJSArray(val)) {
        *out = downcast<JSArray>(val)->length();
        return RJS_OK;
    }
    if (!val.isObject())
        return RJS_BAD_ARG;
    JSValue lenVal = val.get(go, vm.propertyNames->length);
    uint32_t len = lenVal.toUInt32(go);
    if (takeException(vm, nullptr))
        return RJS_EXCEPTION;
    *out = len;
    return RJS_OK;
}

// `sysvFn` must follow the JSC host-call convention for this build:
// EncodedJSValue (SysV x64) fn(JSGlobalObject*, CallFrame*).
// Rust/Zig can produce these directly; C# goes through the trampoline pool.
extern "C" int32_t rjs_function_create(void* g, const char16_t* name, size_t nameLen, uint32_t arity, void* sysvFn, uint64_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    using RawHostFn = EncodedJSValue(JSC_HOST_CALL_ATTRIBUTES*)(JSGlobalObject*, CallFrame*);
    WTF::String n = nameLen ? WTF::String(std::span<const char16_t> { name, nameLen }) : WTF::String();
    JSFunction* f = JSFunction::create(vm, go, arity, n, reinterpret_cast<RawHostFn>(sysvFn), ImplementationVisibility::Public);
    *out = JSValue::encode(JSValue(f));
    return RJS_OK;
}

struct RjsProfileFunctionInfo {
    uint32_t line;
    uint32_t column;
    uint32_t nameLen;
    uint32_t urlLen;
    char name[192];
    char url[768];
};

// Executable metadata avoids observable function.name/displayName getters.
extern "C" int32_t rjs_profile_function_info(void* g, uint64_t value, RjsProfileFunctionInfo* out)
{
    if (!g || !out)
        return 0;
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSFunction* function = dynamicDowncast<JSFunction>(JSValue::decode(value));
    if (!function)
        return 0;
    std::memset(out, 0, sizeof(*out));
    auto copy = [](const String& value, char* destination, size_t capacity) -> uint32_t {
        auto bytes = value.utf8();
        size_t length = bytes.length() < capacity ? bytes.length() : capacity;
        if (length)
            std::memcpy(destination, bytes.data(), length);
        return static_cast<uint32_t>(length);
    };
    if (function->isHostFunction()) {
        auto* executable = static_cast<NativeExecutable*>(function->executable());
        out->nameLen = copy(executable->name(), out->name, sizeof(out->name));
    } else {
        FunctionExecutable* executable = function->jsExecutable();
        out->nameLen = copy(executable->nameWithoutGC(), out->name, sizeof(out->name));
        if (!executable->source().isNull() && executable->source().provider()) {
            out->urlLen = copy(executable->sourceURL(), out->url, sizeof(out->url));
            out->line = executable->firstLine();
            out->column = executable->startColumn();
        }
    }
    return 1;
}

extern "C" int32_t rjs_call(void* g, uint64_t fnVal, uint64_t thisVal, const uint64_t* args, uint32_t argc, uint64_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue f = JSValue::decode(fnVal);
    auto callData = JSC::getCallData(f);
    if (callData.type == CallData::Type::None)
        return RJS_BAD_ARG;
    MarkedArgumentBuffer buf;
    buf.ensureCapacity(argc);
    for (uint32_t i = 0; i < argc; i++)
        buf.append(JSValue::decode(args[i]));
    if (buf.hasOverflowed())
        return RJS_BAD_ARG;
    NakedPtr<Exception> exc;
    JSValue r = JSC::call(go, f, callData, JSValue::decode(thisVal), buf, exc);
    if (exc) {
        if (out)
            *out = JSValue::encode(exc->value());
        return RJS_EXCEPTION;
    }
    if (out)
        *out = JSValue::encode(r);
    return RJS_OK;
}

extern "C" int32_t rjs_construct(void* g, uint64_t ctorVal, const uint64_t* args, uint32_t argc, uint64_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue c = JSValue::decode(ctorVal);
    auto constructData = JSC::getConstructData(c);
    if (constructData.type == CallData::Type::None)
        return RJS_BAD_ARG;
    MarkedArgumentBuffer buf;
    buf.ensureCapacity(argc);
    for (uint32_t i = 0; i < argc; i++)
        buf.append(JSValue::decode(args[i]));
    if (buf.hasOverflowed())
        return RJS_BAD_ARG;
    JSObject* r = JSC::construct(go, c, constructData, buf);
    if (takeException(vm, out))
        return RJS_EXCEPTION;
    *out = JSValue::encode(JSValue(r));
    return RJS_OK;
}

// ---------------------------------------------------------------------------
// Errors / exceptions
// ---------------------------------------------------------------------------

extern "C" void rjs_throw(void* g, uint64_t err)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    auto scope = DECLARE_THROW_SCOPE(vm);
    throwException(go, scope, JSValue::decode(err));
}

extern "C" int32_t rjs_error_create(void* g, uint32_t kind, const char16_t* msg, size_t len, uint64_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    WTF::String m = len ? WTF::String(std::span<const char16_t> { msg, len }) : WTF::String();
    JSObject* e;
    switch (kind) {
    case 1: e = createTypeError(go, m); break;
    case 2: e = createRangeError(go, m); break;
    case 3: e = createSyntaxError(go, m); break;
    default: e = createError(go, m); break;
    }
    *out = JSValue::encode(JSValue(e));
    return RJS_OK;
}

extern "C" int32_t rjs_error_info(void* g, uint64_t err, uint64_t* msgOut, uint64_t* stackOut)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue e = JSValue::decode(err);

    JSValue msg = e.isObject() ? e.get(go, vm.propertyNames->message) : e;
    if (takeException(vm, nullptr)) { msg = e; }
    JSString* msgStr = msg.toString(go);
    if (takeException(vm, nullptr)) { msgStr = jsEmptyString(vm); }
    *msgOut = JSValue::encode(JSValue(msgStr));

    JSValue stack = e.isObject() ? e.get(go, vm.propertyNames->stack) : jsUndefined();
    if (takeException(vm, nullptr)) { stack = jsUndefined(); }
    if (stack.isUndefined()) {
        *stackOut = JSValue::encode(jsEmptyString(vm));
    } else {
        JSString* stackStr = stack.toString(go);
        if (takeException(vm, nullptr)) { stackStr = jsEmptyString(vm); }
        *stackOut = JSValue::encode(JSValue(stackStr));
    }
    return RJS_OK;
}

extern "C" int32_t rjs_exception_pending(void* g, uint64_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    return takeException(vm, out) ? 1 : 0;
}

// ---------------------------------------------------------------------------
// Typed arrays / ArrayBuffers (introspection; creation goes through the C API)
// ---------------------------------------------------------------------------

struct RjsTaInfo {
    void* data;
    uint32_t length;     // element count
    uint32_t byteLength;
    uint32_t byteOffset;
    uint32_t kind;       // RjsTaKind
    uint32_t detached;
    uint32_t shared;
};

extern "C" int32_t rjs_typedarray_info(void* g, uint64_t v, RjsTaInfo* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    auto* view = dynamicDowncast<JSArrayBufferView>(JSValue::decode(v));
    if (!view)
        return RJS_BAD_ARG;
    out->kind = mapTypedArrayType(typedArrayType(view->type()));
    out->detached = view->isDetached() ? 1 : 0;
    out->shared = view->isShared() ? 1 : 0;
    if (out->detached) {
        out->data = nullptr;
        out->length = 0;
        out->byteLength = 0;
        out->byteOffset = 0;
        return RJS_OK;
    }
    out->data = view->vector();
    out->length = static_cast<uint32_t>(view->length());
    out->byteLength = static_cast<uint32_t>(view->byteLength());
    out->byteOffset = static_cast<uint32_t>(view->byteOffset());
    return RJS_OK;
}

struct RjsAbInfo {
    void* data;
    uint64_t byteLength;
    uint32_t detached;
    uint32_t shared;
};

extern "C" int32_t rjs_arraybuffer_info(void* g, uint64_t v, RjsAbInfo* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    auto* jsBuf = dynamicDowncast<JSArrayBuffer>(JSValue::decode(v));
    if (!jsBuf)
        return RJS_BAD_ARG;
    ArrayBuffer* impl = jsBuf->impl();
    if (!impl) {
        out->data = nullptr;
        out->byteLength = 0;
        out->detached = 1;
        out->shared = 0;
        return RJS_OK;
    }
    out->data = impl->data();
    out->byteLength = impl->byteLength();
    out->detached = out->data ? 0 : 1;
    out->shared = impl->isShared() ? 1 : 0;
    return RJS_OK;
}

extern "C" int32_t rjs_arraybuffer_detach(void* g, uint64_t v)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    auto* jsBuf = dynamicDowncast<JSArrayBuffer>(JSValue::decode(v));
    // A locked buffer (a WebAssembly.Memory's) is not detachable: ECMA-262
    // DetachArrayBuffer throws for it, and the caller says so.
    if (!jsBuf || !jsBuf->impl() || !jsBuf->impl()->isDetachable())
        return RJS_BAD_ARG;
    jsBuf->impl()->detach(vm);
    return RJS_OK;
}

// ---------------------------------------------------------------------------
// Promises
// ---------------------------------------------------------------------------

extern "C" int32_t rjs_promise_info(void* g, uint64_t v, uint32_t* statusOut, uint64_t* resultOut)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    auto* p = dynamicDowncast<JSPromise>(JSValue::decode(v));
    if (!p)
        return RJS_BAD_ARG;
    *statusOut = static_cast<uint32_t>(p->status()); // 0 pending, 1 fulfilled, 2 rejected
    *resultOut = JSValue::encode(p->result());
    return RJS_OK;
}

// ---------------------------------------------------------------------------
// Native classes: constructors, accessors, private-name slots
// ---------------------------------------------------------------------------

#include <JavaScriptCore/GetterSetter.h>
#include <JavaScriptCore/PrivateName.h>

// Like rjs_function_create but constructable: `ctorFn` runs for `new F()` /
// Reflect.construct. Inside the construct callback the CallFrame's THIS slot
// holds new.target (JSC convention for C++ constructors).
extern "C" int32_t rjs_function_create_full(void* g, const char16_t* name, size_t nameLen, uint32_t arity, void* callFn, void* ctorFn, uint64_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    using RawHostFn = EncodedJSValue(JSC_HOST_CALL_ATTRIBUTES*)(JSGlobalObject*, CallFrame*);
    WTF::String n = nameLen ? WTF::String(std::span<const char16_t> { name, nameLen }) : WTF::String();
    JSFunction* f = JSFunction::create(vm, go, arity, n,
        reinterpret_cast<RawHostFn>(callFn), ImplementationVisibility::Public, NoIntrinsic,
        reinterpret_cast<RawHostFn>(ctorFn));
    *out = JSValue::encode(JSValue(f));
    return RJS_OK;
}

// Defines a spec accessor property. `getter`/`setter` are function values (or
// undefined for write-only/read-only).
extern "C" int32_t rjs_define_accessor(void* g, uint64_t obj, void* atom, uint64_t getter, uint64_t setter, uint32_t attrs)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue base = JSValue::decode(obj);
    if (!base.isObject())
        return RJS_BAD_ARG;
    GetterSetter* gs = GetterSetter::create(vm, go, JSValue::decode(getter), JSValue::decode(setter));
    asObject(base)->putDirectAccessor(go, *static_cast<Identifier*>(atom), gs, mapAttributes(attrs) | PropertyAttribute::Accessor);
    return RJS_OK;
}

// A PRIVATE-name atom: properties keyed by it are invisible to all of JS
// (not even Object.getOwnPropertySymbols) — internal slots for native classes.
extern "C" void* rjs_atom_private(void* g, const char* p, size_t len)
{
    VM& vm = G(g)->vm();
    JSLockHolder lock(vm);
    auto s = WTF::String::fromUTF8(std::span<const char8_t> { reinterpret_cast<const char8_t*>(p), len });
    PrivateName pn(PrivateName::PrivateSymbol, s);
    return new Identifier(Identifier::fromUid(pn));
}

// The well-known Symbol.iterator as an atom, so native classes can implement
// the iteration protocol (for..of, spread, Array.from) without evaluating JS.
extern "C" void* rjs_atom_iterator(void* g)
{
    VM& vm = G(g)->vm();
    JSLockHolder lock(vm);
    return new Identifier(vm.propertyNames->iteratorSymbol);
}

// Enqueues `fn` on the VM's real microtask queue (the HTML `queueMicrotask`).
// InvokeFunctionJob is JSC's "call this function" microtask; the prebuilt
// exports VM::queueMicrotask(QueuedTask&&) (the JSGlobalObject convenience
// overloads aren't in the import lib), and QueuedTask is header-constructible.
extern "C" int32_t rjs_queue_microtask(void* g, uint64_t fn)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue f = JSValue::decode(fn);
    if (!f.isCallable())
        return RJS_BAD_ARG;
    vm.queueMicrotask(QueuedTask(nullptr, InternalMicrotask::InvokeFunctionJob, 0, go, f));
    return RJS_OK;
}

// Own enumerable string-keyed property names of an object, as a JS array of
// strings (the structured serializer / object walking).
extern "C" int32_t rjs_own_keys(void* g, uint64_t obj, uint64_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue base = JSValue::decode(obj);
    if (!base.isObject())
        return RJS_BAD_ARG;
    JSObject* o = asObject(base);
    // The Bun fork replaced PropertyNameArray construction with a builder.
    PropertyNameArrayBuilder names(vm, PropertyNameMode::Strings, PrivateSymbolMode::Exclude);
    o->methodTable()->getOwnPropertyNames(o, go, names, DontEnumPropertiesMode::Exclude);
    if (takeException(vm, out))
        return RJS_EXCEPTION;
    JSArray* arr = constructEmptyArray(go, nullptr, 0);
    if (takeException(vm, out))
        return RJS_EXCEPTION;
    unsigned i = 0;
    for (const auto& id : names) {
        arr->putDirectIndex(go, i++, jsString(vm, id.string()));
    }
    *out = JSValue::encode(JSValue(arr));
    return RJS_OK;
}

// ---------------------------------------------------------------------------
// Structured serialization support (webapi/serialize.rs, webapi/domexception.rs)
//
// HTML's StructuredSerializeInternal branches on INTERNAL SLOTS ([[DateValue]],
// [[RegExpMatcher]], [[MapData]], [[ErrorData]], [[ArrayBufferData]]…), which
// no script can observe without side effects — `instanceof` and
// `Object.prototype.toString` are both forgeable. These read the cell's own
// type instead, so a brand is exactly what the engine says it is.
// ---------------------------------------------------------------------------

#include <JavaScriptCore/BigIntObject.h>
#include <JavaScriptCore/BooleanObject.h>
#include <JavaScriptCore/DateInstance.h>
#include <JavaScriptCore/NumberObject.h>
#include <JavaScriptCore/RegExpObject.h>
#include <JavaScriptCore/StringObject.h>
#include <JavaScriptCore/SymbolObject.h>
#include <JavaScriptCore/YarrFlags.h>

// Well-known symbols as atoms: 0 iterator, 1 toStringTag, 2 asyncIterator.
extern "C" void* rjs_atom_wellknown(void* g, uint32_t which)
{
    VM& vm = G(g)->vm();
    JSLockHolder lock(vm);
    switch (which) {
    case 0: return new Identifier(vm.propertyNames->iteratorSymbol);
    case 1: return new Identifier(vm.propertyNames->toStringTagSymbol);
    case 2: return new Identifier(vm.propertyNames->asyncIteratorSymbol);
    default: return nullptr;
    }
}

// Stable brand ids (mirrored in glue.rs).
enum RjsBrand : uint32_t {
    RJS_BRAND_NONE = 0,       // not an object
    RJS_BRAND_PLAIN = 1,      // ordinary JSFinalObject
    RJS_BRAND_ARRAY = 2,
    RJS_BRAND_FUNCTION = 3,
    RJS_BRAND_PROXY = 4,
    RJS_BRAND_DATE = 5,
    RJS_BRAND_REGEXP = 6,
    RJS_BRAND_MAP = 7,
    RJS_BRAND_SET = 8,
    RJS_BRAND_ERROR = 9,
    RJS_BRAND_BOOLEAN_OBJECT = 10,
    RJS_BRAND_NUMBER_OBJECT = 11,
    RJS_BRAND_STRING_OBJECT = 12,
    RJS_BRAND_BIGINT_OBJECT = 13,
    RJS_BRAND_SYMBOL_OBJECT = 14,
    RJS_BRAND_ARRAY_BUFFER = 15,
    RJS_BRAND_SHARED_ARRAY_BUFFER = 16,
    RJS_BRAND_VIEW = 17,      // typed array or DataView (taKind says which)
    RJS_BRAND_PROMISE = 18,
    RJS_BRAND_GLOBAL = 19,
    RJS_BRAND_OTHER = 20,     // any other object with internal slots (WeakMap, generator, arguments, C-API object…)
};

enum : uint32_t {
    RJS_BF_DETACHED = 1,
    RJS_BF_RESIZABLE = 2,        // AB: resizable/growable; view: backed by one
    RJS_BF_LENGTH_TRACKING = 4,  // view: length follows its buffer
    RJS_BF_REDJS_SLOT = 8,       // carries a "redjs#…" private-name slot (a redjs platform object)
};

struct RjsBrandInfo {
    uint32_t brand;
    uint32_t flags;
    uint32_t taKind;
    uint32_t reserved;
    uint64_t byteLength;    // ArrayBuffer
    uint64_t byteOffset;    // view: [[ByteOffset]]
    uint64_t length;        // view: [[ArrayLength]] (DataView: [[ByteLength]])
    uint64_t maxByteLength; // resizable ArrayBuffer
    double number;          // Date: [[DateValue]]
    void* data;             // ArrayBuffer bytes (copy before any allocation)
};

namespace {

// A redjs native class keeps its internal slots under private names minted by
// rjs_atom_private as "redjs#…". Class private fields (`#x`) are private names
// too, but their description never starts with "redjs#", and an ordinary
// object that only has those is still an ordinary object to HTML.
bool hasRedjsSlot(JSGlobalObject* go, JSObject* o)
{
    VM& vm = go->vm();
    PropertyNameArrayBuilder names(vm, PropertyNameMode::Symbols, PrivateSymbolMode::Include);
    o->methodTable()->getOwnPropertyNames(o, go, names, DontEnumPropertiesMode::Include);
    if (takeException(vm, nullptr))
        return false;
    for (const auto& id : names) {
        auto* uid = id.impl();
        if (!uid || !uid->isSymbol() || !static_cast<SymbolImpl*>(uid)->isPrivate())
            continue;
        if (WTF::String(uid).startsWith("redjs#"_s))
            return true;
    }
    return false;
}

} // namespace

extern "C" uint32_t rjs_brand(void* g, uint64_t v, RjsBrandInfo* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    *out = RjsBrandInfo {};
    JSValue val = JSValue::decode(v);
    if (!v || !val.isObject())
        return out->brand = RJS_BRAND_NONE;
    JSObject* o = asObject(val);
    uint32_t b = RJS_BRAND_OTHER;
    switch (o->type()) {
    case ProxyObjectType: b = RJS_BRAND_PROXY; break;
    case GlobalObjectType:
    case GlobalProxyType: b = RJS_BRAND_GLOBAL; break;
    case ArrayType:
    case DerivedArrayType: b = RJS_BRAND_ARRAY; break;
    case JSDateType:
        b = RJS_BRAND_DATE;
        out->number = downcast<DateInstance>(o)->internalNumber();
        break;
    case RegExpObjectType: b = RJS_BRAND_REGEXP; break;
    case JSMapType: b = RJS_BRAND_MAP; break;
    case JSSetType: b = RJS_BRAND_SET; break;
    case ErrorInstanceType: b = RJS_BRAND_ERROR; break;
    case BooleanObjectType: b = RJS_BRAND_BOOLEAN_OBJECT; break;
    case NumberObjectType: b = RJS_BRAND_NUMBER_OBJECT; break;
    case StringObjectType:
    case DerivedStringObjectType: b = RJS_BRAND_STRING_OBJECT; break;
    case JSPromiseType: b = RJS_BRAND_PROMISE; break;
    case ArrayBufferType: {
        ArrayBuffer* impl = downcast<JSArrayBuffer>(o)->impl();
        b = (impl && impl->isShared()) ? RJS_BRAND_SHARED_ARRAY_BUFFER : RJS_BRAND_ARRAY_BUFFER;
        if (!impl || impl->isDetached()) {
            out->flags |= RJS_BF_DETACHED;
            break;
        }
        out->data = impl->data();
        out->byteLength = impl->byteLength();
        if (impl->isResizableOrGrowableShared()) {
            out->flags |= RJS_BF_RESIZABLE;
            out->maxByteLength = impl->maxByteLength().value_or(impl->byteLength());
        }
        break;
    }
    case Int8ArrayType:
    case Uint8ArrayType:
    case Uint8ClampedArrayType:
    case Int16ArrayType:
    case Uint16ArrayType:
    case Int32ArrayType:
    case Uint32ArrayType:
    case Float16ArrayType:
    case Float32ArrayType:
    case Float64ArrayType:
    case BigInt64ArrayType:
    case BigUint64ArrayType:
    case DataViewType: {
        auto* view = downcast<JSArrayBufferView>(o);
        b = RJS_BRAND_VIEW;
        out->taKind = mapTypedArrayType(typedArrayType(view->type()));
        if (view->isDetached())
            out->flags |= RJS_BF_DETACHED;
        if (view->isResizableOrGrowableShared())
            out->flags |= RJS_BF_RESIZABLE;
        if (view->isAutoLength())
            out->flags |= RJS_BF_LENGTH_TRACKING;
        out->byteOffset = view->byteOffsetRaw();
        out->length = view->lengthRaw();
        break;
    }
    default:
        if (dynamicDowncast<BigIntObject>(o))
            b = RJS_BRAND_BIGINT_OBJECT;
        else if (dynamicDowncast<SymbolObject>(o))
            b = RJS_BRAND_SYMBOL_OBJECT;
        else if (o->isCallable())
            b = RJS_BRAND_FUNCTION;
        else if (o->type() == FinalObjectType || o->type() == CompiledObjectType)
            b = RJS_BRAND_PLAIN;
        break;
    }
    if ((b == RJS_BRAND_PLAIN || b == RJS_BRAND_ERROR) && hasRedjsSlot(go, o))
        out->flags |= RJS_BF_REDJS_SLOT;
    out->brand = b;
    return b;
}

// The primitive inside a Boolean/Number/String/BigInt/Symbol wrapper object.
extern "C" int32_t rjs_wrapper_value(void* g, uint64_t v, uint64_t* out)
{
    VM& vm = G(g)->vm();
    JSLockHolder lock(vm);
    JSValue w = JSValue::decode(v);
    JSWrapperObject* wo = nullptr;
    if (auto* x = dynamicDowncast<BooleanObject>(w))
        wo = x;
    else if (auto* x = dynamicDowncast<NumberObject>(w))
        wo = x;
    else if (auto* x = dynamicDowncast<StringObject>(w))
        wo = x;
    else if (auto* x = dynamicDowncast<BigIntObject>(w))
        wo = x;
    else if (auto* x = dynamicDowncast<SymbolObject>(w))
        wo = x;
    if (!wo)
        return RJS_BAD_ARG;
    *out = JSValue::encode(wo->internalValue());
    return RJS_OK;
}

// [[OriginalSource]] and [[OriginalFlags]] of a RegExp, as JS strings.
extern "C" int32_t rjs_regexp_parts(void* g, uint64_t v, uint64_t* pattern, uint64_t* flags)
{
    VM& vm = G(g)->vm();
    JSLockHolder lock(vm);
    auto* r = dynamicDowncast<RegExpObject>(JSValue::decode(v));
    if (!r)
        return RJS_BAD_ARG;
    RegExp* re = r->regExp();
    *pattern = JSValue::encode(jsString(vm, re->pattern()));
    auto fs = Yarr::flagsString(re->flags());
    *flags = JSValue::encode(jsString(vm, WTF::String(fs.span())));
    return RJS_OK;
}

// [[Get]] with a VALUE key (ToPropertyKey) — getters run, exceptions come back.
extern "C" int32_t rjs_get_v(void* g, uint64_t obj, uint64_t key, uint64_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    Identifier pk = JSValue::decode(key).toPropertyKey(go);
    if (takeException(vm, out))
        return RJS_EXCEPTION;
    JSValue r = JSValue::decode(obj).get(go, pk);
    if (takeException(vm, out))
        return RJS_EXCEPTION;
    *out = JSValue::encode(r);
    return RJS_OK;
}

// HasOwnProperty with a VALUE key: 1 / 0, -1 exception (in *exc), -2 not an object.
extern "C" int32_t rjs_has_own_v(void* g, uint64_t obj, uint64_t key, uint64_t* exc)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue base = JSValue::decode(obj);
    if (!base.isObject())
        return -2;
    Identifier pk = JSValue::decode(key).toPropertyKey(go);
    if (takeException(vm, exc))
        return -1;
    bool r = asObject(base)->hasOwnProperty(go, pk);
    if (takeException(vm, exc))
        return -1;
    return r ? 1 : 0;
}

// CreateDataProperty with a VALUE key (index keys land as indices).
extern "C" int32_t rjs_define_v(void* g, uint64_t obj, uint64_t key, uint64_t val)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue base = JSValue::decode(obj);
    if (!base.isObject())
        return RJS_BAD_ARG;
    Identifier pk = JSValue::decode(key).toPropertyKey(go);
    if (takeException(vm, nullptr))
        return RJS_EXCEPTION;
    asObject(base)->putDirectMayBeIndex(go, pk, JSValue::decode(val));
    if (takeException(vm, nullptr))
        return RJS_EXCEPTION;
    return RJS_OK;
}

// An OWN DATA property: 1 (+ value), 0 absent or an accessor, -1 exception.
// HTML's Error serialization reads `message` this way ("if IsDataDescriptor").
extern "C" int32_t rjs_get_own_data(void* g, uint64_t obj, void* atom, uint64_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue base = JSValue::decode(obj);
    if (!base.isObject())
        return 0;
    JSObject* o = asObject(base);
    const Identifier& id = *static_cast<Identifier*>(atom);
    PropertySlot slot(o, PropertySlot::InternalMethodType::GetOwnProperty);
    bool has = o->methodTable()->getOwnPropertySlot(o, go, id, slot);
    if (takeException(vm, out))
        return -1;
    if (!has || slot.isAccessor())
        return 0;
    JSValue r = slot.getValue(go, id);
    if (takeException(vm, out))
        return -1;
    *out = JSValue::encode(r);
    return 1;
}

// A JS string from Latin-1 bytes (not UTF-8: every byte is one code unit).
extern "C" int32_t rjs_string_latin1(void* g, const uint8_t* p, size_t len, uint64_t* out)
{
    VM& vm = G(g)->vm();
    JSLockHolder lock(vm);
    if (!len) {
        *out = JSValue::encode(jsEmptyString(vm));
        return RJS_OK;
    }
    *out = JSValue::encode(jsString(vm, WTF::String(std::span<const Latin1Character> { reinterpret_cast<const Latin1Character*>(p), len })));
    return RJS_OK;
}

// A view's [[ViewedArrayBuffer]] (materialised for a fast typed array — the
// serializer needs the buffer's identity, two views on one buffer stay one).
extern "C" int32_t rjs_view_buffer(void* g, uint64_t v, uint64_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    auto* view = dynamicDowncast<JSArrayBufferView>(JSValue::decode(v));
    if (!view)
        return RJS_BAD_ARG;
    JSArrayBuffer* b = view->possiblySharedJSBuffer(go);
    if (takeException(vm, out))
        return RJS_EXCEPTION;
    if (!b)
        return RJS_BAD_ARG;
    *out = JSValue::encode(JSValue(b));
    return RJS_OK;
}

// 1 (+ length) when `v` is a JSArray whose own enumerable properties are
// exactly its indices 0..length-1 — no holes, no accessors, no extra named
// properties — so it can go over the wire as a bare value list.
extern "C" int32_t rjs_array_dense(void* g, uint64_t v, uint32_t* lenOut)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue val = JSValue::decode(v);
    if (!isJSArray(val))
        return 0;
    JSArray* a = downcast<JSArray>(val);
    uint32_t len = a->length();
    for (uint32_t i = 0; i < len; i++) {
        if (!a->canGetIndexQuickly(i))
            return 0;
    }
    PropertyNameArrayBuilder names(vm, PropertyNameMode::Strings, PrivateSymbolMode::Exclude);
    a->getOwnNonIndexPropertyNames(go, names, DontEnumPropertiesMode::Exclude);
    if (takeException(vm, nullptr))
        return 0;
    for (const auto& id : names) {
        (void)id;
        return 0;
    }
    *lenOut = len;
    return 1;
}

// HTML's property step — "if ! HasOwnProperty(value, key) is true, then let
// inputValue be ? value.[[Get]](key, value)" — as ONE own lookup. Only for an
// ordinary object or an Array (proxies never get here), where the two are the
// same thing: 1 present (*out = the value; a getter has run), 0 absent,
// -1 exception (in *out).
extern "C" int32_t rjs_own_get_v(void* g, uint64_t obj, uint64_t key, uint64_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue base = JSValue::decode(obj);
    if (!base.isObject())
        return 0;
    JSObject* o = asObject(base);
    Identifier pk = JSValue::decode(key).toPropertyKey(go);
    if (takeException(vm, out))
        return -1;
    PropertySlot slot(o, PropertySlot::InternalMethodType::Get);
    bool has = o->methodTable()->getOwnPropertySlot(o, go, pk, slot);
    if (takeException(vm, out))
        return -1;
    if (!has)
        return 0;
    JSValue r = slot.getValue(go, pk);
    if (takeException(vm, out))
        return -1;
    *out = JSValue::encode(r);
    return 1;
}

// rjs_own_get_v for an array-index key, without making a string of it; an
// Array's plain element is read straight out of its butterfly.
extern "C" int32_t rjs_own_get_index(void* g, uint64_t obj, uint32_t index, uint64_t* out)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue base = JSValue::decode(obj);
    if (!base.isObject())
        return 0;
    JSObject* o = asObject(base);
    if (auto* a = dynamicDowncast<JSArray>(o)) {
        if (a->canGetIndexQuickly(index)) {
            *out = JSValue::encode(a->getIndexQuickly(index));
            return 1;
        }
    }
    PropertySlot slot(o, PropertySlot::InternalMethodType::Get);
    bool has = o->methodTable()->getOwnPropertySlotByIndex(o, go, index, slot);
    if (takeException(vm, out))
        return -1;
    if (!has)
        return 0;
    JSValue r = slot.getValue(go, static_cast<uint64_t>(index));
    if (takeException(vm, out))
        return -1;
    *out = JSValue::encode(r);
    return 1;
}

// CreateDataProperty(obj, index, val) — never a setter on the prototype chain.
extern "C" int32_t rjs_define_index(void* g, uint64_t obj, uint32_t index, uint64_t val)
{
    JSGlobalObject* go = G(g);
    VM& vm = go->vm();
    JSLockHolder lock(vm);
    JSValue base = JSValue::decode(obj);
    if (!base.isObject())
        return RJS_BAD_ARG;
    asObject(base)->putDirectIndex(go, index, JSValue::decode(val));
    if (takeException(vm, nullptr))
        return RJS_EXCEPTION;
    return RJS_OK;
}

// The VM's soft stack limit on this thread (0 when it has none yet). The
// serializer recurses natively and stops where JS would: a graph too deep to
// walk is a RangeError, as in a browser, never a native stack overflow.
extern "C" uintptr_t rjs_stack_soft_limit(void* g)
{
    VM& vm = G(g)->vm();
    JSLockHolder lock(vm);
    return reinterpret_cast<uintptr_t>(vm.softStackLimit());
}

// ---------------------------------------------------------------------------
// Memory release (level transitions / background): full GC + return free
// malloc pages to the OS (bmalloc scavenge).
// ---------------------------------------------------------------------------

#include <wtf/FastMalloc.h>

// ---------------------------------------------------------------------------
// JSC's own timers.
//
// The incremental sweeper, the eden/full GC activity callbacks and the
// stop-if-necessary timer are JSRunLoopTimers: RunLoop timers on the thread
// that created the VM. This prebuilt's RunLoop (USE_BUN_EVENT_LOOP, with
// Bun's hooks left at their defaults) is the GENERIC one, whose timers fire
// only when something runs that RunLoop — and nothing did. So no timer-driven
// collection ever ran: the heap grew by allocation-triggered eden collections,
// was never swept in the background, and never shrank. The Argon client sat
// at a 280 MB GC heap capacity for 85 MB of live objects after two idle
// minutes; one full collection took it to 110 MB and 175 MB of RAM went back
// (vue-native research/reports/m26/perf-mem-2.md).
//
// One non-blocking turn (RunMode::Iterate: the due timers and the dispatched
// functions, then return) from the pump, between waves, is how a host that
// owns the loop services them; the timers then decide WHEN to collect and
// sweep, exactly as in Safari or Bun.
// ---------------------------------------------------------------------------

#include <wtf/RunLoop.h>

extern "C" void rjs_run_loop_cycle()
{
    WTF::RunLoop::cycle();
}

// rjs_release_memory, after throwing away all compiled code (bytecode, JIT)
// that is not running — what JSC's VM::shrinkFootprintWhenIdle does. Every
// function compiles again from its source the next time it is called. A
// measuring tool: what is left after it is the program's DATA, and the
// difference is what its CODE held.
extern "C" void rjs_release_memory_and_code(void* g)
{
    VM& vm = G(g)->vm();
    {
        JSLockHolder lock(vm);
        vm.deleteAllCode(DeleteAllCodeIfNotCollecting);
        vm.heap.collectNow(Sync, CollectionScope::Full);
        vm.heap.collectNow(Sync, CollectionScope::Full);
    }
    WTF::releaseFastMallocFreeMemory();
}

extern "C" void rjs_release_memory(void* g)
{
    VM& vm = G(g)->vm();
    {
        JSLockHolder lock(vm);
        vm.heap.collectNow(Sync, CollectionScope::Full);
        vm.heap.collectNow(Sync, CollectionScope::Full); // finalize weaks/finalizers freed by pass 1
    }
    WTF::releaseFastMallocFreeMemory();
}
