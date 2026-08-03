//! Runs the `@engine/ui` layout conformance suite INSIDE a redjs context, against the real
//! `RedYoga` host class — the gate `UI-51` (native layout backend) is decided on.
//!
//! `cargo run --example yoga_conformance -- <bundle.js>`
//!
//! Driven by `scripts/yoga-native-conformance.ts`, which esbuilds
//! `packages/ui/test/native/yogaConformanceEntry.ts` and passes the bundle path here. The bundle
//! sets `globalThis.__YOGA_CONFORMANCE` to a JSON report; this prints it and exits non-zero when
//! the native backend disagrees with the wasm goldens.
//!
//! Why an example and not a `tests/` case: the bundle is produced by the JS toolchain, so the
//! artifact does not exist at `cargo test` time and the run has to be driven from the outside.

use std::ptr;

fn main() {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: cargo run --example yoga_conformance -- <bundle.js>");
            std::process::exit(2);
        }
    };
    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            std::process::exit(2);
        }
    };

    let ctx = redjs::api::Context::new();
    // Installs the web shim AND the native host classes (`webapi::install` → `crate::ui::install`,
    // which is what defines `globalThis.RedYoga`). No script/asset roots: the bundle is
    // self-contained and touches no filesystem.
    unsafe {
        redjs::ffi::redjs_webshim_install(ctx.raw(), ptr::null(), 0, ptr::null(), 0);
    }

    if let Err(e) = ctx.eval_named(&src, "yoga-conformance.js") {
        eprintln!("bundle threw: {e:?}");
        std::process::exit(1);
    }

    let value = match ctx.eval("globalThis.__YOGA_CONFORMANCE") {
        Ok(v) => v,
        Err(e) => {
            eprintln!("could not read the report: {e:?}");
            std::process::exit(1);
        }
    };
    let report = match ctx.to_rust_string(value) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("report is not a string: {e:?}");
            std::process::exit(1);
        }
    };

    println!("{report}");
    // The script layer parses and pretty-prints; the exit code is the machine-readable half.
    if report.contains("\"ok\":true") {
        std::process::exit(0);
    }
    std::process::exit(1);
}
