//! C ABI ownership and cache-acceptance contract used by the C# benchmarks.
use redjs::{api::Context, ffi};
use std::ptr::{null, null_mut};

#[test]
fn compiled_bytecode_ffi_owns_payload_and_reports_acceptance() {
    let context = Context::new();
    let source = b"(function(n) { return n + 1; })";
    let url = b"bench://ffi.js";
    let (mut handle, mut bytes, mut length, mut result) = (null_mut(), null(), 0, 0);
    unsafe {
        assert_eq!(ffi::redjs_bytecode_generate(
            context.raw(), source.as_ptr(), source.len(), url.as_ptr(), url.len(),
            u32::MAX, &mut handle, &mut bytes, &mut length, &mut result,
        ), ffi::REDJS_OK);
        assert!(!handle.is_null());
        assert!(!bytes.is_null());
        assert!(length > 0);
        let mut used = 0;
        assert_eq!(ffi::redjs_eval8_cached(
            context.raw(), source.as_ptr(), source.len(), url.as_ptr(), url.len(),
            bytes, length, &mut used, &mut result,
        ), ffi::REDJS_OK);
        assert_eq!(used, 1);
        let function = redjs::api::Value(result);
        context.protect(function);
        ffi::redjs_bytecode_free(handle);
        context.gc();
        let value = context.call(function, redjs::api::Value::undefined(), &[redjs::api::Value::int32(41)]).unwrap();
        assert_eq!(value.as_number(), Some(42.0));
        context.unprotect(function);
        ffi::redjs_bytecode_free(null_mut());
        assert_eq!(ffi::redjs_aot_run(null_mut(), 1, &mut result), ffi::REDJS_BAD_ARG);
        assert_eq!(ffi::redjs_bytecode_generate(
            null_mut(), source.as_ptr(), source.len(), url.as_ptr(), url.len(),
            0, &mut handle, &mut bytes, &mut length, &mut result,
        ), ffi::REDJS_BAD_ARG);
    }
}

#[cfg(not(feature = "native-aot"))]
#[test]
fn native_images_are_explicitly_unavailable_in_a_jit_only_build() {
    unsafe {
        assert_eq!(ffi::redjs_aot_build_info(null_mut(), 0), 0);
        assert_eq!(ffi::redjs_aot_is_active(), 0);
        assert_eq!(ffi::redjs_aot_load_handle(0, null(), 0), 0);
    }
}
