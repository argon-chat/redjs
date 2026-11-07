//! Build pipeline for redjs:
//!
//! 1. Generates the SysV->MS trampoline pool (`$OUT_DIR/trampolines.rs`).
//!    JSC host functions on Windows x64 use the System V calling convention
//!    (`JSC_HOST_CALL_ATTRIBUTES = __attribute__((sysv_abi))` in this Bun
//!    WebKit build). .NET cannot emit SysV functions on Windows, so C# host
//!    functions are entered through a pre-compiled `extern "sysv64"` Rust
//!    trampoline that forwards to the stored MS-ABI pointer. rustc handles
//!    the ABI transition (xmm6-15/rsi/rdi preservation, shadow space).
//!
//! 2. Compiles `cpp/glue.cpp` against the Bun WebKit *internal* headers with
//!    the exact flags of the prebuilt (clang-cl, /MT, NDEBUG, C++23,
//!    force-included cmakeconfig.h).
//!
//! 3. Compiles the vendored Yoga flexbox engine (`cpp/yoga`, facebook/yoga
//!    v3.2.1 — the exact commit the `yoga-layout` npm pin ships) into its own
//!    static lib for the `RedYoga` host class. Plain portable C++20: it must
//!    NOT see the WebKit cmakeconfig force-include.
//!
//! 4. Compiles oven-sh/mimalloc (`cpp/mimalloc`, fetched by setup.ps1). This
//!    JSC build (`USE_MIMALLOC 1`, `USE_EXTERNAL_MIMALLOC 1`) allocates
//!    through `mi_*` and leaves the allocator to the embedder, the way Bun
//!    links its own. It is a normal (bundled) static lib, so it lands INSIDE
//!    the thin redjs.lib and a NativeAOT consumer gets it without listing it.
//!
//! 5. With `indexeddb-sqlite` only: compiles `cpp/idb_sqlite.c` — SQLite3
//!    Multiple Ciphers (`../prebuilt/sqlite3mc`, fetched by setup.ps1) as ONE
//!    translation unit whose `sqlite3_*` API is `static`, trimmed to the
//!    ChaCha20-Poly1305 cipher and size-optimised (-Os, /Gy /Gw so the final
//!    link's /OPT:REF drops what IndexedDB never calls). Bundled into the
//!    rlib like mimalloc.
//!
//! 6. Links the WebKit static libs. They are linked with the `-bundle`
//!    modifier so the `staticlib` artifact (redjs.lib for NativeAOT) stays
//!    thin — the consuming link (ILC) must also pass the WebKit libs. The
//!    `cdylib` artifact (redjs.dll) is always fully self-contained.
//!
//! Before any of it, the prebuilt, the mimalloc tree and (with
//! `indexeddb-sqlite`) the sqlite3mc tree are checked against `../pins.txt`:
//! a mismatch is a build error with the command that fixes it, because
//! linking whatever happens to be on disk is how a checkout keeps the OLD
//! engine after pulling a new pin.
//!
//! Prebuilt location: `BUN_WEBKIT_DIR` env or `../prebuilt/bun-webkit`.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// Number of pre-compiled host-function trampolines. Each registered C# host
/// function consumes one slot for the lifetime of the process.
const TRAMPOLINE_COUNT: usize = 2048;

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    generate_trampolines(&out_dir);

    let webkit = env::var("BUN_WEBKIT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| manifest.parent().unwrap().join("prebuilt").join("bun-webkit"));
    let lib = webkit.join("lib");
    let inc = webkit.join("include");

    let pins_path = manifest.parent().unwrap().join("pins.txt");
    let pins = read_pins(&pins_path);
    check_prebuilt(&webkit, &pins);
    let mimalloc = manifest.join("cpp").join("mimalloc");
    check_mimalloc(&mimalloc, &pins);

    // --- C++ glue against WebKit internal headers -------------------------
    // Flags mirror how the prebuilt itself was produced (proven in
    // battle_test/bench_jsc): clang-cl, C++23, static CRT, NDEBUG, and the
    // prebuilt's own cmakeconfig.h force-included so every USE_/ENABLE_ flag
    // (incl. USE_BUN_JSC_ADDITIONS) matches its ABI exactly.
    let mut glue = cc::Build::new();
    glue.cpp(true)
        .compiler("clang-cl")
        .static_crt(true)
        .file("cpp/glue.cpp")
        .include(&inc)
        .include("cpp/compat") // PlatformEnableWin.h is missing from the prebuilt archive
        .flag("/std:c++23preview")
        .flag("/W0")
        .flag("-Wno-inconsistent-dllimport")
        .flag("/FIcmakeconfig.h")
        .define("NDEBUG", None)
        .define("STATICALLY_LINKED_WITH_JavaScriptCore", "1")
        .define("STATICALLY_LINKED_WITH_WTF", "1")
        .define("STATICALLY_LINKED_WITH_BMALLOC", "1")
        .define("BUILDING_WITH_CMAKE", "1")
        .define("NOMINMAX", None)
        .define("WIN32", None)
        .define("_WINDOWS", None)
        .define("WIN32_LEAN_AND_MEAN", "1")
        .define("_CRT_SECURE_NO_WARNINGS", None);

    // An AOT-enabled prebuilt leaves its generated builtin table to the
    // embedder. Use the exact real table shipped in the verified archive:
    // invented/empty tables invalidate the engine's builtin IDs and layouts.
    let configuration = fs::read_to_string(inc.join("cmakeconfig.h"))
        .expect("the checked WebKit configuration is readable");
    if configuration
        .lines()
        .any(|line| line.trim() == "#define BUN_WEBKIT_ENABLE_AOT 1")
    {
        let generated = inc.join("JavaScriptCore").join("aot");
        let native_aot = std::env::var_os("CARGO_FEATURE_NATIVE_AOT").is_some();
        println!("cargo:rerun-if-env-changed=REDJS_AOT_BUILTIN_TABLE_DIR");
        let table = if native_aot {
            let table = PathBuf::from(std::env::var_os("REDJS_AOT_BUILTIN_TABLE_DIR")
                .expect("native-aot requires REDJS_AOT_BUILTIN_TABLE_DIR; run scripts/aot/generate-table.ts"));
            let receipt = fs::read_to_string(table.join(".redjs-aot-table"))
                .expect("native-aot table lacks its pinned generation receipt");
            assert!(
                receipt.contains("7ca0042d3ba23ed60fd9cd8af1a1ce6ec3f8d074")
                    && receipt.contains(pin(&pins, "webkit.version")),
                "AOT frontend/engine receipt mismatch"
            );
            table
        } else {
            generated.clone()
        };
        if native_aot {
            println!(
                "cargo:rustc-env=REDJS_AOT_ENGINE_VERSION={}",
                pin(&pins, "webkit.version")
            );
            use sha2::{Digest, Sha256};
            let mut hash = Sha256::new();
            for file in [
                ".redjs-aot-table",
                "AOTBuiltinTableArrays.h",
                "AOTBuiltinTableAttachments.cpp",
                "AOTBuiltinTableBuiltins.cpp",
                "AOTBuiltinTableNames.cpp",
                "AOTBuiltinTableOverloads.cpp",
                "AOTBuiltinTableTypes.cpp",
            ] {
                let path = table.join(file);
                println!("cargo:rerun-if-changed={}", path.display());
                hash.update(file.as_bytes());
                hash.update(fs::read(path).expect("AOT table input missing"));
            }
            hash.update(
                fs::read(inc.join("JavaScriptCore/aot/AOTWire.cpp"))
                    .expect("AOT schema hash missing"),
            );
            println!("cargo:rustc-env=REDJS_AOT_TABLE_ID={:x}", hash.finalize());
            glue.file("cpp/aot.cpp");
        }
        glue.include(inc.join("JavaScriptCore"))
            .include(&generated)
            .include("cpp/aot")
            .file("cpp/aot_support.cpp");
        for name in [
            "AOTBuiltinTableAttachments.cpp",
            "AOTBuiltinTableBuiltins.cpp",
            "AOTBuiltinTableEngineBindings.cpp",
            "AOTBuiltinTableNames.cpp",
            "AOTBuiltinTableOverloads.cpp",
            "AOTBuiltinTableTypes.cpp",
            "AOTSchemaInlineBuiltins.cpp",
        ] {
            let source = if name == "AOTBuiltinTableEngineBindings.cpp"
                || name == "AOTSchemaInlineBuiltins.cpp"
            {
                generated.join(name)
            } else {
                table.join(name)
            };
            assert!(
                source.is_file(),
                "AOT prebuilt lacks generated source {}",
                source.display()
            );
            println!("cargo:rerun-if-changed={}", source.display());
            glue.file(source);
        }
        println!("cargo:rerun-if-changed=cpp/aot_support.cpp");
        println!("cargo:rerun-if-changed=cpp/aot.cpp");
        println!("cargo:rerun-if-changed=cpp/aot");
    }
    glue.compile("redjs_glue");

    // --- vendored Yoga (flexbox layout for the RedYoga host class) ---------
    // Ordinary self-contained C++20 — same compiler/CRT as the rest of the
    // crate (clang-cl + /MT, so it links with `+crt-static`) but NONE of the
    // WebKit configuration: no cmakeconfig force-include, no WebKit includes.
    // `cpp/` on the include path is what makes yoga's own `#include
    // <yoga/...>` resolve. This flag set is the one proven to reproduce the
    // wasm layout goldens (scripts/yoga-parity.ts).
    // Feature-gated: embedders without the RedYoga host class (vue-native uses
    // gpui/taffy for layout) skip both the compile and the vendored-sources
    // requirement.
    if env::var_os("CARGO_FEATURE_YOGA_HOST").is_some() {
        let yoga_dir = manifest.join("cpp").join("yoga");
        let mut yoga = cc::Build::new();
        yoga.cpp(true)
            .compiler("clang-cl")
            .static_crt(true)
            .include(manifest.join("cpp"))
            .flag("/std:c++20")
            .flag("/EHsc")
            .flag("/W0")
            .define("NDEBUG", None)
            .define("NOMINMAX", None)
            .define("_CRT_SECURE_NO_WARNINGS", None);
        let mut yoga_sources = Vec::new();
        collect_cpp(&yoga_dir, &mut yoga_sources);
        yoga_sources.sort(); // deterministic object order
        if yoga_sources.is_empty() {
            panic!("no .cpp sources under {} — vendored yoga is missing", yoga_dir.display());
        }
        yoga.files(&yoga_sources).compile("redjs_yoga");
        // Directory form: cargo walks it recursively, so headers and any future
        // .cpp under cpp/yoga/ are covered without listing them. ONLY with the
        // feature on: a rerun-if-changed path that does not exist is "always
        // stale" to cargo, and in a checkout without the vendored yoga
        // (vue-native) that re-ran this script — and recompiled redjs and every
        // crate above it — on every single build.
        println!("cargo:rerun-if-changed=cpp/yoga");
    }

    // --- mimalloc (JSC's allocator; the embedder provides it) --------------
    // Mirrors Bun's scripts/build/deps/mimalloc.ts (bun-v1.4.4) for Windows:
    // the single unity TU `src/static.c`, compiled AS C++ (`/TP` — cc-rs
    // picks the language from the extension, and Bun builds it as C++ because
    // the C++ code that uses mimalloc's types must agree with it on layout),
    // no malloc override (Bun: on Windows the static CRT's own
    // `_expand`/`_msize`/`free` collide with alloc-override.c), and these
    // defines, none of which is optional:
    //
    //   MI_STATIC_LIB            the static-library symbol visibility.
    //   MI_SKIP_COLLECT_ON_EXIT  no heap walk at exit.
    //   MI_NO_PROCESS_DETACH     no mi_process_done at all: it tears down locks
    //                            and TLS while other static destructors may
    //                            still free().
    //   MI_FREE_USE_PAGEMAP      LOAD-BEARING. Without it page metadata sits at
    //                            256 MiB boundaries, every arena must start on
    //                            one, and mi_manage_os_memory_ex refuses JSC's
    //                            structure heap whenever its reservation is
    //                            256 MiB or less — an abort at startup.
    //   MI_BUILD_RELEASE         release mimalloc in every profile: it is a
    //                            dependency we do not debug, and a debug-profile
    //                            host must not run a differently-checked
    //                            allocator under the same JSC (MI_DEBUG=2
    //                            otherwise, since nothing here defines NDEBUG).
    //
    // `MI_CMAKE_BUILD_TYPE` only labels mimalloc's verbose banner.
    //
    // It is NOT the Rust global allocator: Rust keeps the system heap, mimalloc
    // serves JSC (and whatever JSC hands out) only.
    cc::Build::new()
        .cpp(true)
        .compiler("clang-cl")
        .static_crt(true)
        .file(mimalloc.join("src").join("static.c"))
        .include(mimalloc.join("include"))
        .flag("/TP")
        .flag("-w")
        .define("MI_STATIC_LIB", None)
        .define("MI_SKIP_COLLECT_ON_EXIT", "1")
        .define("MI_NO_PROCESS_DETACH", "1")
        .define("MI_FREE_USE_PAGEMAP", "1")
        .define("MI_BUILD_RELEASE", None)
        .define("MI_CMAKE_BUILD_TYPE", "release")
        .compile("redjs_mimalloc");

    // --- SQLite3 Multiple Ciphers (IndexedDB on disk) ----------------------
    // ONE C translation unit, `cpp/idb_sqlite.c`, which #includes the pinned
    // amalgamation from prebuilt/sqlite3mc (setup.ps1) and exports only its
    // own `rjs_sql_*` wrappers: every sqlite3_* function is `static` there.
    // Only with `indexeddb-sqlite`; without it nothing is checked or compiled.
    //
    // Optimised for SIZE in every profile (/O1): the owner weighs the binary,
    // and the trimmed -O1 object adds ~0.76 MB to an exe where -O2 adds ~1.17
    // (research/reports/m25/indexeddb-p2-impl.md has the numbers, and the
    // benchmarks that say what it costs). /Gy + /Gw put every function and
    // global in its own COMDAT, so the linker drops what nothing reaches.
    if env::var_os("CARGO_FEATURE_INDEXEDDB_SQLITE").is_some() {
        let mc = manifest.parent().unwrap().join("prebuilt").join("sqlite3mc");
        check_sqlite3mc(&mc, &pins);
        cc::Build::new()
            .compiler("clang-cl")
            .static_crt(true)
            .file("cpp/idb_sqlite.c")
            .include(&mc)
            .opt_level_str("s")
            .flag("/Gy")
            .flag("/Gw")
            .flag("-w")
            .define("NDEBUG", None)
            .define("_CRT_SECURE_NO_WARNINGS", None)
            .compile("redjs_sqlite");
        println!("cargo:rerun-if-changed=cpp/idb_sqlite.c");
        println!("cargo:rerun-if-changed={}", mc.join(".redjs-pin").display());
    }

    println!("cargo:rustc-link-search=native={}", lib.display());

    // WebKit static libs. `-bundle`: not embedded into the staticlib artifact
    // (kept thin; NativeAOT link adds these libs itself), still linked into
    // the cdylib/test binaries as usual.
    for l in ["JavaScriptCore", "WTF", "bmalloc", "sicudt", "sicuin", "sicuuc"] {
        println!("cargo:rustc-link-lib=static:-bundle={l}");
    }

    // Windows system libs WebKit pulls in. `ntdll`: the JIT/LLInt unwind-info
    // registration (`RtlAddGrowableFunctionTable`) — glue.cpp also carries it
    // as a /DEFAULTLIB directive for hand-written consumer links.
    for l in [
        "user32", "ole32", "oleaut32", "advapi32", "shell32", "shlwapi", "winmm", "bcrypt",
        "dbghelp", "userenv", "ws2_32", "synchronization", "usp10", "rpcrt4", "crypt32", "ntdll",
    ] {
        println!("cargo:rustc-link-lib=dylib={l}");
    }

    // No identical-COMDAT folding in the images THIS package links (redjs.dll,
    // the conformance suite, the examples). rustc passes `/OPT:REF,ICF` to
    // link.exe for every optimized MSVC build, and JSC tells constructors from
    // non-constructors by comparing native function POINTERS — fold two
    // identical bodies and one of them changes behaviour (Bun hit exactly
    // this: `BigInt` became "not a constructor"; WebKit itself links
    // /OPT:NOICF). The last /OPT wins in link.exe, and `-C link-arg` lands
    // after rustc's own. This does not reach a consumer's binary — cargo
    // scopes link args to the package that prints them — so an embedder that
    // links its own executable (vue-native's vn-app) says the same thing in
    // its own build script.
    println!("cargo:rustc-link-arg=/OPT:NOICF");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=cpp/glue.cpp");
    println!("cargo:rerun-if-changed=cpp/compat/wtf/PlatformEnableWin.h");
    // setup.ps1 rewrites the stamp only after a verified fetch, so it stands
    // for the whole mimalloc tree.
    println!("cargo:rerun-if-changed={}", mimalloc.join(".redjs-pin").display());
    println!("cargo:rerun-if-changed={}", pins_path.display());
    println!("cargo:rerun-if-changed={}", inc.join("cmakeconfig.h").display());
    println!("cargo:rerun-if-env-changed=BUN_WEBKIT_DIR");
}

/// `pins.txt`: `key = value` lines, `#` comments. See that file.
fn read_pins(path: &Path) -> Vec<(String, String)> {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e} — it pins the prebuilt redjs builds against", path.display()));
    text.lines()
        .filter_map(|raw| {
            let line = raw.split('#').next().unwrap().trim();
            let (k, v) = line.split_once('=')?;
            Some((k.trim().to_owned(), v.trim().to_owned()))
        })
        .collect()
}

fn pin<'a>(pins: &'a [(String, String)], key: &str) -> &'a str {
    pins.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .unwrap_or_else(|| panic!("pins.txt has no `{key}`"))
}

/// The prebuilt names its own build in `include/cmakeconfig.h`
/// (`#define BUN_WEBKIT_VERSION "<sha>"`); that is compared, not a directory
/// name or a stamp, so a tarball extracted by hand is held to the pin too.
fn check_prebuilt(webkit: &Path, pins: &[(String, String)]) {
    let want = pin(pins, "webkit.version");
    let cfg = webkit.join("include").join("cmakeconfig.h");
    let fix = "Run vendor/redjs/setup.ps1 (packages/jscore/setup.ps1 in PEWPEW): it fetches the pinned \
               prebuilt and REPLACES one at any other version. Or point BUN_WEBKIT_DIR at an \
               extraction of the pinned tarball.";
    let Ok(text) = fs::read_to_string(&cfg) else {
        panic!(
            "\n\nno bun-webkit prebuilt at {} (missing include/cmakeconfig.h).\n{fix}\n\
             pins.txt wants {want}.\n",
            webkit.display()
        );
    };
    let have = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("#define BUN_WEBKIT_VERSION "))
        .map(|v| v.trim().trim_matches('"').to_owned());
    match have {
        Some(have) if have == want => {}
        have => panic!(
            "\n\nthe bun-webkit prebuilt at {} is {}, but redjs pins {want} (pins.txt).\n\
             Building would compile the glue against one engine's internal headers and link\n\
             whatever is on disk — refusing.\n{fix}\n",
            webkit.display(),
            have.as_deref().unwrap_or("unidentified (no BUN_WEBKIT_VERSION)"),
        ),
    }
    let want_aot = pin(pins, "webkit.aot");
    let have_aot = text.lines().find_map(|line| {
        line.trim().strip_prefix("#define BUN_WEBKIT_ENABLE_AOT ")
    }).map(str::trim);
    assert_eq!(have_aot, Some(want_aot),
        "JSC AOT variant mismatch at {}: wanted ENABLE_AOT={want_aot}. {fix}", webkit.display());
    if !webkit.join("lib").join("JavaScriptCore.lib").is_file() {
        panic!(
            "\n\n{} names the pinned build but has no lib/JavaScriptCore.lib — an interrupted \
             extraction?\n{fix}\n",
            webkit.display()
        );
    }
}

/// setup.ps1 writes `cpp/mimalloc/.redjs-pin` (the commit) only after the
/// extracted tree's sha256 matched pins.txt.
fn check_mimalloc(dir: &Path, pins: &[(String, String)]) {
    let want = pin(pins, "mimalloc.commit");
    let have = fs::read_to_string(dir.join(".redjs-pin")).ok().map(|s| s.trim().to_owned());
    if have.as_deref() != Some(want) || !dir.join("src").join("static.c").is_file() {
        panic!(
            "\n\nmimalloc sources at {} are {}, but redjs pins oven-sh/mimalloc@{want} (pins.txt).\n\
             This JSC build allocates through the embedder's mimalloc, so redjs compiles it.\n\
             Run vendor/redjs/setup.ps1 (packages/jscore/setup.ps1 in PEWPEW) to fetch it.\n",
            dir.display(),
            have.as_deref().unwrap_or("missing"),
        );
    }
}

/// setup.ps1 writes `prebuilt/sqlite3mc/.redjs-pin` (`<version> <tree sha256>`)
/// only after the release asset's sha256 AND the extracted tree's matched
/// pins.txt; a stamp naming anything else is a tree this build refuses.
fn check_sqlite3mc(dir: &Path, pins: &[(String, String)]) {
    let want = format!("{} {}", pin(pins, "sqlite3mc.version"), pin(pins, "sqlite3mc.tree-sha256"));
    let have = fs::read_to_string(dir.join(".redjs-pin")).ok().map(|s| s.trim().to_owned());
    if have.as_deref() != Some(want.as_str()) || !dir.join("sqlite3mc_amalgamation.c").is_file() {
        panic!(
            "\n\nthe SQLite3 Multiple Ciphers amalgamation at {} is {}, but redjs pins `{want}` (pins.txt).\n\
             The `indexeddb-sqlite` feature compiles it into IndexedDB's persistent backend.\n\
             Run vendor/redjs/setup.ps1 to fetch and verify it.\n",
            dir.display(),
            have.as_deref().unwrap_or("missing"),
        );
    }
}

/// Collects every `.cpp` under `dir`, recursively.
fn collect_cpp(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_cpp(&path, out);
        } else if path.extension().is_some_and(|e| e == "cpp") {
            out.push(path);
        }
    }
}

fn generate_trampolines(out_dir: &std::path::Path) {
    let mut s = String::with_capacity(TRAMPOLINE_COUNT * 160);
    let _ = writeln!(
        s,
        "// @generated by build.rs — SysV entry points for host functions.\n\
         pub(crate) const TRAMPOLINE_COUNT: usize = {TRAMPOLINE_COUNT};\n"
    );
    for i in 0..TRAMPOLINE_COUNT {
        let _ = writeln!(
            s,
            "unsafe extern \"sysv64\" fn t{i}(g: *mut core::ffi::c_void, cf: *mut core::ffi::c_void) -> u64 {{ crate::trampoline::dispatch({i}, g, cf) }}"
        );
    }
    let _ = writeln!(
        s,
        "pub(crate) static TRAMPOLINES: [unsafe extern \"sysv64\" fn(*mut core::ffi::c_void, *mut core::ffi::c_void) -> u64; {TRAMPOLINE_COUNT}] = ["
    );
    for i in 0..TRAMPOLINE_COUNT {
        let _ = writeln!(s, "    t{i},");
    }
    let _ = writeln!(s, "];");
    fs::write(out_dir.join("trampolines.rs"), s).unwrap();
}
