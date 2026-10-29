//! The native Web-API layer: standards-shaped classes defined through JSC's
//! own object model (native constructors, prototype methods/getters via
//! accessors, private-name internal slots, real `instanceof`).
//!
//! ## This tree is not a compatibility shim
//!
//! `CLAUDE.md` rule 1 bans backward-compatibility shims — `compat/`, `legacy/`,
//! `*-old.ts`, deprecated re-exports. That rule is about this repo's own
//! history and does NOT apply here. Nothing in this directory bridges to dead
//! code: these are implementations of *current* web platform specifications for
//! a runtime (JavaScriptCore) that ships the language and none of the browser.
//! They are forward-looking, they are the reason application code can run on
//! the native player at all, and deleting one because the folder looks like a
//! polyfill directory would remove a working API, not a crutch.
//!
//! Native here: DOMException / QuotaExceededError (every "throw an `X`
//! DOMException" in this tree is a real one — see [`domexception`]),
//! Event / CustomEvent / EventTarget, TextEncoder / TextDecoder,
//! Blob / File / FileList (in memory or path-backed, see [`blob`] and
//! [`file`]), URL / URLSearchParams, Headers / Request / Response / fetch (over a
//! real HTTP/1.1 + TLS client, see [`http`], with an RFC 6265bis cookie store
//! behind `credentials` and the WebSocket handshake, see [`cookies`]),
//! ImageBitmap / HTMLImageElement / Image / createImageBitmap, document /
//! window (+ host hooks), UIEvent / MouseEvent / KeyboardEvent / WheelEvent /
//! FocusEvent / InputEvent / PointerEvent / DragEvent / DataTransfer,
//! `navigator` (a real Navigator whose `userAgent` and `onLine` the embedder
//! sets, see [`navigator`]), DOMPoint / DOMMatrix, Path2D, FormData, XMLSerializer, `structuredClone`
//! over a byte-level structured serializer (see [`serialize`]), the
//! Streams queueing cores and the gzip/deflate codec, and the alert / confirm /
//! prompt host hooks. Only genuinely dumb stubs remain in the embedded
//! `bootstrap.js` (console fmt, atob/btoa, crypto, OffscreenCanvas-2D over
//! native pixels, silent Web Audio, misc instanceof brands).
//!
//! IndexedDB (`indexeddb`, on by default) is its own subtree — see
//! [`indexeddb`] for its client/server shape.
//!
//! Two more surfaces are behind cargo features, because an embedder that has
//! no answer for them should not ship a global that lies: `localStorage` /
//! `sessionStorage` / `StorageEvent` (`storages`, see [`storage`]) and
//! `navigator.clipboard` (`clipboard`, see [`clipboard`]). Both read through a
//! host trait declared here, exactly as [`permissions`] does.
//!
//! One more embedded script, `streams.js`, is NOT a stub: it is the
//! promise-shaped surface of the Streams classes over the Rust state machine in
//! [`streams`]. See that module's header for what is in Rust, what stayed in JS,
//! and why.
//!
//! Install order: `web::install` (timers/rAF/performance) must come first —
//! image loading and bootstrap use the scheduler. Within this function
//! `streams::install_surface` must follow both `streams::install` and
//! `compression::install`, because `CompressionStream` wraps `RedZip`.

pub mod abort;
#[cfg(feature = "audio")]
pub mod audio;
pub mod blob;
#[cfg(feature = "clipboard")]
pub mod clipboard;
pub mod compression;
pub mod cookies;
pub mod crypto;
pub mod dialogs;
pub mod dom;
pub mod domexception;
pub mod events;
pub mod fetch;
pub mod file;
pub mod formdata;
pub mod fsutil;
pub mod geometry;
pub mod headers;
#[cfg(feature = "audio")]
pub mod hrtf;
pub mod http;
pub mod image;
#[cfg(feature = "indexeddb")]
pub mod indexeddb;
#[cfg(feature = "audio")]
pub mod mediadevices;
pub mod mime;
pub mod navigator;
#[cfg(feature = "audio")]
pub mod osc;
pub mod path2d;
pub mod permissions;
#[cfg(feature = "audio")]
pub mod pcm;
pub mod platform;
pub mod request;
pub mod serialize;
#[cfg(feature = "storages")]
pub mod storage;
pub mod streams;
pub mod textcodec;
pub mod uievents;
pub mod url;
pub mod util;
pub mod websocket;
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

/// `crate::context` fan-out: every per-context registry in this tree drops the
/// gone context's key, so a context created later at the same address installs
/// from nothing instead of being mistaken for an already-installed one. Nothing
/// here touches JS — the heap these entries pointed into no longer exists.
pub(crate) fn forget(gone: crate::context::Gone) {
    util::forget_state(gone);
    abort::forget(gone);
    blob::forget(gone);
    domexception::forget(gone);
    file::forget(gone);
    #[cfg(feature = "audio")]
    {
        audio::forget(gone);
        mediadevices::forget(gone);
    }
    compression::forget(gone);
    dialogs::forget(gone);
    fetch::forget(gone);
    formdata::forget(gone);
    fsutil::forget(gone);
    geometry::forget(gone);
    headers::forget(gone);
    #[cfg(feature = "indexeddb")]
    indexeddb::forget(gone);
    navigator::forget(gone);
    path2d::forget(gone);
    permissions::forget(gone);
    platform::forget(gone);
    serialize::forget(gone);
    #[cfg(feature = "storages")]
    storage::forget(gone);
    streams::forget(gone);
    uievents::forget(gone);
    url::forget(gone);
    websocket::forget(gone);
}

pub unsafe fn install(g: *mut c_void, scripts_root: &str, assets_root: &str) -> i32 {
    // Before the idempotence check: a recycled address whose old registrations
    // were never released is detected here, purged, and installed afresh.
    crate::context::ensure(g);
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
    let a_flags = atom(g, "flags");
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
        a_flags,
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

    // Before any class whose methods throw "an X DOMException": they resolve
    // the prototype at throw time, but nothing here may throw a plain Error
    // that a later install would have made a DOMException.
    domexception::install(g);

    textcodec::install(g);

    let (blob_cls, blob) = blob::install(g);
    (*st_mut).blob_class = blob_cls;
    (*st_mut).blob = blob;
    // File.prototype chains to Blob.prototype, so the Blob class must exist.
    file::install(g);

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
    streams::install(g);
    compression::install(g);
    // Needs RedReadableCore/RedWritableCore AND RedZip to exist first. Fatal
    // like bootstrap.js: a context without ReadableStream is not the shape
    // anything downstream is written against, and a half-installed environment
    // fails later and further away than the install does.
    if streams::install_surface(g) != 0 {
        return 1;
    }
    // `Blob.prototype.stream` builds on the ReadableStream that just appeared.
    blob::install_stream(g);
    uievents::install(g, ev_proto);
    // After streams (it captures their prototypes) and before any user
    // script: the Map/Set/Error/ArrayBuffer intrinsics it reads through are
    // captured here, so a later override of `Map.prototype.entries` cannot
    // reach into a clone.
    serialize::install(g);
    // Before mediadevices / permissions / clipboard: each of them attaches to
    // whatever `navigator` exists, and it should be the real Navigator.
    navigator::install(g);
    #[cfg(feature = "audio")]
    {
        audio::install(g);
        // After audio: `navigator.mediaDevices` only ever hands out ids that
        // `AudioContext.setSinkId` can select, so the two belong together.
        mediadevices::install(g, et_proto);
    }
    abort::install(g, et_proto);
    // `navigator.permissions` answers from whatever `PermissionHost` the
    // embedder installed (possibly none, possibly not yet) — the hook is read
    // per call, so install order here is free.
    permissions::install(g, et_proto);
    // `WebSocket` is a real EventTarget and `MessageEvent`/`CloseEvent` are
    // real `Event` subclasses, so this needs both prototypes — and therefore
    // must follow `events::install`. Nothing connects at install time; the
    // first socket is the first `new WebSocket(...)`.
    websocket::install(g, ev_proto, et_proto);
    // `navigator.clipboard` answers from whatever `ClipboardHost` the embedder
    // installed. AFTER `dom::install`, because it also lands `execCommand` on
    // `document`; the hook itself is read per call, so nothing else orders it.
    #[cfg(feature = "clipboard")]
    clipboard::install(g, et_proto);
    // `localStorage` / `sessionStorage` + `StorageEvent`, over whatever
    // `StorageHost` the embedder installed. Needs `Event.prototype`
    // (StorageEvent subclasses it) and evaluates its own small script, so it is
    // fatal like `streams::install_surface`: a context whose `localStorage` is
    // the raw un-proxied object would silently lose named-property access.
    #[cfg(feature = "storages")]
    if storage::install(g, ev_proto) != 0 {
        return 1;
    }
    // `indexedDB` & co. After `domexception` (every IDB failure is a real
    // DOMException), `serialize` (values are stored as its bytes) and
    // `navigator` (it hangs `navigator.storage` there). Nothing spawns here:
    // the server thread starts at the first open.
    #[cfg(feature = "indexeddb")]
    indexeddb::install(g, ev_proto, et_proto);
    platform::install(g, et_proto);
    #[cfg(feature = "physics")]
    crate::physics::install(g);
    crate::ui::install(g);
    #[cfg(feature = "mesh")]
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
