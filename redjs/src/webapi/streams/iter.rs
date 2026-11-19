use super::binding::*;
use super::readable;

const ITERATOR: &str = "ReadableStreamAsyncIterator";

unsafe fn object(cx: Env, obj: u64) -> JsResult {
    if glue::rjs_kind(cx.g, obj) < 8 {
        Err(cx.type_error("The iterator must return an object"))
    } else {
        Ok(obj)
    }
}
unsafe fn stream_from(cx: Env, iterable: u64) -> JsResult {
    object(cx, iterable)?;
    let async_method = cx.symbol(iterable, 2)?;
    let sync = matches!(async_method, NULL | UNDEF);
    let f = if sync {
        cx.symbol(iterable, 0)?
    } else {
        async_method
    };
    if glue::rjs_kind(cx.g, f) != 9 {
        return Err(cx.type_error("ReadableStream.from requires an iterable"));
    }
    let iterator = object(cx, cx.invoke(f, iterable, &[])?)?;
    let t = cx.record();
    cx.put(t, "iterator", iterator);
    cx.put(t, "next", cx.prop(iterator, "next")?);
    cx.put(t, "sync", enc_bool(sync));
    let source = cx.record();
    cx.put(source, "pull", cx.callback(from_pull, t));
    cx.put(source, "cancel", cx.callback(from_cancel, t));
    readable::create(cx, UNDEF, source, UNDEF, 0.)
}
unsafe fn step_result(cx: Env, t: u64, result: u64, sync: bool) -> JsResult {
    object(cx, result)?;
    let done = to_bool(cx.g, cx.prop(result, "done")?);
    let s = cx.field(t, "state");
    if sync {
        let value = cx.prop(result, "value")?;
        let op = cx.record();
        cx.put(op, "state", s);
        cx.put(op, "done", enc_bool(done));
        return Ok(cx.then(value, op, Some(sync_value), None));
    }
    if done {
        readable::close(cx, s)
    } else {
        readable::enqueue(cx, s, cx.prop(result, "value")?)
    }
}
stream_fn!(async_step(cx, cf) throws {
    step_result(cx, cx.capture(cf), cf_arg(cf, 0), false)
});
stream_fn!(sync_value(cx, cf) throws {
    let op = cx.capture(cf);
    let s = cx.field(op, "state");
    if cx.field(op, "done") == TRUE {
        readable::close(cx, s)
    } else {
        readable::enqueue(cx, s, cf_arg(cf, 0))
    }
});
stream_fn!(from_pull(cx, cf) throws {
    let t = cx.capture(cf);
    cx.put(t, "state", readable::controller_stream(cx, cf_arg(cf, 0))?);
    let next = cx.field(t, "next");
    if glue::rjs_kind(cx.g, next) != 9 {
        return Err(cx.type_error("The iterator's next must be callable"));
    }
    let result = cx.invoke(next, cx.field(t, "iterator"), &[])?;
    if cx.field(t, "sync") == TRUE {
        step_result(cx, t, result, true)
    } else {
        Ok(cx.then(result, t, Some(async_step), None))
    }
});
stream_fn!(returned(cx, cf) throws {
    object(cx, cf_arg(cf, 0))?;
    Ok(UNDEF)
});
stream_fn!(discard(_cx, _cf) throws {
    Ok(UNDEF)
});
stream_fn!(from_cancel(cx, cf) throws {
    let t = cx.capture(cf);
    let iterator = cx.field(t, "iterator");
    let ret = cx.prop(iterator, "return")?;
    if matches!(ret, UNDEF | NULL) {
        return Ok(UNDEF);
    }
    if glue::rjs_kind(cx.g, ret) != 9 {
        return Err(cx.type_error("The iterator's return must be callable"));
    }
    let result = cx.invoke(ret, iterator, &[cf_arg(cf, 0)])?;
    if cx.field(t, "sync") == TRUE {
        object(cx, result)?;
        cx.prop(result, "done")?;
        Ok(cx.then(cx.prop(result, "value")?, UNDEF, Some(discard), None))
    } else {
        Ok(cx.then(result, UNDEF, Some(returned), None))
    }
});
stream_fn!(from(cx, cf) throws {
    stream_from(cx, cf_arg(cf, 0))
});

stream_fn!(values(cx, cf) throws {
    let stream = readable::state_of(cx, cf_this(cf))?;
    let options = cx.dictionary(cf_arg(cf, 0))?;
    let prevent = to_bool(cx.g, cx.prop(options, "preventCancel")?);
    let reader = readable::acquire(cx, UNDEF, stream)?;
    let proto = cx.ctor(ITERATOR);
    let obj = new_object(cx.g, proto);
    let t = cx.record();
    cx.put(t, "brand", proto);
    cx.put(t, "reader", readable::reader_data(cx, reader)?);
    cx.put(t, "prevent", enc_bool(prevent));
    cx.attach(obj, t);
    Ok(obj)
});

unsafe fn finish(cx: Env, t: u64) {
    if cx.field(t, "done") != TRUE {
        cx.put(t, "done", TRUE);
        readable::release(cx, cx.field(t, "reader"));
    }
}
stream_fn!(read_ok(cx, cf) throws {
    let t = cx.capture(cf);
    let result = cf_arg(cf, 0);
    let done = cx.field(result, "done") == TRUE;
    if done {
        finish(cx, t);
    }
    Ok(cx.iter_result(cx.field(result, "value"), done))
});
stream_fn!(read_err(cx, cf) throws {
    finish(cx, cx.capture(cf));
    Err(cf_arg(cf, 0))
});
stream_fn!(return_ok(cx, cf) throws {
    Ok(cx.iter_result(cx.field(cx.capture(cf), "value"), true))
});

unsafe fn advance(cx: Env, op: u64) -> JsResult {
    let t = cx.field(op, "iterator");
    let ret = cx.field(op, "return") == TRUE;
    let v = cx.field(op, "value");
    if cx.field(t, "done") == TRUE {
        return Ok(cx.iter_result(if ret { v } else { UNDEF }, true));
    }
    let r = cx.field(t, "reader");
    if ret {
        let value = if cx.field(t, "prevent") == TRUE {
            Ok(UNDEF)
        } else {
            readable::cancel(cx, cx.field(r, "stream"), v)
        };
        finish(cx, t);
        return Ok(cx.then(cx.promises(value), op, Some(return_ok), None));
    }
    Ok(cx.then(
        cx.promises(readable::read(cx, r, true)),
        t,
        Some(read_ok),
        Some(read_err),
    ))
}
stream_fn!(advance_job(cx, cf) throws {
    advance(cx, cx.capture(cf))
});
unsafe fn operation(cx: Env, obj: u64, ret: bool, value: u64) -> JsResult {
    let t = cx.data(obj, ITERATOR)?;
    let op = cx.record();
    cx.put(op, "iterator", t);
    cx.put(op, "return", enc_bool(ret));
    cx.put(op, "value", value);
    let old = cx.field(t, "ongoing");
    let p = if old == UNDEF {
        cx.resolved(advance(cx, op)?)
    } else {
        cx.then(old, op, Some(advance_job), Some(advance_job))
    };
    cx.put(t, "ongoing", p);
    Ok(p)
}
stream_fn!(next(cx, cf) promises {
    operation(cx, cf_this(cf), false, UNDEF)
});
stream_fn!(end(cx, cf) promises {
    operation(cx, cf_this(cf), true, cf_arg(cf, 0))
});

pub(super) unsafe fn install(cx: Env) {
    let ctor = cx.ctor("ReadableStream");
    let proto = cx.field(ctor, "prototype");
    define(cx.g, ctor, "from", make_fn(cx.g, "from", 1, from), 2);
    let values = make_fn(cx.g, "values", 0, values);
    define(cx.g, proto, "values", values, 2);
    let symbol = glue::rjs_atom_wellknown(cx.g, 2);
    glue::rjs_define(cx.g, proto, symbol, values, 2);
    glue::rjs_atom_free(cx.g, symbol);
    let iter = new_object(cx.g, glue::rjs_async_iterator_prototype(cx.g));
    define(cx.g, iter, "next", make_fn(cx.g, "next", 0, next), 0);
    define(cx.g, iter, "return", make_fn(cx.g, "return", 1, end), 0);
    to_string_tag(cx.g, iter, "ReadableStreamAsyncIterator");
    cx.put(super::classes(cx.g).interfaces, ITERATOR, iter);
}
