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
#include <algorithm>
#include <cstring>
#include <memory>
#include <mutex>
#include <unordered_set>
#include <utility>
#if defined(_WIN32)
#include <windows.h>
#else
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>
#include <climits>
#endif

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
std::once_flag nativeHelpersInitialized;
thread_local bool imageActive { false };
// Profile values are part of the loader ABI, never of the image ABI.
enum class MemoryProfile : uint32_t { Default = 0, Compact = 1 };
constexpr uint32_t noMemoryProfile = UINT32_MAX;
std::atomic<uint32_t> initializedMemoryProfile { noMemoryProfile };
thread_local MemoryProfile imageMemoryProfile { MemoryProfile::Default };
thread_local const void* initialImageView { nullptr };
thread_local std::unordered_set<JSGlobalObject*> startedRealms;
const AOT::ModuleHost moduleHost { };

void registerEmbeddingHost()
{
    // Engine-only mode locks built-ins during realm creation. A hosted realm
    // lets the embedder install Web APIs before hostDidCreateRealm() locks it.
    AOT::BuiltinBindings::bindNativeHelpersOfEngine();
    // registerHost() retains this process-lifetime set. Keep the engine helpers:
    // an empty host set would hide direct helpers used by compiled code.
    static const AOT::BuiltinBindingSet bindings {
        .directFunctions = AOT::BuiltinBindings::directFunctions(),
    };
    AOT::BuiltinBindings::registerHost(bindings);
}
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

namespace {
// Both entry points transfer an owned file here. Only Accepted releases it
// and the view into the engine's process-lifetime borrow; every failure drops
// the RAII owners, including the cell-cage check before image publication.
struct OwnedHandle {
#if defined(_WIN32)
    HANDLE value;
    explicit OwnedHandle(HANDLE value) : value(value) { }
    OwnedHandle(const OwnedHandle&) = delete;
    OwnedHandle& operator=(const OwnedHandle&) = delete;
    ~OwnedHandle() { if (value && value != INVALID_HANDLE_VALUE) CloseHandle(value); }
    HANDLE release() { return std::exchange(value, INVALID_HANDLE_VALUE); }
#else
    int value;
    explicit OwnedHandle(int value) : value(value) { }
    OwnedHandle(const OwnedHandle&) = delete;
    OwnedHandle& operator=(const OwnedHandle&) = delete;
    ~OwnedHandle() { if (value >= 0) close(value); }
    int release() { return std::exchange(value, -1); }
#endif
};
struct OwnedView {
    void* value;
    size_t length;
    explicit OwnedView(void* value, size_t length) : value(value), length(length) { }
    OwnedView(const OwnedView&) = delete;
    OwnedView& operator=(const OwnedView&) = delete;
    ~OwnedView() {
#if defined(_WIN32)
        if (value) UnmapViewOfFile(value);
#else
        if (value) munmap(value, length);
#endif
    }
    void* release() { return std::exchange(value, nullptr); }
};
AOT::Wire::ImageVerdict checkLoad(uint32_t rawProfile)
{
    using AOT::Wire::ImageVerdict;
    if (rawProfile > static_cast<uint32_t>(MemoryProfile::Compact))
        return ImageVerdict::OptionsConflict;
    auto initializedProfile = initializedMemoryProfile.load(std::memory_order_acquire);
    if (initializedProfile != noMemoryProfile && initializedProfile != rawProfile)
        return ImageVerdict::OptionsConflict;
    if (contextWasCreated.load(std::memory_order_acquire))
        return ImageVerdict::AlreadyInUse;
    if (compilerInitialized.load(std::memory_order_acquire))
        return ImageVerdict::OptionsConflict;
    return ImageVerdict::Accepted;
}
uint32_t loadOwnedFile(decltype(OwnedHandle::value) owned, const uint8_t* identity, uint32_t rawProfile)
{
    using AOT::Wire::ImageVerdict;
    OwnedHandle file(owned);
    auto preflight = checkLoad(rawProfile);
    if (preflight != ImageVerdict::Accepted)
        return static_cast<uint32_t>(preflight);
    if (!identity)
        return static_cast<uint32_t>(ImageVerdict::AlreadyInUse);
    auto profile = static_cast<MemoryProfile>(rawProfile);
    auto initializedProfile = initializedMemoryProfile.load(std::memory_order_acquire);
#if defined(_WIN32)
    LARGE_INTEGER size;
    if (!GetFileSizeEx(file.value, &size) || size.QuadPart <= 0)
        return static_cast<uint32_t>(ImageVerdict::Truncated);
    // Immutable metadata/input view; the engine maps executable regions and
    // writable private clones separately through this exact retained file.
    OwnedHandle mapping(CreateFileMappingW(file.value, nullptr, PAGE_READONLY, 0, 0, nullptr));
    auto fileSize = static_cast<size_t>(size.QuadPart);
    OwnedView view(mapping.value ? MapViewOfFile(mapping.value, FILE_MAP_READ, 0, 0, 0) : nullptr, fileSize);
#else
    struct stat metadata;
    if (fstat(file.value, &metadata) || !S_ISREG(metadata.st_mode) || metadata.st_size <= 0
        || static_cast<uint64_t>(metadata.st_size) > SIZE_MAX)
        return static_cast<uint32_t>(ImageVerdict::Truncated);
    auto fileSize = static_cast<size_t>(metadata.st_size);
    auto* mapped = mmap(nullptr, fileSize, PROT_READ, MAP_PRIVATE, file.value, 0);
    OwnedView view(mapped == MAP_FAILED ? nullptr : mapped, fileSize);
#endif
    if (!view.value)
        return static_cast<uint32_t>(ImageVerdict::CannotMapCode);
    bool customized = false;
    Options::initialize([&] {
        customized = true;
        initializedMemoryProfile.store(rawProfile, std::memory_order_release);
        if (profile == MemoryProfile::Compact) {
            Options::structureHeapSizeInKB() = 262144;
            Options::markedBlockCageSizeInMB() = 1024;
        }
        Options::useAOT() = true;
        Options::useJIT() = false;
        Options::useSharedArrayBuffer() = true;
        Options::useTemporal() = true;
        Options::useExplicitResourceManagement() = true;
    });
    if (rjs_init(nullptr) || !Options::useAOT() || Options::useJIT()
        || (profile == MemoryProfile::Compact && ((!customized && initializedProfile != rawProfile)
            || Options::structureHeapSizeInKB() != 262144 || Options::markedBlockCageSizeInMB() != 1024))) {
        return static_cast<uint32_t>(ImageVerdict::OptionsConflict);
    }
    // Register the embedding host before any VM/realm exists, once even if an image is rejected.
    std::call_once(nativeHelpersInitialized, registerEmbeddingHost);
    std::array<uint8_t, 16> expected;
    std::memcpy(expected.data(), identity, 16);
    std::span<const uint8_t> imageBytes { static_cast<const uint8_t*>(view.value), fileSize };
    // Validate before reading the header and before publishing a borrowed view.
    auto checked = AOT::Image::check(imageBytes, expected);
    if (checked != ImageVerdict::Accepted)
        return static_cast<uint32_t>(checked);
    const auto& header = *reinterpret_cast<const AOT::ImageHeader*>(view.value);
    if (!header.options.useWideReferences() && !MarkedBlockCage::require())
        return static_cast<uint32_t>(ImageVerdict::NeedsCellCage);
#if defined(_WIN32)
    auto descriptor = reinterpret_cast<intptr_t>(file.value);
#else
    auto descriptor = static_cast<intptr_t>(file.value);
#endif
    auto verdict = AOT::useImage(imageBytes, AOT::ImageFile { descriptor, 0 }, expected);
    if (verdict != ImageVerdict::Accepted)
        return static_cast<uint32_t>(verdict);
    imageMemoryProfile = profile;
    initialImageView = view.release();
    file.release();
    imageActive = true;
    return static_cast<uint32_t>(verdict);
}
}

extern "C" uint32_t rjs_aot_load_file_with_profile(const char16_t* path, size_t length, const uint8_t* identity, uint32_t rawProfile)
{
    auto verdict = checkLoad(rawProfile);
    if (verdict != AOT::Wire::ImageVerdict::Accepted)
        return static_cast<uint32_t>(verdict);
    if (!path || !identity)
        return static_cast<uint32_t>(AOT::Wire::ImageVerdict::AlreadyInUse);
#if defined(_WIN32)
    std::wstring terminated(reinterpret_cast<const wchar_t*>(path), length);
    HANDLE file = CreateFileW(terminated.c_str(), GENERIC_READ | GENERIC_EXECUTE,
        FILE_SHARE_READ | FILE_SHARE_DELETE, nullptr, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr);
    if (file == INVALID_HANDLE_VALUE)
        return static_cast<uint32_t>(AOT::Wire::ImageVerdict::CannotReadExecutable);
#else
    // The public C ABI keeps UTF-16 paths on every platform. Rust's Unix API
    // uses the descriptor entry point so non-UTF-8 filesystem paths also work.
    if (std::find(path, path + length, char16_t { 0 }) != path + length)
        return static_cast<uint32_t>(AOT::Wire::ImageVerdict::CannotReadExecutable);
    auto terminated = String(std::span<const char16_t> { path, length }).utf8();
    int file = open(reinterpret_cast<const char*>(terminated.data()), O_RDONLY | O_CLOEXEC);
    if (file < 0)
        return static_cast<uint32_t>(AOT::Wire::ImageVerdict::CannotReadExecutable);
#endif
    return loadOwnedFile(file, identity, rawProfile);
}

extern "C" uint32_t rjs_aot_load_handle_with_profile(intptr_t rawFile, const uint8_t* identity, uint32_t rawProfile)
{
    auto verdict = checkLoad(rawProfile);
    if (verdict != AOT::Wire::ImageVerdict::Accepted)
        return static_cast<uint32_t>(verdict);
    if (!identity)
        return static_cast<uint32_t>(AOT::Wire::ImageVerdict::AlreadyInUse);
#if defined(_WIN32)
    HANDLE original = reinterpret_cast<HANDLE>(rawFile);
    if (!original || original == INVALID_HANDLE_VALUE)
        return static_cast<uint32_t>(AOT::Wire::ImageVerdict::CannotReadExecutable);
    HANDLE duplicate = INVALID_HANDLE_VALUE;
    if (!DuplicateHandle(GetCurrentProcess(), original, GetCurrentProcess(), &duplicate, 0, FALSE, DUPLICATE_SAME_ACCESS))
        return static_cast<uint32_t>(AOT::Wire::ImageVerdict::CannotReadExecutable);
#else
    if (rawFile < 0 || rawFile > INT_MAX)
        return static_cast<uint32_t>(AOT::Wire::ImageVerdict::CannotReadExecutable);
    int duplicate = fcntl(static_cast<int>(rawFile), F_DUPFD_CLOEXEC, 0);
    if (duplicate < 0)
        return static_cast<uint32_t>(AOT::Wire::ImageVerdict::CannotReadExecutable);
#endif
    return loadOwnedFile(duplicate, identity, rawProfile);
}

// Retain the original C loader ABI and its engine-default capacity policy.
extern "C" uint32_t rjs_aot_load_file(const char16_t* path, size_t length, const uint8_t* identity)
{
    return rjs_aot_load_file_with_profile(path, length, identity, static_cast<uint32_t>(MemoryProfile::Default));
}

extern "C" uint32_t rjs_aot_memory_settings(uint64_t* structureHeapKB, uint64_t* cellCageMB, uint32_t* readOnly)
{
    if (!imageActive)
        return noMemoryProfile;
    if (structureHeapKB)
        *structureHeapKB = Options::structureHeapSizeInKB();
    if (cellCageMB)
        *cellCageMB = Options::markedBlockCageSizeInMB();
    if (readOnly) {
#if defined(_WIN32)
        MEMORY_BASIC_INFORMATION info { };
        *readOnly = VirtualQuery(initialImageView, &info, sizeof(info)) == sizeof(info)
            && info.Protect == PAGE_READONLY;
#else
        *readOnly = initialImageView != nullptr; // OwnedView was mapped PROT_READ.
#endif
    }
    return static_cast<uint32_t>(imageMemoryProfile);
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
    // Custom embedding realms must apply the image's own evaluation policy,
    // just as the engine's normal host does. This is a no-op for images that
    // explicitly admit unknown JavaScript; closed images keep JS eval disabled.
    AOT::refuseEvaluationUnlessAdmitted(global);
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
