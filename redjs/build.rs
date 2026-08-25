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
//! 4. Links the WebKit static libs. They are linked with the `-bundle`
//!    modifier so the `staticlib` artifact (redjs.lib for NativeAOT) stays
//!    thin — the consuming link (ILC) must also pass the WebKit libs. The
//!    `cdylib` artifact (redjs.dll) is always fully self-contained.
//!
//! Prebuilt location: `BUN_WEBKIT_DIR` env or `../prebuilt/bun-webkit`.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

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

    if !lib.join("JavaScriptCore.lib").exists() {
        println!(
            "cargo:warning=JavaScriptCore.lib not found under {}. Run packages/jscore/setup.ps1 \
             (or set BUN_WEBKIT_DIR).",
            lib.display()
        );
    }

    // --- C++ glue against WebKit internal headers -------------------------
    // Flags mirror how the prebuilt itself was produced (proven in
    // battle_test/bench_jsc): clang-cl, C++23, static CRT, NDEBUG, and the
    // prebuilt's own cmakeconfig.h force-included so every USE_/ENABLE_ flag
    // (incl. USE_BUN_JSC_ADDITIONS) matches its ABI exactly.
    cc::Build::new()
        .cpp(true)
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
        .define("_CRT_SECURE_NO_WARNINGS", None)
        .compile("redjs_glue");

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
    }

    println!("cargo:rustc-link-search=native={}", lib.display());

    // WebKit static libs. `-bundle`: not embedded into the staticlib artifact
    // (kept thin; NativeAOT link adds these libs itself), still linked into
    // the cdylib/test binaries as usual.
    for l in ["JavaScriptCore", "WTF", "bmalloc", "sicudt", "sicuin", "sicuuc"] {
        println!("cargo:rustc-link-lib=static:-bundle={l}");
    }

    // Windows system libs WebKit pulls in.
    for l in [
        "user32", "ole32", "oleaut32", "advapi32", "shell32", "shlwapi", "winmm", "bcrypt",
        "dbghelp", "userenv", "ws2_32", "synchronization", "usp10", "rpcrt4", "crypt32",
    ] {
        println!("cargo:rustc-link-lib=dylib={l}");
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=cpp/glue.cpp");
    println!("cargo:rerun-if-changed=cpp/compat/wtf/PlatformEnableWin.h");
    // Directory form: cargo walks it recursively, so headers and any future
    // .cpp under cpp/yoga/ are covered without listing them.
    println!("cargo:rerun-if-changed=cpp/yoga");
    println!("cargo:rerun-if-env-changed=BUN_WEBKIT_DIR");
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
