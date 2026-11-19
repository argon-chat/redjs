use super::*;
use crate::webapi::mediastream as media;
use std::sync::atomic::Ordering;

const SWITCHES: [&str; 3] = ["echoCancellation", "noiseSuppression", "autoGainControl"];

pub(super) unsafe fn copy_constraints(cx: Env, obj: u64) -> JsResult {
    let copy = new_object(cx.g, UNDEF);
    let kind = glue::rjs_kind(cx.g, obj);
    if kind < 8 || kind == 9 {
        return Ok(copy);
    }
    let mut keys = UNDEF;
    if glue::rjs_enumerable_keys(cx.g, obj, &mut keys) != 0 {
        return Err(keys);
    }
    for i in 0..array_len(cx.g, keys) {
        let key = get_i(cx.g, keys, i);
        let mut value = UNDEF;
        if glue::rjs_get_v(cx.g, obj, key, &mut value) != 0 {
            return Err(value);
        }
        if value != UNDEF {
            glue::rjs_define_v(cx.g, copy, key, value);
        }
    }
    Ok(copy)
}

unsafe fn overconstrained(cx: Env, target: u64, constraint: u64, message: u64) -> JsResult {
    let ctor = cx.ctor(OVERCONSTRAINED);
    let proto = if target == UNDEF {
        UNDEF
    } else {
        cx.prop(target, "prototype")?
    };
    let proto = if glue::rjs_kind(cx.g, proto) >= 8 {
        proto
    } else {
        cx.field(ctor, "prototype")
    };
    let error = domexception::create_for_subclass(cx.g, proto, OVERCONSTRAINED, message);
    let data = cx.record();
    cx.put(data, "brand", ctor);
    cx.put(data, "constraint", constraint);
    cx.attach(error, data);
    Ok(error)
}

pub(super) unsafe fn failure(cx: Env, name: &str, message: &str) -> u64 {
    if name == OVERCONSTRAINED {
        return overconstrained(
            cx,
            UNDEF,
            js_string(cx.g, "deviceId"),
            js_string(cx.g, message),
        )
        .unwrap();
    }
    domexception::create(cx.g, name, message)
}
media_fn!(new_error(cx, cf) throws {
    let constraint = cx.string(cf_arg(cf, 0))?;
    let msg = cf_arg(cf, 1);
    let msg = if msg == UNDEF {
        js_string(cx.g, "")
    } else {
        cx.string(msg)?
    };
    overconstrained(cx, cf_new_target(cf), constraint, msg)
});
media_fn!(constraint(cx, cf) throws {
    Ok(cx.field(cx.data(cf_this(cf), OVERCONSTRAINED)?, "constraint"))
});

pub(super) unsafe fn device(cx: Env, value: u64) -> Result<(Option<String>, bool), u64> {
    let mut id = value;
    let mut exact = false;
    let array_test = cx.field(cx.interfaces, "isArray");
    let is_array = |v| cx.invoke(array_test, UNDEF, &[v]).map(|r| r == TRUE);
    if glue::rjs_kind(cx.g, value) >= 8 && !is_array(value)? {
        id = cx.prop(value, "exact")?;
        exact = id != UNDEF;
        if !exact {
            id = cx.prop(value, "ideal")?;
        }
    }
    if is_array(id)? {
        id = cx.prop(id, "0")?;
    }
    if glue::rjs_kind(cx.g, id) != 5 {
        return Ok((None, false));
    }
    Ok((read_string(cx.g, id).filter(|s| !s.is_empty()), exact))
}

unsafe fn switch(cx: Env, options: u64, key: &str) -> Result<(bool, bool), u64> {
    let value = cx.prop(options, key)?;
    if matches!(value, TRUE | FALSE) {
        return Ok((value == TRUE, value == TRUE));
    }
    if glue::rjs_kind(cx.g, value) >= 8 {
        let exact = cx.prop(value, "exact")?;
        let ideal = cx.prop(value, "ideal")?;
        let selected = if matches!(exact, TRUE | FALSE) {
            exact
        } else {
            ideal
        };
        return Ok((selected != FALSE, exact == TRUE || ideal == TRUE));
    }
    Ok((true, false))
}

media_fn!(get_user_media(cx, cf) promises {
    if !proto_chain_has(cx.g, cf_this(cf), cx.field(cx.interfaces, "devicesProto")) {
        return Err(cx.type_error("Illegal invocation"));
    }
    let options = cx.dictionary(cf_arg(cf, 0))?;
    let audio = cx.prop(options, "audio")?;
    let video = to_bool(cx.g, cx.prop(options, "video")?);
    if video {
        media::advise_once("getUserMedia({ video }) is refused: this runtime captures audio only.");
        return Err(failure(
            cx,
            "NotSupportedError",
            "Video capture is not supported by this runtime.",
        ));
    }
    if !to_bool(cx.g, audio) {
        return Err(cx.type_error("At least one of audio and video must be requested"));
    }
    let options = if audio == TRUE { cx.record() } else { audio };
    let ec = switch(cx, options, SWITCHES[0])?;
    let ns = switch(cx, options, SWITCHES[1])?;
    let agc = switch(cx, options, SWITCHES[2])?;
    if !media::can_process() && (ec.1 || ns.1 || agc.1) {
        media::advise_once("getUserMedia: capture processing was requested, but this host supplies the raw device signal.");
    }
    let (device_id, exact) = device(cx, cx.prop(options, "deviceId")?)?;
    let gate = media::gate_for(cx.g as usize);
    if let Some((name, message)) = media::refusal(gate) {
        return Err(failure(cx, name, &message));
    }
    let requests = cx.field(cx.interfaces, "pending");
    let sequence = &state_of(cx.g).unwrap().next_token;
    let token = loop {
        let token = sequence.fetch_add(1, Ordering::Relaxed);
        if get_i(cx.g, requests, token) == UNDEF {
            break token;
        }
    };
    let pending = cx.record();
    let promise = cx.pending();
    cx.put(pending, "promise", promise);
    cx.put(pending, "constraints", options);
    glue::rjs_define_index(cx.g, requests, token, pending);
    let request = media::AudioRequest {
        device_id,
        exact,
        settings: media::Settings {
            echo_cancellation: ec.0,
            noise_suppression: ns.0,
            auto_gain_control: agc.0,
        },
    };
    if gate == media::Gate::Ask {
        media::ask_then_open(cx.g as usize, token, request);
    } else {
        media::open_microphone(cx.g as usize, token, request);
    }
    Ok(promise)
});

pub(super) unsafe fn settle(cx: Env, token: u32, result: OpenResult) {
    let requests = cx.field(cx.interfaces, "pending");
    let pending = get_i(cx.g, requests, token);
    if pending == UNDEF {
        if let OpenResult::Ok(id) = result {
            media::stop(id);
            media::release(id);
        }
        return;
    }
    let key = atom(cx.g, &token.to_string());
    glue::rjs_delete(cx.g, requests, key);
    glue::rjs_atom_free(cx.g, key);
    let result = match result {
        OpenResult::Ok(id) => {
            let handle = media::new_handle(cx.g, id);
            let stream = track::wrap(cx, handle, cx.field(pending, "constraints"), UNDEF)
                .and_then(|t| stream_for(cx, t));
            if stream.is_err() {
                media::stop(id);
            }
            stream
        }
        OpenResult::Err(name, msg) => Err(failure(cx, name, &msg)),
    };
    cx.settle(cx.field(pending, "promise"), result);
}
media_fn!(supported(cx, _cf) throws {
    let result = new_object(cx.g, UNDEF);
    for name in [
        "deviceId",
        "groupId",
        "sampleRate",
        "sampleSize",
        "channelCount",
        "latency",
    ]
    .into_iter()
    .chain(SWITCHES)
    {
        cx.put(result, name, TRUE);
    }
    Ok(result)
});

pub(super) unsafe fn install(cx: Env, global: u64) {
    let dom = cx.field(global, "DOMException");
    let class = cx.register(ClassSpec {
        name: OVERCONSTRAINED,
        arity: 1,
        construct: Some(new_error),
        parent_proto: cx.field(dom, "prototype"),
        methods: &[],
        getters: &[("constraint", constraint)],
        accessors: &[],
    });
    set_proto(cx, class.ctor, dom);
    cx.put(
        cx.interfaces,
        "isArray",
        cx.field(cx.field(global, "Array"), "isArray"),
    );
    cx.put(cx.interfaces, "pending", cx.record());
    let devices = cx.field(global, "MediaDevices");
    if devices == UNDEF {
        return;
    }
    let proto = cx.field(devices, "prototype");
    cx.put(cx.interfaces, "devicesProto", proto);
    define(
        cx.g,
        proto,
        "getUserMedia",
        make_fn(cx.g, "getUserMedia", 1, get_user_media),
        2,
    );
    if cx.field(proto, "getSupportedConstraints") == UNDEF {
        define(
            cx.g,
            proto,
            "getSupportedConstraints",
            make_fn(cx.g, "getSupportedConstraints", 0, supported),
            2,
        );
    }
}
