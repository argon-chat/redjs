//! Real JS/native recursion through a getter must never alias a Rust FnMut.
use redjs::api::{Context, HostArgs, Value};
use std::mem::ManuallyDrop;

#[test]
fn mutable_callbacks_reject_self_recursion_and_recover_after_exceptions() {
    let context = ManuallyDrop::new(Context::new());
    let mut calls = 0;
    let callback = context.function("mutableHost", 1, move |global, frame| {
        calls += 1;
        let args = unsafe { HostArgs::new(frame) };
        if args.arg(0).is_undefined() { return Value::int32(calls).0; }
        let mut result = Value::undefined().0;
        let status = unsafe { redjs::glue::rjs_call(global, args.arg(0).0,
            Value::undefined().0, std::ptr::null(), 0, &mut result) };
        if status != 0 { unsafe { redjs::ffi::redjs_throw(global, result) }; }
        result
    }).unwrap();
    context.global_set("mutableHost", callback).unwrap();
    let result = context.eval(r#"(() => {
        const options = { get value() { return mutableHost(); } };
        try { mutableHost(() => options.value); return false; }
        catch (error) { return error instanceof TypeError && /recursively/.test(error.message); }
    })()"#).unwrap();
    assert_eq!(result.as_bool(), Some(true));
    // The rejected inner invocation never entered FnMut, and the outer guard
    // was released even though JS threw through the callback.
    assert_eq!(context.eval("mutableHost()").unwrap().as_number(), Some(2.0));
    let other = context.function("otherHost", 0, |_, _| Value::int32(42).0).unwrap();
    context.global_set("otherHost", other).unwrap();
    assert_eq!(context.eval("mutableHost(() => otherHost())").unwrap().as_number(), Some(42.0));
    let caught = context.eval(r#"(() => {
        try { mutableHost(() => { throw new Error('expected'); }); } catch (error) { return error.message; }
    })()"#).unwrap();
    assert_eq!(context.read_string(caught).unwrap(), "expected");
    assert_eq!(context.eval("mutableHost()").unwrap().as_number(), Some(5.0));
}
