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
#include <JavaScriptCore/JSGlobalObject.h>
#include <JavaScriptCore/JSLock.h>
#include <JavaScriptCore/JSPromise.h>
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
extern "C" const char icudt73_dat[];
extern "C" void udata_setCommonData_73(const void* data, int32_t* err);

extern "C" int32_t rjs_init(const char* options)
{
    static bool icuReady = false;
    if (!icuReady) {
        icuReady = true;
        int32_t err = 0;
        udata_setCommonData_73(icudt73_dat, &err);
        if (err > 0)
            fprintf(stderr, "[redjs] udata_setCommonData failed: %d\n", err);
    }
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
        o = constructEmptyObject(go, static_cast<JSObject*>(nullptr));
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
    if (!jsBuf || !jsBuf->impl())
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

// Own enumerable string-keyed property names of an object, as a JS array of
// strings (structuredClone / object walking).
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
// Memory release (level transitions / background): full GC + return free
// malloc pages to the OS (bmalloc scavenge).
// ---------------------------------------------------------------------------

#include <wtf/FastMalloc.h>

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
