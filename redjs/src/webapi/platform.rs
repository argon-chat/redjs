//! Native platform odds and ends: `atob`/`btoa`, `ImageData`,
//! `BroadcastChannel`.
//!
//! `BroadcastChannel` is a real `EventTarget`; `postMessage` structured-clones
//! the value and delivers a `message` event asynchronously (next pump tick)
//! to every OTHER open channel with the same name in this context — the same
//! observable semantics as same-agent channels in a browser.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::{clone, events, fsutil};
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

unsafe extern "sysv64" fn js_atob(g: *mut c_void, cf: *mut c_void) -> u64 {
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

unsafe extern "sysv64" fn js_btoa(g: *mut c_void, cf: *mut c_void) -> u64 {
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

unsafe extern "sysv64" fn imagedata_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
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
        unsafe extern "sysv64" fn $fn_name(g: *mut c_void, cf: *mut c_void) -> u64 {
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

unsafe extern "sysv64" fn imagedata_get_colorspace(g: *mut c_void, _cf: *mut c_void) -> u64 {
    js_string(g, "srgb")
}

// ---- BroadcastChannel ----------------------------------------------------------

unsafe extern "sysv64" fn bc_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
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

unsafe extern "sysv64" fn bc_get_name(g: *mut c_void, cf: *mut c_void) -> u64 {
    let slots = slots_of(g, cf_this(cf));
    if !is_cell(slots) {
        return throw_type_error(g, "BroadcastChannel.name on wrong this");
    }
    get(g, slots, "name")
}

unsafe extern "sysv64" fn bc_post_message(g: *mut c_void, cf: *mut c_void) -> u64 {
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
    let data = match clone::clone(g, cf_arg(cf, 0)) {
        Ok(v) => v,
        Err(e) => return super::domexception::throw(g, "DataCloneError", &e),
    };
    crate::ffi::redjs_protect(g, data);

    // Deliver on the next pump tick (spec: a queued task), to every OTHER
    // channel with this name that is still open.
    crate::web::schedule_native(g, 0.0, Box::new(move |g| {
        let targets: Vec<u64> = channels()
            .lock()
            .unwrap()
            .get(&(g as usize))
            .and_then(|m| m.get(&name))
            .map(|v| v.iter().copied().filter(|&t| t != this).collect())
            .unwrap_or_default();
        for t in targets {
            let ev = events::new_trusted_event(g, "message");
            define(g, ev, "data", data, 0);
            events::dispatch(g, t, ev);
        }
        crate::ffi::redjs_unprotect(g, data);
    }));
    UNDEF
}

unsafe extern "sysv64" fn bc_close(g: *mut c_void, cf: *mut c_void) -> u64 {
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
