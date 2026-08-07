//! The native Web-API layer: standards-shaped classes defined through JSC's
//! own object model (native constructors, prototype methods/getters via
//! accessors, private-name internal slots, real `instanceof`).
//!
//! Native here: Event / CustomEvent / EventTarget, TextEncoder / TextDecoder,
//! Blob, URL / URLSearchParams, Headers / Request(min) / Response / fetch,
//! ImageBitmap / HTMLImageElement / Image / createImageBitmap, document /
//! window (+ host hooks). Only genuinely dumb stubs remain in the embedded
//! `bootstrap.js` (console fmt, atob/btoa, crypto, OffscreenCanvas-2D over
//! native pixels, silent Web Audio, misc instanceof brands).
//!
//! Install order: `web::install` (timers/rAF/performance) must come first —
//! image loading and bootstrap use the scheduler.

pub mod abort;
pub mod audio;
pub mod blob;
pub mod clone;
pub mod crypto;
pub mod dialogs;
pub mod dom;
pub mod events;
pub mod fetch;
pub mod formdata;
pub mod fsutil;
pub mod geometry;
pub mod hrtf;
pub mod image;
pub mod path2d;
pub mod platform;
pub mod textcodec;
pub mod uievents;
pub mod url;
pub mod util;
pub mod xmlser;

use core::ffi::c_void;

use util::*;

const BOOTSTRAP_JS: &str = include_str!("../bootstrap.js");

/// Shared harness for the web-API tests: a real JSC context with the whole
/// native layer installed — the same thing the player runs. Every assertion in
/// the modules below goes through actual JS, because the half these modules can
/// get wrong is the JSC wiring (prototype chains, `instanceof`, accessor
/// `this`), and pure-Rust unit tests cannot see any of it.
#[cfg(test)]
pub mod testing {
    use crate::api::{Context, Error};

    pub fn ctx() -> Context {
        let c = Context::new();
        unsafe {
            crate::ffi::redjs_webshim_install(c.raw(), core::ptr::null(), 0, core::ptr::null(), 0);
        }
        c
    }

    /// Evaluate; the result stringified. Panics with the JS message on throw.
    pub fn s(c: &Context, src: &str) -> String {
        match c.eval(src) {
            Ok(v) => c.to_rust_string(v).unwrap_or_else(|_| "<unstringifiable>".into()),
            Err(Error::Exception(e)) => {
                let (m, _) = c.error_info(e);
                panic!("eval threw:\n  {src}\n  → {m}")
            }
            Err(e) => panic!("eval failed:\n  {src}\n  → {e}"),
        }
    }

    /// Evaluate expecting a boolean `true`. The workhorse: a JS expression that
    /// is false fails loudly with its own source as the message.
    pub fn ok(c: &Context, src: &str) {
        let got = s(c, src);
        assert_eq!(got, "true", "expected true from: {src}");
    }

    /// The message of the exception `src` throws. Panics if it did NOT throw —
    /// which is what makes a refusal test unable to pass by accident.
    pub fn throws(c: &Context, src: &str) -> String {
        match c.eval(src) {
            Err(Error::Exception(e)) => c.error_info(e).0,
            Err(e) => panic!("expected a JS exception, got {e}: {src}"),
            Ok(v) => {
                let shown = c.to_rust_string(v).unwrap_or_else(|_| "<unstringifiable>".into());
                panic!("expected a throw from `{src}`, but it returned {shown}")
            }
        }
    }
}

unsafe extern "sysv64" fn print(g: *mut c_void, cf: *mut c_void) -> u64 {
    let level = dec_f64(cf_arg(cf, 0)).unwrap_or(0.0) as i32;
    let msg = read_string(g, cf_arg(cf, 1)).unwrap_or_default();
    match level {
        2 => eprintln!("[js:warn] {msg}"),
        3 => eprintln!("[js:error] {msg}"),
        _ => println!("[js] {msg}"),
    }
    UNDEF
}

pub unsafe fn install(g: *mut c_void, scripts_root: &str, assets_root: &str) -> i32 {
    if has_state(g) {
        return 0; // idempotent
    }
    fsutil::set_roots(g, scripts_root, assets_root);

    // Native console fallback for the bootstrap.
    {
        let n16: Vec<u16> = "__rjs_print".encode_utf16().collect();
        let mut f = UNDEF;
        crate::glue::rjs_function_create(g, n16.as_ptr(), n16.len(), 2, print as *const c_void, &mut f);
        define(g, crate::glue::rjs_global(g), "__rjs_print", f, 2);
    }

    // Private-name internal slots + hot atoms.
    let s_listeners = atom_priv(g, "redjs#listeners");
    let s_event = atom_priv(g, "redjs#event");
    let s_image = atom_priv(g, "redjs#imageBitmap");
    let s_usp = atom_priv(g, "redjs#searchParams");
    let s_iter_target = atom_priv(g, "redjs#iterTarget");
    let a_type = atom(g, "type");
    let a_cb = atom(g, "callback");
    let a_once = atom(g, "once");
    let a_handle_event = atom(g, "handleEvent");

    // JSON.parse for Response.json.
    let json_parse = {
        let global = crate::glue::rjs_global(g);
        let json = get(g, global, "JSON");
        let parse = get(g, json, "parse");
        crate::ffi::redjs_protect(g, parse);
        parse
    };

    // Publish a provisional state early: class installers and their runtime
    // callbacks resolve slots/atoms through it. Classes are patched in below
    // (single-threaded install; runtime callbacks can't run until JS runs).
    let global_proxy = get(g, crate::glue::rjs_global(g), "globalThis");
    crate::ffi::redjs_protect(g, global_proxy);

    let st = put_state(g, WebState {
        g,
        global_proxy,
        s_listeners,
        s_event,
        s_image,
        s_usp,
        s_iter_target,
        a_type,
        a_cb,
        a_once,
        a_handle_event,
        event: Class { ctor: UNDEF, proto: UNDEF },
        custom_event: Class { ctor: UNDEF, proto: UNDEF },
        event_target: Class { ctor: UNDEF, proto: UNDEF },
        blob_class: core::ptr::null_mut(),
        blob: Class { ctor: UNDEF, proto: UNDEF },
        url_class: core::ptr::null_mut(),
        url: Class { ctor: UNDEF, proto: UNDEF },
        usp_class: core::ptr::null_mut(),
        usp: Class { ctor: UNDEF, proto: UNDEF },
        usp_iter_class: core::ptr::null_mut(),
        usp_iter_proto: UNDEF,
        bitmap_class: core::ptr::null_mut(),
        bitmap: Class { ctor: UNDEF, proto: UNDEF },
        image: Class { ctor: UNDEF, proto: UNDEF },
        response_class: core::ptr::null_mut(),
        response: Class { ctor: UNDEF, proto: UNDEF },
        document: UNDEF,
        elements: UNDEF,
        json_parse,
    });
    let st_mut = st as *const WebState as *mut WebState;

    let (event, custom_event, event_target) = events::install(g);
    let ev_proto = event.proto;
    (*st_mut).event = event;
    (*st_mut).custom_event = custom_event;
    let et_proto = event_target.proto;
    (*st_mut).event_target = event_target;

    textcodec::install(g);

    let (blob_cls, blob) = blob::install(g);
    (*st_mut).blob_class = blob_cls;
    (*st_mut).blob = blob;

    let (url_cls, url, usp_cls, usp, usp_iter_cls, usp_iter_proto) = url::install(g);
    (*st_mut).url_class = url_cls;
    (*st_mut).url = url;
    (*st_mut).usp_class = usp_cls;
    (*st_mut).usp = usp;
    (*st_mut).usp_iter_class = usp_iter_cls;
    (*st_mut).usp_iter_proto = usp_iter_proto;

    let (bitmap_cls, bitmap, image) = image::install(g, et_proto);
    (*st_mut).bitmap_class = bitmap_cls;
    (*st_mut).bitmap = bitmap;
    (*st_mut).image = image;

    let (resp_cls, response) = fetch::install(g);
    (*st_mut).response_class = resp_cls;
    (*st_mut).response = response;

    let (document, elements) = dom::install(g, et_proto);
    (*st_mut).document = document;
    (*st_mut).elements = elements;

    crypto::install(g);
    formdata::install(g);
    geometry::install(g);
    path2d::install(g);
    xmlser::install(g);
    dialogs::install(g);
    uievents::install(g, ev_proto);
    clone::install(g);
    audio::install(g);
    abort::install(g, et_proto);
    platform::install(g, et_proto);
    crate::physics::install(g);
    crate::ui::install(g);
    crate::mesh::install(g);

    // The dumb-stub remainder.
    let mut out = 0u64;
    let url_name = "redjs/bootstrap.js";
    let rc = crate::glue::rjs_eval8(
        g,
        BOOTSTRAP_JS.as_ptr(),
        BOOTSTRAP_JS.len(),
        url_name.as_ptr(),
        url_name.len(),
        &mut out,
    );
    if rc == 1 {
        let (mut msg, mut stack) = (0u64, 0u64);
        crate::glue::rjs_error_info(g, out, &mut msg, &mut stack);
        let m = read_string(g, msg).unwrap_or_default();
        eprintln!("[redjs] bootstrap.js failed: {m}");
        return 1;
    }
    0
}
