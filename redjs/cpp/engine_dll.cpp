// Built into the engine DLL only (REDJS_ENGINE_DLL, build.rs).
//
// Bun's ICU calls a weak `bun_icu_maybe_decompress` for every common-data
// item (src/icu.rs). In a static link the Rust definition satisfies it by
// name; across a DLL boundary the DLL cannot see the executable's symbols,
// so this definition forwards to the one Rust hands over (`glue::init`)
// before `rjs_init` touches ICU. Until then items pass through unchanged,
// which is what the weak default would do.
#include <cstdint>

using IcuDecompress = const void* (*)(const void*, int32_t*);

static IcuDecompress icuDecompress;

extern "C" void rjs_set_icu_decompress(IcuDecompress hook)
{
    icuDecompress = hook;
}

extern "C" const void* bun_icu_maybe_decompress(const void* pointer, int32_t* length)
{
    return icuDecompress ? icuDecompress(pointer, length) : pointer;
}

// The hook's zstd dictionary, which sicudt.lib carries. A function, because
// data is not reachable through an import library's thunks.
extern "C" const uint8_t bun_icu_zstd_dict[];
extern "C" const uint32_t bun_icu_zstd_dict_size;

extern "C" const void* rjs_icu_zstd_dict(uint32_t* size)
{
    *size = bun_icu_zstd_dict_size;
    return bun_icu_zstd_dict;
}
