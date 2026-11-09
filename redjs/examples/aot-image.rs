use redjs::api::{Context, HostArgs, Value};
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("info") => println!("{{\"target\":\"x86_64-windows\",\"frontend\":\"{}\",\"engine\":\"{}\",\"tableId\":\"{}\",\"compileOptions\":\"{}\"}}", redjs::aot::FRONTEND_REVISION,redjs::aot::ENGINE_VERSION,redjs::aot::BUILD_TABLE_ID,redjs::aot::COMPILE_OPTIONS),
        Some("run-verified") => return run_verified(args.get(2).ok_or("image path required")?, selected_profile(args.get(3).map(String::as_str))?),
        Some("reject-handles") => return reject_handles(args.get(2).ok_or("corrupt image path required")?),
        Some("run-stress") => return run_stress(args.get(2).ok_or("image path required")?, selected_profile(args.get(3).map(String::as_str))?),
        Some("run-web") => return run_web(args.get(2).ok_or("image path required")?, selected_profile(args.get(3).map(String::as_str))?, args.get(4).is_some_and(|x| x=="switch"), false),
        Some("run-web-host-startup") => return run_web(args.get(2).ok_or("image path required")?, selected_profile(args.get(3).map(String::as_str))?, false, true),
        Some("run-web-host-startup-jit") => return run_web_host_startup_jit(args.get(2).ok_or("source path required")?),
        Some("reject-image") => {
            let path=args.get(2).ok_or("image path required")?;
            let identity: [u8;16]=std::fs::read(format!("{path}.identity"))?.try_into().map_err(|_|"invalid identity")?;
            let profile=selected_profile(args.get(3).map(String::as_str))?;
            let error=unsafe{redjs::aot::load_image_with_profile(Path::new(path),identity,profile)}.expect_err("invalid image accepted");
            assert!(!redjs::aot::is_active());assert!(redjs::aot::memory_settings().is_none());
            println!("rejected={error};active=false");
        }
        Some("reject-late-profile") => {
            let path=args.get(2).ok_or("image path required")?;
            let identity: [u8;16]=std::fs::read(format!("{path}.identity"))?.try_into().map_err(|_|"invalid identity")?;
            let _jit=Context::new();
            let error=unsafe{redjs::aot::load_image_with_profile(Path::new(path),identity,redjs::aot::MemoryProfile::Compact)}.expect_err("late profile accepted");
            assert!(!redjs::aot::is_active());assert!(redjs::aot::memory_settings().is_none());
            println!("late_rejected={error};active=false");
        }
        Some("compile") => {
            let input = std::fs::read(args.get(2).ok_or("IR path required")?)?;
            let image = unsafe { redjs::aot::compile_ir(&input) }?;
            let out = args.get(3).ok_or("output path required")?;
            std::fs::write(out, &image.bytes)?;
            std::fs::write(format!("{out}.identity"), image.identity)?;
            std::fs::write(
                format!("{out}.entries"),
                image
                    .entries
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )?;
            println!(
                "image_bytes={} entries={:?} diagnostics={}",
                image.bytes.len(),
                image.entries,
                image.diagnostics
            );
        }
        Some("run") => {
            let path = args.get(2).ok_or("image path required")?;
            let identity: [u8; 16] = std::fs::read(format!("{path}.identity"))?
                .try_into()
                .map_err(|_| "invalid identity")?;
            load_selected(path,identity,selected_profile(args.get(3).map(String::as_str))?,false)?;
            let context = Context::new();
            let host = context.function("__vn_aot_probe", 1, |_global, frame| {
                let args = unsafe { HostArgs::new(frame) };
                Value::number(args.arg(0).as_number().unwrap_or(0.0) * 7.0).0
            })?;
            context.global_set("__vn_aot_probe", host)?;
            let entries = std::fs::read_to_string(format!("{path}.entries"))?;
            let entry: u32 = entries.lines().next().ok_or("no entry")?.parse()?;
            assert!(matches!(context.run_aot_module(0), Err(redjs::api::Error::BadArg)));
    assert!(matches!(context.run_aot_module(u32::MAX), Err(redjs::api::Error::BadArg)));
    let evaluation = context.run_aot_module(entry).map_err(|error| match error {
                redjs::api::Error::Exception(value) => context
                    .to_rust_string(value)
                    .unwrap_or_else(|_| "JS exception".into()),
                other => other.to_string(),
            })?;
            if matches!(evaluation, redjs::aot::ModuleEvaluation::Pending(_)) { return Err("entry still awaits host work".into()); }
            context.gc();
            let result = context.global_get("__vn_aot_answer")?;
            println!(
                "aot_active={} answer={}",
                redjs::aot::is_active(),
                context.to_number(result)?
            );
            if context.to_number(result)? != 42.0 {
                return Err("native callback result did not equal 42".into());
            }
        }
        _ => {
            return Err("usage: aot-image compile INPUT.ir OUTPUT.image | run OUTPUT.image [default|compact] | run-web OUTPUT.image [default|compact] [switch] | run-web-host-startup IMAGE [default|compact] | run-web-host-startup-jit SOURCE.js | run-stress IMAGE [default|compact] | run-verified IMAGE [default|compact] | reject-handles CORRUPT_IMAGE | reject-image IMAGE [default|compact] | reject-late-profile IMAGE".into())
        }
    }
    Ok(())
}

fn selected_profile(
    value: Option<&str>,
) -> Result<redjs::aot::MemoryProfile, Box<dyn std::error::Error>> {
    match value {
        None | Some("default") => Ok(redjs::aot::MemoryProfile::Default),
        Some("compact") => Ok(redjs::aot::MemoryProfile::Compact),
        _ => Err("memory profile must be default or compact".into()),
    }
}
fn load_selected(
    path: &str,
    identity: [u8; 16],
    profile: redjs::aot::MemoryProfile,
    check_switch: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use redjs::aot::{self, MemoryProfile};
    if profile == MemoryProfile::Default {
        unsafe { aot::load_image(Path::new(path), identity) }?;
    } else {
        unsafe { aot::load_image_with_profile(Path::new(path), identity, profile) }?;
    }
    let settings = aot::memory_settings().ok_or("accepted image lacks settings")?;
    assert_eq!(settings.profile, profile);
    assert!(
        settings.initial_image_read_only,
        "initial image mapping must be read-only"
    );
    if profile == MemoryProfile::Compact {
        assert_eq!(settings.structure_heap_size_kb, 262144);
        assert_eq!(settings.marked_block_cage_size_mb, 1024);
    }
    println!("memory={settings:?}");
    if check_switch {
        let other = if profile == MemoryProfile::Default {
            MemoryProfile::Compact
        } else {
            MemoryProfile::Default
        };
        let error = unsafe { aot::load_image_with_profile(Path::new(path), identity, other) }
            .expect_err("profile switch accepted");
        assert_eq!(aot::memory_settings(), Some(settings));
        assert!(aot::is_active());
        println!("switch_rejected={error};original_profile_preserved=true");
    }
    Ok(())
}

fn run_web(
    path: &str,
    profile: redjs::aot::MemoryProfile,
    check_switch: bool,
    verify_host_startup: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let identity: [u8; 16] = std::fs::read(format!("{path}.identity"))?
        .try_into()
        .map_err(|_| "invalid identity")?;
    load_selected(path, identity, profile, check_switch)?;
    run_web_loaded(path, verify_host_startup)
}
fn run_web_loaded(path: &str, verify_host_startup: bool) -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::{Duration, Instant};
    let context = Context::new();
    unsafe {
        assert_eq!(redjs::ffi::redjs_web_install(context.raw()), 0);
        assert_eq!(
            redjs::ffi::redjs_webshim_install(
                context.raw(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                0
            ),
            0
        );
    }
    println!("web_bootstrap_before_native_entry=ok");
    context.global_set("__VN_AOT_HOST__", context.global())?;
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let host = context.function("__vn_aot_probe", 1, move |global, frame| {
        count.fetch_add(1, Ordering::SeqCst);
        let args = unsafe { HostArgs::new(frame) };
        let value = args.arg(0).as_number().unwrap_or(0.0);
        unsafe { redjs::ffi::redjs_gc(global, 0) };
        Value::number(value * 7.0).0
    })?;
    context.global_set("__vn_aot_probe", host)?;
    let entry: u32 = std::fs::read_to_string(format!("{path}.entries"))?
        .lines()
        .next()
        .ok_or("no entry")?
        .parse()?;
    let evaluation = context.run_aot_module(entry).map_err(|error| match error {
        redjs::api::Error::Exception(value) => context
            .to_rust_string(value)
            .unwrap_or_else(|_| "JS exception".into()),
        other => other.to_string(),
    })?;
    println!("initial={evaluation:?}");
    if verify_host_startup {
        assert_eq!(
            context.to_rust_string(context.global_get("__vn_aot_math_guard")?)?,
            "native-locked",
            "compiled Math.abs write was accepted or did not preserve the original"
        );
        let refusal = context.to_rust_string(context.global_get("__vn_aot_math_refusal")?)?;
        assert!(
            refusal.contains("Math.abs"),
            "unexpected mutation refusal: {refusal}"
        );
        println!("compiled_math_abs_after_host_startup=native-locked;{refusal}");
        let result = context.eval_named("20 + 22", "host-eval-probe.js")?;
        assert_eq!(context.to_number(result)?, 42.0);
        let policy = context.eval_named("JSON.stringify([(() => {try {(0,eval)('42');return 'allowed';}catch(e){return e.name;}})(), (() => {try {new Function('return 42');return 'allowed';}catch(e){return e.name;}})()])", "image-policy-probe.js")?;
        assert_eq!(context.to_rust_string(policy)?, "[\"EvalError\",\"EvalError\"]", "closed fixture must retain its image's evaluation policy");
        println!("closed_image_js_eval_and_function=refused;trusted_host_program=42");

    }
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        unsafe { redjs::ffi::redjs_pump(context.raw(), 7, std::ptr::null_mut()) };
        if context
            .to_number(context.global_get("__vn_aot_answer")?)
            .unwrap_or(0.0)
            == 42.0
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    context.gc();
    assert!(matches!(
        context.run_aot_module(0),
        Err(redjs::api::Error::BadArg)
    ));
    assert!(matches!(
        context.run_aot_module(u32::MAX),
        Err(redjs::api::Error::BadArg)
    ));
    for (name, expected) in [
        ("__vn_aot_clone", "ok"),
        ("__vn_aot_url", "example.invalid"),
        ("__vn_aot_event", "1"),
        ("__vn_aot_async", "42"),
        (
            "__vn_aot_builtins",
            "[true,\"ok\",42,2,true,\"abc\",1234,6]",
        ),
    ] {
        assert_eq!(
            context.to_rust_string(context.global_get(name)?)?,
            expected,
            "{name}"
        );
    }
    for name in [
        "__vn_aot_clone",
        "__vn_aot_url",
        "__vn_aot_event",
        "__vn_aot_async",
        "__vn_aot_builtins",
    ] {
        println!(
            "{name}={}",
            context.to_rust_string(context.global_get(name)?)?
        );
    }
    for name in [
        "__vn_aot_plain",
        "__vn_aot_array",
        "__vn_aot_callback",
        "__vn_aot_map",
        "__vn_aot_date",
        "__vn_aot_error",
    ] {
        let value = context.global_get(name)?;
        let mut info = redjs::glue::RjsBrandInfo::default();
        let brand = unsafe { redjs::glue::rjs_brand(context.raw(), value.0, &mut info) };
        println!("brand:{name}={brand} flags={}", info.flags);
    }
    let answer = context.to_number(context.global_get("__vn_aot_answer")?)?;
    println!(
        "native={};answer={answer};rust_calls={}",
        redjs::aot::is_active(),
        calls.load(Ordering::SeqCst)
    );
    assert_eq!(answer, 42.0);
    assert!(calls.load(Ordering::SeqCst) >= 2);
    Ok(())
}

fn run_web_host_startup_jit(source_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    assert!(
        !redjs::aot::is_active(),
        "JIT contrast requires a fresh process without an image"
    );
    let context = Context::new();
    unsafe {
        assert_eq!(redjs::ffi::redjs_web_install(context.raw()), 0);
        assert_eq!(
            redjs::ffi::redjs_webshim_install(
                context.raw(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                0
            ),
            0
        );
    }
    context.global_set("__VN_AOT_HOST__", context.global())?;
    let callback = context.function("__vn_aot_probe", 1, |_global, frame| {
        let arguments = unsafe { HostArgs::new(frame) };
        Value::number(arguments.arg(0).as_number().unwrap_or(0.0) * 7.0).0
    })?;
    context.global_set("__vn_aot_probe", callback)?;
    let source = std::fs::read_to_string(source_path)?;
    // Direct embedding API, not the JS indirect eval disabled by native images.
    context.eval_named(&source, "aot-host-startup-jit.js")?;
    assert_eq!(
        context.to_rust_string(context.global_get("__vn_aot_math_guard")?)?,
        "jit-writable",
        "JIT contrast did not restore its accepted write"
    );
    assert_eq!(
        context.to_rust_string(context.global_get("__vn_aot_math_refusal")?)?,
        ""
    );
    println!("same_source_math_abs=jit-writable;original_restored=true");
    Ok(())
}
fn run_stress(
    path: &str,
    profile: redjs::aot::MemoryProfile,
) -> Result<(), Box<dyn std::error::Error>> {
    use redjs::{ffi, glue, webapi::util as u};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let identity: [u8; 16] = std::fs::read(format!("{path}.identity"))?
        .try_into()
        .map_err(|_| "invalid identity")?;
    load_selected(path, identity, profile, true)?;
    let context = Context::new();
    context.global_set("__VN_AOT_HOST__", context.global())?;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let host = context.function("__vn_aot_stress_gc", 1, move |global, frame| unsafe {
        let args = HostArgs::new(frame);
        let token = args.arg(0).as_number().unwrap_or(f64::NAN);
        if !token.is_finite() || token < 0.0 || token.fract() != 0.0 {
            return u::throw_type_error(global, "invalid stress GC token");
        }
        counter.fetch_add(1, Ordering::SeqCst);
        ffi::redjs_gc(global, 0);
        // Reenter the compiled JS callback while the allocating compiled loop
        // and its Rust callback are live. That callback never calls this host.
        let reenter = u::get(global, glue::rjs_global(global), "__vn_aot_stress_reenter");
        match u::call(global, reenter, u::UNDEF, &[Value::number(token).0]) {
            Ok(value) if u::dec_f64(value) == Some(((token as u32) ^ 0x5A5A) as f64) => {
                Value::number(token + 1.0).0
            }
            Ok(_) => u::throw_type_error(global, "compiled reentry result corrupted"),
            Err(error) => {
                glue::rjs_throw(global, error);
                u::UNDEF
            }
        }
    })?;
    context.global_set("__vn_aot_stress_gc", host)?;
    let entry: u32 = std::fs::read_to_string(format!("{path}.entries"))?
        .lines()
        .next()
        .ok_or("no entry")?
        .parse()?;
    let evaluation = context.run_aot_module(entry).map_err(|error| match error {
        redjs::api::Error::Exception(value) => context
            .to_rust_string(value)
            .unwrap_or_else(|_| "JS exception".into()),
        other => other.to_string(),
    })?;
    if matches!(evaluation, redjs::aot::ModuleEvaluation::Pending(_)) {
        return Err("stress unexpectedly awaits host work".into());
    }
    context.gc();
    let verified = context.call(
        context.global_get("__vn_aot_stress_verify")?,
        Value::undefined(),
        &[],
    )?;
    assert_eq!(
        context.to_number(verified)?,
        128.0,
        "retained samples after final GC"
    );
    assert_eq!(
        context.to_rust_string(context.global_get("__vn_aot_stress_result")?)?,
        "ok"
    );
    let stats = context.to_rust_string(context.global_get("__vn_aot_stress_stats")?)?;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        61,
        "explicit GC/reentry callback count"
    );
    println!("stress={stats}");
    println!(
        "native={};stress=ok;rust_gc_reentries={}",
        redjs::aot::is_active(),
        calls.load(Ordering::SeqCst)
    );
    Ok(())
}

fn open_image(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .access_mode(0xA0000000)
        .share_mode(1 | 4)
        .open(path)
}
fn run_verified(
    path: &str,
    profile: redjs::aot::MemoryProfile,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{Seek, SeekFrom};
    use std::os::windows::fs::symlink_file;
    let original = std::fs::canonicalize(path)?;
    let fixture = original
        .parent()
        .ok_or("image parent required")?
        .join(format!("verified-file-{}", std::process::id()));
    std::fs::create_dir(&fixture)?;
    let replacement = fixture.join("replacement.image");
    let alias = fixture.join("selected.image");
    let mut corrupt = std::fs::read(&original)?;
    corrupt[..16].fill(0);
    std::fs::write(&replacement, corrupt)?;
    symlink_file(&original, &alias)?;
    let identity: [u8; 16] = std::fs::read(format!("{path}.identity"))?
        .try_into()
        .map_err(|_| "invalid identity")?;
    let mut file = open_image(&alias)?;
    let verified_sha = redjs::aot::digest_reader(&file)?;
    std::fs::remove_file(&alias)?;
    symlink_file(&replacement, &alias)?;
    let reopened = open_image(&alias)?;
    assert_ne!(
        redjs::aot::digest_reader(reopened)?,
        verified_sha,
        "symlink was not retargeted"
    );
    file.seek(SeekFrom::Start(0))?;
    assert_eq!(
        redjs::aot::digest_reader(&file)?,
        verified_sha,
        "pinned file changed"
    );
    unsafe { redjs::aot::load_image_file_with_profile(&file, identity, profile) }?;
    assert!(file.metadata()?.len() > 0, "loader consumed caller handle");
    let settings = redjs::aot::memory_settings().ok_or("missing verified-file settings")?;
    assert_eq!(settings.profile, profile);
    assert!(settings.initial_image_read_only);
    drop(file); // Engine must keep its own duplicate, including executable mappings.
    println!("memory={settings:?}");
    println!("retargeted=true;verified_file_sha={verified_sha};reopened_path_differs=true;caller_file_dropped=true;fixture={}",fixture.display());
    run_web_loaded(path, false)
}
fn reject_handles(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    unsafe extern "C" {
        fn rjs_aot_load_handle_with_profile(file: isize, identity: *const u8, profile: u32) -> u32;
    }
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn GetProcessHandleCount(process: isize, count: *mut u32) -> i32;
    }
    use std::io::{Seek, SeekFrom};
    use std::os::windows::io::AsRawHandle;
    let mut file = open_image(Path::new(path))?;
    let expected = redjs::aot::digest_reader(&file)?;
    let identity: [u8; 16] = std::fs::read(format!("{path}.identity"))?
        .try_into()
        .map_err(|_| "invalid identity")?;
    let handle = file.as_raw_handle() as isize;
    // Warm the engine before counting so one-time initialization is excluded.
    assert_eq!(
        unsafe { rjs_aot_load_handle_with_profile(handle, identity.as_ptr(), 0) },
        3
    );
    let count = || {
        let mut value = 0;
        assert_ne!(
            unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut value) },
            0
        );
        value
    };
    let before = count();
    for _ in 0..1000 {
        assert_eq!(
            unsafe { rjs_aot_load_handle_with_profile(0, identity.as_ptr(), 0) },
            17
        );
        assert_eq!(
            unsafe { rjs_aot_load_handle_with_profile(-1, identity.as_ptr(), 0) },
            17
        );
        assert_eq!(
            unsafe { rjs_aot_load_handle_with_profile(handle, identity.as_ptr(), u32::MAX) },
            11
        );
        assert_eq!(
            unsafe { rjs_aot_load_handle_with_profile(handle, identity.as_ptr(), 0) },
            3
        );
    }
    let after = count();
    assert_eq!(after, before, "failed loads leaked kernel handles");
    file.seek(SeekFrom::Start(0))?;
    assert_eq!(
        redjs::aot::digest_reader(&file)?,
        expected,
        "caller handle invalidated"
    );
    assert!(!redjs::aot::is_active());
    assert!(redjs::aot::memory_settings().is_none());
    println!("rejected_iterations=1000;handles_before={before};handles_after={after};caller_file_usable=true;active=false");
    Ok(())
}
