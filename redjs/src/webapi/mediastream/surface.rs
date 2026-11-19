use super::{state_of, MsState, OpenResult};
use crate::capi;
use crate::webapi::{class_binding::*, crypto, domexception, events};

const STREAM: &str = "MediaStream";
const TRACK: &str = "MediaStreamTrack";
const OVERCONSTRAINED: &str = "OverconstrainedError";

unsafe fn env(g: *mut c_void) -> Env {
    let state = state_of(g).expect("MediaStream is not installed");
    Env::new(g, state.slot, state.interfaces)
}

macro_rules! media_fn {
    ($($tokens:tt)*) => { native_callback!(env, $($tokens)*); };
}

#[path = "capture.rs"]
mod capture;
#[path = "nodes.rs"]
mod nodes;
#[path = "track.rs"]
mod track;

pub(super) unsafe fn wrap(g: *mut c_void, handle: u64) -> JsResult {
    track::wrap(env(g), handle, UNDEF, UNDEF)
}

pub(super) unsafe fn settle(g: *mut c_void, token: u32, result: OpenResult) {
    capture::settle(env(g), token, result);
}

pub(super) unsafe fn ended(g: *mut c_void, id: u64) {
    let cx = env(g);
    let map = state_of(g).unwrap().weak_tracks;
    let object = capi::JSWeakObjectMapGet(g, map, id as usize as *mut c_void);
    capi::JSWeakObjectMapRemove(g, map, id as usize as *mut c_void);
    if !object.is_null() {
        let obj = object as usize as u64;
        let data = cx.slot(obj);
        if cx.field(data, "ended") != TRUE {
            cx.put(data, "ended", TRUE);
            events::dispatch(g, obj, events::new_trusted_event(g, "ended"));
        }
    }
}

unsafe fn set_proto(cx: Env, obj: u64, proto: u64) {
    capi::JSObjectSetPrototype(
        cx.g,
        obj as usize as capi::JSObjectRef,
        proto as usize as capi::JSValueRef,
    );
}

unsafe fn array_push(cx: Env, arr: u64, v: u64) {
    glue::rjs_define_index(cx.g, arr, array_len(cx.g, arr), v);
}

unsafe fn clone_array(cx: Env, arr: u64) -> u64 {
    let copy = new_array(cx.g);
    for i in 0..array_len(cx.g, arr) {
        glue::rjs_define_index(cx.g, copy, i, get_i(cx.g, arr, i));
    }
    copy
}

unsafe fn add_track_to(cx: Env, tracks: u64, t: u64) -> JsResult {
    cx.data(t, TRACK)?;
    if !(0..array_len(cx.g, tracks)).any(|i| get_i(cx.g, tracks, i) == t) {
        array_push(cx, tracks, t);
    }
    Ok(UNDEF)
}

unsafe fn make_stream(cx: Env, target: u64, tracks: u64) -> JsResult {
    let state = cx.record();
    cx.put(state, "tracks", tracks);
    cx.put(state, "id", crypto::new_uuid(cx.g));
    cx.instance(STREAM, target, state)
}

unsafe fn stream_for(cx: Env, track: u64) -> JsResult {
    let tracks = new_array(cx.g);
    array_push(cx, tracks, track);
    make_stream(cx, UNDEF, tracks)
}

media_fn!(stream_ctor(cx, cf) throws {
    let init = cf_arg(cf, 0);
    let tracks = if let Some(state) = cx.try_data(init, STREAM) {
        clone_array(cx, cx.field(state, "tracks"))
    } else {
        let tracks = new_array(cx.g);
        if init != UNDEF {
            if glue::rjs_kind(cx.g, init) < 8 {
                return Err(cx.type_error("MediaStream requires a stream or iterable of tracks"));
            }
            let method = cx.symbol(init, 0)?;
            if glue::rjs_kind(cx.g, method) != 9 {
                return Err(cx.type_error("MediaStream requires an iterable of tracks"));
            }
            let iter = cx.invoke(method, init, &[])?;
            let next = cx.method(iter, "next")?;
            if next == UNDEF {
                return Err(cx.type_error("The track iterator must provide next()"));
            }
            loop {
                let step = cx.invoke(next, iter, &[])?;
                if glue::rjs_kind(cx.g, step) < 8 {
                    return Err(cx.type_error("The track iterator must return an object"));
                }
                if to_bool(cx.g, cx.prop(step, "done")?) {
                    break;
                }
                let track = cx.prop(step, "value")?;
                if let Err(error) = add_track_to(cx, tracks, track) {
                    if let Ok(ret) = cx.method(iter, "return") {
                        let _ = cx.invoke(ret, iter, &[]);
                    }
                    return Err(error);
                }
            }
        }
        tracks
    };
    make_stream(cx, cf_new_target(cf), tracks)
});

unsafe fn members(cx: Env, obj: u64) -> JsResult {
    Ok(cx.field(cx.data(obj, STREAM)?, "tracks"))
}

media_fn!(stream_id(cx, cf) throws {
    Ok(cx.field(cx.data(cf_this(cf), STREAM)?, "id"))
});
media_fn!(active(cx, cf) throws {
    let tracks = members(cx, cf_this(cf))?;
    Ok(enc_bool((0..array_len(cx.g, tracks)).any(|i| {
        cx.field(cx.slot(get_i(cx.g, tracks, i)), "ended") != TRUE
    })))
});
media_fn!(get_tracks(cx, cf) throws {
    Ok(clone_array(cx, members(cx, cf_this(cf))?))
});
media_fn!(video_tracks(cx, cf) throws {
    cx.data(cf_this(cf), STREAM)?;
    Ok(new_array(cx.g))
});
media_fn!(by_id(cx, cf) throws {
    let tracks = members(cx, cf_this(cf))?;
    let id = cx.string(cf_arg(cf, 0))?;
    for i in 0..array_len(cx.g, tracks) {
        let track = get_i(cx.g, tracks, i);
        if glue::rjs_strict_equals(cx.g, id, cx.field(cx.slot(track), "id")) != 0 {
            return Ok(track);
        }
    }
    Ok(NULL)
});
media_fn!(add_track(cx, cf) throws {
    add_track_to(cx, members(cx, cf_this(cf))?, cf_arg(cf, 0))
});
media_fn!(remove_track(cx, cf) throws {
    let tracks = members(cx, cf_this(cf))?;
    let track = cf_arg(cf, 0);
    cx.data(track, TRACK)?;
    let n = array_len(cx.g, tracks);
    if let Some(i) = (0..n).find(|&i| get_i(cx.g, tracks, i) == track) {
        for j in i + 1..n {
            glue::rjs_define_index(cx.g, tracks, j - 1, get_i(cx.g, tracks, j));
        }
        glue::rjs_set(cx.g, tracks, key(cx.g, "length"), enc_f64((n - 1) as f64));
    }
    Ok(UNDEF)
});
media_fn!(clone_stream(cx, cf) throws {
    let tracks = members(cx, cf_this(cf))?;
    let cloned = new_array(cx.g);
    for i in 0..array_len(cx.g, tracks) {
        array_push(cx, cloned, track::clone_track(cx, get_i(cx.g, tracks, i))?);
    }
    make_stream(cx, UNDEF, cloned)
});

macro_rules! handler {
    ($get:ident, $set:ident, $class:expr, $name:literal) => {
        media_fn!($get(cx, cf) throws {
            cx.data(cf_this(cf), $class)?;
            Ok(events::ordered_handler(cx.g, cf_this(cf), $name))
        });
        media_fn!($set(cx, cf) throws {
            cx.data(cf_this(cf), $class)?;
            events::set_ordered_handler(cx.g, cf_this(cf), $name, cf_arg(cf, 0));
            Ok(UNDEF)
        });
    };
}
handler!(on_add, set_on_add, STREAM, "addtrack");
handler!(on_remove, set_on_remove, STREAM, "removetrack");
handler!(on_ended, set_on_ended, TRACK, "ended");
handler!(on_mute, set_on_mute, TRACK, "mute");
handler!(on_unmute, set_on_unmute, TRACK, "unmute");

unsafe extern "C" fn weak_map_destroyed(_: *mut c_void, _: *mut c_void) {}

pub(super) unsafe fn install(g: *mut c_void) {
    let interfaces = new_object(g, NULL);
    let slot = atom_priv(g, "redjs#mediaStream");
    let global = glue::rjs_global(g);
    glue::rjs_define(g, global, slot, interfaces, 0);
    let st = Box::leak(Box::new(MsState {
        handle_cls: make_host_class::<super::TrackHandle>("RedMediaStreamTrackHandle"),
        interfaces,
        slot,
        weak_tracks: capi::JSWeakObjectMapCreate(g, core::ptr::null_mut(), weak_map_destroyed),
        next_token: super::AtomicU32::new(1),
    }));
    super::states().lock().unwrap().insert(g as usize, st);
    let cx = env(g);
    let target = cx.field(global, "EventTarget");
    let parent = cx.field(target, "prototype");
    let stream = cx.register(ClassSpec {
        name: STREAM,
        arity: 0,
        construct: Some(stream_ctor),
        parent_proto: parent,
        methods: &[
            ("getTracks", 0, get_tracks),
            ("getAudioTracks", 0, get_tracks),
            ("getVideoTracks", 0, video_tracks),
            ("getTrackById", 1, by_id),
            ("addTrack", 1, add_track),
            ("removeTrack", 1, remove_track),
            ("clone", 0, clone_stream),
        ],
        getters: &[("id", stream_id), ("active", active)],
        accessors: &[
            ("onaddtrack", on_add, set_on_add),
            ("onremovetrack", on_remove, set_on_remove),
        ],
    });
    set_proto(cx, stream.ctor, target);
    track::install(cx, target);
    capture::install(cx, global);
    nodes::install(cx, global);
}
