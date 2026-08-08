//! Runs a real Electron application's first screen INSIDE a redjs context, rendered through
//! `@engine/ui` (HTML-vocabulary adapter → retained tree → native Yoga → the CSS cascade).
//!
//! `cargo run --release --example argon_ui -- <file.js> [<file.js> …]`
//!
//! Each file is evaluated in order as a classic script (the last one is the app bundle). The
//! example then pumps the web scheduler — timers, rAF, microtasks — until the bundle sets
//! `globalThis.__ARGON_DONE`, and prints `globalThis.__ARGON_REPORT`.
//!
//! Same shape and same reason as `yoga_conformance`: the artifact under test is produced by the JS
//! toolchain, so the run has to be driven from outside `cargo test`.

fn main() {
    let files: Vec<String> = std::env::args().skip(1).collect();
    if files.is_empty() {
        eprintln!("usage: cargo run --release --example argon_ui -- <file.js> [<file.js> …]");
        std::process::exit(2);
    }

    // The bundle's directory is the fetch/Image resolution base.
    let root = std::path::Path::new(files.last().unwrap())
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let root16: Vec<u16> = root.encode_utf16().collect();

    let ctx = redjs::api::Context::new();
    unsafe {
        // The scheduler FIRST: `performance`, rAF and the timer globals come from here, and the
        // web shim's async image loads are driven by it (documented on `redjs_webshim_install`).
        redjs::ffi::redjs_web_install(ctx.raw());
        redjs::ffi::redjs_webshim_install(
            ctx.raw(),
            root16.as_ptr(),
            root16.len(),
            root16.as_ptr(),
            root16.len(),
        );
    }

    for path in &files {
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cannot read {path}: {e}");
                std::process::exit(2);
            }
        };
        eprintln!("[argon_ui] eval {path} ({} KB)", src.len() / 1024);
        if let Err(e) = ctx.eval_named(&src, path) {
            // The exception VALUE is useless on its own; JSC stacks also carry no message line
            // (see `pewpew-jscore-redjs`), so name and message are read separately.
            if let redjs::api::Error::Exception(v) = e {
                eprintln!("[argon_ui] {path} threw: {}", describe(&ctx, v));
            } else {
                eprintln!("[argon_ui] {path} threw: {e:?}");
            }
            // Keep going: a throw in a prelude is worth seeing alongside whatever still ran.
        }
    }

    // Pump. The budget is generous but finite — a screen that never finishes is a finding, and the
    // report is printed either way.
    let mut stats = redjs::ffi::RjsPumpStats::default();
    let mut frames = 0u32;
    let max_frames = 20_000u32;
    let start = std::time::Instant::now();
    loop {
        unsafe { redjs::ffi::redjs_pump(ctx.raw(), 7, &mut stats) };
        frames += 1;
        let done = ctx
            .eval("globalThis.__ARGON_DONE === true")
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if done {
            eprintln!("[argon_ui] done after {frames} frames / {:?}", start.elapsed());
            break;
        }
        if frames >= max_frames || start.elapsed().as_secs() > 120 {
            eprintln!("[argon_ui] BUDGET EXHAUSTED after {frames} frames — printing what exists");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    match ctx
        .eval("String(globalThis.__ARGON_REPORT ?? '(no report)')")
        .and_then(|v| ctx.to_rust_string(v))
    {
        Ok(s) => println!("{s}"),
        Err(e) => {
            eprintln!("could not read the report: {e:?}");
            std::process::exit(1);
        }
    }
}

/// `name: message` + stack for a thrown value. JSC's `error.stack` carries NO message line, so the
/// two are read separately and joined here — the single most repeated debugging tax in this runtime.
fn describe(ctx: &redjs::api::Context, v: redjs::api::Value) -> String {
    let mut msg = 0u64;
    let mut stack = 0u64;
    let ok = unsafe { redjs::ffi::redjs_error_info(ctx.raw(), v.0, &mut msg, &mut stack) };
    if ok != 0 {
        return format!("<non-error value: {:#x}>", v.0);
    }
    let s = |x: u64| {
        ctx.to_rust_string(redjs::api::Value(x))
            .unwrap_or_else(|_| String::from("<?>"))
    };
    format!("{}\n{}", s(msg), s(stack))
}
