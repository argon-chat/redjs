//! Standalone JSC -> wgpu -> DirectX 12 compute/readback probe.
//! No GPUI, browser, account/profile, audio device or window is involved.
use redjs::api::Context;
use std::time::{Duration, Instant};

fn main() {
    let c = Context::new();
    unsafe {
        assert_eq!(redjs::ffi::redjs_web_install(c.raw()), 0);
        assert_eq!(
            redjs::ffi::redjs_webshim_install(c.raw(), core::ptr::null(), 0, core::ptr::null(), 0),
            0
        );
    }
    c.eval(include_str!("webgpu.js"))
        .expect("WebGPU probe script failed");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        unsafe {
            redjs::ffi::redjs_lock(c.raw());
            redjs::ffi::redjs_pump(
                c.raw(),
                redjs::web::PUMP_TIMERS | redjs::web::PUMP_MICROTASKS,
                core::ptr::null_mut(),
            );
            redjs::ffi::redjs_unlock(c.raw());
        }
        let value = c
            .eval("globalThis.__webgpuResult")
            .expect("Probe result unavailable");
        let result = c
            .to_rust_string(value)
            .expect("Probe result must be a string");
        if result != "pending" {
            println!("{result}");
            assert!(result.starts_with("PASS "), "{result}");
            break;
        }
        assert!(Instant::now() < deadline, "WebGPU probe timed out");
        std::thread::sleep(Duration::from_millis(2));
    }
}
