use super::super::abort;
use super::binding::*;
use super::{readable, writable, ReadableState, WritableState};

#[derive(Clone, Copy, PartialEq)]
enum Stop {
    SourceClosed,
    SourceError,
    DestinationError,
    DestinationClosed,
    Signal,
}

unsafe fn finish(cx: Env, t: u64, result: JsResult) {
    if cx.field(t, "finished") == TRUE {
        return;
    }
    cx.put(t, "finished", TRUE);
    let entry = cx.field(t, "abortEntry");
    if entry != UNDEF {
        abort::remove_algorithm(cx.g, cx.field(t, "signal"), entry);
    }
    writable::release(cx, cx.field(t, "writer"));
    readable::release(cx, cx.field(t, "reader"));
    cx.settle(cx.field(t, "result"), result);
}

unsafe fn action_done(cx: Env, t: u64) {
    let n = dec_f64(cx.field(t, "actions")).unwrap_or(1.) as i32 - 1;
    cx.put(t, "actions", enc_i32(n));
    if n == 0 {
        let result = if cx.field(t, "success") == TRUE {
            Ok(UNDEF)
        } else {
            Err(cx.field(t, "reason"))
        };
        finish(cx, t, result);
    }
}
stream_fn!(succeeded(cx, cf) throws {
    action_done(cx, cx.capture(cf));
    Ok(UNDEF)
});
stream_fn!(failed(cx, cf) throws {
    finish(cx, cx.capture(cf), Err(cf_arg(cf, 0)));
    Ok(UNDEF)
});

unsafe fn run_actions(cx: Env, t: u64) {
    let r = cx.field(t, "source");
    let w = cx.field(t, "destination");
    let reason = cx.field(t, "reason");
    let mut actions = [None, None];
    if cx.field(t, "abort") == TRUE
        && (writable::status(cx, w) == WritableState::Writable || cx.field(t, "sourceError") == TRUE)
    {
        actions[0] = Some(writable::abort_stream(cx, w, reason));
    } else if cx.field(t, "close") == TRUE {
        actions[0] = match writable::status(cx, w) {
            WritableState::Errored => Some(Err(cx.field(w, "error"))),
            WritableState::Closed => None,
            _ if writable::closing(cx, w) => None,
            _ => Some(writable::close(cx, w)),
        };
    }
    if cx.field(t, "cancel") == TRUE && readable::status(cx, r) == ReadableState::Readable {
        actions[1] = Some(readable::cancel(cx, r, reason));
    }
    let n = actions.iter().flatten().count();
    cx.put(t, "actions", enc_i32(n.max(1) as i32));
    if n == 0 {
        action_done(cx, t);
    }
    for action in actions.into_iter().flatten() {
        cx.then(cx.promises(action), t, Some(succeeded), Some(failed));
    }
}
stream_fn!(after_writes(cx, cf) throws {
    run_actions(cx, cx.capture(cf));
    Ok(UNDEF)
});

unsafe fn shutdown(cx: Env, t: u64, cause: Stop, reason: u64) {
    if cx.field(t, "stopping") == TRUE {
        return;
    }
    cx.put(t, "stopping", TRUE);
    cx.put(t, "success", enc_bool(cause == Stop::SourceClosed));
    cx.put(t, "sourceError", enc_bool(cause == Stop::SourceError));
    cx.put(t, "reason", reason);
    cx.put(
        t,
        "abort",
        enc_bool(
            matches!(cause, Stop::Signal | Stop::SourceError)
                && cx.field(t, "preventAbort") != TRUE,
        ),
    );
    cx.put(
        t,
        "cancel",
        enc_bool(
            matches!(
                cause,
                Stop::Signal | Stop::DestinationClosed | Stop::DestinationError
            ) && cx.field(t, "preventCancel") != TRUE,
        ),
    );
    cx.put(
        t,
        "close",
        enc_bool(cause == Stop::SourceClosed && cx.field(t, "preventClose") != TRUE),
    );
    let w = cx.field(t, "destination");
    let last = cx.field(t, "lastWrite");
    if writable::status(cx, w) == WritableState::Writable && !writable::closing(cx, w) {
        cx.then(last, t, Some(after_writes), Some(after_writes));
    } else {
        run_actions(cx, t);
    }
}
stream_fn!(on_signal(cx, cf) throws {
    shutdown(cx, cx.capture(cf), Stop::Signal, cf_arg(cf, 0));
    Ok(UNDEF)
});
stream_fn!(source_failed(cx, cf) throws {
    shutdown(cx, cx.capture(cf), Stop::SourceError, cf_arg(cf, 0));
    Ok(UNDEF)
});
stream_fn!(destination_failed(cx, cf) throws {
    shutdown(cx, cx.capture(cf), Stop::DestinationError, cf_arg(cf, 0));
    Ok(UNDEF)
});
stream_fn!(source_closed(cx, cf) throws {
    let t = cx.capture(cf);
    // read() may have drained the last chunk and settled closed before its own reaction runs.
    if cx.field(t, "reading") != TRUE {
        shutdown(cx, t, Stop::SourceClosed, UNDEF);
    }
    Ok(UNDEF)
});
unsafe fn closed_destination(cx: Env, t: u64) {
    shutdown(
        cx,
        t,
        Stop::DestinationClosed,
        cx.type_error("The pipe destination is closed"),
    );
}
stream_fn!(destination_closed(cx, cf) throws {
    closed_destination(cx, cx.capture(cf));
    Ok(UNDEF)
});
stream_fn!(on_chunk(cx, cf) throws {
    let t = cx.capture(cf);
    cx.put(t, "reading", FALSE);
    if cx.field(t, "stopping") == TRUE {
        return Ok(UNDEF);
    }
    let result = cf_arg(cf, 0);
    if cx.field(result, "done") == TRUE {
        shutdown(cx, t, Stop::SourceClosed, UNDEF);
        return Ok(UNDEF);
    }
    let w = cx.field(t, "writer");
    let write = cx.promises(writable::write(cx, w, cx.field(result, "value")));
    cx.put(t, "lastWrite", write);
    cx.then(write, t, None, Some(destination_failed));
    if readable::status(cx, cx.field(t, "source")) == ReadableState::Closed {
        shutdown(cx, t, Stop::SourceClosed, UNDEF);
    } else {
        pump(cx, t);
    }
    Ok(UNDEF)
});
stream_fn!(on_ready(cx, cf) throws {
    let t = cx.capture(cf);
    if cx.field(t, "stopping") != TRUE {
        if readable::status(cx, cx.field(t, "source")) == ReadableState::Closed {
            shutdown(cx, t, Stop::SourceClosed, UNDEF);
            return Ok(UNDEF);
        }
        cx.put(t, "reading", TRUE);
        let p = cx.promises(readable::read(cx, cx.field(t, "reader"), true));
        cx.then(p, t, Some(on_chunk), Some(source_failed));
    }
    Ok(UNDEF)
});
unsafe fn pump(cx: Env, t: u64) {
    cx.then(
        cx.field(cx.field(t, "writer"), "ready"),
        t,
        Some(on_ready),
        Some(destination_failed),
    );
}

unsafe fn pipe(cx: Env, r: u64, w: u64, opts: u64) -> JsResult {
    let opts = cx.dictionary(opts)?;
    let t = cx.record();
    for name in ["preventAbort", "preventCancel", "preventClose"] {
        cx.put(t, name, enc_bool(to_bool(cx.g, cx.prop(opts, name)?)));
    }
    let signal = cx.prop(opts, "signal")?;
    if signal != UNDEF && !abort::is_signal(cx.g, signal) {
        return Err(cx.type_error("signal must be an AbortSignal"));
    }
    if readable::locked(cx, r) || writable::locked(cx, w) {
        return Err(cx.type_error("Cannot pipe a locked stream"));
    }
    let reader = readable::reader_data(cx, readable::acquire(cx, UNDEF, r)?)?;
    let writer = writable::writer_data(cx, writable::acquire(cx, UNDEF, w)?)?;
    let p = cx.pending();
    for (k, v) in [
        ("source", r),
        ("destination", w),
        ("reader", reader),
        ("writer", writer),
        ("result", p),
        ("signal", signal),
    ] {
        cx.put(t, k, v);
    }
    if signal != UNDEF {
        if abort::is_aborted(cx.g, signal) {
            shutdown(cx, t, Stop::Signal, abort::reason(cx.g, signal));
            return Ok(p);
        }
        let entry = abort::add_algorithm(cx.g, signal, cx.callback(on_signal, t));
        cx.put(t, "abortEntry", entry);
    }
    if readable::status(cx, r) == ReadableState::Errored {
        shutdown(cx, t, Stop::SourceError, cx.field(r, "error"));
    } else if writable::status(cx, w) == WritableState::Errored {
        shutdown(cx, t, Stop::DestinationError, cx.field(w, "error"));
    } else if readable::status(cx, r) == ReadableState::Closed {
        shutdown(cx, t, Stop::SourceClosed, UNDEF);
    } else if writable::status(cx, w) == WritableState::Closed || writable::closing(cx, w) {
        closed_destination(cx, t);
    } else {
        cx.then(
            cx.field(reader, "closed"),
            t,
            Some(source_closed),
            Some(source_failed),
        );
        cx.then(
            cx.field(writer, "closed"),
            t,
            Some(destination_closed),
            Some(destination_failed),
        );
        pump(cx, t);
    }
    Ok(p)
}

#[crate::host_fn]
pub(super) unsafe fn pipe_to(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cx = Env::new(g);
    cx.promises((|| {
        pipe(
            cx,
            readable::state_of(cx, cf_this(cf))?,
            writable::state_of(cx, cf_arg(cf, 0))?,
            cf_arg(cf, 1),
        )
    })())
}
#[crate::host_fn]
pub(super) unsafe fn pipe_through(g: *mut c_void, cf: *mut c_void) -> u64 {
    let cx = Env::new(g);
    cx.throws((|| {
        let r = readable::state_of(cx, cf_this(cf))?;
        let pair = cf_arg(cf, 0);
        let out = cx.prop(pair, "readable")?;
        readable::state_of(cx, out)?;
        let w = writable::state_of(cx, cx.prop(pair, "writable")?)?;
        cx.handled(pipe(cx, r, w, cf_arg(cf, 1))?);
        Ok(out)
    })())
}
