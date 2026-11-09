use core::ffi::c_void;

// This must type-check against the *target* ABI even when the proc macro
// itself runs on another architecture (for example Linux -> Apple Silicon).
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
type HostFn = unsafe extern "sysv64" fn(*mut c_void, *mut c_void) -> u64;
#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
type HostFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u64;

#[redjs_macros::host_fn]
#[inline(never)]
unsafe fn callback(global: *mut c_void, frame: *mut c_void) -> u64 {
    (global as usize).wrapping_add(frame as usize) as u64
}

#[test]
fn target_host_callback_passes_arguments_and_returns_a_value() {
    let function: HostFn = callback;
    let function = std::hint::black_box(function);
    assert_eq!(unsafe { function(17usize as *mut c_void, 25usize as *mut c_void) }, 42);
}
