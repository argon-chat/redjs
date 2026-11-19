use super::binding::*;
use super::readable;

unsafe fn branch(cx: Env, t: u64, first: bool) -> JsResult {
    let info = cx.record();
    cx.put(info, "tee", t);
    cx.put(info, "first", enc_bool(first));
    let source = cx.record();
    cx.put(source, "pull", cx.callback(pull, t));
    cx.put(source, "cancel", cx.callback(cancel, info));
    readable::create(cx, UNDEF, source, UNDEF, 1.)
}

pub(super) unsafe fn tee_impl(cx: Env, obj: u64) -> JsResult {
    let s = readable::state_of(cx, obj)?;
    let reader = readable::acquire(cx, UNDEF, s)?;
    let r = readable::reader_data(cx, reader)?;
    let t = cx.record();
    cx.put(t, "stream", s);
    cx.put(t, "reader", r);
    cx.put(t, "canceled", cx.pending());
    let a = branch(cx, t, true)?;
    let b = branch(cx, t, false)?;
    cx.put(t, "a", readable::state_of(cx, a)?);
    cx.put(t, "b", readable::state_of(cx, b)?);
    cx.then(cx.field(r, "closed"), t, None, Some(failed));
    let arr = new_array(cx.g);
    glue::rjs_define_index(cx.g, arr, 0, a);
    glue::rjs_define_index(cx.g, arr, 1, b);
    Ok(arr)
}
#[crate::host_fn]
pub(super) unsafe fn tee(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cx = Env::new(g);
    cx.throws(tee_impl(cx, cf_this(cf)))
}
unsafe fn read(cx: Env, t: u64) {
    if cx.field(t, "reading") == TRUE {
        cx.put(t, "again", TRUE);
        return;
    }
    cx.put(t, "reading", TRUE);
    let pending = readable::read(cx, cx.field(t, "reader"), true);
    cx.then(cx.promises(pending), t, Some(after_read), Some(failed));
}
stream_fn!(pull(cx, cf) throws {
    read(cx, cx.capture(cf));
    Ok(UNDEF)
});
stream_fn!(after_read(cx, cf) throws {
    let t = cx.capture(cf);
    let res = cf_arg(cf, 0);
    let done = cx.field(res, "done") == TRUE;
    let value = cx.field(res, "value");
    cx.put(t, "again", FALSE);
    for name in ["a", "b"] {
        let s = cx.field(t, name);
        if readable::status(cx, s) == super::ReadableState::Readable {
            if done {
                readable::close(cx, s)?;
            } else {
                readable::enqueue(cx, s, value)?;
            }
        }
    }
    cx.put(t, "reading", FALSE);
    if done && (cx.field(t, "cancelA") != TRUE || cx.field(t, "cancelB") != TRUE) {
        cx.settle(cx.field(t, "canceled"), Ok(UNDEF));
    } else if !done && cx.field(t, "again") == TRUE {
        read(cx, t);
    }
    Ok(UNDEF)
});
stream_fn!(failed(cx, cf) throws {
    let t = cx.capture(cf);
    let e = cf_arg(cf, 0);
    for name in ["a", "b"] {
        readable::error(cx, cx.field(t, name), e);
    }
    if cx.field(t, "cancelA") != TRUE || cx.field(t, "cancelB") != TRUE {
        cx.settle(cx.field(t, "canceled"), Ok(UNDEF));
    }
    Ok(UNDEF)
});
stream_fn!(cancel(cx, cf) throws {
    let this = cx.capture(cf);
    let t = cx.field(this, "tee");
    let first = cx.field(this, "first") == TRUE;
    cx.put(t, if first { "cancelA" } else { "cancelB" }, TRUE);
    cx.put(t, if first { "reasonA" } else { "reasonB" }, cf_arg(cf, 0));
    let p = cx.field(t, "canceled");
    if cx.field(t, if first { "cancelB" } else { "cancelA" }) == TRUE {
        let reasons = new_array(cx.g);
        glue::rjs_define_index(cx.g, reasons, 0, cx.field(t, "reasonA"));
        glue::rjs_define_index(cx.g, reasons, 1, cx.field(t, "reasonB"));
        cx.settle(p, readable::cancel(cx, cx.field(t, "stream"), reasons));
    }
    Ok(p)
});
