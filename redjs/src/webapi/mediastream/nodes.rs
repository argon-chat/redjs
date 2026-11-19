use super::*;
use crate::webapi::{audio, mediastream as media};

const SOURCE: &str = "MediaStreamAudioSourceNode";
const DESTINATION: &str = "MediaStreamAudioDestinationNode";

unsafe fn context(cx: Env, obj: u64) -> JsResult {
    if !proto_chain_has(cx.g, obj, cx.field(cx.interfaces, "audioContextProto")) {
        return Err(cx.type_error("The first argument must be an AudioContext"));
    }
    Ok(obj)
}

unsafe fn node_proto(cx: Env, class: &str, target: u64) -> JsResult {
    if target != UNDEF {
        let proto = cx.prop(target, "prototype")?;
        if glue::rjs_kind(cx.g, proto) >= 8 {
            return Ok(proto);
        }
    }
    Ok(cx.field(cx.ctor(class), "prototype"))
}

unsafe fn attach(cx: Env, node: u64, stream: u64, class: &str) -> u64 {
    let state = cx.record();
    cx.put(state, "stream", stream);
    cx.put(state, "brand", cx.ctor(class));
    cx.attach(node, state);
    node
}

unsafe fn source(cx: Env, target: u64, context_obj: u64, stream: u64) -> JsResult {
    let ctx = context(cx, context_obj)?;
    let tracks = members(cx, stream)?;
    let track = (0..array_len(cx.g, tracks))
        .map(|i| get_i(cx.g, tracks, i))
        .min_by_key(|&t| read_string(cx.g, cx.field(cx.slot(t), "id")).unwrap_or_default())
        .ok_or_else(|| {
            domexception::create(cx.g, "InvalidStateError", "MediaStream has no audio track")
        })?;
    let args = [ctx, track, node_proto(cx, SOURCE, target)?];
    let node = cx.invoke(cx.field(cx.interfaces, "sourceNode"), UNDEF, &args)?;
    Ok(attach(cx, node, stream, SOURCE))
}

unsafe fn destination(cx: Env, target: u64, context_obj: u64, options: u64) -> JsResult {
    let ctx = context(cx, context_obj)?;
    let options = cx.dictionary(options)?;
    let channels = cx.prop(options, "channelCount")?;
    let count = if channels == UNDEF {
        2.
    } else {
        cx.number(channels)?
    };
    if !(1.0..=32.0).contains(&count) {
        return Err(domexception::create(
            cx.g,
            "NotSupportedError",
            &format!("channelCount {count} is outside the range 1 to 32"),
        ));
    }
    if count > 2. {
        media::advise_once(&format!("MediaStreamAudioDestinationNode: channelCount {count} is mixed to stereo; the track carries 2 channels."));
    }
    let args = [ctx, node_proto(cx, DESTINATION, target)?, enc_f64(count)];
    let pair = cx.invoke(cx.field(cx.interfaces, "destinationNode"), UNDEF, &args)?;
    let track = track::wrap(cx, get_i(cx.g, pair, 1), UNDEF, UNDEF)?;
    let stream = stream_for(cx, track)?;
    Ok(attach(cx, get_i(cx.g, pair, 0), stream, DESTINATION))
}
media_fn!(source_ctor(cx, cf) throws {
    let ctx = context(cx, cf_arg(cf, 0))?;
    let options = cx.dictionary(cf_arg(cf, 1))?;
    source(cx, cf_new_target(cf), ctx, cx.prop(options, "mediaStream")?)
});
media_fn!(dest_ctor(cx, cf) throws {
    destination(cx, cf_new_target(cf), cf_arg(cf, 0), cf_arg(cf, 1))
});
media_fn!(create_source(cx, cf) throws {
    source(cx, UNDEF, cf_this(cf), cf_arg(cf, 0))
});
media_fn!(create_dest(cx, cf) throws {
    destination(cx, UNDEF, cf_this(cf), UNDEF)
});
media_fn!(get_source(cx, cf) throws {
    Ok(cx.field(cx.data(cf_this(cf), SOURCE)?, "stream"))
});
media_fn!(get_dest(cx, cf) throws {
    Ok(cx.field(cx.data(cf_this(cf), DESTINATION)?, "stream"))
});

pub(super) unsafe fn install(cx: Env, global: u64) {
    let ctx = cx.field(global, "AudioContext");
    let node = cx.field(global, "AudioNode");
    if ctx == UNDEF || node == UNDEF {
        return;
    }
    let proto = cx.field(ctx, "prototype");
    cx.put(cx.interfaces, "audioContextProto", proto);
    for (name, func) in [
        ("sourceNode", audio::ms_source_node as HostFn),
        ("destinationNode", audio::ms_dest_node),
    ] {
        cx.put(cx.interfaces, name, make_fn(cx.g, name, 3, func));
    }
    for (name, ctor, arity, member, get) in [
        (
            SOURCE,
            source_ctor as HostFn,
            2,
            "mediaStream",
            get_source as HostFn,
        ),
        (DESTINATION, dest_ctor, 1, "stream", get_dest),
    ] {
        let class = cx.register(ClassSpec {
            name,
            arity,
            construct: Some(ctor),
            parent_proto: cx.field(node, "prototype"),
            methods: &[],
            getters: &[(member, get)],
            accessors: &[],
        });
        set_proto(cx, class.ctor, node);
    }
    for (name, arity, func) in [
        ("createMediaStreamSource", 1, create_source as HostFn),
        ("createMediaStreamDestination", 0, create_dest),
    ] {
        define(cx.g, proto, name, make_fn(cx.g, name, arity, func), 2);
    }
}
