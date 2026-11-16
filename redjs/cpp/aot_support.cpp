// The AOT-era JSC archive delegates trimming policy to its embedder.
// redjs ships the complete engine, so every feature is Present. Unexpected
// removed-feature entries must diagnose/throw rather than silently succeed.
#include "config.h"
#include <JavaScriptCore/AOTRemovableFeature.h>
#include <JavaScriptCore/Error.h>
#include <JavaScriptCore/ExceptionHelpers.h>
#include <JavaScriptCore/ExceptionScope.h>
#include <JavaScriptCore/JSGlobalObject.h>
#include <JavaScriptCore/JSCInlines.h>
#include <wtf/SHA1.h>
#include <wtf/text/MakeString.h>
#include <cstdio>
#include <cstdlib>
#include <new>
#if ENABLE(WEBASSEMBLY)
#include <JavaScriptCore/WasmStreamingParser.h>
#endif

using namespace WTF;

namespace JSC {

extern "C" const RemovableFeatureState Bun__removableFeatureState = {
    .magic = { 'B', 'U', 'N', 'F', 'E', 'A', 'T', 0 },
    .isTrimmed = 0,
    .removedRangeCount = 0,
    .removedRangesOffset = 0,
    .removal = { },
};

extern "C" const char* Bun__removableFeatureName(uint16_t feature)
{
    switch (feature) {
#define REDJS_FEATURE_NAME(name, index) case index: return #name;
        FOR_EACH_ENGINE_REMOVABLE_FEATURE(REDJS_FEATURE_NAME)
#undef REDJS_FEATURE_NAME
    default:
        return nullptr;
    }
}

extern "C" [[noreturn]] void Bun__abortForRemovedFeature(uint16_t feature)
{
    const char* name = Bun__removableFeatureName(feature);
    std::fprintf(stderr, "redjs: unexpected removed-feature entry %u (%s) in an untrimmed engine\n",
        static_cast<unsigned>(feature), name ? name : "unknown");
    std::abort();
}

static EncodedJSValue throwUnexpectedRemovedFeature(JSGlobalObject* global, uint16_t feature, const String& function)
{
    auto scope = DECLARE_THROW_SCOPE(global->vm());
    const char* name = Bun__removableFeatureName(feature);
    String featureName = name ? String::fromUTF8(name) : String("unknown"_s);
    auto message = makeString("Unexpected removed-feature entry in untrimmed redjs: "_s,
        featureName, function.isEmpty() ? String() : makeString(" ("_s, function, ')'));
    throwException(global, scope, createError(global, message));
    return { };
}

extern "C" EncodedJSValue Bun__throwForRemovedFeature(JSGlobalObject* global, uint16_t feature)
{
    return throwUnexpectedRemovedFeature(global, feature, { });
}

extern "C" EncodedJSValue Bun__throwForRemovedFunction(JSGlobalObject* global, uint16_t feature, StringImpl* name)
{
    return throwUnexpectedRemovedFeature(global, feature, name ? String(name) : String());
}

extern "C" EncodedJSValue Bun__throwForRemovedBuiltin(JSGlobalObject* global, uint16_t feature, uint32_t builtin)
{
    return throwUnexpectedRemovedFeature(global, feature, makeString("builtin #"_s, builtin));
}

} // namespace JSC

extern "C" bool mi_on_thread_idle_start(void) noexcept;
extern "C" void mi_on_thread_idle_end(void) noexcept;

// mimalloc starts its scavenger thread only when a second thread appears or one first parks.
extern "C" void Bun__didFinishFirstCollectionOfCompiledProgram()
{
    if (mi_on_thread_idle_start())
        mi_on_thread_idle_end();
}

#if ENABLE(WEBASSEMBLY)
// The engine leaves the SHA-1 of streamed WebAssembly to the embedder; WTF's fits its context.
static_assert(sizeof(WTF::SHA1) <= JSC::Wasm::SourceHasher::contextSize && alignof(WTF::SHA1) <= 8);

extern "C" void Bun__initializeSHA1(void* context)
{
    new (context) WTF::SHA1();
}

extern "C" void Bun__updateSHA1(void* context, std::span<const uint8_t> bytes)
{
    static_cast<WTF::SHA1*>(context)->addBytes(bytes);
}

extern "C" void Bun__finishSHA1(void* context, std::array<uint8_t, 20>& digest)
{
    auto* sha1 = static_cast<WTF::SHA1*>(context);
    sha1->computeHash(digest);
    sha1->~SHA1();
}
#endif
