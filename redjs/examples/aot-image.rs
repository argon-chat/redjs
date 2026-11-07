use redjs::api::{Context, HostArgs, Value};
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("info") => println!("{{\"target\":\"x86_64-windows\",\"frontend\":\"{}\",\"engine\":\"{}\",\"tableId\":\"{}\",\"compileOptions\":\"{}\"}}", redjs::aot::FRONTEND_REVISION,redjs::aot::ENGINE_VERSION,redjs::aot::BUILD_TABLE_ID,redjs::aot::COMPILE_OPTIONS),
        Some("run-web") => return run_web(args.get(2).ok_or("image path required")?),
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
            unsafe { redjs::aot::load_image(Path::new(path), identity) }?;
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
            return Err("usage: aot-image compile INPUT.ir OUTPUT.image | run OUTPUT.image".into())
        }
    }
    Ok(())
}

fn run_web(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::{Duration, Instant};
    let identity: [u8; 16] = std::fs::read(format!("{path}.identity"))?
        .try_into()
        .map_err(|_| "invalid identity")?;
    unsafe { redjs::aot::load_image(Path::new(path), identity) }?;
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
