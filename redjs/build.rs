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
//! 3. Compiles oven-sh/mimalloc (`cpp/mimalloc`, fetched by setup.ps1). This
//!    JSC build (`USE_MIMALLOC 1`, `USE_EXTERNAL_MIMALLOC 1`) allocates
//!    through `mi_*` and leaves the allocator to the embedder, the way Bun
//!    links its own. It is a normal (bundled) static lib, so it lands INSIDE
//!    the thin redjs.lib and a NativeAOT consumer gets it without listing it.
//!
//! 4. With `indexeddb-sqlite` only: compiles `cpp/idb_sqlite.c` — SQLite3
//!    Multiple Ciphers (`../prebuilt/sqlite3mc`, fetched by setup.ps1) as ONE
//!    translation unit whose `sqlite3_*` API is `static`, trimmed to the
//!    ChaCha20-Poly1305 cipher and size-optimised (-Os, /Gy /Gw so the final
//!    link's /OPT:REF drops what IndexedDB never calls). Bundled into the
//!    rlib like mimalloc.
//!
//! 5. Links the WebKit static libs. They are linked with the `-bundle`
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
//! Unix uses platform-specific directories under `prebuilt`, Clang with the
//! platform C++ standard library, and native system libraries/frameworks.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// Hooks Bun defines for its WebKit and redjs leaves undefined: weak
/// references in the prebuilt that must stay null (see the macOS link below).
const BUN_HOOKS: [&str; 8] = [
    "Bun__reportUnhandledError",
    "Bun__thisThreadHasVM",
    "WTFTimer__cancel",
    "WTFTimer__create",
    "WTFTimer__deinit",
    "WTFTimer__isActive",
    "WTFTimer__secondsUntilTimer",
    "WTFTimer__update",
];

/// The prebuilt's static libraries on Windows.
const WINDOWS_WEBKIT_LIBS: &[&str] = &["JavaScriptCore", "WTF", "bmalloc", "sicudt", "sicuin", "sicuuc"];

/// Windows system libs WebKit pulls in. `ntdll`: the JIT/LLInt unwind-info
/// registration (`RtlAddGrowableFunctionTable`) — glue.cpp also carries it
/// as a /DEFAULTLIB directive for hand-written consumer links.
const WINDOWS_SYSTEM_LIBS: &[&str] = &[
    "user32",
    "ole32",
    "oleaut32",
    "advapi32",
    "shell32",
    "shlwapi",
    "winmm",
    "bcrypt",
    "dbghelp",
    "userenv",
    "ws2_32",
    "synchronization",
    "usp10",
    "rpcrt4",
    "crypt32",
    "ntdll",
];

/// The Rust files whose `extern "C"` blocks import from the C++ half — what
/// an engine DLL exports, less what it does not define (a declaration behind
/// a feature the C++ was built without). SQLite (`webapi/indexeddb`) stays
/// in the Rust image and is not listed.
const ENGINE_IMPORTERS: &[&str] = &["aot.rs", "capi.rs", "glue.rs", "icu.rs", "profiling.rs", "web.rs"];

/// Number of pre-compiled host-function trampolines. Each registered C# host
/// function consumes one slot for the lifetime of the process.
const TRAMPOLINE_COUNT: usize = 2048;

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let windows = target_os == "windows";
    let platform = match (target_os.as_str(), target_arch.as_str()) {
        ("windows", "x86_64") => "windows-amd64",
        ("linux", "x86_64") => "linux-amd64",
        ("macos", "x86_64") => "macos-amd64",
        ("macos", "aarch64") => "macos-arm64",
        _ => panic!("redjs has no pinned prebuilt for {target_os}/{target_arch}"),
    };

    generate_trampolines(&out_dir);

    // `REDJS_ENGINE_DLL=redjs.dll` (Windows): the C++ half — glue, mimalloc,
    // JavaScriptCore, WTF, bmalloc and ICU — is linked into that DLL rather
    // than into the consuming image, which links its import library. An
    // embedder that ships its parts as separate files keeps the engine, which
    // changes only with the pinned prebuilt, out of the file that changes
    // with every release. See `link_engine_dll`.
    println!("cargo:rerun-if-env-changed=REDJS_ENGINE_DLL");
    println!("cargo:rustc-check-cfg=cfg(redjs_engine_dll)");
    let engine_dll = env::var("REDJS_ENGINE_DLL").ok().filter(|name| windows && !name.is_empty());
    if let Some(name) = &engine_dll {
        assert!(
            name.ends_with(".dll") && !name.contains(['/', '\\', ':', '"']),
            "REDJS_ENGINE_DLL must be a file name ending in .dll"
        );
        println!("cargo:rustc-cfg=redjs_engine_dll");
    }

    let webkit = env::var("BUN_WEBKIT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let directory = if windows {
                "bun-webkit".to_owned()
            } else {
                format!("bun-webkit-{platform}")
            };
            manifest.parent().unwrap().join("prebuilt").join(directory)
        });
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
    let mut glue = native_build(true, windows);
    glue.file("cpp/glue.cpp")
        .include(&inc)
        .include("cpp/compat") // PlatformEnableWin.h is missing from the prebuilt archive
        .define("NDEBUG", None)
        .define("STATICALLY_LINKED_WITH_JavaScriptCore", "1")
        .define("STATICALLY_LINKED_WITH_WTF", "1")
        .define("STATICALLY_LINKED_WITH_BMALLOC", "1")
        .define("BUILDING_WITH_CMAKE", "1");
    if windows {
        glue.flag("/std:c++23preview")
            .flag("/W0")
            .flag("-Wno-inconsistent-dllimport")
            .flag("/FIcmakeconfig.h")
            .define("NOMINMAX", None)
            .define("WIN32", None)
            .define("_WINDOWS", None)
            .define("WIN32_LEAN_AND_MEAN", "1")
            .define("_CRT_SECURE_NO_WARNINGS", None);
    } else {
        // WebKit's release headers require optimized inlines even when the
        // Rust host is built in debug mode.
        glue.opt_level(2)
            .flag("-std=c++23")
            .flag("-include")
            .flag("cmakeconfig.h")
            .flag("-fno-exceptions")
            .flag("-fno-rtti")
            .flag("-w")
            // The prebuilt is WebKit's JSCOnly port. Without its define the
            // headers pick PLATFORM(MAC) on macOS, turning on Cocoa features
            // the archive was not built with (and does not ship headers for).
            // On Linux no platform is picked either way.
            .define("BUILDING_JSCONLY__", None);
    }

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
            println!("cargo:rustc-env=REDJS_AOT_TARGET_ARCH={target_arch}");
            println!("cargo:rustc-env=REDJS_AOT_TARGET_OS={target_os}");
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
            // Unix prebuilts already compile the engine-owned bindings and
            // wire schema into libJavaScriptCore. Only frontend tables remain
            // external; compiling these two again would duplicate symbols.
            if !windows
                && (name == "AOTBuiltinTableEngineBindings.cpp"
                    || name == "AOTSchemaInlineBuiltins.cpp")
            {
                continue;
            }
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
    if engine_dll.is_some() {
        // Into the DLL, not the Rust link.
        glue.file("cpp/engine_dll.cpp").cargo_metadata(false);
        println!("cargo:rerun-if-changed=cpp/engine_dll.cpp");
    }
    glue.compile("redjs_glue");

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
    let mut allocator = native_build(true, windows);
    if windows {
        allocator.flag("/TP");
    } else {
        allocator.flag("-x").flag("c++");
    }
    allocator
        .cargo_metadata(engine_dll.is_none())
        .file(mimalloc.join("src").join("static.c"))
        .include(mimalloc.join("include"))
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
        let mc = manifest
            .parent()
            .unwrap()
            .join("prebuilt")
            .join("sqlite3mc");
        check_sqlite3mc(&mc, &pins);
        let mut sqlite = native_build(false, windows);
        if windows {
            sqlite
                .flag("/Gy")
                .flag("/Gw")
                .define("_CRT_SECURE_NO_WARNINGS", None);
        } else {
            sqlite.flag("-ffunction-sections").flag("-fdata-sections");
        }
        sqlite
            .file("cpp/idb_sqlite.c")
            .include(&mc)
            .opt_level_str("s")
            .flag("-w")
            .define("NDEBUG", None)
            .compile("redjs_sqlite");
        println!("cargo:rerun-if-changed=cpp/idb_sqlite.c");
        println!("cargo:rerun-if-changed={}", mc.join(".redjs-pin").display());
    }

    if let Some(name) = &engine_dll {
        link_engine_dll(&manifest, &out_dir, &lib, name);
    } else {
        println!("cargo:rustc-link-search=native={}", lib.display());
    }

    // WebKit static libs. `-bundle`: not embedded into the staticlib artifact
    // (kept thin; NativeAOT link adds these libs itself), still linked into
    // the cdylib/test binaries as usual. In the engine DLL instead, with one.
    let libraries: &[&str] = if engine_dll.is_some() {
        &[]
    } else if windows {
        WINDOWS_WEBKIT_LIBS
    } else if target_os == "linux" {
        // Bun's ICU (79, with its zstd-compressed data) under names of our
        // own: a distribution's libicu-dev puts libicuuc.a/.so of another ICU
        // in /usr/lib, and a pkg-config crate's `-L /usr/lib/...` ahead of
        // ours would make `-licuuc` link that one (undefined `u_*_79`).
        for name in ["icui18n", "icuuc", "icudata"] {
            let alias = out_dir.join(format!("libredjs_{name}.a"));
            let _ = std::fs::remove_file(&alias);
            link_alias(&lib.join(format!("lib{name}.a")), &alias)
                .unwrap_or_else(|e| panic!("linking {} to Bun's lib{name}.a: {e}", alias.display()));
        }
        println!("cargo:rustc-link-search=native={}", out_dir.display());
        &[
            "JavaScriptCore",
            "WTF",
            "bmalloc",
            "redjs_icui18n",
            "redjs_icuuc",
            "redjs_icudata",
        ]
    } else {
        &["JavaScriptCore", "WTF", "bmalloc"]
    };
    for l in libraries {
        println!("cargo:rustc-link-lib=static:-bundle={l}");
    }

    // Windows system libs WebKit pulls in (with an engine DLL, its own link
    // takes them).
    if windows {
        if engine_dll.is_none() {
            for l in WINDOWS_SYSTEM_LIBS {
                println!("cargo:rustc-link-lib=dylib={l}");
            }
        }
    } else if target_os == "linux" {
        for library in ["dl", "pthread", "m", "rt", "atomic"] {
            println!("cargo:rustc-link-lib=dylib={library}");
        }
    } else {
        println!("cargo:rustc-link-lib=dylib=icucore");
        for framework in ["CoreFoundation", "Foundation", "Security"] {
            println!("cargo:rustc-link-lib=framework={framework}");
        }
        // Bun's hooks (its timer and error reporting) are weak references in
        // the prebuilt, and JSC tests them for null: unset, RunLoop takes its
        // generic path, the one redjs drives (glue.cpp, `rjs_run_loop_cycle`).
        // ELF leaves an unresolved weak reference null by itself; ld64 still
        // demands a definition unless told the symbol may be missing, and then
        // dyld binds the weak import to null. Images this package links only
        // (see /OPT:NOICF below): an embedder's executable passes the same
        // list, `BUN_HOOKS` here.
        if target_os == "macos" {
            for symbol in BUN_HOOKS {
                println!("cargo:rustc-link-arg=-Wl,-U,_{symbol}");
            }
        }
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
    if windows {
        println!("cargo:rustc-link-arg=/OPT:NOICF");
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=cpp/glue.cpp");
    println!("cargo:rerun-if-changed=cpp/compat/wtf/PlatformEnableWin.h");
    // setup.ps1 rewrites the stamp only after a verified fetch, so it stands
    // for the whole mimalloc tree.
    println!(
        "cargo:rerun-if-changed={}",
        mimalloc.join(".redjs-pin").display()
    );
    println!("cargo:rerun-if-changed={}", pins_path.display());
    println!(
        "cargo:rerun-if-changed={}",
        inc.join("cmakeconfig.h").display()
    );
    println!("cargo:rerun-if-env-changed=BUN_WEBKIT_DIR");
}

/// Links the engine DLL (`REDJS_ENGINE_DLL`) from the glue and mimalloc this
/// script compiled and the prebuilt's libraries, and points the Rust link at
/// its import library.
///
/// - Its exports are the names the Rust side declares in `extern "C"` blocks
///   (`ENGINE_IMPORTERS`) that one of those libraries defines, read from
///   their archive symbol tables (`link /dump /linkermember:1`, which reads
///   the table only, not 1.4 GB of JavaScriptCore.lib), and every public
///   `mi_*` function of mimalloc.
/// - `/OPT:NOICF`, as for every image JSC is in (see below), and `/Brepro`,
///   so the same inputs give the same DLL.
/// - It lands in OUT_DIR and is copied beside the images of this profile
///   (`target/<triple>/<profile>/` and its `deps/`), where they load it from.
fn link_engine_dll(manifest: &Path, out_dir: &Path, webkit_lib: &Path, name: &str) {
    let target = env::var("TARGET").unwrap();
    let tool = |name: &str| {
        cc::windows_registry::find_tool(&target, "link.exe")
            .unwrap_or_else(|| panic!("REDJS_ENGINE_DLL needs the MSVC linker ({name})"))
            .to_command()
    };
    let mut libraries: Vec<PathBuf> = ["redjs_glue.lib", "redjs_mimalloc.lib"].iter().map(|l| out_dir.join(l)).collect();
    libraries.extend(WINDOWS_WEBKIT_LIBS.iter().map(|l| webkit_lib.join(format!("{l}.lib"))));

    let mut defined = std::collections::HashSet::new();
    let mut mimalloc = Vec::new();
    for library in &libraries {
        let output = tool("link /dump")
            .args(["/dump", "/nologo", "/linkermember:1"])
            .arg(library)
            .output()
            .unwrap_or_else(|e| panic!("link /dump {}: {e}", library.display()));
        assert!(output.status.success(), "link /dump {} failed", library.display());
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut words = line.split_whitespace();
            if let (Some(offset), Some(symbol), None) = (words.next(), words.next(), words.next()) {
                if u64::from_str_radix(offset, 16).is_ok() {
                    if library.ends_with("redjs_mimalloc.lib") && symbol.starts_with("mi_") {
                        mimalloc.push(symbol.to_owned());
                    }
                    defined.insert(symbol.to_owned());
                }
            }
        }
    }

    let mut exports = std::collections::BTreeSet::new();
    for file in ENGINE_IMPORTERS {
        let path = manifest.join("src").join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        exports.extend(extern_c_names(&text).into_iter().filter(|n| defined.contains(n)));
    }
    // mimalloc's public API too: embedders read its stats and options (the
    // vue-native devtools memory report) from outside this crate.
    exports.extend(mimalloc);
    for required in ["rjs_init", "rjs_set_icu_decompress", "rjs_icu_zstd_dict", "JSGlobalContextCreate"] {
        assert!(exports.contains(required), "the engine DLL would not export {required}");
    }

    let stem = name.trim_end_matches(".dll");
    let def = out_dir.join(format!("{stem}.def"));
    let mut text = format!("LIBRARY \"{name}\"\nEXPORTS\n");
    for export in &exports {
        writeln!(text, "    {export}").unwrap();
    }
    fs::write(&def, text).unwrap();

    let dll = out_dir.join(name);
    let implib = out_dir.join("redjs_engine.lib");
    let output = tool("link")
        .args(["/nologo", "/DLL", "/MACHINE:X64", "/INCREMENTAL:NO", "/OPT:REF", "/OPT:NOICF", "/Brepro"])
        .arg(format!("/OUT:{}", dll.display()))
        .arg(format!("/IMPLIB:{}", implib.display()))
        .arg(format!("/DEF:{}", def.display()))
        .args(&libraries)
        .args(WINDOWS_SYSTEM_LIBS.iter().map(|l| format!("{l}.lib")))
        .output()
        .expect("cannot run the MSVC linker for the engine DLL");
    assert!(
        output.status.success(),
        "linking {name} failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=dylib=redjs_engine");
    // OUT_DIR is target/<triple>/<profile>/build/redjs-<hash>/out.
    if let Some(profile) = out_dir.ancestors().nth(3) {
        for directory in [profile.to_path_buf(), profile.join("deps")] {
            if directory.is_dir() {
                fs::copy(&dll, directory.join(name)).unwrap_or_else(|e| panic!("copying {name} to {}: {e}", directory.display()));
            }
        }
    }
}

/// The function and static names declared in the `extern "C" { … }` blocks
/// of a Rust source file.
fn extern_c_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("extern \"C\" {") {
        let body = &rest[at + "extern \"C\" {".len()..];
        let mut depth = 1;
        let mut end = body.len();
        for (i, c) in body.char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let block = &body[..end];
        let words: Vec<&str> = block
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|w| !w.is_empty())
            .collect();
        for pair in words.windows(2) {
            if pair[0] == "fn" || pair[0] == "static" {
                let name = if pair[1] == "mut" { continue } else { pair[1] };
                names.push(name.to_owned());
            }
        }
        for triple in words.windows(3) {
            if triple[0] == "static" && triple[1] == "mut" {
                names.push(triple[2].to_owned());
            }
        }
        rest = &body[end..];
    }
    names
}

fn native_build(cpp: bool, windows: bool) -> cc::Build {
    let mut build = cc::Build::new();
    build.cpp(cpp);
    if windows {
        build.compiler("clang-cl").static_crt(true);
    } else {
        let variable = if cpp { "CXX" } else { "CC" };
        let compiler =
            env::var(variable).unwrap_or_else(|_| if cpp { "clang++" } else { "clang" }.into());
        build.compiler(compiler).pic(true);
    }
    build
}

/// `pins.txt`: `key = value` lines, `#` comments. See that file.
fn read_pins(path: &Path) -> Vec<(String, String)> {
    let text = fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e} — it pins the prebuilt redjs builds against",
            path.display()
        )
    });
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
    let fix =
        "Run setup.ps1 (or setup.sh) at the redjs repository root: it fetches the pinned \
               prebuilt and REPLACES one at any other version. Or point BUN_WEBKIT_DIR at an extraction of the pinned tarball.";
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
            have.as_deref()
                .unwrap_or("unidentified (no BUN_WEBKIT_VERSION)"),
        ),
    }
    let want_aot = pin(pins, "webkit.aot");
    let have_aot = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("#define BUN_WEBKIT_ENABLE_AOT "))
        .map(str::trim);
    assert_eq!(
        have_aot,
        Some(want_aot),
        "JSC AOT variant mismatch at {}: wanted ENABLE_AOT={want_aot}. {fix}",
        webkit.display()
    );
    let library = if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        "JavaScriptCore.lib"
    } else {
        "libJavaScriptCore.a"
    };
    if !webkit.join("lib").join(library).is_file() {
        panic!(
            "\n\n{} names the pinned build but has no lib/{library} — an interrupted \
             extraction?\n{fix}\n",
            webkit.display()
        );
    }
}

/// setup.ps1 writes `cpp/mimalloc/.redjs-pin` (the commit) only after the
/// extracted tree's sha256 matched pins.txt.
fn check_mimalloc(dir: &Path, pins: &[(String, String)]) {
    let want = pin(pins, "mimalloc.commit");
    let have = fs::read_to_string(dir.join(".redjs-pin"))
        .ok()
        .map(|s| s.trim().to_owned());
    if have.as_deref() != Some(want) || !dir.join("src").join("static.c").is_file() {
        panic!(
            "\n\nmimalloc sources at {} are {}, but redjs pins oven-sh/mimalloc@{want} (pins.txt).\n\
             This JSC build allocates through the embedder's mimalloc, so redjs compiles it.\n\
             Run setup.ps1 (or setup.sh) at the redjs repository root to fetch it.\n",
            dir.display(),
            have.as_deref().unwrap_or("missing"),
        );
    }
}

/// setup.ps1 writes `prebuilt/sqlite3mc/.redjs-pin` (`<version> <tree sha256>`)
/// only after the release asset's sha256 AND the extracted tree's matched
/// pins.txt; a stamp naming anything else is a tree this build refuses.
fn check_sqlite3mc(dir: &Path, pins: &[(String, String)]) {
    let want = format!(
        "{} {}",
        pin(pins, "sqlite3mc.version"),
        pin(pins, "sqlite3mc.tree-sha256")
    );
    let have = fs::read_to_string(dir.join(".redjs-pin"))
        .ok()
        .map(|s| s.trim().to_owned());
    if have.as_deref() != Some(want.as_str()) || !dir.join("sqlite3mc_amalgamation.c").is_file() {
        panic!(
            "\n\nthe SQLite3 Multiple Ciphers amalgamation at {} is {}, but redjs pins `{want}` (pins.txt).\n\
             The `indexeddb-sqlite` feature compiles it into IndexedDB's persistent backend.\n\
             Run setup.ps1 (or setup.sh) at the redjs repository root to fetch and verify it.\n",
            dir.display(),
            have.as_deref().unwrap_or("missing"),
        );
    }
}

fn generate_trampolines(out_dir: &std::path::Path) {
    let abi = if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        "sysv64"
    } else {
        "C"
    };
    let mut s = String::with_capacity(TRAMPOLINE_COUNT * 160);
    let _ = writeln!(
        s,
        "// @generated by build.rs — SysV entry points for host functions.\n\
         pub(crate) const TRAMPOLINE_COUNT: usize = {TRAMPOLINE_COUNT};\n"
    );
    for i in 0..TRAMPOLINE_COUNT {
        let _ = writeln!(
            s,
            "unsafe extern \"{abi}\" fn t{i}(g: *mut core::ffi::c_void, cf: *mut core::ffi::c_void) -> u64 {{ crate::trampoline::dispatch({i}, g, cf) }}"
        );
    }
    let _ = writeln!(
        s,
        "pub(crate) static TRAMPOLINES: [unsafe extern \"{abi}\" fn(*mut core::ffi::c_void, *mut core::ffi::c_void) -> u64; {TRAMPOLINE_COUNT}] = ["
    );
    for i in 0..TRAMPOLINE_COUNT {
        let _ = writeln!(s, "    t{i},");
    }
    let _ = writeln!(s, "];");
    fs::write(out_dir.join("trampolines.rs"), s).unwrap();
}

/// `alias` naming `target`: a symlink, or a copy on a host without them
/// (a cross build from Windows).
fn link_alias(target: &std::path::Path, alias: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, alias);
    #[cfg(not(unix))]
    return std::fs::copy(target, alias).map(|_| ());
}
