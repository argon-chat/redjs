use super::super::{abort, domexception, websocket};
use super::binding::*;
use super::{readable, writable};

const CLASS: &str = "WebSocketStream";
const HIGH_WATER_BYTES: u64 = 1 << 20;

unsafe fn methods(cx: Env) -> u64 {
    cx.slot(cx.ctor(CLASS))
}

unsafe fn close_socket(cx: Env, state: u64, args: &[u64]) -> JsResult {
    cx.invoke(
        cx.field(methods(cx), "close"),
        cx.field(state, "socket"),
        args,
    )
}

unsafe fn above_high_water(cx: Env, state: u64) -> bool {
    let (open, bytes) = websocket::stream_send_state(cx.g, cx.field(state, "socket"));
    open && bytes > HIGH_WATER_BYTES
}

unsafe fn poll_drain(cx: Env, state: u64) {
    crate::web::schedule_native(
        cx.g,
        4.,
        Box::new(move |g| {
            let cx = Env::new(g);
            if above_high_water(cx, state) {
                poll_drain(cx, state);
            } else {
                let promise = cx.field(state, "drain");
                cx.put(state, "drain", UNDEF);
                cx.settle(promise, Ok(UNDEF));
                crate::ffi::redjs_unprotect(g, state);
            }
        }),
    );
}

stream_fn!(write(cx, cf) throws {
    let state = cx.capture(cf);
    let socket = cx.field(state, "socket");
    if !websocket::stream_send_state(cx.g, socket).0 {
        return Err(domexception::create(
            cx.g,
            "InvalidStateError",
            "WebSocketStream: the socket is not open",
        ));
    }
    cx.invoke(cx.field(methods(cx), "send"), socket, &[cf_arg(cf, 0)])?;
    if !above_high_water(cx, state) {
        return Ok(UNDEF);
    }
    let promise = cx.pending();
    cx.put(state, "drain", promise);
    crate::ffi::redjs_protect(cx.g, state);
    poll_drain(cx, state);
    Ok(promise)
});

stream_fn!(close_quiet(cx, cf) throws {
    close_socket(cx, cx.capture(cf), &[])
});

stream_fn!(on_open(cx, cf) throws {
    let state = cx.capture(cf);
    let socket = cx.field(state, "socket");
    let info = new_object(cx.g, UNDEF);
    for name in ["readable", "writable"] {
        cx.put(info, name, cx.field(state, name));
    }
    for name in ["protocol", "extensions"] {
        cx.put(info, name, cx.prop(socket, name)?);
    }
    cx.put(state, "connected", TRUE);
    cx.settle(cx.field(state, "opened"), Ok(info));
    Ok(UNDEF)
});

stream_fn!(on_message(cx, cf) throws {
    let state = cx.capture(cf);
    let stream = cx.field(state, "source");
    if readable::status(cx, stream) == super::ReadableState::Readable {
        readable::enqueue(cx, stream, cx.prop(cf_arg(cf, 0), "data")?)?;
    }
    Ok(UNDEF)
});

stream_fn!(on_close(cx, cf) throws {
    let state = cx.capture(cf);
    let entry = cx.field(state, "abortEntry");
    if entry != UNDEF {
        abort::remove_algorithm(cx.g, cx.field(state, "signal"), entry);
        cx.put(state, "signal", UNDEF);
        cx.put(state, "abortEntry", UNDEF);
    }
    if cx.field(state, "connected") != TRUE {
        let error = domexception::create(
            cx.g,
            "NetworkError",
            "WebSocketStream: the connection could not be established",
        );
        cx.settle(cx.field(state, "opened"), Err(error));
    }
    let event = cf_arg(cf, 0);
    let code = cx.prop(event, "code")?;
    let readable = cx.field(state, "source");
    if to_bool(cx.g, cx.prop(event, "wasClean")?) {
        let _ = readable::close(cx, readable);
        let info = new_object(cx.g, UNDEF);
        cx.put(info, "closeCode", code);
        cx.put(info, "reason", cx.prop(event, "reason")?);
        cx.settle(cx.field(state, "closed"), Ok(info));
    } else {
        let message = format!(
            "WebSocketStream: the connection closed abnormally ({})",
            cx.number(code)?
        );
        let error = domexception::create(cx.g, "NetworkError", &message);
        readable::error(cx, readable, error);
        cx.settle(cx.field(state, "closed"), Err(error));
    }
    Ok(UNDEF)
});

stream_fn!(construct(cx, cf) throws {
    let options = cx.dictionary(cf_arg(cf, 1))?;
    let protocols = cx.prop(options, "protocols")?;
    let signal = cx.prop(options, "signal")?;
    let args = [cf_arg(cf, 0), protocols];
    let state = cx.record();
    let instance = cx.instance(CLASS, cf_new_target(cf), state)?;
    let mut socket = UNDEF;
    if glue::rjs_construct(
        cx.g,
        cx.field(methods(cx), "socket"),
        args.as_ptr(),
        2,
        &mut socket,
    ) != 0
    {
        return Err(socket);
    }
    cx.put(state, "socket", socket);
    cx.put(state, "url", cx.prop(socket, "url")?);
    cx.put(state, "opened", cx.pending());
    cx.put(state, "closed", cx.handled(cx.pending()));
    let close = cx.callback(close_quiet, state);
    let source = cx.record();
    cx.put(source, "cancel", close);
    let readable = readable::create(cx, UNDEF, source, UNDEF, 1.)?;
    cx.put(state, "readable", readable);
    cx.put(state, "source", readable::state_of(cx, readable)?);
    let sink = cx.record();
    cx.put(sink, "write", cx.callback(write, state));
    cx.put(sink, "close", close);
    cx.put(sink, "abort", close);
    cx.put(state, "writable", writable::create(cx, UNDEF, sink, UNDEF)?);
    websocket::bind_stream(
        cx.g,
        socket,
        cx.callback(on_open, state),
        cx.callback(on_message, state),
        cx.callback(on_close, state),
    );
    if !matches!(signal, NULL | UNDEF) {
        let installed = if abort::is_signal(cx.g, signal) {
            if abort::is_aborted(cx.g, signal) {
                close_socket(cx, state, &[])
            } else {
                cx.put(state, "signal", signal);
                cx.put(
                    state,
                    "abortEntry",
                    abort::add_algorithm(cx.g, signal, close),
                );
                Ok(UNDEF)
            }
        } else {
            attach_signal(cx, state, signal, close)
        };
        if let Err(error) = installed {
            let _ = close_socket(cx, state, &[]);
            cx.handled(cx.field(state, "opened"));
            return Err(error);
        }
    }
    Ok(instance)
});

unsafe fn attach_signal(cx: Env, state: u64, signal: u64, callback: u64) -> JsResult {
    if to_bool(cx.g, cx.prop(signal, "aborted")?) {
        return close_socket(cx, state, &[]);
    }
    let options = cx.record();
    cx.put(options, "once", TRUE);
    let listener = cx.prop(signal, "addEventListener")?;
    if glue::rjs_kind(cx.g, listener) != 9 {
        return Err(cx.type_error("WebSocketStream requires an AbortSignal"));
    }
    cx.invoke(
        listener,
        signal,
        &[js_string(cx.g, "abort"), callback, options],
    )
}

stream_fn!(url(cx, cf) throws {
    Ok(cx.field(cx.data(cf_this(cf), CLASS)?, "url"))
});
stream_fn!(opened(cx, cf) throws {
    Ok(cx.field(cx.data(cf_this(cf), CLASS)?, "opened"))
});
stream_fn!(closed(cx, cf) throws {
    Ok(cx.field(cx.data(cf_this(cf), CLASS)?, "closed"))
});
stream_fn!(close(cx, cf) throws {
    let state = cx.data(cf_this(cf), CLASS)?;
    let info = cx.dictionary(cf_arg(cf, 0))?;
    close_socket(
        cx,
        state,
        &[cx.prop(info, "closeCode")?, cx.prop(info, "reason")?],
    )
});

pub(super) unsafe fn install(cx: Env, socket: &Class) {
    let class = cx.register(ClassSpec {
        name: CLASS,
        arity: 1,
        construct: Some(construct),
        parent_proto: 0,
        methods: &[("close", 0, close)],
        getters: &[("url", url), ("opened", opened), ("closed", closed)],
        accessors: &[],
    });
    let methods = cx.record();
    cx.put(methods, "socket", socket.ctor);
    cx.put(methods, "send", cx.field(socket.proto, "send"));
    cx.put(methods, "close", cx.field(socket.proto, "close"));
    cx.attach(class.ctor, methods);
}
