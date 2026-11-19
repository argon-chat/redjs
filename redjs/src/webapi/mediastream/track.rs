use super::*;
use crate::webapi::{mediadevices, mediastream as media};

pub(super) unsafe fn wrap(cx: Env, handle: u64, constraints: u64, from: u64) -> JsResult {
    let id = media::handle_id(cx.g, handle)
        .ok_or_else(|| cx.type_error("Expected a media track handle"))?;
    let core = media::track(id)
        .ok_or_else(|| domexception::create(cx.g, "InvalidStateError", "The track is gone"))?;
    let data = cx.record();
    let ctor = cx.ctor(TRACK);
    cx.put(data, "brand", ctor);
    cx.put(data, "id", crypto::new_uuid(cx.g));
    cx.put(data, "ended", enc_bool(core.ended()));
    cx.put(data, "enabled", enc_bool(core.enabled()));
    let hint = if from == UNDEF {
        js_string(cx.g, "")
    } else {
        cx.field(from, "contentHint")
    };
    cx.put(data, "contentHint", hint);
    cx.put(data, "label", js_string(cx.g, &core.source.label));
    cx.put(
        data,
        "constraints",
        capture::copy_constraints(cx, constraints)?,
    );
    cx.attach(handle, data);
    set_proto(cx, handle, cx.field(ctor, "prototype"));
    // TrackHandle is a JSC callback object. Its finalizer releases the native track.
    let map = state_of(cx.g).unwrap().weak_tracks;
    capi::JSWeakObjectMapSet(
        cx.g,
        map,
        id as usize as *mut c_void,
        handle as usize as capi::JSObjectRef,
    );
    Ok(handle)
}

unsafe fn id(cx: Env, obj: u64) -> Result<u64, u64> {
    cx.data(obj, TRACK)?;
    media::handle_id(cx.g, obj).ok_or_else(|| cx.type_error("Expected a media track"))
}

pub(super) unsafe fn clone_track(cx: Env, obj: u64) -> JsResult {
    let data = cx.data(obj, TRACK)?;
    let copy = media::handle_id(cx.g, obj)
        .and_then(media::clone_track)
        .ok_or_else(|| {
            domexception::create(
                cx.g,
                "InvalidStateError",
                "Cannot clone a track that is gone",
            )
        })?;
    wrap(
        cx,
        media::new_handle(cx.g, copy),
        cx.field(data, "constraints"),
        data,
    )
}

media_fn!(track_id(cx, cf) throws {
    Ok(cx.field(cx.data(cf_this(cf), TRACK)?, "id"))
});
media_fn!(label(cx, cf) throws {
    Ok(cx.field(cx.data(cf_this(cf), TRACK)?, "label"))
});
media_fn!(kind(cx, cf) throws {
    cx.data(cf_this(cf), TRACK)?;
    Ok(js_string(cx.g, "audio"))
});
media_fn!(muted(cx, cf) throws {
    cx.data(cf_this(cf), TRACK)?;
    Ok(FALSE)
});
media_fn!(ready_state(cx, cf) throws {
    let ended = cx.field(cx.data(cf_this(cf), TRACK)?, "ended") == TRUE;
    Ok(js_string(cx.g, if ended { "ended" } else { "live" }))
});
media_fn!(enabled(cx, cf) throws {
    Ok(cx.field(cx.data(cf_this(cf), TRACK)?, "enabled"))
});
media_fn!(set_enabled(cx, cf) throws {
    let obj = cf_this(cf);
    let id = id(cx, obj)?;
    let enabled = to_bool(cx.g, cf_arg(cf, 0));
    cx.put(cx.slot(obj), "enabled", enc_bool(enabled));
    media::set_enabled(id, enabled);
    Ok(UNDEF)
});
media_fn!(hint(cx, cf) throws {
    Ok(cx.field(cx.data(cf_this(cf), TRACK)?, "contentHint"))
});
media_fn!(set_hint(cx, cf) throws {
    let data = cx.data(cf_this(cf), TRACK)?;
    let value = cx.string(cf_arg(cf, 0))?;
    if matches!(
        read_string(cx.g, value).as_deref(),
        Some("" | "speech" | "speech-recognition" | "music")
    ) {
        cx.put(data, "contentHint", value);
    }
    Ok(UNDEF)
});
media_fn!(stop(cx, cf) throws {
    let obj = cf_this(cf);
    let id = id(cx, obj)?;
    let data = cx.slot(obj);
    if cx.field(data, "ended") != TRUE {
        cx.put(data, "ended", TRUE);
        media::stop(id);
    }
    Ok(UNDEF)
});
media_fn!(clone(cx, cf) throws {
    clone_track(cx, cf_this(cf))
});
media_fn!(native_id(cx, cf) throws {
    let id = media::handle_id(cx.g, cf_arg(cf, 0)).unwrap_or(0);
    Ok(enc_f64(id as f64))
});
media_fn!(constraints(cx, cf) throws {
    capture::copy_constraints(cx, cx.field(cx.data(cf_this(cf), TRACK)?, "constraints"))
});
media_fn!(apply_constraints(cx, cf) promises {
    let obj = cf_this(cf);
    let id = id(cx, obj)?;
    let constraints = cf_arg(cf, 0);
    if constraints != UNDEF
        && (glue::rjs_kind(cx.g, constraints) < 8 || glue::rjs_kind(cx.g, constraints) == 9)
    {
        return Err(cx.type_error("applyConstraints requires a MediaTrackConstraints object"));
    }
    let value = if constraints == UNDEF {
        UNDEF
    } else {
        cx.prop(constraints, "deviceId")?
    };
    let (wanted, exact) = capture::device(cx, value)?;
    if exact {
        if let Some(track) = media::track(id) {
            let device = match &track.source.kind {
                media::SourceKind::Microphone { device_id } => device_id.as_str(),
                _ => "",
            };
            if wanted.as_deref() != Some(device) {
                return Err(capture::failure(
                    cx,
                    "OverconstrainedError",
                    "Cannot switch a track to another device.",
                ));
            }
        }
    }
    media::advise_once("MediaStreamTrack.applyConstraints() records the constraints but does not reconfigure capture.");
    cx.put(
        cx.slot(obj),
        "constraints",
        capture::copy_constraints(cx, constraints)?,
    );
    Ok(cx.resolved(UNDEF))
});
media_fn!(settings(cx, cf) throws {
    let id = id(cx, cf_this(cf))?;
    let out = new_object(cx.g, UNDEF);
    let Some(core) = media::track(id) else {
        return Ok(out);
    };
    let source = &core.source;
    if let media::SourceKind::Microphone { device_id } = &source.kind {
        cx.put(out, "deviceId", js_string(cx.g, device_id));
        cx.put(out, "groupId", js_string(cx.g, ""));
        cx.put(out, "sampleRate", enc_f64(source.sample_rate() as f64));
        cx.put(out, "sampleSize", enc_i32(16));
        cx.put(out, "channelCount", enc_f64(source.channels() as f64));
        cx.put(out, "latency", enc_f64(0.01));
        cx.put(
            out,
            "echoCancellation",
            enc_bool(source.settings.echo_cancellation),
        );
        cx.put(
            out,
            "noiseSuppression",
            enc_bool(source.settings.noise_suppression),
        );
        cx.put(
            out,
            "autoGainControl",
            enc_bool(source.settings.auto_gain_control),
        );
    } else {
        cx.put(out, "channelCount", enc_f64(source.channels() as f64));
        cx.put(out, "sampleRate", enc_f64(source.sample_rate() as f64));
    }
    Ok(out)
});
media_fn!(capabilities(cx, cf) throws {
    let id = id(cx, cf_this(cf))?;
    let out = new_object(cx.g, UNDEF);
    if let Some(core) = media::track(id) {
        if let media::SourceKind::Microphone { device_id } = &core.source.kind {
            cx.put(out, "deviceId", js_string(cx.g, device_id));
            cx.put(out, "groupId", js_string(cx.g, ""));
            mediadevices::fill_capabilities(
                cx.g,
                out,
                core.source.sample_rate(),
                core.source.channels(),
                media::can_process(),
            );
        }
    }
    Ok(out)
});

pub(super) unsafe fn install(cx: Env, target: u64) {
    let class = cx.register(ClassSpec {
        name: TRACK,
        arity: 0,
        construct: None,
        parent_proto: cx.field(target, "prototype"),
        methods: &[
            ("stop", 0, stop),
            ("clone", 0, clone),
            ("getSettings", 0, settings),
            ("getConstraints", 0, constraints),
            ("getCapabilities", 0, capabilities),
            ("applyConstraints", 0, apply_constraints),
        ],
        getters: &[
            ("kind", kind),
            ("id", track_id),
            ("label", label),
            ("muted", muted),
            ("readyState", ready_state),
        ],
        accessors: &[
            ("enabled", enabled, set_enabled),
            ("contentHint", hint, set_hint),
            ("onended", on_ended, set_on_ended),
            ("onmute", on_mute, set_on_mute),
            ("onunmute", on_unmute, set_on_unmute),
        ],
    });
    set_proto(cx, class.ctor, target);
    define(
        cx.g,
        glue::rjs_global(cx.g),
        "__rjs_mediaTrackId",
        make_fn(cx.g, "__rjs_mediaTrackId", 1, native_id),
        2,
    );
}
