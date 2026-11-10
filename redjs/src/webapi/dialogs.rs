//! `alert` / `confirm` / `prompt` (HTML §"Simple dialogs").
//!
//! These are the one item on the list with **no implementation to write**. A
//! modal dialog is a window-system thing; the embedding host owns the window, so
//! all this module does is route the three calls to a host hook and carry the
//! answer back. There is no Rust behaviour here beyond the plumbing, and that
//! is the correct amount.
//!
//! ## No handler means a throw, not a shrug
//!
//! With no handler registered, all three THROW a named error naming the
//! registration entry point. The alternative — return `undefined`/`false`/`null`
//! and carry on — is the exact failure the brief rules out: `confirm()` that
//! always answers "no" is a program that silently takes the other branch
//! forever, and nothing in the log says why. A throw stops at the call site.
//!
//! The hook is called from Rust with the ordinary C ABI (never by JSC, so no
//! SysV trampoline), matching `redjs_content_provider_set`.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::util::*;
use crate::glue;

pub const KIND_ALERT: i32 = 0;
pub const KIND_CONFIRM: i32 = 1;
pub const KIND_PROMPT: i32 = 2;

/// Receives the string a `prompt` handler wants to return. Called at most once.
pub type DialogSink = unsafe extern "C" fn(ctx: *mut c_void, ptr: *const u16, len: usize);

/// `kind` is one of [`KIND_ALERT`] / [`KIND_CONFIRM`] / [`KIND_PROMPT`].
///
/// Return value: ignored for `alert`; 0 = false / cancelled, non-zero = true /
/// accepted for `confirm` and `prompt`. A `prompt` that returns non-zero must
/// call `sink` exactly once with the entered text; returning non-zero without
/// calling it yields the empty string.
///
/// Called on the JS thread, inside the VM, with the caller blocked — a handler
/// that pumps a modal message loop must not re-enter JS.
pub type DialogFn = unsafe extern "C" fn(
    kind: i32,
    message: *const u16,
    message_len: usize,
    default_value: *const u16,
    default_len: usize,
    sink: DialogSink,
    sink_ctx: *mut c_void,
) -> i32;

fn handlers() -> &'static Mutex<HashMap<usize, usize>> {
    static H: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register (or clear, with `None`) the dialog handler for a context.
pub fn set_handler(g: *mut c_void, handler: Option<DialogFn>) {
    let mut map = handlers().lock().unwrap();
    match handler {
        Some(f) => map.insert(g as usize, f as usize),
        None => map.remove(&(g as usize)),
    };
}

pub(crate) fn forget(gone: crate::context::Gone) {
    handlers().lock().unwrap().remove(&gone.key);
}

fn handler_of(g: *mut c_void) -> Option<DialogFn> {
    let raw = *handlers().lock().unwrap().get(&(g as usize))?;
    Some(unsafe { core::mem::transmute::<usize, DialogFn>(raw) })
}

unsafe extern "C" fn collect(ctx: *mut c_void, ptr: *const u16, len: usize) {
    let out = &mut *(ctx as *mut Option<String>);
    *out = Some(if ptr.is_null() || len == 0 {
        String::new()
    } else {
        String::from_utf16_lossy(core::slice::from_raw_parts(ptr, len))
    });
}

fn refusal(name: &str) -> String {
    format!(
        "{name}() needs a host dialog handler and none is registered for this context. A modal \
         dialog belongs to the window system, which the embedding host owns; register one with \
         redjs_dialog_handler_set(). Refusing rather than answering for the user: a {name}() that \
         silently returns the default takes the same branch forever with nothing in the log."
    )
}

/// Run one dialog. `Ok(None)` = cancelled (prompt), `Ok(Some(s))` = the text,
/// and for `confirm` the text is `"1"`/`"0"`.
unsafe fn ask(g: *mut c_void, kind: i32, message: &str, default_value: &str) -> Option<(i32, Option<String>)> {
    let f = handler_of(g)?;
    let msg: Vec<u16> = message.encode_utf16().collect();
    let def: Vec<u16> = default_value.encode_utf16().collect();
    let mut out: Option<String> = None;
    let rc = f(
        kind,
        msg.as_ptr(),
        msg.len(),
        def.as_ptr(),
        def.len(),
        collect,
        &mut out as *mut Option<String> as *mut c_void,
    );
    Some((rc, out))
}

#[crate::host_fn]
unsafe fn js_alert(g: *mut c_void, cf: *mut c_void) -> u64 {
    let msg = if cf_argc(cf) == 0 { String::new() } else { read_string(g, cf_arg(cf, 0)).unwrap_or_default() };
    match ask(g, KIND_ALERT, &msg, "") {
        Some(_) => UNDEF,
        None => throw_type_error(g, &refusal("alert")),
    }
}

#[crate::host_fn]
unsafe fn js_confirm(g: *mut c_void, cf: *mut c_void) -> u64 {
    let msg = if cf_argc(cf) == 0 { String::new() } else { read_string(g, cf_arg(cf, 0)).unwrap_or_default() };
    match ask(g, KIND_CONFIRM, &msg, "") {
        Some((rc, _)) => enc_bool(rc != 0),
        None => throw_type_error(g, &refusal("confirm")),
    }
}

#[crate::host_fn]
unsafe fn js_prompt(g: *mut c_void, cf: *mut c_void) -> u64 {
    let msg = if cf_argc(cf) == 0 { String::new() } else { read_string(g, cf_arg(cf, 0)).unwrap_or_default() };
    // Spec: the second argument defaults to the empty string, and that default
    // is what a cancelled dialog does NOT return — cancel is null.
    let default_value = match cf_arg(cf, 1) {
        v if v == UNDEF => String::new(),
        v => read_string(g, v).unwrap_or_default(),
    };
    match ask(g, KIND_PROMPT, &msg, &default_value) {
        Some((rc, text)) => {
            if rc == 0 {
                NULL
            } else {
                js_string(g, &text.unwrap_or_default())
            }
        }
        None => throw_type_error(g, &refusal("prompt")),
    }
}

pub unsafe fn install(g: *mut c_void) {
    let global = glue::rjs_global(g);
    define(g, global, "alert", make_fn(g, "alert", 1, js_alert), 2);
    define(g, global, "confirm", make_fn(g, "confirm", 1, js_confirm), 2);
    define(g, global, "prompt", make_fn(g, "prompt", 2, js_prompt), 2);
}

#[cfg(test)]
mod jsc_tests {
    use super::*;
    use crate::webapi::testing::*;
    use std::sync::atomic::{AtomicI32, Ordering};

    /// What the last handler call saw — so the tests can assert the arguments
    /// crossed, not merely that something was returned.
    static LAST_KIND: AtomicI32 = AtomicI32::new(-1);
    static ANSWER: AtomicI32 = AtomicI32::new(0);

    /// The handler statics above are process-global while `cargo test` runs the
    /// tests in parallel threads, so the ones that drive the handler take this
    /// first. (Found by the tests themselves: without it, `prompt` read the
    /// `ANSWER` its neighbour had just set and returned null.)
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        static L: OnceLock<Mutex<()>> = OnceLock::new();
        L.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
    }

    unsafe extern "C" fn test_handler(
        kind: i32,
        message: *const u16,
        message_len: usize,
        default_value: *const u16,
        default_len: usize,
        sink: DialogSink,
        sink_ctx: *mut c_void,
    ) -> i32 {
        LAST_KIND.store(kind, Ordering::SeqCst);
        let msg = String::from_utf16_lossy(core::slice::from_raw_parts(message, message_len));
        let def = String::from_utf16_lossy(core::slice::from_raw_parts(default_value, default_len));
        if kind == KIND_PROMPT {
            // Echo the message and the default back so the test can see both
            // arrived intact.
            let reply: Vec<u16> = format!("{msg}|{def}").encode_utf16().collect();
            sink(sink_ctx, reply.as_ptr(), reply.len());
        }
        ANSWER.load(Ordering::SeqCst)
    }

    #[test]
    fn without_a_handler_all_three_refuse_by_name() {
        let c = ctx();
        for call in ["alert('x')", "confirm('x')", "prompt('x')"] {
            let msg = throws(&c, call);
            assert!(msg.contains("redjs_dialog_handler_set"), "{call}: {msg}");
        }
    }

    #[test]
    fn a_registered_handler_receives_the_call_and_its_answer_comes_back() {
        let _guard = exclusive();
        let c = ctx();
        ANSWER.store(1, Ordering::SeqCst);
        set_handler(c.raw(), Some(test_handler));

        ok(&c, "confirm('really?') === true");
        assert_eq!(LAST_KIND.load(Ordering::SeqCst), KIND_CONFIRM);

        ANSWER.store(0, Ordering::SeqCst);
        ok(&c, "confirm('really?') === false");

        // alert returns undefined and must not throw once a handler exists.
        ok(&c, "alert('hi') === undefined");
        assert_eq!(LAST_KIND.load(Ordering::SeqCst), KIND_ALERT);

        set_handler(c.raw(), None);
    }

    #[test]
    fn prompt_carries_the_message_and_default_and_distinguishes_cancel_from_empty() {
        let _guard = exclusive();
        let c = ctx();
        set_handler(c.raw(), Some(test_handler));

        ANSWER.store(1, Ordering::SeqCst);
        let out = s(&c, "prompt('name?', 'anon')");
        assert_eq!(out, "name?|anon", "the message or the default did not reach the handler");

        // Cancel is null, NOT the default string — the distinction the whole
        // return type exists for.
        ANSWER.store(0, Ordering::SeqCst);
        ok(&c, "prompt('name?', 'anon') === null");

        set_handler(c.raw(), None);
        // …and clearing the handler restores the refusal, so the test above
        // cannot be passing because the throw was never there.
        let msg = throws(&c, "prompt('x')");
        assert!(msg.contains("redjs_dialog_handler_set"), "{msg}");
    }
}
