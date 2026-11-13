//! `ErrorEvent` and `reportError` (HTML "report an exception"). Uncanceled, the exception goes to the
//! host's error handler, like an uncaught one.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::events;
use super::util::*;
use crate::glue;

struct Classes {
    error_event: Class,
}

unsafe impl Send for Classes {}
unsafe impl Sync for Classes {}

fn registry() -> &'static Mutex<HashMap<usize, &'static Classes>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static Classes>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> &'static Classes {
    registry().lock().unwrap().get(&(g as usize)).copied().expect("webapi::report_error not installed")
}

pub(crate) fn forget(gone: crate::context::Gone) {
    registry().lock().unwrap().remove(&gone.key);
}

unsafe fn slots(g: *mut c_void, obj: u64) -> Option<u64> {
    let s = get_a(g, obj, state(g).s_event);
    is_cell(s).then_some(s)
}

unsafe fn field(g: *mut c_void, cf: *mut c_void, key: &str) -> u64 {
    match slots(g, cf_this(cf)) {
        Some(s) if get(g, s, "message") != UNDEF => get(g, s, key),
        _ => throw_type_error(g, &format!("ErrorEvent.{key} called on a non-ErrorEvent")),
    }
}

#[crate::host_fn]
unsafe fn ee_message(g: *mut c_void, cf: *mut c_void) -> u64 {
    field(g, cf, "message")
}
#[crate::host_fn]
unsafe fn ee_filename(g: *mut c_void, cf: *mut c_void) -> u64 {
    field(g, cf, "filename")
}
#[crate::host_fn]
unsafe fn ee_lineno(g: *mut c_void, cf: *mut c_void) -> u64 {
    field(g, cf, "lineno")
}
#[crate::host_fn]
unsafe fn ee_colno(g: *mut c_void, cf: *mut c_void) -> u64 {
    field(g, cf, "colno")
}
#[crate::host_fn]
unsafe fn ee_error(g: *mut c_void, cf: *mut c_void) -> u64 {
    field(g, cf, "error")
}

unsafe fn fill(g: *mut c_void, obj: u64, message: &str, filename: &str, lineno: u32, colno: u32, error: u64) {
    if let Some(s) = slots(g, obj) {
        define(g, s, "message", js_string(g, message), 0);
        define(g, s, "filename", js_string(g, filename), 0);
        define(g, s, "lineno", enc_f64(lineno as f64), 0);
        define(g, s, "colno", enc_f64(colno as f64), 0);
        define(g, s, "error", error, 0);
    }
}

unsafe fn init_string(g: *mut c_void, init: u64, key: &str) -> Result<String, ()> {
    if !is_cell(init) {
        return Ok(String::new());
    }
    let v = try_get(g, init, key)?;
    if v == UNDEF {
        return Ok(String::new());
    }
    Ok(read_string(g, to_js_string(g, v)?).unwrap_or_default())
}

unsafe fn init_u32(g: *mut c_void, init: u64, key: &str) -> Result<u32, ()> {
    if !is_cell(init) {
        return Ok(0);
    }
    let v = try_get(g, init, key)?;
    if v == UNDEF {
        return Ok(0);
    }
    let n = to_number(g, v)?;
    Ok(if n.is_finite() { n.trunc().rem_euclid(4294967296.0) as u32 } else { 0 })
}

#[crate::host_fn]
unsafe fn ee_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "Failed to construct 'ErrorEvent': 1 argument required, but only 0 present.");
    }
    let init = cf_arg(cf, 1);
    let obj = ordinary_create(g, cf_new_target(cf), classes(g).error_event.proto);
    events::init_event(g, obj, cf_arg(cf, 0), init, false);
    let Ok(message) = init_string(g, init, "message") else { return UNDEF };
    let Ok(filename) = init_string(g, init, "filename") else { return UNDEF };
    let Ok(lineno) = init_u32(g, init, "lineno") else { return UNDEF };
    let Ok(colno) = init_u32(g, init, "colno") else { return UNDEF };
    let error = if is_cell(init) {
        match try_get(g, init, "error") {
            Ok(v) if v != UNDEF => v,
            Ok(_) => NULL,
            Err(()) => return UNDEF,
        }
    } else {
        NULL
    };
    fill(g, obj, &message, &filename, lineno, colno, error);
    obj
}

unsafe fn number_prop(g: *mut c_void, obj: u64, key: &str) -> u32 {
    dec_f64(get(g, obj, key)).filter(|n| n.is_finite() && *n >= 0.0).map_or(0, |n| n as u32)
}

/// Fires a cancelable `error` at the global (`onerror` called with HTML's five arguments, `true`
/// cancels) and reports the exception unless something canceled it.
pub(crate) unsafe fn report_exception(g: *mut c_void, exc: u64) {
    let (header, _) = crate::errors::describe(g, exc);
    let message = format!("Uncaught {header}");
    let (filename, lineno, colno) = if is_cell(exc) {
        (read_string(g, get(g, exc, "sourceURL")).unwrap_or_default(), number_prop(g, exc, "line"), number_prop(g, exc, "column"))
    } else {
        (String::new(), 0, 0)
    };
    let ev = new_object(g, classes(g).error_event.proto);
    events::init_event(g, ev, js_string(g, "error"), UNDEF, false);
    if let Some(s) = slots(g, ev) {
        define(g, s, "isTrusted", TRUE, 0);
        define(g, s, "cancelable", TRUE, 0);
    }
    fill(g, ev, &message, &filename, lineno, colno, exc);

    let global = glue::rjs_global(g);
    let handler = get(g, global, "onerror");
    let attribute = events::is_callable(g, handler);
    if attribute {
        define(g, global, "onerror", NULL, 0);
    }
    let mut canceled = !events::dispatch(g, global, ev);
    if attribute {
        define(g, global, "onerror", handler, 0);
        let args = [js_string(g, &message), js_string(g, &filename), enc_f64(lineno as f64), enc_f64(colno as f64), exc];
        match call(g, handler, global, &args) {
            Ok(r) => canceled |= r == TRUE,
            Err(e) => crate::errors::report(g, e, crate::errors::ErrKind::Listener),
        }
    }
    if !canceled {
        crate::errors::report(g, exc, crate::errors::ErrKind::Reported);
    }
}

#[crate::host_fn]
unsafe fn report_error(g: *mut c_void, cf: *mut c_void) -> u64 {
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "Failed to execute 'reportError': 1 argument required, but only 0 present.");
    }
    report_exception(g, cf_arg(cf, 0));
    UNDEF
}

pub unsafe fn install(g: *mut c_void, event_proto: u64) {
    let error_event = define_class(g, &ClassSpec {
        name: "ErrorEvent",
        arity: 1,
        construct: Some(ee_construct),
        parent_proto: event_proto,
        methods: &[],
        getters: &[
            ("message", ee_message),
            ("filename", ee_filename),
            ("lineno", ee_lineno),
            ("colno", ee_colno),
            ("error", ee_error),
        ],
        accessors: &[],
    });
    define(g, glue::rjs_global(g), "reportError", make_fn(g, "reportError", 1, report_error), 2);
    registry().lock().unwrap().insert(g as usize, Box::leak(Box::new(Classes { error_event })));
}

#[cfg(test)]
mod tests {
    use crate::webapi::testing::{ctx, ok, s};

    #[test]
    fn error_event_carries_its_init() {
        let c = ctx();
        ok(&c, "new ErrorEvent('error') instanceof Event && new ErrorEvent('x').error === null");
        assert_eq!(
            s(&c, "(() => { const e = new ErrorEvent('error', { message: 'm', filename: 'f.js', lineno: 3, colno: 4.9, error: 7 }); \
                   return [e.type, e.message, e.filename, e.lineno, e.colno, e.error].join('|'); })()"),
            "error|m|f.js|3|4|7"
        );
        ok(&c, "(() => { try { ErrorEvent.prototype.message; return false; } catch (e) { return e instanceof TypeError; } })()");
    }

    #[test]
    fn report_error_fires_at_the_global_and_onerror_gets_five_arguments() {
        let c = ctx();
        assert_eq!(
            s(&c, "(() => { const seen = []; \
                   const l = (e) => { seen.push([e.type, e.isTrusted, e.cancelable, e.message, e.error && e.error.message].join(',')); e.preventDefault(); }; \
                   addEventListener('error', l); \
                   globalThis.onerror = (m, f, l2, c2, err) => { seen.push(['onerror', typeof m, typeof l2, err.message].join(',')); }; \
                   reportError(new TypeError('boom')); \
                   removeEventListener('error', l); globalThis.onerror = null; \
                   return seen.join(' / ') + ' / ' + (typeof onerror); })()"),
            "error,true,true,Uncaught TypeError: boom,boom / onerror,string,number,boom / object"
        );
        ok(&c, "(() => { try { reportError(); return false; } catch (e) { return e instanceof TypeError; } })()");
    }
}
