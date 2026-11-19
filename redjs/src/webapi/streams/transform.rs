use super::binding::*;
use super::{readable, writable, ReadableState, WritableState};

const STREAM: &str = "TransformStream";
const CONTROLLER: &str = "TransformStreamDefaultController";

unsafe fn pressure(cx: Env, t: u64, enabled: bool) {
    if cx.field(t, "pressure") == enc_bool(enabled) {
        return;
    }
    let old = cx.field(t, "change");
    if old != UNDEF {
        cx.settle(old, Ok(UNDEF));
    }
    cx.put(t, "change", cx.pending());
    cx.put(t, "pressure", enc_bool(enabled));
}

unsafe fn clear(cx: Env, t: u64) {
    for key in ["transformer", "transform", "flush", "cancel"] {
        cx.put(t, key, UNDEF);
    }
}

unsafe fn error_writable(cx: Env, t: u64, reason: u64) {
    clear(cx, t);
    writable::error(cx, cx.field(t, "w"), reason);
    pressure(cx, t, false);
}
unsafe fn error(cx: Env, t: u64, reason: u64) {
    readable::error(cx, cx.field(t, "r"), reason);
    error_writable(cx, t, reason);
}

unsafe fn enqueue(cx: Env, t: u64, chunk: u64) -> JsResult {
    let r = cx.field(t, "r");
    let was_readable = readable::status(cx, r) == ReadableState::Readable;
    if let Err(e) = readable::enqueue(cx, r, chunk) {
        if was_readable && readable::status(cx, r) == ReadableState::Errored {
            let reason = cx.field(r, "error");
            error_writable(cx, t, reason);
            return Err(reason);
        }
        return Err(e);
    }
    if !readable::wants_more(cx, r) {
        pressure(cx, t, true);
    }
    Ok(UNDEF)
}

stream_fn!(on_start(cx, cf) throws {
    Ok(cx.field(cx.capture(cf), "started"))
});
stream_fn!(on_pull(cx, cf) throws {
    let t = cx.capture(cf);
    pressure(cx, t, false);
    Ok(cx.field(t, "change"))
});

unsafe fn transform(cx: Env, t: u64, chunk: u64) -> JsResult {
    let alg = cx.field(t, "transform");
    let result = if alg == UNDEF {
        enqueue(cx, t, chunk)
    } else {
        cx.invoke(
            alg,
            cx.field(t, "transformer"),
            &[chunk, cx.field(t, "controller")],
        )
    };
    Ok(cx.then(cx.promises(result), t, None, Some(transform_failed)))
}
stream_fn!(transform_failed(cx, cf) throws {
    let e = cf_arg(cf, 0);
    error(cx, cx.capture(cf), e);
    Err(e)
});
stream_fn!(after_pressure(cx, cf) throws {
    let job = cx.capture(cf);
    let t = cx.field(job, "state");
    let w = cx.field(t, "w");
    if writable::status(cx, w) != WritableState::Writable {
        return Err(cx.field(w, "error"));
    }
    transform(cx, t, cx.field(job, "chunk"))
});
stream_fn!(on_write(cx, cf) throws {
    let t = cx.capture(cf);
    if cx.field(t, "pressure") == TRUE {
        let job = cx.record();
        cx.put(job, "state", t);
        cx.put(job, "chunk", cf_arg(cf, 0));
        return Ok(cx.then(cx.field(t, "change"), job, Some(after_pressure), None));
    }
    transform(cx, t, cf_arg(cf, 0))
});

#[derive(Clone, Copy)]
enum Finish {
    Flush,
    Abort,
    Cancel,
}
unsafe fn finish(cx: Env, t: u64, action: Finish, reason: u64) -> JsResult {
    let finish = cx.field(t, "finished");
    if finish != UNDEF {
        return Ok(finish);
    }
    let p = cx.pending();
    cx.put(t, "finished", p);
    cx.put(t, "reason", reason);
    let (key, arg, ok, err): (_, _, HostFn, HostFn) = match action {
        Finish::Flush => ("flush", cx.field(t, "controller"), flushed, flush_failed),
        Finish::Abort => ("cancel", reason, aborted, abort_failed),
        Finish::Cancel => ("cancel", reason, canceled, cancel_failed),
    };
    let callback = cx.field(t, key);
    let result = if callback == UNDEF {
        UNDEF
    } else {
        // Web IDL adopts the callback result into a fresh promise, even for a native Promise.
        match cx.invoke(callback, cx.field(t, "transformer"), &[arg]) {
            Ok(value) => cx.resolved(value),
            Err(reason) => cx.rejected(reason),
        }
    };
    clear(cx, t);
    cx.then(result, t, Some(ok), Some(err));
    Ok(p)
}
unsafe fn finish_readable(cx: Env, t: u64, flush: bool, result: JsResult) {
    let r = cx.field(t, "r");
    let result = match result {
        Err(e) => {
            readable::error(cx, r, e);
            Err(e)
        }
        Ok(_) if readable::status(cx, r) == ReadableState::Errored => Err(cx.field(r, "error")),
        Ok(_) => {
            if flush {
                let _ = readable::close(cx, r);
            } else {
                readable::error(cx, r, cx.field(t, "reason"));
            }
            Ok(UNDEF)
        }
    };
    cx.settle(cx.field(t, "finished"), result);
}
stream_fn!(flushed(cx, cf) throws {
    finish_readable(cx, cx.capture(cf), true, Ok(UNDEF));
    Ok(UNDEF)
});
stream_fn!(flush_failed(cx, cf) throws {
    finish_readable(cx, cx.capture(cf), true, Err(cf_arg(cf, 0)));
    Ok(UNDEF)
});
stream_fn!(aborted(cx, cf) throws {
    finish_readable(cx, cx.capture(cf), false, Ok(UNDEF));
    Ok(UNDEF)
});
stream_fn!(abort_failed(cx, cf) throws {
    finish_readable(cx, cx.capture(cf), false, Err(cf_arg(cf, 0)));
    Ok(UNDEF)
});
stream_fn!(canceled(cx, cf) throws {
    let t = cx.capture(cf);
    let w = cx.field(t, "w");
    let result = if writable::status(cx, w) == WritableState::Errored {
        Err(cx.field(w, "error"))
    } else {
        error_writable(cx, t, cx.field(t, "reason"));
        Ok(UNDEF)
    };
    cx.settle(cx.field(t, "finished"), result);
    Ok(UNDEF)
});
stream_fn!(cancel_failed(cx, cf) throws {
    let t = cx.capture(cf);
    let e = cf_arg(cf, 0);
    error_writable(cx, t, e);
    cx.settle(cx.field(t, "finished"), Err(e));
    Ok(UNDEF)
});
stream_fn!(on_close(cx, cf) throws {
    finish(cx, cx.capture(cf), Finish::Flush, UNDEF)
});
stream_fn!(on_abort(cx, cf) throws {
    finish(cx, cx.capture(cf), Finish::Abort, cf_arg(cf, 0))
});
stream_fn!(on_cancel(cx, cf) throws {
    finish(cx, cx.capture(cf), Finish::Cancel, cf_arg(cf, 0))
});

unsafe fn create(cx: Env, target: u64, transformer: u64, ws: u64, rs: u64) -> JsResult {
    let source = cx.dictionary(transformer)?;
    let ws = cx.dictionary(ws)?;
    let rs = cx.dictionary(rs)?;
    let t = cx.record();
    let object = cx.instance(STREAM, target, t)?;
    cx.put(t, "object", object);
    let c = cx.record();
    cx.put(c, "state", t);
    let controller = cx.instance(CONTROLLER, UNDEF, c)?;
    let start = cx.method(source, "start")?;
    for key in ["cancel", "flush", "transform"] {
        cx.put(t, key, cx.method(source, key)?);
    }
    if cx.prop(source, "readableType")? != UNDEF || cx.prop(source, "writableType")? != UNDEF {
        return Err(cx.error(
            2,
            "Transform stream readableType and writableType must be undefined",
        ));
    }
    let started = cx.pending();
    cx.put(t, "transformer", source);
    cx.put(t, "controller", controller);
    cx.put(t, "started", started);
    pressure(cx, t, true);

    let sink = cx.record();
    for (key, f) in [
        ("start", on_start as HostFn),
        ("write", on_write),
        ("close", on_close),
        ("abort", on_abort),
    ] {
        cx.put(sink, key, cx.callback(f, t));
    }
    let writable = writable::create(cx, UNDEF, sink, ws)?;
    cx.put(t, "writable", writable);
    cx.put(t, "w", writable::state_of(cx, writable)?);
    let source_record = cx.record();
    for (key, f) in [
        ("start", on_start as HostFn),
        ("pull", on_pull),
        ("cancel", on_cancel),
    ] {
        cx.put(source_record, key, cx.callback(f, t));
    }
    let readable = readable::create(cx, UNDEF, source_record, rs, 0.)?;
    cx.put(t, "readable", readable);
    cx.put(t, "r", readable::state_of(cx, readable)?);
    let result = cx.invoke(start, source, &[controller])?;
    cx.settle(started, Ok(result));
    Ok(object)
}
unsafe fn ctl(cx: Env, obj: u64) -> JsResult {
    Ok(cx.field(cx.data(obj, CONTROLLER)?, "state"))
}
stream_fn!(constructor(cx, cf) throws {
    create(
        cx,
        cf_new_target(cf),
        cf_arg(cf, 0),
        cf_arg(cf, 1),
        cf_arg(cf, 2),
    )
});
stream_fn!(get_readable(cx, cf) throws {
    Ok(cx.field(cx.data(cf_this(cf), STREAM)?, "readable"))
});
stream_fn!(get_writable(cx, cf) throws {
    Ok(cx.field(cx.data(cf_this(cf), STREAM)?, "writable"))
});
stream_fn!(ctl_desired(cx, cf) throws {
    Ok(readable::desired(cx, cx.field(ctl(cx, cf_this(cf))?, "r")))
});
stream_fn!(ctl_enqueue(cx, cf) throws {
    enqueue(cx, ctl(cx, cf_this(cf))?, cf_arg(cf, 0))
});
stream_fn!(ctl_error(cx, cf) throws {
    error(cx, ctl(cx, cf_this(cf))?, cf_arg(cf, 0));
    Ok(UNDEF)
});
stream_fn!(ctl_terminate(cx, cf) throws {
    let t = ctl(cx, cf_this(cf))?;
    let _ = readable::close(cx, cx.field(t, "r"));
    error_writable(cx, t, cx.type_error("The transform stream was terminated"));
    Ok(UNDEF)
});

unsafe fn codec(cx: Env, cf: *mut c_void, decompress: bool) -> JsResult {
    let args = [cx.string(cf_arg(cf, 0))?, enc_bool(decompress)];
    let zip = cx.construct(super::super::compression::constructor(cx.g), &args)?;
    let t = cx.record();
    cx.put(t, "transform", cx.callback(zip_chunk, zip));
    cx.put(t, "flush", cx.callback(zip_flush, zip));
    // Let a codec consume and validate its first input before a reader attaches.
    let strategy = cx.record();
    cx.put(strategy, "highWaterMark", enc_i32(1));
    let transform = create(cx, UNDEF, t, UNDEF, strategy)?;
    let data = cx.record();
    cx.put(data, "transform", cx.data(transform, STREAM)?);
    cx.instance(
        if decompress {
            "DecompressionStream"
        } else {
            "CompressionStream"
        },
        cf_new_target(cf),
        data,
    )
}
unsafe fn zip_output(cx: Env, zip: u64, method: &str, args: &[u64], ctl_obj: u64) -> JsResult {
    let out = cx.invoke(cx.prop(zip, method)?, zip, args)?;
    if cx.number(cx.prop(out, "byteLength")?)? != 0. {
        enqueue(cx, ctl(cx, ctl_obj)?, out)?;
    }
    Ok(UNDEF)
}
stream_fn!(zip_chunk(cx, cf) throws {
    zip_output(cx, cx.capture(cf), "push", &[cf_arg(cf, 0)], cf_arg(cf, 1))
});
stream_fn!(zip_flush(cx, cf) throws {
    zip_output(cx, cx.capture(cf), "finish", &[], cf_arg(cf, 0))
});
stream_fn!(new_compression(cx, cf) throws {
    codec(cx, cf, false)
});
stream_fn!(new_decompression(cx, cf) throws {
    codec(cx, cf, true)
});
unsafe fn side(cx: Env, obj: u64, kind: &str, side: &str) -> JsResult {
    Ok(cx.field(cx.field(cx.data(obj, kind)?, "transform"), side))
}
stream_fn!(compressed_read(cx, cf) throws {
    side(cx, cf_this(cf), "CompressionStream", "readable")
});
stream_fn!(compressed_write(cx, cf) throws {
    side(cx, cf_this(cf), "CompressionStream", "writable")
});
stream_fn!(decompressed_read(cx, cf) throws {
    side(cx, cf_this(cf), "DecompressionStream", "readable")
});
stream_fn!(decompressed_write(cx, cf) throws {
    side(cx, cf_this(cf), "DecompressionStream", "writable")
});

pub(super) unsafe fn install(cx: Env) {
    cx.register(ClassSpec {
        name: CONTROLLER,
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[
            ("enqueue", 0, ctl_enqueue),
            ("error", 0, ctl_error),
            ("terminate", 0, ctl_terminate),
        ],
        getters: &[("desiredSize", ctl_desired)],
        accessors: &[],
    });
    for (name, arity, new, read, write) in [
        (
            STREAM,
            0,
            constructor as HostFn,
            get_readable as HostFn,
            get_writable as HostFn,
        ),
        (
            "CompressionStream",
            1,
            new_compression,
            compressed_read,
            compressed_write,
        ),
        (
            "DecompressionStream",
            1,
            new_decompression,
            decompressed_read,
            decompressed_write,
        ),
    ] {
        cx.register(ClassSpec {
            name,
            arity,
            construct: Some(new),
            parent_proto: 0,
            methods: &[],
            getters: &[("readable", read), ("writable", write)],
            accessors: &[],
        });
    }
}
