use super::super::{abort, domexception};
use super::binding::*;
use super::{classes, WritableCore, WritableState};
use WritableState::{Closed, Errored, Erroring, Writable};

const STREAM: &str = "WritableStream";
const WRITER: &str = "WritableStreamDefaultWriter";
const CONTROLLER: &str = "WritableStreamDefaultController";

unsafe fn core<'a>(cx: Env, s: u64) -> &'a mut WritableCore {
    host_data(cx.g, classes(cx.g).writable_cls, cx.field(s, "core")).unwrap()
}

unsafe fn reject_monitor(cx: Env, writer: u64, key: &str, reason: u64) {
    let mut p = cx.field(writer, key);
    let (mut status, mut value) = (0, UNDEF);
    glue::rjs_promise_info(cx.g, p, &mut status, &mut value);
    if status != 0 {
        p = cx.handled(cx.pending());
        cx.put(writer, key, p);
    }
    cx.settle(p, Err(reason));
}

unsafe fn update_pressure(cx: Env, s: u64) {
    let core = core(cx, s);
    if core.state != Writable || core.close_requested {
        return;
    }
    let pressure = core.desired_size() <= 0.;
    if pressure == core.backpressure {
        return;
    }
    core.backpressure = pressure;
    let writer = cx.field(s, "writer");
    if writer == UNDEF {
        return;
    }
    if pressure {
        cx.put(writer, "ready", cx.handled(cx.pending()));
    } else {
        cx.settle(cx.field(writer, "ready"), Ok(UNDEF));
    }
}

unsafe fn reject_close_and_closed(cx: Env, s: u64) {
    let reason = cx.field(s, "error");
    let close = cx.field(s, "close");
    if close != UNDEF {
        cx.settle(close, Err(reason));
        cx.put(s, "close", UNDEF);
    }
    let writer = cx.field(s, "writer");
    if writer != UNDEF {
        reject_monitor(cx, writer, "closed", reason);
    }
}

unsafe fn clear_algorithms(cx: Env, s: u64) {
    for key in [
        "sink",
        "writeAlgorithm",
        "abortAlgorithm",
        "closeAlgorithm",
        "size",
    ] {
        cx.put(s, key, UNDEF);
    }
}

unsafe fn finish_error(cx: Env, s: u64) {
    core(cx, s).state = Errored;
    core(cx, s).queue.reset();
    let reason = cx.field(s, "error");
    while let Some(req) = cx.pop(cx.field(s, "queue")) {
        cx.settle(cx.field(req, "promise"), Err(reason));
    }
    let abort = cx.field(s, "abort");
    cx.put(s, "abort", UNDEF);
    if abort != UNDEF && cx.field(abort, "alreadyErroring") != TRUE {
        let result = cx.invoke(
            cx.field(s, "abortAlgorithm"),
            cx.field(s, "sink"),
            &[cx.field(abort, "reason")],
        );
        clear_algorithms(cx, s);
        cx.then(
            cx.promises(result),
            abort,
            Some(aborted),
            Some(abort_rejected),
        );
    } else {
        clear_algorithms(cx, s);
        if abort != UNDEF {
            cx.settle(cx.field(abort, "promise"), Err(reason));
        }
        reject_close_and_closed(cx, s);
    }
}

unsafe fn after_abort(cx: Env, abort: u64, result: JsResult) {
    cx.settle(cx.field(abort, "promise"), result);
    reject_close_and_closed(cx, cx.field(abort, "stream"));
}
stream_fn!(aborted(cx, cf) throws {
    after_abort(cx, cx.capture(cf), Ok(UNDEF));
    Ok(UNDEF)
});
stream_fn!(abort_rejected(cx, cf) throws {
    after_abort(cx, cx.capture(cf), Err(cf_arg(cf, 0)));
    Ok(UNDEF)
});

pub(super) unsafe fn error(cx: Env, s: u64, reason: u64) {
    if core(cx, s).state == Writable {
        core(cx, s).state = Erroring;
        cx.put(s, "error", reason);
        let w = cx.field(s, "writer");
        if w != UNDEF {
            reject_monitor(cx, w, "ready", reason);
        }
    }
    let c = core(cx, s);
    if c.state == Erroring && c.started && !c.in_flight_write && !c.in_flight_close {
        finish_error(cx, s);
    }
}

unsafe fn advance(cx: Env, s: u64) {
    let c = core(cx, s);
    if !c.started || c.in_flight_close || c.in_flight_write {
        return;
    }
    if c.state == Erroring {
        finish_error(cx, s);
        return;
    }
    if c.state != Writable {
        return;
    }
    let close_requested = c.close_requested;
    if let Some(req) = cx.pop(cx.field(s, "queue")) {
        core(cx, s).in_flight_write = true;
        cx.put(s, "inFlight", cx.field(req, "promise"));
        let args = [cx.field(req, "chunk"), cx.field(s, "controller")];
        let value = cx.invoke(cx.field(s, "writeAlgorithm"), cx.field(s, "sink"), &args);
        cx.then(cx.promises(value), s, Some(written), Some(write_failed));
    } else if close_requested {
        core(cx, s).in_flight_close = true;
        cx.put(s, "inFlight", cx.field(s, "close"));
        cx.put(s, "close", UNDEF);
        let value = cx.invoke(cx.field(s, "closeAlgorithm"), cx.field(s, "sink"), &[]);
        clear_algorithms(cx, s);
        cx.then(cx.promises(value), s, Some(closed), Some(close_failed));
    }
}

unsafe fn finish_request(cx: Env, s: u64, result: JsResult) {
    let p = cx.field(s, "inFlight");
    cx.put(s, "inFlight", UNDEF);
    cx.settle(p, result);
}
stream_fn!(written(cx, cf) throws {
    let s = cx.capture(cf);
    core(cx, s).in_flight_write = false;
    finish_request(cx, s, Ok(UNDEF));
    core(cx, s).dequeue();
    update_pressure(cx, s);
    advance(cx, s);
    Ok(UNDEF)
});
stream_fn!(write_failed(cx, cf) throws {
    let s = cx.capture(cf);
    let reason = cf_arg(cf, 0);
    core(cx, s).in_flight_write = false;
    finish_request(cx, s, Err(reason));
    if core(cx, s).state == Writable {
        clear_algorithms(cx, s);
    }
    error(cx, s, reason);
    Ok(UNDEF)
});
unsafe fn finish_close(cx: Env, s: u64, result: JsResult) {
    core(cx, s).in_flight_close = false;
    finish_request(cx, s, result);
    let abort = cx.field(s, "abort");
    if abort != UNDEF {
        cx.put(s, "abort", UNDEF);
        cx.settle(cx.field(abort, "promise"), result);
    }
    match result {
        Ok(_) => {
            core(cx, s).state = Closed;
            cx.put(s, "error", UNDEF);
            let writer = cx.field(s, "writer");
            if writer != UNDEF {
                cx.settle(cx.field(writer, "closed"), Ok(UNDEF));
            }
        }
        Err(e) => error(cx, s, e),
    }
}
stream_fn!(closed(cx, cf) throws {
    finish_close(cx, cx.capture(cf), Ok(UNDEF));
    Ok(UNDEF)
});
stream_fn!(close_failed(cx, cf) throws {
    finish_close(cx, cx.capture(cf), Err(cf_arg(cf, 0)));
    Ok(UNDEF)
});
stream_fn!(started(cx, cf) throws {
    let s = cx.capture(cf);
    core(cx, s).started = true;
    advance(cx, s);
    Ok(UNDEF)
});
stream_fn!(start_failed(cx, cf) throws {
    let s = cx.capture(cf);
    core(cx, s).started = true;
    error(cx, s, cf_arg(cf, 0));
    Ok(UNDEF)
});

pub(super) unsafe fn create(cx: Env, target: u64, sink: u64, strategy: u64) -> JsResult {
    let strategy = cx.dictionary(strategy)?;
    let mark = cx.prop(strategy, "highWaterMark")?;
    let size = cx.method(strategy, "size")?;
    let sink = if sink == UNDEF {
        cx.record()
    } else if glue::rjs_kind(cx.g, sink) < 8 {
        return Err(cx.type_error("The underlying sink must be an object"));
    } else {
        sink
    };
    let abort = cx.method(sink, "abort")?;
    let close = cx.method(sink, "close")?;
    let start = cx.method(sink, "start")?;
    let ty = cx.prop(sink, "type")?;
    let write = cx.method(sink, "write")?;
    if ty != UNDEF {
        return Err(cx.error(2, "An underlying sink cannot specify a type"));
    }
    let hwm = cx.hwm(mark, 1.)?;
    let s = cx.record();
    let obj = cx.instance(STREAM, target, s)?;
    cx.put(s, "object", obj);
    let mut machine = WritableCore::new(hwm);
    machine.backpressure = hwm == 0.;
    let inner = host_create(cx.g, classes(cx.g).writable_cls, UNDEF, machine);
    cx.put(s, "core", inner);
    let c = cx.record();
    cx.put(c, "stream", s);
    let controller = cx.instance(CONTROLLER, UNDEF, c)?;
    cx.put(s, "signal", abort::new_signal(cx.g, false, UNDEF));
    for (k, v) in [
        ("sink", sink),
        ("size", size),
        ("writeAlgorithm", write),
        ("closeAlgorithm", close),
        ("abortAlgorithm", abort),
        ("controller", controller),
        ("queue", cx.record()),
    ] {
        cx.put(s, k, v);
    }
    let value = cx.invoke(start, sink, &[controller])?;
    cx.then(cx.resolved(value), s, Some(started), Some(start_failed));
    Ok(obj)
}

pub(super) unsafe fn state_of(cx: Env, obj: u64) -> JsResult {
    cx.data(obj, STREAM)
}
pub(super) unsafe fn status(cx: Env, s: u64) -> WritableState {
    core(cx, s).state
}
pub(super) unsafe fn closing(cx: Env, s: u64) -> bool {
    core(cx, s).close_requested
}
pub(super) unsafe fn locked(cx: Env, s: u64) -> bool {
    core(cx, s).locked
}
pub(super) unsafe fn acquire(cx: Env, target: u64, s: u64) -> JsResult {
    if locked(cx, s) {
        return Err(cx.type_error("This WritableStream is already locked"));
    }
    let w = cx.record();
    let object = cx.instance(WRITER, target, w)?;
    cx.put(w, "object", object);
    cx.put(w, "stream", s);
    cx.put(s, "writer", w);
    core(cx, s).locked = true;
    let ready = cx.handled(cx.pending());
    let closed = cx.handled(cx.pending());
    cx.put(w, "ready", ready);
    cx.put(w, "closed", closed);
    match core(cx, s).state {
        Writable => {
            if !core(cx, s).backpressure || core(cx, s).close_requested {
                cx.settle(ready, Ok(UNDEF));
            }
        }
        Closed => {
            cx.settle(ready, Ok(UNDEF));
            cx.settle(closed, Ok(UNDEF));
        }
        Erroring => cx.settle(ready, Err(cx.field(s, "error"))),
        Errored => {
            let e = cx.field(s, "error");
            cx.settle(ready, Err(e));
            cx.settle(closed, Err(e));
        }
    }
    Ok(object)
}

pub(super) unsafe fn release(cx: Env, w: u64) {
    let s = cx.field(w, "stream");
    if s == UNDEF {
        return;
    }
    let e = cx.type_error("This writer has been released");
    reject_monitor(cx, w, "ready", e);
    reject_monitor(cx, w, "closed", e);
    core(cx, s).locked = false;
    cx.put(s, "writer", UNDEF);
    cx.put(w, "stream", UNDEF);
}

pub(super) unsafe fn write(cx: Env, w: u64, chunk: u64) -> JsResult {
    let s = writer_stream(cx, w)?;
    let size = cx.field(s, "size");
    let n = if size == UNDEF {
        Ok(1.)
    } else {
        cx.invoke(size, UNDEF, &[chunk]).and_then(|v| cx.number(v))
    };
    if cx.field(w, "stream") != s {
        return Err(cx.type_error("This writer has been released"));
    }
    if matches!(core(cx, s).state, Errored | Erroring) {
        return Err(cx.field(s, "error"));
    }
    if core(cx, s).close_requested || core(cx, s).state == Closed {
        return Err(cx.type_error("Cannot write to a closing or closed WritableStream"));
    }
    match n {
        Err(e) => {
            error(cx, s, e);
            return Err(e);
        }
        Ok(n) if core(cx, s).enqueue(n).is_err() => {
            let e = cx.error(2, "The chunk size must be a finite, non-negative number");
            error(cx, s, e);
            return Err(e);
        }
        Ok(_) => (),
    }
    let request = cx.record();
    let p = cx.pending();
    cx.put(request, "promise", p);
    cx.put(request, "chunk", chunk);
    cx.push(cx.field(s, "queue"), request);
    update_pressure(cx, s);
    advance(cx, s);
    Ok(p)
}

pub(super) unsafe fn close(cx: Env, s: u64) -> JsResult {
    if matches!(core(cx, s).state, Errored | Closed) || core(cx, s).close_requested {
        return Err(cx.type_error("This WritableStream is closing, closed or errored"));
    }
    let p = cx.pending();
    cx.put(s, "close", p);
    core(cx, s).close_requested = true;
    let w = cx.field(s, "writer");
    if w != UNDEF && core(cx, s).state == Writable && core(cx, s).backpressure {
        cx.settle(cx.field(w, "ready"), Ok(UNDEF));
    }
    advance(cx, s);
    Ok(p)
}

pub(super) unsafe fn abort_stream(cx: Env, s: u64, reason: u64) -> JsResult {
    if matches!(core(cx, s).state, Closed | Errored) {
        return Ok(cx.resolved(UNDEF));
    }
    let signal = cx.field(s, "signal");
    let signal_reason = if reason == UNDEF {
        domexception::create(cx.g, "AbortError", "The operation was aborted")
    } else {
        reason
    };
    abort::abort_signal(cx.g, signal, signal_reason);
    if matches!(core(cx, s).state, Closed | Errored) {
        return Ok(cx.resolved(UNDEF));
    }
    let abort = cx.field(s, "abort");
    if abort != UNDEF {
        return Ok(cx.field(abort, "promise"));
    }
    let p = cx.pending();
    let abort = cx.record();
    for (k, v) in [
        ("reason", reason),
        ("promise", p),
        ("alreadyErroring", enc_bool(core(cx, s).state == Erroring)),
        ("stream", s),
    ] {
        cx.put(abort, k, v);
    }
    cx.put(s, "abort", abort);
    error(cx, s, reason);
    Ok(p)
}

pub(super) unsafe fn writer_data(cx: Env, writer: u64) -> JsResult {
    cx.data(writer, WRITER)
}
unsafe fn writer_stream(cx: Env, w: u64) -> JsResult {
    let s = cx.field(w, "stream");
    if s == UNDEF {
        Err(cx.type_error("This writer has been released"))
    } else {
        Ok(s)
    }
}
pub(super) unsafe fn controller_stream(cx: Env, controller: u64) -> JsResult {
    Ok(cx.field(cx.data(controller, CONTROLLER)?, "stream"))
}

stream_fn!(constructor(cx, cf) throws {
    create(cx, cf_new_target(cf), cf_arg(cf, 0), cf_arg(cf, 1))
});
stream_fn!(writer_ctor(cx, cf) throws {
    acquire(cx, cf_new_target(cf), state_of(cx, cf_arg(cf, 0))?)
});
stream_fn!(stream_locked(cx, cf) throws {
    Ok(enc_bool(locked(cx, state_of(cx, cf_this(cf))?)))
});
stream_fn!(get_writer(cx, cf) throws {
    acquire(cx, UNDEF, state_of(cx, cf_this(cf))?)
});
stream_fn!(stream_close(cx, cf) promises {
    let s = state_of(cx, cf_this(cf))?;
    if locked(cx, s) {
        return Err(cx.type_error("Cannot close a locked WritableStream"));
    }
    close(cx, s)
});
stream_fn!(stream_abort(cx, cf) promises {
    let s = state_of(cx, cf_this(cf))?;
    if locked(cx, s) {
        return Err(cx.type_error("Cannot abort a locked WritableStream"));
    }
    abort_stream(cx, s, cf_arg(cf, 0))
});
stream_fn!(writer_abort(cx, cf) promises {
    abort_stream(
        cx,
        writer_stream(cx, writer_data(cx, cf_this(cf))?)?,
        cf_arg(cf, 0),
    )
});
stream_fn!(writer_write(cx, cf) promises {
    write(cx, writer_data(cx, cf_this(cf))?, cf_arg(cf, 0))
});
stream_fn!(writer_close(cx, cf) promises {
    close(cx, writer_stream(cx, writer_data(cx, cf_this(cf))?)?)
});
stream_fn!(writer_release(cx, cf) throws {
    release(cx, writer_data(cx, cf_this(cf))?);
    Ok(UNDEF)
});
stream_fn!(writer_ready(cx, cf) promises {
    Ok(cx.field(writer_data(cx, cf_this(cf))?, "ready"))
});
stream_fn!(writer_closed(cx, cf) promises {
    Ok(cx.field(writer_data(cx, cf_this(cf))?, "closed"))
});
stream_fn!(writer_desired_size(cx, cf) throws {
    let c = core(cx, writer_stream(cx, writer_data(cx, cf_this(cf))?)?);
    Ok(match c.state {
        Closed => enc_i32(0),
        Errored | Erroring => NULL,
        Writable => enc_f64(c.desired_size()),
    })
});
stream_fn!(signal(cx, cf) throws {
    Ok(cx.field(controller_stream(cx, cf_this(cf))?, "signal"))
});
stream_fn!(controller_error(cx, cf) throws {
    let s = controller_stream(cx, cf_this(cf))?;
    if core(cx, s).state == Writable {
        clear_algorithms(cx, s);
        error(cx, s, cf_arg(cf, 0));
    }
    Ok(UNDEF)
});

pub(super) unsafe fn install(cx: Env) {
    cx.register(ClassSpec {
        name: CONTROLLER,
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[("error", 0, controller_error)],
        getters: &[("signal", signal)],
        accessors: &[],
    });
    cx.register(ClassSpec {
        name: WRITER,
        arity: 1,
        construct: Some(writer_ctor),
        parent_proto: 0,
        methods: &[
            ("write", 0, writer_write),
            ("close", 0, writer_close),
            ("abort", 0, writer_abort),
            ("releaseLock", 0, writer_release),
        ],
        getters: &[
            ("desiredSize", writer_desired_size),
            ("ready", writer_ready),
            ("closed", writer_closed),
        ],
        accessors: &[],
    });
    cx.register(ClassSpec {
        name: STREAM,
        arity: 0,
        construct: Some(constructor),
        parent_proto: 0,
        methods: &[
            ("getWriter", 0, get_writer),
            ("close", 0, stream_close),
            ("abort", 0, stream_abort),
        ],
        getters: &[("locked", stream_locked)],
        accessors: &[],
    });
}
