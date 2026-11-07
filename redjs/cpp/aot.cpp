// Native images are a separate execution backend, never a bytecode cache.
// Its compiler consumes the exact pinned Bun/WebKit wire schema.
#include "config.h"
#include <JavaScriptCore/JavaScript.h>
#include <JavaScriptCore/ExceptionScope.h>
#include <JavaScriptCore/AOTBuiltinTableBindings.h>
#include <JavaScriptCore/AOTCompile.h>
#include <JavaScriptCore/AOTCompileOptionWords.h>
#include <JavaScriptCore/AOTHost.h>
#include <JavaScriptCore/AOTImageLoading.h>
#include <JavaScriptCore/AOTModuleHost.h>
#include <JavaScriptCore/AOTRealmStartup.h>
#include <JavaScriptCore/APICast.h>
#include <JavaScriptCore/Completion.h>
#include <JavaScriptCore/Exception.h>
#include <JavaScriptCore/GlobalObjectMethodTable.h>
#include <JavaScriptCore/JSGlobalObjectInlines.h>
#include <JavaScriptCore/JSCInlines.h>
#include <JavaScriptCore/JSPromise.h>
#include <JavaScriptCore/MarkedBlockCage.h>
#include <JavaScriptCore/Options.h>
#include <JavaScriptCore/RuntimeFlags.h>
#include <JavaScriptCore/VM.h>
#include <wtf/text/StringBuilder.h>
#include <atomic>
#include <cstring>
#include <memory>
#include <unordered_set>
#include <windows.h>

using namespace JSC;
using namespace WTF;

extern "C" int32_t rjs_init(const char*);

namespace {
class AotCatchScope final : public ExceptionScope {
public:
    ALWAYS_INLINE AotCatchScope(VM& vm) : ExceptionScope(vm) { }
};
struct RjsAotCompilation {
    std::unique_ptr<AOT::CompileOutput> output;
    CString<char8_t> diagnostic;
};
std::atomic<bool> contextWasCreated { false };
std::atomic<bool> compilerInitialized { false };
thread_local bool imageActive { false };
thread_local std::unordered_set<JSGlobalObject*> startedRealms;
const AOT::ModuleHost moduleHost { };

void reportUncaught(JSGlobalObject*, Exception*)
{
    std::fprintf(stderr, "[redjs AOT] uncaught exception at event loop\n");
}

const GlobalObjectMethodTable methodTable {
    &JSGlobalObject::supportsRichSourceInfo,
    &JSGlobalObject::shouldInterruptScript,
    &JSGlobalObject::javaScriptRuntimeFlags,
    &JSGlobalObject::shouldInterruptScriptBeforeTimeout,
    nullptr, nullptr, nullptr, nullptr, nullptr, nullptr,
    &JSGlobalObject::promiseRejectionTracker,
    reportUncaught,
    &JSGlobalObject::currentScriptExecutionOwner,
    &JSGlobalObject::scriptExecutionStatus,
    &JSGlobalObject::reportViolationForUnsafeEval,
    nullptr, nullptr, nullptr,
    &JSGlobalObject::deriveShadowRealmGlobalObject,
    &JSGlobalObject::codeForEval,
    &JSGlobalObject::canCompileStrings,
    &JSGlobalObject::trustedScriptStructure,
    nullptr, nullptr, &moduleHost,
};
}

extern "C" void* rjs_aot_compile(const uint8_t* program, size_t length, const uint8_t* identity)
{
    if ((!program && length) || !identity || imageActive || contextWasCreated.load(std::memory_order_acquire))
        return nullptr;
    compilerInitialized.store(true, std::memory_order_release);
    if (rjs_init(nullptr))
        return nullptr;
    auto compilation = std::make_unique<RjsAotCompilation>();
    auto options = AOT::makeDefaultCompileOptions();
    options.target.referenceWidth = AOT::Wire::ReferenceWidth::Narrow;
    options.target.imageForm = AOT::Wire::ImageForm::PositionIndependent;
    options.threadCount = 1;
    std::memcpy(options.executableIdentity.data(), identity, 16);
    compilation->output = AOT::compileProgram({ program, length }, options);
    StringBuilder messages;
    if (!compilation->output)
        messages.append("native compiler returned no output"_s);
    else {
        const auto& diagnostics = compilation->output->diagnostics;
        for (size_t index = 0; index < diagnostics.userDiagnostics().size(); ++index) {
            messages.append(diagnostics.messageOfUserDiagnostic(index));
            messages.append('\n');
        }
        for (size_t index = 0; index < diagnostics.internalErrors().size(); ++index) {
            messages.append(diagnostics.messageOfInternalError(index));
            messages.append('\n');
        }
    }
    compilation->diagnostic = messages.toString().utf8();
    return compilation.release();
}

extern "C" size_t rjs_aot_compilation_image(void* handle, const uint8_t** bytes)
{
    auto* compilation = static_cast<RjsAotCompilation*>(handle);
    if (!compilation || !compilation->output || compilation->output->diagnostics.hasErrors())
        return 0;
    *bytes = compilation->output->image.span().data();
    return compilation->output->image.size();
}

extern "C" size_t rjs_aot_compilation_entries(void* handle, const uint32_t** entries)
{
    auto* compilation = static_cast<RjsAotCompilation*>(handle);
    if (!compilation || !compilation->output)
        return 0;
    *entries = compilation->output->entryModuleIDs.span().data();
    return compilation->output->entryModuleIDs.size();
}

extern "C" size_t rjs_aot_compilation_diagnostic(void* handle, const uint8_t** bytes)
{
    auto* compilation = static_cast<RjsAotCompilation*>(handle);
    if (!compilation)
        return 0;
    *bytes = reinterpret_cast<const uint8_t*>(compilation->diagnostic.data());
    return compilation->diagnostic.length();
}

extern "C" void rjs_aot_compilation_free(void* handle)
{
    delete static_cast<RjsAotCompilation*>(handle);
}

extern "C" uint32_t rjs_aot_load_file(const char16_t* path, size_t length, const uint8_t* identity)
{
    using AOT::Wire::ImageVerdict;
    if (!path || !identity || contextWasCreated.load(std::memory_order_acquire))
        return static_cast<uint32_t>(ImageVerdict::AlreadyInUse);
    if (compilerInitialized.load(std::memory_order_acquire))
        return static_cast<uint32_t>(ImageVerdict::OptionsConflict);
    std::wstring terminated(reinterpret_cast<const wchar_t*>(path), length);
    HANDLE file = CreateFileW(terminated.c_str(), GENERIC_READ | GENERIC_EXECUTE,
        FILE_SHARE_READ | FILE_SHARE_DELETE, nullptr, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr);
    LARGE_INTEGER size;
    if (file == INVALID_HANDLE_VALUE)
        return static_cast<uint32_t>(ImageVerdict::CannotReadExecutable);
    if (!GetFileSizeEx(file, &size) || size.QuadPart <= 0) {
        CloseHandle(file);
        return static_cast<uint32_t>(ImageVerdict::Truncated);
    }
    HANDLE mapping = CreateFileMappingW(file, nullptr, PAGE_WRITECOPY, 0, 0, nullptr);
    void* bytes = mapping ? MapViewOfFile(mapping, FILE_MAP_COPY, 0, 0, 0) : nullptr;
    if (mapping)
        CloseHandle(mapping);
    if (!bytes) {
        CloseHandle(file);
        return static_cast<uint32_t>(ImageVerdict::CannotMapCode);
    }
    Options::initialize([] {
        Options::useAOT() = true;
        Options::useJIT() = false;
        Options::useSharedArrayBuffer() = true;
        Options::useTemporal() = true;
        Options::useExplicitResourceManagement() = true;
    });
    if (rjs_init(nullptr) || !Options::useAOT() || Options::useJIT()) {
        UnmapViewOfFile(bytes); CloseHandle(file);
        return static_cast<uint32_t>(ImageVerdict::OptionsConflict);
    }
    AOT::BuiltinBindings::bindNativeHelpersOfEngine();
    std::array<uint8_t, 16> expected;
    std::memcpy(expected.data(), identity, 16);
    auto verdict = AOT::useImage({ static_cast<const uint8_t*>(bytes), static_cast<size_t>(size.QuadPart) },
        AOT::ImageFile { reinterpret_cast<intptr_t>(file), 0 }, expected);
    if (verdict != ImageVerdict::Accepted) {
        UnmapViewOfFile(bytes);
        CloseHandle(file);
        return static_cast<uint32_t>(verdict);
    }
    // Accepted images borrow their mappings and file for the thread's lifetime.
    if (!AOT::Image::current()->header().options.useWideReferences() && !MarkedBlockCage::require())
        return static_cast<uint32_t>(ImageVerdict::NeedsCellCage);
    imageActive = true;
    return static_cast<uint32_t>(verdict);
}

extern "C" void* rjs_aot_create_context()
{
    contextWasCreated.store(true, std::memory_order_release);
    if (!imageActive)
        return nullptr;
    Ref<VM> vm = VM::createContextGroup();
    JSLockHolder lock(vm.ptr());
    auto* global = JSGlobalObject::createWithCustomMethodTable(vm.get(),
        JSGlobalObject::createStructure(vm.get(), jsNull()), &methodTable);
    startedRealms.erase(global);
    return const_cast<OpaqueJSContext*>(JSGlobalContextRetain(toGlobalRef(global)));
}

extern "C" int32_t rjs_aot_run(void* context, uint32_t module, uint64_t* result)
{
    if (!context || !module || !imageActive)
        return 2;
    const auto* image = AOT::Image::current();
    if (!image || module > image->programTables().modules().size() || !image->programTables().modules()[module - 1].isEntryPoint())
        return 2;
    auto* global = static_cast<JSGlobalObject*>(context);
    auto& vm = global->vm();
    JSLockHolder lock(vm);
    AotCatchScope scope(vm);
    if (startedRealms.insert(global).second)
        AOT::hostDidCreateRealm(global);
    auto* promise = AOT::importModule(global, AOT::ModuleID(module - 1), std::nullopt);
    if (scope.exception()) {
        *result = JSValue::encode(scope.exception()->value());
        (void)scope.tryClearException();
        return 1;
    }
    vm.drainMicrotasks();
    if (promise->status() == JSPromise::Status::Rejected) {
        *result = JSValue::encode(promise->result());
        return 1;
    }
    *result = JSValue::encode(promise);
    return promise->status() == JSPromise::Status::Pending ? 3 : 0;
}

extern "C" int32_t rjs_aot_is_active()
{
    return imageActive ? 1 : 0;
}

extern "C" void rjs_aot_forget_context(void* context)
{
    startedRealms.erase(static_cast<JSGlobalObject*>(context));
}
