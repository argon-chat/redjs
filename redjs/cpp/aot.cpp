// Native images are a separate execution backend, never a bytecode cache.
// Its compiler consumes the exact pinned Bun/WebKit wire schema.
#include "config.h"
#include <JavaScriptCore/JavaScript.h>
#include <JavaScriptCore/ExceptionScope.h>
#include <JavaScriptCore/AOTBuiltinTableBindings.h>
#include <JavaScriptCore/AOTDriver.h>
#include <JavaScriptCore/AOTCompileOptionWords.h>
#include <JavaScriptCore/AOTHost.h>
#include <JavaScriptCore/AOTImageLoading.h>
#include <JavaScriptCore/AOTFaultTable.h>
#include <JavaScriptCore/AOTImage.h>
#include <JavaScriptCore/WasmCodeInImage.h>
#include <wtf/AccessibleAddress.h>
#include <wtf/threads/Signals.h>
#include <JavaScriptCore/AOTModuleHost.h>
#include <JavaScriptCore/AOTRealmStartup.h>
#include <JavaScriptCore/AOTWireDiagnosticColumns.h>
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
thread_local bool imageAdmitsHostFiles { false };
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

#if REDJS_AOT_COMPILER
extern "C" void* rjs_aot_compile(const uint8_t* program, size_t length, const uint8_t* identity)
{
    if ((!program && length) || !identity || imageActive || contextWasCreated.load(std::memory_order_acquire))
        return nullptr;
    compilerInitialized.store(true, std::memory_order_release);
    if (rjs_init(nullptr))
        return nullptr;
    // A front end that writes the program to a file puts the options it compiled with behind it.
    if (length > sizeof(AOT::Wire::CompileOptions)) {
        AOT::Wire::CompileOptions behind;
        std::memcpy(&behind, program + length - sizeof(behind), sizeof(behind));
        if (behind.size == sizeof(behind))
            length -= sizeof(behind);
    }
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
        const auto& output = *compilation->output;
        auto text = output.section(AOT::Wire::ResultSection::Text);
        auto append = [&](AOT::Wire::TextRange range) {
            if (range.offset <= text.size() && range.length <= text.size() - range.offset)
                messages.append(String::fromUTF8ReplacingInvalidSequences(text.subspan(range.offset, range.length)));
            messages.append('\n');
        };
        for (const auto& record : output.records<AOT::Wire::UserDiagnosticRecord>(AOT::Wire::ResultSection::UserDiagnostics))
            append(record.message);
        for (const auto& record : output.records<AOT::Wire::InternalErrorRecord>(AOT::Wire::ResultSection::InternalErrors))
            append(record.message);
    }
    compilation->diagnostic = messages.toString().utf8();
    return compilation.release();
}

static bool compiledWithoutErrors(const AOT::CompileOutput& output)
{
    if (!output.hasImage() || !output.section(AOT::Wire::ResultSection::InternalErrors).empty())
        return false;
    for (const auto& record : output.records<AOT::Wire::UserDiagnosticRecord>(AOT::Wire::ResultSection::UserDiagnostics)) {
        if (AOT::Wire::severityOf(record.kind) == AOT::Wire::DiagnosticSeverity::Error)
            return false;
    }
    return true;
}

extern "C" size_t rjs_aot_compilation_image(void* handle, const uint8_t** bytes)
{
    auto* compilation = static_cast<RjsAotCompilation*>(handle);
    if (!compilation || !compilation->output || !compiledWithoutErrors(*compilation->output))
        return 0;
    auto image = compilation->output->section(AOT::Wire::ResultSection::Image);
    *bytes = image.data();
    return image.size();
}

extern "C" size_t rjs_aot_compilation_entries(void* handle, const uint32_t** entries)
{
    auto* compilation = static_cast<RjsAotCompilation*>(handle);
    if (!compilation || !compilation->output)
        return 0;
    auto ids = compilation->output->records<uint32_t>(AOT::Wire::ResultSection::EntryModuleIds);
    *entries = ids.data();
    return ids.size();
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
#endif

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
        return ImageVerdict::OptionsConflict;
    if (compilerInitialized.load(std::memory_order_acquire))
        return ImageVerdict::OptionsConflict;
    return ImageVerdict::Accepted;
}
AOT::Wire::ImageVerdict initializeForImage(uint32_t rawProfile)
{
    using AOT::Wire::ImageVerdict;
    auto profile = static_cast<MemoryProfile>(rawProfile);
    auto initializedProfile = initializedMemoryProfile.load(std::memory_order_acquire);
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
        Options::alwaysUseSharedArrayBuffer() = true;
        Options::useTemporal() = true;
        Options::useExplicitResourceManagement() = true;
#if REDJS_NO_WASM
        Options::useWasm() = false;
#endif
    });
    if (rjs_init(nullptr) || !Options::useAOT() || Options::useJIT()
        || (profile == MemoryProfile::Compact && ((!customized && initializedProfile != rawProfile)
            || Options::structureHeapSizeInKB() != 262144 || Options::markedBlockCageSizeInMB() != 1024))) {
        return ImageVerdict::OptionsConflict;
    }
    // Register the embedding host before any VM/realm exists, once even if an image is rejected.
    std::call_once(nativeHelpersInitialized, registerEmbeddingHost);
    return ImageVerdict::Accepted;
}
#if defined(_WIN32)
// What AOT::useImage does for an image in the executable's own sections, for one in the sections of
// another module the loader mapped (a container DLL): the engine looks for such images in the
// executable only. Windows images are position independent and their code needs no mapping of its
// own, so this is the check, the low pages, the protections, then create and publish.
const uint8_t* moduleBaseOf(const void* address)
{
    HMODULE module = nullptr;
    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, static_cast<LPCWSTR>(address), &module))
        return nullptr;
    return reinterpret_cast<const uint8_t*>(module);
}
bool liesInOneSectionOf(const uint8_t* base, std::span<const uint8_t> range, bool executed)
{
    if (range.empty())
        return !executed;
    auto* headers = reinterpret_cast<const IMAGE_NT_HEADERS*>(base + reinterpret_cast<const IMAGE_DOS_HEADER*>(base)->e_lfanew);
    const IMAGE_SECTION_HEADER* sections = IMAGE_FIRST_SECTION(headers);
    for (unsigned index = 0; index < headers->FileHeader.NumberOfSections; ++index) {
        const IMAGE_SECTION_HEADER& section = sections[index];
        const uint8_t* start = base + section.VirtualAddress;
        if (range.data() < start || static_cast<uint64_t>(range.data() - start) >= section.Misc.VirtualSize)
            continue;
        if (range.size() > section.Misc.VirtualSize - static_cast<uint64_t>(range.data() - start) || (section.Characteristics & IMAGE_SCN_MEM_WRITE))
            return false;
        // As in the engine: on Windows the parts that are not executed may lie in an executable section.
        return !!(section.Characteristics & IMAGE_SCN_MEM_EXECUTE) == executed || !executed;
    }
    return false;
}
bool isExecuteRead(std::span<const uint8_t> code)
{
    MEMORY_BASIC_INFORMATION region;
    return VirtualQuery(code.data(), &region, sizeof(region)) && region.Protect == PAGE_EXECUTE_READ
        && static_cast<const uint8_t*>(region.BaseAddress) + region.RegionSize >= code.data() + code.size();
}
AOT::Wire::ImageVerdict useImageInModule(std::span<const uint8_t> bytes, const std::array<uint8_t, 16>& identity)
{
    using AOT::Wire::ImageVerdict;
    static Lock lock;
    Locker locker { lock };
    if (const auto* image = AOT::Image::find(bytes)) {
        AOT::Image::setCurrentOfThread(image);
        return ImageVerdict::Accepted;
    }
    if (auto verdict = AOT::Image::check(bytes, identity); verdict != ImageVerdict::Accepted)
        return verdict;
    const auto& header = *reinterpret_cast<const AOT::ImageHeader*>(bytes.data());
    size_t page = WTF::pageSize();
    if (reinterpret_cast<uintptr_t>(bytes.data()) % page || header.codeOffset % page || header.size % page)
        return ImageVerdict::Misaligned;
    const uint8_t* base = moduleBaseOf(bytes.data());
    size_t endOfExecutedPart = header.codeOffset + header.executableSize;
    std::span<const uint8_t> executed = bytes.subspan(header.codeOffset, header.executableSize);
    if (!base || base == reinterpret_cast<const uint8_t*>(GetModuleHandleW(nullptr))
        || moduleBaseOf(bytes.data() + header.size - 1) != base
        || !liesInOneSectionOf(base, bytes.first(header.codeOffset), false)
        || !liesInOneSectionOf(base, executed, true)
        || !liesInOneSectionOf(base, bytes.subspan(endOfExecutedPart, header.size - endOfExecutedPart), false))
        return ImageVerdict::OutsideItsSegments;
    if (header.sections[static_cast<size_t>(AOT::ImageSection::FaultSites)].size && lowestAccessibleAddress() < AOT::FaultSite::reach)
        return ImageVerdict::LowPagesAccessible;
    if (!isExecuteRead(executed))
        return ImageVerdict::CannotMapCode;
    AOT::Image::publish(AOT::Image::create(bytes, executed.data()));
    const AOT::Image& image = *AOT::Image::current();
#if ENABLE(WEBASSEMBLY_OMGJIT)
    Wasm::CodeInImage::addImage(image.section<uint8_t>(AOT::ImageSection::WebAssemblyDirectory), image.webAssemblyCode());
#else
    UNUSED_PARAM(image);
#endif
    activateSignalHandlersFor(Signal::AccessFault);
    return ImageVerdict::Accepted;
}
#endif
// Without a file, the image must already lie in the sections of the executable's own module, or
// with `inModule` in those of a module the host loaded.
AOT::Wire::ImageVerdict useImageAt(std::span<const uint8_t> imageBytes, std::optional<AOT::ImageFile> file, const uint8_t* identity, uint32_t rawProfile, bool inModule = false)
{
    using AOT::Wire::ImageVerdict;
    auto initialized = initializeForImage(rawProfile);
    if (initialized != ImageVerdict::Accepted)
        return initialized;
    std::array<uint8_t, 16> expected;
    std::memcpy(expected.data(), identity, 16);
    // Validate before reading the header and before publishing a borrowed view.
    auto checked = AOT::Image::check(imageBytes, expected);
    if (checked != ImageVerdict::Accepted)
        return checked;
    const auto& header = *reinterpret_cast<const AOT::ImageHeader*>(imageBytes.data());
    if (!header.options.useWideReferences() && !MarkedBlockCage::require())
        return ImageVerdict::NeedsCellCage;
#if defined(_WIN32)
    auto verdict = inModule ? useImageInModule(imageBytes, expected) : AOT::useImage(imageBytes, file, expected);
#else
    if (inModule)
        return ImageVerdict::Unsupported;
    auto verdict = AOT::useImage(imageBytes, file, expected);
#endif
    if (verdict != ImageVerdict::Accepted)
        return verdict;
    imageMemoryProfile = static_cast<MemoryProfile>(rawProfile);
    // The host runs its preludes and binds native globals before the entry: code the image has not seen.
    imageAdmitsHostFiles = AOT::Image::current() && AOT::Image::current()->loadsCodeOutsideImage();
    imageActive = true;
    return verdict;
}
uint32_t loadOwnedFile(decltype(OwnedHandle::value) owned, const uint8_t* identity, uint32_t rawProfile)
{
    using AOT::Wire::ImageVerdict;
    OwnedHandle file(owned);
    auto preflight = checkLoad(rawProfile);
    if (preflight != ImageVerdict::Accepted)
        return static_cast<uint32_t>(preflight);
    if (!identity)
        return static_cast<uint32_t>(ImageVerdict::CannotReadExecutable);
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
#if defined(_WIN32)
    auto descriptor = reinterpret_cast<intptr_t>(file.value);
#else
    auto descriptor = static_cast<intptr_t>(file.value);
#endif
    std::span<const uint8_t> imageBytes { static_cast<const uint8_t*>(view.value), fileSize };
    auto verdict = useImageAt(imageBytes, AOT::ImageFile { descriptor, 0 }, identity, rawProfile);
    if (verdict != ImageVerdict::Accepted)
        return static_cast<uint32_t>(verdict);
    initialImageView = view.release();
    file.release();
    return static_cast<uint32_t>(verdict);
}
}

extern "C" uint32_t rjs_aot_load_file_with_profile(const char16_t* path, size_t length, const uint8_t* identity, uint32_t rawProfile)
{
    auto verdict = checkLoad(rawProfile);
    if (verdict != AOT::Wire::ImageVerdict::Accepted)
        return static_cast<uint32_t>(verdict);
    if (!path || !identity)
        return static_cast<uint32_t>(AOT::Wire::ImageVerdict::CannotReadExecutable);
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

// An image the OS loader mapped as sections of the executable itself: used in place, never copied.
extern "C" uint32_t rjs_aot_load_mapped_with_profile(const uint8_t* bytes, size_t length, const uint8_t* identity, uint32_t rawProfile)
{
    auto verdict = checkLoad(rawProfile);
    if (verdict != AOT::Wire::ImageVerdict::Accepted)
        return static_cast<uint32_t>(verdict);
    if (!bytes || !length || !identity)
        return static_cast<uint32_t>(AOT::Wire::ImageVerdict::CannotReadExecutable);
    verdict = useImageAt({ bytes, length }, std::nullopt, identity, rawProfile);
    if (verdict == AOT::Wire::ImageVerdict::Accepted)
        initialImageView = bytes;
    return static_cast<uint32_t>(verdict);
}

// An image the OS loader mapped as sections of a module the host loaded, such as a container DLL.
extern "C" uint32_t rjs_aot_load_module_with_profile(const uint8_t* bytes, size_t length, const uint8_t* identity, uint32_t rawProfile)
{
    auto verdict = checkLoad(rawProfile);
    if (verdict != AOT::Wire::ImageVerdict::Accepted)
        return static_cast<uint32_t>(verdict);
    if (!bytes || !length || !identity)
        return static_cast<uint32_t>(AOT::Wire::ImageVerdict::CannotReadExecutable);
    verdict = useImageAt({ bytes, length }, std::nullopt, identity, rawProfile, true);
    if (verdict == AOT::Wire::ImageVerdict::Accepted)
        initialImageView = bytes;
    return static_cast<uint32_t>(verdict);
}

extern "C" uint32_t rjs_aot_load_handle_with_profile(intptr_t rawFile, const uint8_t* identity, uint32_t rawProfile)
{
    auto verdict = checkLoad(rawProfile);
    if (verdict != AOT::Wire::ImageVerdict::Accepted)
        return static_cast<uint32_t>(verdict);
    if (!identity)
        return static_cast<uint32_t>(AOT::Wire::ImageVerdict::CannotReadExecutable);
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
    AOT::rejectEvaluationUnlessAllowed(global);
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

extern "C" int32_t rjs_aot_admits_host_files()
{
    return imageActive && imageAdmitsHostFiles ? 1 : 0;
}

extern "C" int32_t rjs_aot_has_closed_library()
{
    const auto* image = imageActive ? AOT::Image::current() : nullptr;
    return image && image->isBuiltWithLibrary() && !image->allowsLibrarySource()
        && !image->admitsUnknownJavaScript() && !image->loadsCodeOutsideImage();
}

extern "C" void rjs_aot_forget_context(void* context)
{
    startedRealms.erase(static_cast<JSGlobalObject*>(context));
}
