use super::binding::*;
use super::{classes, ReadableCore, ReadableState};

const STREAM: &str = "ReadableStream";
const READER: &str = "ReadableStreamDefaultReader";
const CONTROLLER: &str = "ReadableStreamDefaultController";

unsafe fn core<'a>(cx: Env, s: u64) -> &'a mut ReadableCore {
    host_data(cx.g, classes(cx.g).readable_cls, cx.field(s, "core")).unwrap()
}

unsafe fn clear_algorithms(cx: Env, s: u64) {
    for field in ["source", "pull", "cancel", "size"] {
        cx.put(s, field, UNDEF);
    }
}

unsafe fn read_result(cx: Env, chunk: u64, done: bool, internal: bool) -> u64 {
    if internal {
        // Internal pipe and tee operations must never consult Object.prototype.then.
        let result = cx.record();
        cx.put(result, "value", chunk);
        cx.put(result, "done", enc_bool(done));
        result
    } else {
        cx.iter_result(chunk, done)
    }
}

unsafe fn drain(cx: Env, s: u64, error: Option<u64>) {
    core(cx, s).state = if error.is_some() {
        ReadableState::Errored
    } else {
        ReadableState::Closed
    };
    if let Some(reason) = error {
        cx.put(s, "error", reason);
    }
    let reader = cx.field(s, "reader");
    if reader != UNDEF {
        cx.settle(cx.field(reader, "closed"), error.map_or(Ok(UNDEF), Err));
    }
    let queue = cx.field(s, "requests");
    while let Some(request) = cx.pop(queue) {
        cx.settle(
            cx.field(request, "promise"),
            error.map_or_else(
                || {
                    Ok(read_result(
                        cx,
                        UNDEF,
                        true,
                        cx.field(request, "internal") == TRUE,
                    ))
                },
                Err,
            ),
        );
    }
}

pub(super) unsafe fn error(cx: Env, s: u64, reason: u64) {
    if core(cx, s).state != ReadableState::Readable {
        return;
    }
    core(cx, s).queue.reset();
    cx.put(s, "chunks", cx.record());
    clear_algorithms(cx, s);
    drain(cx, s, Some(reason));
}

unsafe fn has_requests(cx: Env, s: u64) -> usize {
    usize::from(cx.field(cx.field(s, "requests"), "head") != UNDEF)
}

unsafe fn pull(cx: Env, s: u64) {
    let demand = has_requests(cx, s);
    if core(cx, s).begin_pull(demand) {
        run_pull(cx, s);
    }
}

unsafe fn run_pull(cx: Env, s: u64) {
    let result = cx.invoke(
        cx.field(s, "pull"),
        cx.field(s, "source"),
        &[cx.field(s, "controller")],
    );
    let result = cx.promises(result);
    cx.then(result, s, Some(after_pull), Some(on_error));
}

stream_fn!(after_pull(cx, cf) throws {
    let s = cx.capture(cf);
    let demand = has_requests(cx, s);
    if core(cx, s).end_pull(demand) {
        run_pull(cx, s);
    }
    Ok(UNDEF)
});

stream_fn!(on_error(cx, cf) throws {
    error(cx, cx.capture(cf), cf_arg(cf, 0));
    Ok(UNDEF)
});
stream_fn!(on_start(cx, cf) throws {
    let s = cx.capture(cf);
    core(cx, s).started = true;
    pull(cx, s);
    Ok(UNDEF)
});

pub(super) unsafe fn enqueue(cx: Env, s: u64, chunk: u64) -> JsResult {
    if !core(cx, s).can_close_or_enqueue() {
        return Err(cx.type_error("Cannot enqueue into a closing, closed or errored stream"));
    }
    if let Some(request) = cx.pop(cx.field(s, "requests")) {
        let result = read_result(cx, chunk, false, cx.field(request, "internal") == TRUE);
        cx.settle(cx.field(request, "promise"), Ok(result));
    } else {
        let size = cx.field(s, "size");
        let n = if size == UNDEF {
            Ok(1.)
        } else {
            cx.invoke(size, UNDEF, &[chunk]).and_then(|v| cx.number(v))
        };
        let n = match n {
            Ok(n) => n,
            Err(e) => {
                error(cx, s, e);
                return Err(e);
            }
        };
        if core(cx, s).enqueue(n).is_err() {
            let e = cx.error(2, "The chunk size must be a finite, non-negative number");
            error(cx, s, e);
            return Err(e);
        }
        cx.push(cx.field(s, "chunks"), chunk);
    }
    pull(cx, s);
    Ok(UNDEF)
}

pub(super) unsafe fn close(cx: Env, s: u64) -> JsResult {
    if !core(cx, s).can_close_or_enqueue() {
        return Err(cx.type_error("Cannot close a closing, closed or errored stream"));
    }
    if core(cx, s).request_close() {
        clear_algorithms(cx, s);
        drain(cx, s, None);
    }
    Ok(UNDEF)
}

pub(super) unsafe fn desired(cx: Env, s: u64) -> u64 {
    core(cx, s).desired_size().map_or(NULL, enc_f64)
}
pub(super) unsafe fn wants_more(cx: Env, s: u64) -> bool {
    let n = has_requests(cx, s);
    core(cx, s).should_call_pull(n)
}
pub(super) unsafe fn status(cx: Env, s: u64) -> ReadableState {
    core(cx, s).state
}

pub(super) unsafe fn controller_stream(cx: Env, controller: u64) -> JsResult {
    Ok(cx.field(cx.data(controller, CONTROLLER)?, "stream"))
}
stream_fn!(desired_size(cx, cf) throws {
    Ok(desired(cx, controller_stream(cx, cf_this(cf))?))
});
stream_fn!(controller_enqueue(cx, cf) throws {
    enqueue(cx, controller_stream(cx, cf_this(cf))?, cf_arg(cf, 0))
});
stream_fn!(controller_close(cx, cf) throws {
    close(cx, controller_stream(cx, cf_this(cf))?)
});
stream_fn!(controller_error(cx, cf) throws {
    error(cx, controller_stream(cx, cf_this(cf))?, cf_arg(cf, 0));
    Ok(UNDEF)
});

pub(super) unsafe fn create(
    cx: Env,
    target: u64,
    source: u64,
    strategy: u64,
    fallback_hwm: f64,
) -> JsResult {
    let strategy = cx.dictionary(strategy)?;
    let mark = cx.prop(strategy, "highWaterMark")?;
    let size = cx.method(strategy, "size")?;
    let source = if source == UNDEF {
        cx.record()
    } else if glue::rjs_kind(cx.g, source) < 8 {
        return Err(cx.type_error("The underlying source must be an object"));
    } else {
        source
    };
    let cancel = cx.method(source, "cancel")?;
    let pull = cx.method(source, "pull")?;
    let start = cx.method(source, "start")?;
    let ty = cx.prop(source, "type")?;
    if ty != UNDEF {
        let mut text = UNDEF;
        if glue::rjs_to_string(cx.g, ty, &mut text) != 0 {
            return Err(text);
        }
        if read_string(cx.g, text).as_deref() == Some("bytes") {
            return Err(cx.type_error("ReadableStream byte streams (type: \"bytes\") are not implemented; use a default stream with Uint8Array chunks"));
        }
        return Err(cx.error(2, "Unknown readable stream type"));
    }
    let hwm = cx.hwm(mark, fallback_hwm)?;
    let s = cx.record();
    let object = cx.instance(STREAM, target, s)?;
    cx.put(s, "object", object);
    let inner = host_create(
        cx.g,
        classes(cx.g).readable_cls,
        UNDEF,
        ReadableCore::new(hwm),
    );
    cx.put(s, "core", inner);
    let c = cx.record();
    cx.put(c, "stream", s);
    let controller = cx.instance(CONTROLLER, UNDEF, c)?;
    for (k, v) in [
        ("source", source),
        ("size", size),
        ("pull", pull),
        ("cancel", cancel),
        ("controller", controller),
        ("requests", cx.record()),
        ("chunks", cx.record()),
    ] {
        cx.put(s, k, v);
    }
    let started = cx.invoke(start, source, &[controller])?;
    cx.then(cx.resolved(started), s, Some(on_start), Some(on_error));
    Ok(object)
}

pub(super) unsafe fn state_of(cx: Env, obj: u64) -> JsResult {
    cx.data(obj, STREAM)
}
pub(super) unsafe fn locked(cx: Env, s: u64) -> bool {
    core(cx, s).locked
}
pub(super) unsafe fn acquire(cx: Env, target: u64, s: u64) -> JsResult {
    if locked(cx, s) {
        return Err(cx.type_error("This ReadableStream is already locked"));
    }
    let r = cx.record();
    let reader = cx.instance(READER, target, r)?;
    cx.put(r, "object", reader);
    cx.put(r, "stream", s);
    cx.put(s, "reader", r);
    core(cx, s).locked = true;
    let closed = cx.handled(cx.pending());
    cx.put(r, "closed", closed);
    match core(cx, s).state {
        ReadableState::Closed => cx.settle(closed, Ok(UNDEF)),
        ReadableState::Errored => cx.settle(closed, Err(cx.field(s, "error"))),
        ReadableState::Readable => (),
    }
    Ok(reader)
}

pub(super) unsafe fn read(cx: Env, r: u64, internal: bool) -> JsResult {
    let s = cx.field(r, "stream");
    if s == UNDEF {
        return Err(cx.type_error("This reader has been released"));
    }
    core(cx, s).disturbed = true;
    match core(cx, s).state {
        ReadableState::Closed => return Ok(cx.resolved(read_result(cx, UNDEF, true, internal))),
        ReadableState::Errored => return Err(cx.field(s, "error")),
        ReadableState::Readable => (),
    }
    if let Some(chunk) = cx.pop(cx.field(s, "chunks")) {
        core(cx, s).dequeue();
        if core(cx, s).close_if_drained() {
            clear_algorithms(cx, s);
            drain(cx, s, None);
        } else {
            pull(cx, s);
        }
        return Ok(cx.resolved(read_result(cx, chunk, false, internal)));
    }
    let p = cx.pending();
    let request = cx.record();
    cx.put(request, "promise", p);
    cx.put(request, "internal", enc_bool(internal));
    cx.push(cx.field(s, "requests"), request);
    pull(cx, s);
    Ok(p)
}

pub(super) unsafe fn release(cx: Env, r: u64) {
    let s = cx.field(r, "stream");
    if s == UNDEF {
        return;
    }
    let err = cx.type_error("This reader has been released");
    if core(cx, s).state != ReadableState::Readable {
        cx.put(r, "closed", cx.handled(cx.pending()));
    }
    cx.settle(cx.field(r, "closed"), Err(err));
    cx.put(r, "stream", UNDEF);
    cx.put(s, "reader", UNDEF);
    core(cx, s).locked = false;
    let requests = cx.field(s, "requests");
    while let Some(request) = cx.pop(requests) {
        cx.settle(cx.field(request, "promise"), Err(err));
    }
}

stream_fn!(discard(_cx, _cf) throws {
    Ok(UNDEF)
});

pub(super) unsafe fn cancel(cx: Env, s: u64, reason: u64) -> JsResult {
    core(cx, s).disturbed = true;
    match core(cx, s).state {
        ReadableState::Closed => return Ok(cx.resolved(UNDEF)),
        ReadableState::Errored => return Err(cx.field(s, "error")),
        ReadableState::Readable => (),
    }
    core(cx, s).queue.reset();
    cx.put(s, "chunks", cx.record());
    drain(cx, s, None);
    let result = cx.invoke(cx.field(s, "cancel"), cx.field(s, "source"), &[reason]);
    clear_algorithms(cx, s);
    Ok(cx.then(cx.promises(result), UNDEF, Some(discard), None))
}

stream_fn!(constructor(cx, cf) throws {
    create(cx, cf_new_target(cf), cf_arg(cf, 0), cf_arg(cf, 1), 1.)
});
stream_fn!(reader_ctor(cx, cf) throws {
    acquire(cx, cf_new_target(cf), state_of(cx, cf_arg(cf, 0))?)
});
stream_fn!(stream_locked(cx, cf) throws {
    Ok(enc_bool(locked(cx, state_of(cx, cf_this(cf))?)))
});
stream_fn!(get_reader(cx, cf) throws {
    let s = state_of(cx, cf_this(cf))?;
    let opts = cx.dictionary(cf_arg(cf, 0))?;
    let mode = cx.prop(opts, "mode")?;
    if mode != UNDEF {
        let mut text = UNDEF;
        if glue::rjs_to_string(cx.g, mode, &mut text) != 0 {
            return Err(text);
        }
        return Err(cx.type_error("ReadableStream.getReader: BYOB mode is not supported"));
    }
    acquire(cx, UNDEF, s)
});
stream_fn!(stream_cancel(cx, cf) promises {
    let s = state_of(cx, cf_this(cf))?;
    if locked(cx, s) {
        return Err(cx.type_error("Cannot cancel a locked ReadableStream"));
    }
    cancel(cx, s, cf_arg(cf, 0))
});
pub(super) unsafe fn reader_data(cx: Env, object: u64) -> JsResult {
    cx.data(object, READER)
}
stream_fn!(reader_closed(cx, cf) promises {
    Ok(cx.field(reader_data(cx, cf_this(cf))?, "closed"))
});
stream_fn!(reader_read(cx, cf) promises {
    read(cx, reader_data(cx, cf_this(cf))?, false)
});
stream_fn!(reader_release(cx, cf) throws {
    release(cx, reader_data(cx, cf_this(cf))?);
    Ok(UNDEF)
});
stream_fn!(reader_cancel(cx, cf) promises {
    let s = cx.field(reader_data(cx, cf_this(cf))?, "stream");
    if s == UNDEF {
        return Err(cx.type_error("This reader has been released"));
    }
    cancel(cx, s, cf_arg(cf, 0))
});

pub(super) unsafe fn install(cx: Env) {
    cx.register(ClassSpec {
        name: CONTROLLER,
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[
            ("enqueue", 0, controller_enqueue),
            ("close", 0, controller_close),
            ("error", 0, controller_error),
        ],
        getters: &[("desiredSize", desired_size)],
        accessors: &[],
    });
    cx.register(ClassSpec {
        name: READER,
        arity: 1,
        construct: Some(reader_ctor),
        parent_proto: 0,
        methods: &[
            ("read", 0, reader_read),
            ("releaseLock", 0, reader_release),
            ("cancel", 0, reader_cancel),
        ],
        getters: &[("closed", reader_closed)],
        accessors: &[],
    });
    cx.register(ClassSpec {
        name: STREAM,
        arity: 0,
        construct: Some(constructor),
        parent_proto: 0,
        methods: &[
            ("getReader", 0, get_reader),
            ("cancel", 0, stream_cancel),
            ("tee", 0, super::tee::tee),
            ("pipeTo", 1, super::pipe::pipe_to),
            ("pipeThrough", 1, super::pipe::pipe_through),
        ],
        getters: &[("locked", stream_locked)],
        accessors: &[],
    });
}
