//! Native platform odds and ends: `atob`/`btoa`, `ImageData`,
//! `BroadcastChannel`.
//!
//! `BroadcastChannel` is a real `EventTarget`; `postMessage` serializes the
//! value ONCE (the structured serializer, [`super::serialize`]) and queues one
//! task per destination — every OTHER open channel of that name in this
//! context, in creation order — that deserializes its own copy and fires a
//! trusted `MessageEvent` (`messageerror` if that deserialization throws).
//! The same observable semantics as same-agent channels in a browser.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use super::{events, fsutil, serialize, websocket};
use super::util::*;
use crate::glue;

struct PlatformClasses {
    imagedata: Class,
    bc: Class,
    /// Private slot bag ({data,width,height} / {name,closed}).
    s_plat: *mut c_void,
}

unsafe impl Send for PlatformClasses {}
unsafe impl Sync for PlatformClasses {}

fn registry() -> &'static Mutex<HashMap<usize, &'static PlatformClasses>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static PlatformClasses>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> &'static PlatformClasses {
    registry().lock().unwrap().get(&(g as usize)).copied().expect("webapi::platform not installed")
}

/// Open channels per context: name → protected channel objects.
fn channels() -> &'static Mutex<HashMap<usize, HashMap<String, Vec<u64>>>> {
    static C: OnceLock<Mutex<HashMap<usize, HashMap<String, Vec<u64>>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn forget(gone: crate::context::Gone) {
    registry().lock().unwrap().remove(&gone.key);
    channels().lock().unwrap().remove(&gone.key);
}

unsafe fn slots_of(g: *mut c_void, obj: u64) -> u64 {
    get_a(g, obj, classes(g).s_plat)
}

// ---- atob / btoa -------------------------------------------------------------

#[crate::host_fn]
unsafe fn js_atob(g: *mut c_void, cf: *mut c_void) -> u64 {
    let input = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let cleaned: String = input.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    match fsutil::b64_decode(&cleaned) {
        // atob returns a latin1 string: byte i → char with code i.
        Some(bytes) => {
            let units: Vec<u16> = bytes.iter().map(|&b| b as u16).collect();
            let mut out = UNDEF;
            glue::rjs_string16(g, units.as_ptr(), units.len(), &mut out);
            out
        }
        // HTML: "throw an InvalidCharacterError DOMException".
        None => super::domexception::throw(g, "InvalidCharacterError", "atob: the string is not correctly encoded"),
    }
}

#[crate::host_fn]
unsafe fn js_btoa(g: *mut c_void, cf: *mut c_void) -> u64 {
    let input = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let mut bytes = Vec::with_capacity(input.len());
    for ch in input.chars() {
        let code = ch as u32;
        if code > 255 {
            return super::domexception::throw(
                g,
                "InvalidCharacterError",
                "btoa: the string contains characters outside of the Latin1 range",
            );
        }
        bytes.push(code as u8);
    }
    js_string(g, &fsutil::b64_encode(&bytes))
}

// ---- ImageData ----------------------------------------------------------------

#[crate::host_fn]
unsafe fn imagedata_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let a0 = cf_arg(cf, 0);
    let (data, w, h);
    if let Some(width) = dec_f64(a0) {
        // new ImageData(width, height)
        w = width.max(0.0) as usize;
        h = dec_f64(cf_arg(cf, 1)).unwrap_or(0.0).max(0.0) as usize;
        if w == 0 || h == 0 {
            return throw_type_error(g, "ImageData constructor: width and height must be non-zero");
        }
        let mut ta = UNDEF;
        if crate::ffi::redjs_typedarray_create(g, 3 /* u8 clamped */, w * h * 4, &mut ta) != 0 {
            return UNDEF;
        }
        data = ta;
    } else {
        // new ImageData(data, width[, height])
        let mut info = crate::ffi::RjsTaInfo::default();
        if glue::rjs_typedarray_info(g, a0, &mut info) != 0 || info.kind != 3 {
            return throw_type_error(g, "ImageData constructor: data must be a Uint8ClampedArray");
        }
        let len = info.length as usize;
        w = dec_f64(cf_arg(cf, 1)).unwrap_or(0.0).max(0.0) as usize;
        if w == 0 || len % 4 != 0 || (len / 4) % w != 0 {
            return throw_type_error(g, "ImageData constructor: invalid data length for the given width");
        }
        let h_arg = dec_f64(cf_arg(cf, 2));
        h = h_arg.map(|v| v.max(0.0) as usize).unwrap_or(len / 4 / w);
        if w * h * 4 != len {
            return throw_type_error(g, "ImageData constructor: data length does not match dimensions");
        }
        data = a0;
    }

    let obj = ordinary_create(g, cf_new_target(cf), c.imagedata.proto);
    let slots = new_object(g, UNDEF);
    define(g, slots, "data", data, 0);
    define(g, slots, "width", enc_i32(w as i32), 0);
    define(g, slots, "height", enc_i32(h as i32), 0);
    glue::rjs_define(g, obj, c.s_plat, slots, 0);
    obj
}

macro_rules! imagedata_getter {
    ($fn_name:ident, $slot:literal) => {
        #[crate::host_fn]
        unsafe fn $fn_name(g: *mut c_void, cf: *mut c_void) -> u64 {
            let slots = slots_of(g, cf_this(cf));
            if !is_cell(slots) {
                return throw_type_error(g, "ImageData getter on wrong this");
            }
            get(g, slots, $slot)
        }
    };
}

imagedata_getter!(imagedata_get_data, "data");
imagedata_getter!(imagedata_get_width, "width");
imagedata_getter!(imagedata_get_height, "height");

#[crate::host_fn]
unsafe fn imagedata_get_colorspace(g: *mut c_void, _cf: *mut c_void) -> u64 {
    js_string(g, "srgb")
}

/// `(data, width, height)` of an ImageData — `None` for anything else,
/// including a BroadcastChannel (same slot atom, `{name, closed}` bag).
pub(crate) unsafe fn image_data_parts(g: *mut c_void, v: u64) -> Option<(u64, u32, u32)> {
    let c = registry().lock().unwrap().get(&(g as usize)).copied()?;
    let slots = get_a(g, v, c.s_plat);
    if !is_cell(slots) {
        return None;
    }
    let w = dec_f64(get(g, slots, "width"))?;
    let h = dec_f64(get(g, slots, "height"))?;
    Some((get(g, slots, "data"), w as u32, h as u32))
}

/// An ImageData over an existing Uint8ClampedArray of exactly `w*h*4`
/// elements; `None` if `data` is not that.
pub(crate) unsafe fn create_image_data(g: *mut c_void, data: u64, w: u32, h: u32) -> Option<u64> {
    let c = registry().lock().unwrap().get(&(g as usize)).copied()?;
    let mut info = crate::ffi::RjsTaInfo::default();
    if w == 0 || h == 0 || glue::rjs_typedarray_info(g, data, &mut info) != 0 || info.kind != 3 {
        return None;
    }
    if info.length as u64 != w as u64 * h as u64 * 4 {
        return None;
    }
    let obj = new_object(g, c.imagedata.proto);
    let slots = new_object(g, UNDEF);
    define(g, slots, "data", data, 0);
    define(g, slots, "width", enc_i32(w as i32), 0);
    define(g, slots, "height", enc_i32(h as i32), 0);
    glue::rjs_define(g, obj, c.s_plat, slots, 0);
    Some(obj)
}

// ---- BroadcastChannel ----------------------------------------------------------

#[crate::host_fn]
unsafe fn bc_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let name = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let obj = ordinary_create(g, cf_new_target(cf), c.bc.proto);
    let slots = new_object(g, UNDEF);
    define(g, slots, "name", js_string(g, &name), 0);
    define(g, slots, "closed", enc_bool(false), 0);
    glue::rjs_define(g, obj, c.s_plat, slots, 0);

    crate::ffi::redjs_protect(g, obj);
    channels().lock().unwrap().entry(g as usize).or_default().entry(name).or_default().push(obj);
    obj
}

#[crate::host_fn]
unsafe fn bc_get_name(g: *mut c_void, cf: *mut c_void) -> u64 {
    let slots = slots_of(g, cf_this(cf));
    if !is_cell(slots) {
        return throw_type_error(g, "BroadcastChannel.name on wrong this");
    }
    get(g, slots, "name")
}

#[crate::host_fn]
unsafe fn bc_post_message(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let slots = slots_of(g, this);
    if !is_cell(slots) {
        return throw_type_error(g, "BroadcastChannel.postMessage on wrong this");
    }
    if get(g, slots, "closed") == 0x7 {
        // Spec: "If this's closed flag is true, then throw an InvalidStateError".
        return super::domexception::throw(g, "InvalidStateError", "BroadcastChannel.postMessage: channel is closed");
    }
    let name = read_string(g, get(g, slots, "name")).unwrap_or_default();
    // "Let serialized be StructuredSerialize(message). Rethrow any exceptions."
    // Once — every destination deserializes its own copy from these bytes.
    let serialized = match serialize::serialize(g, cf_arg(cf, 0), &serialize::Options { for_storage: false, transfer: &[] }) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            glue::rjs_throw(g, e);
            return UNDEF;
        }
    };
    let origin = source_origin(g);

    // The destinations are fixed NOW: every other open channel of this name,
    // in creation order — each gets its own queued task.
    let destinations: Vec<u64> = channels()
        .lock()
        .unwrap()
        .get(&(g as usize))
        .and_then(|m| m.get(&name))
        .map(|v| v.iter().copied().filter(|&t| t != this).collect())
        .unwrap_or_default();
    for dest in destinations {
        crate::ffi::redjs_protect(g, dest);
        let serialized = Arc::clone(&serialized);
        let origin = origin.clone();
        crate::web::schedule_native(g, 0.0, Box::new(move |g| {
            let slots = slots_of(g, dest);
            if is_cell(slots) && get(g, slots, "closed") != TRUE {
                // A deserialization that throws is a `messageerror`, not a throw.
                let ev = match serialize::deserialize_ref(g, &serialized) {
                    Ok(data) => websocket::new_message_event_of(g, "message", data, &origin),
                    Err(_) => websocket::new_message_event_of(g, "messageerror", NULL, &origin),
                };
                events::dispatch(g, dest, ev);
            }
            crate::ffi::redjs_unprotect(g, dest);
        }));
    }
    UNDEF
}

/// The serialization of the source origin for `MessageEvent.origin`:
/// `location.origin` when the global has one, else `""`.
unsafe fn source_origin(g: *mut c_void) -> String {
    let loc = get(g, glue::rjs_global(g), "location");
    if !is_cell(loc) {
        return String::new();
    }
    let o = get(g, loc, "origin");
    if glue::rjs_kind(g, o) == 5 { read_string(g, o).unwrap_or_default() } else { String::new() }
}

#[crate::host_fn]
unsafe fn bc_close(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let slots = slots_of(g, this);
    if !is_cell(slots) {
        return throw_type_error(g, "BroadcastChannel.close on wrong this");
    }
    if get(g, slots, "closed") == 0x7 {
        return UNDEF;
    }
    define(g, slots, "closed", enc_bool(true), 0);
    let name = read_string(g, get(g, slots, "name")).unwrap_or_default();
    let removed = {
        let mut reg = channels().lock().unwrap();
        match reg.get_mut(&(g as usize)).and_then(|m| m.get_mut(&name)) {
            Some(list) => {
                let before = list.len();
                list.retain(|&t| t != this);
                before != list.len()
            }
            None => false,
        }
    };
    if removed {
        crate::ffi::redjs_unprotect(g, this);
    }
    UNDEF
}

// ---- install --------------------------------------------------------------------

pub unsafe fn install(g: *mut c_void, event_target_proto: u64) {
    let s_plat = atom_priv(g, "redjs#platform");

    let global = glue::rjs_global(g);
    define(g, global, "atob", make_fn(g, "atob", 1, js_atob), 2);
    define(g, global, "btoa", make_fn(g, "btoa", 1, js_btoa), 2);

    let imagedata = define_class(g, &ClassSpec {
        name: "ImageData",
        arity: 2,
        construct: Some(imagedata_construct),
        parent_proto: 0,
        methods: &[],
        getters: &[
            ("data", imagedata_get_data),
            ("width", imagedata_get_width),
            ("height", imagedata_get_height),
            ("colorSpace", imagedata_get_colorspace),
        ],
        accessors: &[],
    });

    let bc = define_class(g, &ClassSpec {
        name: "BroadcastChannel",
        arity: 1,
        construct: Some(bc_construct),
        parent_proto: event_target_proto,
        methods: &[("postMessage", 1, bc_post_message), ("close", 0, bc_close)],
        getters: &[("name", bc_get_name)],
        accessors: &[],
    });

    let boxed: &'static PlatformClasses = Box::leak(Box::new(PlatformClasses { imagedata, bc, s_plat }));
    registry().lock().unwrap().insert(g as usize, boxed);
}

#[cfg(test)]
mod tests {
    use crate::webapi::testing::{ctx, ok, s};

    fn pump(c: &crate::api::Context) {
        let mut stats = crate::ffi::RjsPumpStats::default();
        unsafe { crate::ffi::redjs_pump(c.raw(), 7, &mut stats) };
    }

    #[test]
    fn broadcast_channel_delivers_a_fresh_clone_per_receiver() {
        let c = ctx();
        s(&c, "globalThis.got = []; const a = new BroadcastChannel('ch'), b = new BroadcastChannel('ch'), tx = new BroadcastChannel('ch'); \
               a.onmessage = e => got.push(['a', e]); b.addEventListener('message', e => got.push(['b', e])); \
               globalThis.payload = { list: [1, 2], when: new Date(3) }; tx.postMessage(payload); \
               a.close(); /* closed before its task ran: nothing for a */ 0");
        assert_eq!(s(&c, "got.length"), "0");
        pump(&c);
        ok(&c, "got.length === 1 && got[0][0] === 'b' && got[0][1] instanceof MessageEvent && got[0][1].isTrusted \
                && got[0][1].data !== payload && got[0][1].data.list[1] === 2 && got[0][1].data.when.getTime() === 3 \
                && typeof got[0][1].origin === 'string'");
    }

    #[test]
    fn broadcast_channel_post_rethrows_serialization_errors() {
        let c = ctx();
        ok(&c, "(() => { const tx = new BroadcastChannel('x'); try { tx.postMessage(() => 1); return false; } \
                catch (e) { return e instanceof DOMException && e.name === 'DataCloneError'; } })()");
        ok(&c, "(() => { const tx = new BroadcastChannel('x'); tx.close(); try { tx.postMessage(1); return false; } \
                catch (e) { return e instanceof DOMException && e.name === 'InvalidStateError'; } })()");
    }
}
