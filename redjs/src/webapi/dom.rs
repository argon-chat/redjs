//! `document` / `window` DOM-ish layer over the native EventTarget, plus the
//! host hooks (resize / visibility / DOMContentLoaded / element registry)
//! exported through the C ABI — no `__rp` JS seam.
//!
//! `window` IS the global object (apps read window.innerWidth and assign
//! globals through it): we define the window surface directly on globalThis;
//! its EventTarget slots live on the global object like any other target.

use core::ffi::c_void;

use super::events;
use super::util::*;
use crate::glue;

fn advise_document_cookie(write: bool) {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        eprintln!(
            "[redjs] document.cookie (cookies C-4): the page is {}, a URL with no cookies, so it \
             reads \"\" and a write is dropped ({}). Cookies here belong to fetch(…, {{ credentials: \
             \"include\" }}) and WebSocket, and script never sees them.",
            super::cookies::PAGE_ORIGIN,
            if write { "this was a write" } else { "this was a read" }
        );
    });
}

#[crate::host_fn]
unsafe fn document_cookie_get(g: *mut c_void, _cf: *mut c_void) -> u64 {
    advise_document_cookie(false);
    js_string(g, "")
}

#[crate::host_fn]
unsafe fn document_cookie_set(_g: *mut c_void, _cf: *mut c_void) -> u64 {
    advise_document_cookie(true);
    UNDEF
}

#[crate::host_fn]
unsafe fn get_element_by_id(g: *mut c_void, cf: *mut c_void) -> u64 {
    let st = state(g);
    let id = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let v = get(g, st.elements, &id);
    if is_cell(v) { v } else { NULL }
}

#[crate::host_fn]
unsafe fn query_selector(g: *mut c_void, cf: *mut c_void) -> u64 {
    let st = state(g);
    let sel = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    if let Some(id) = sel.strip_prefix('#') {
        let v = get(g, st.elements, id);
        return if is_cell(v) { v } else { NULL };
    }
    // `canvas` → the first registered canvas-branded element.
    if sel == "canvas" {
        let canvas = get(g, st.elements, "canvas");
        return if is_cell(canvas) { canvas } else { NULL };
    }
    NULL
}

#[crate::host_fn]
unsafe fn create_element(g: *mut c_void, cf: *mut c_void) -> u64 {
    let st = state(g);
    let tag = read_string(g, cf_arg(cf, 0)).unwrap_or_default().to_ascii_lowercase();
    if tag == "img" {
        // new Image()
        let image_ctor = get(g, glue::rjs_global(g), "Image");
        let mut out = UNDEF;
        if glue::rjs_construct(g, image_ctor, core::ptr::null(), 0, &mut out) == 0 {
            return out;
        }
        return NULL;
    }
    // Generic dumb element on HTMLElement.prototype (instanceof works).
    let html_proto = get(g, get(g, glue::rjs_global(g), "HTMLElement"), "prototype");
    let el = new_object(g, if is_cell(html_proto) { html_proto } else { st.event_target.proto });
    define(g, el, "tagName", js_string(g, &tag.to_ascii_uppercase()), 0);
    define(g, el, "style", new_object(g, UNDEF), 0);
    let noop = {
        let n16: Vec<u16> = "noop".encode_utf16().collect();
        let mut f = UNDEF;
        glue::rjs_function_create(g, n16.as_ptr(), n16.len(), 0, dom_noop as *const c_void, &mut f);
        f
    };
    define(g, el, "setAttribute", noop, 2);
    define(g, el, "getAttribute", noop, 2);
    define(g, el, "appendChild", noop, 2);
    define(g, el, "removeChild", noop, 2);
    el
}

#[crate::host_fn]
unsafe fn dom_noop(_g: *mut c_void, _cf: *mut c_void) -> u64 {
    NULL
}

// ---------------------------------------------------------------------------
// Element hierarchy + matchMedia + IntersectionObserver
// (app code instanceof-checks these — absence throws ReferenceError)
// ---------------------------------------------------------------------------

/// `matchMedia(query)` → a MediaQueryList-shaped EventTarget that never
/// matches (this runtime evaluates no media features).
#[crate::host_fn]
unsafe fn match_media(g: *mut c_void, cf: *mut c_void) -> u64 {
    let st = state(g);
    let query = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let mql = new_object(g, st.event_target.proto);
    define(g, mql, "matches", FALSE, 0);
    define(g, mql, "media", js_string(g, &query), 0);
    define(g, mql, "onchange", NULL, 0);
    // Legacy addListener/removeListener alias the EventTarget methods.
    let add = get(g, mql, "addEventListener");
    let rem = get(g, mql, "removeEventListener");
    define(g, mql, "addListener", add, 2);
    define(g, mql, "removeListener", rem, 2);
    mql
}

#[crate::host_fn]
unsafe fn io_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    // IntersectionObserver(callback, options?) — inert: nothing intersects in
    // this runtime; observe/unobserve/disconnect are no-ops, records are empty.
    let proto = {
        let nt = cf_new_target(cf);
        let p = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
        if is_cell(p) { p } else { get(g, get(g, glue::rjs_global(g), "IntersectionObserver"), "prototype") }
    };
    let obj = new_object(g, proto);
    define(g, obj, "root", NULL, 0);
    define(g, obj, "rootMargin", js_string(g, "0px 0px 0px 0px"), 0);
    obj
}

#[crate::host_fn]
unsafe fn io_noop(_g: *mut c_void, _cf: *mut c_void) -> u64 {
    UNDEF
}

#[crate::host_fn]
unsafe fn io_take_records(g: *mut c_void, _cf: *mut c_void) -> u64 {
    new_array(g)
}

/// Element / HTMLElement / SVGElement: real prototype chain off EventTarget so
/// `x instanceof HTMLElement` works (and is false for non-elements). Illegal
/// constructors per spec. Returns HTMLElement.prototype for createElement.
unsafe fn install_element_classes(g: *mut c_void, event_target_proto: u64) -> u64 {
    let element = define_class(g, &ClassSpec {
        name: "Element",
        arity: 0,
        construct: None,
        parent_proto: event_target_proto,
        methods: &[],
        getters: &[],
        accessors: &[],
    });
    // Node alias: some libs check `instanceof Node`.
    define(g, glue::rjs_global(g), "Node", get(g, glue::rjs_global(g), "Element"), 2);

    let html_element = define_class(g, &ClassSpec {
        name: "HTMLElement",
        arity: 0,
        construct: None,
        parent_proto: element.proto,
        methods: &[],
        getters: &[],
        accessors: &[],
    });
    define_class(g, &ClassSpec {
        name: "SVGElement",
        arity: 0,
        construct: None,
        parent_proto: element.proto,
        methods: &[],
        getters: &[],
        accessors: &[],
    });
    html_element.proto
}

pub unsafe fn install(g: *mut c_void, event_target_proto: u64) -> (u64, u64) {
    let global = glue::rjs_global(g);

    let html_proto = install_element_classes(g, event_target_proto);
    let _ = html_proto;

    // IntersectionObserver (inert) + matchMedia.
    define_class(g, &ClassSpec {
        name: "IntersectionObserver",
        arity: 1,
        construct: Some(io_construct),
        parent_proto: 0,
        methods: &[
            ("observe", 1, io_noop),
            ("unobserve", 1, io_noop),
            ("disconnect", 0, io_noop),
            ("takeRecords", 0, io_take_records),
        ],
        getters: &[],
        accessors: &[],
    });

    // elements registry (plain object, native-only).
    let elements = new_object(g, UNDEF);
    crate::ffi::redjs_protect(g, elements);

    // document: EventTarget instance with document surface.
    let document = new_object(g, event_target_proto);
    define(g, document, "readyState", js_string(g, "loading"), 0);
    define(g, document, "visibilityState", js_string(g, "visible"), 0);
    define(g, document, "hidden", FALSE, 0);
    define(g, document, "title", js_string(g, ""), 0);

    let def_fn = |target: u64, name: &str, arity: u32, f: HostFn| {
        let n16: Vec<u16> = name.encode_utf16().collect();
        let mut func = UNDEF;
        glue::rjs_function_create(g, n16.as_ptr(), n16.len(), arity, f as *const c_void, &mut func);
        define(g, target, name, func, 2);
    };
    def_fn(document, "getElementById", 1, get_element_by_id);
    def_fn(document, "querySelector", 1, query_selector);
    def_fn(document, "createElement", 1, create_element);

    // `document.cookie` (cookies.rs C-4): the document's URL is
    // `app://redpew/`, a scheme with no cookies, so — as for a browser
    // document with an opaque origin — it reads "" and a write is dropped.
    // The jar itself is never reachable from script (HttpOnly or not).
    {
        let getter = make_fn(g, "get cookie", 0, document_cookie_get);
        let setter = make_fn(g, "set cookie", 1, document_cookie_set);
        let a = atom(g, "cookie");
        glue::rjs_define_accessor(g, document, a, getter, setter, 0);
        glue::rjs_atom_free(g, a);
    }

    // body/head/documentElement: dumb elements.
    let mk_el = |tag: &str| -> u64 {
        let el = new_object(g, event_target_proto);
        define(g, el, "tagName", js_string(g, &tag.to_ascii_uppercase()), 0);
        define(g, el, "style", new_object(g, UNDEF), 0);
        el
    };
    define(g, document, "body", mk_el("body"), 0);
    define(g, document, "head", mk_el("head"), 0);
    define(g, document, "documentElement", mk_el("html"), 0);

    crate::ffi::redjs_protect(g, document);
    define(g, global, "document", document, 2);

    // window surface on globalThis. `window` must BE the globalThis proxy
    // (=== globalThis), not the JSGlobalObject.
    let global_proxy = get(g, global, "globalThis");
    define(g, global, "window", global_proxy, 2);
    define(g, global, "self", global_proxy, 2);
    define(g, global, "innerWidth", enc_i32(0), 0);
    define(g, global, "innerHeight", enc_i32(0), 0);
    define(g, global, "devicePixelRatio", enc_f64(1.0), 0);
    // add/remove/dispatchEventListener come from EventTarget.prototype methods
    // defined as bare globals (their `this` defaults to the global object).
    def_fn(global, "addEventListener", 2, events::add_event_listener);
    def_fn(global, "removeEventListener", 2, events::remove_event_listener);
    def_fn(global, "dispatchEvent", 1, events::dispatch_event);
    def_fn(global, "matchMedia", 1, match_media);

    (document, elements)
}

// ---------------------------------------------------------------------------
// Host hooks (called from the C ABI)
// ---------------------------------------------------------------------------

pub unsafe fn resize(g: *mut c_void, w: i32, h: i32, dpr: f64) {
    let global = glue::rjs_global(g);
    define(g, global, "innerWidth", enc_i32(w), 0);
    define(g, global, "innerHeight", enc_i32(h), 0);
    if dpr > 0.0 {
        define(g, global, "devicePixelRatio", enc_f64(dpr), 0);
    }
    let ev = events::new_trusted_event(g, "resize");
    events::dispatch(g, global, ev);
}

pub unsafe fn set_visibility(g: *mut c_void, hidden: bool) {
    let st = state(g);
    define(g, st.document, "hidden", enc_bool(hidden), 0);
    define(g, st.document, "visibilityState", js_string(g, if hidden { "hidden" } else { "visible" }), 0);
    let ev = events::new_trusted_event(g, "visibilitychange");
    events::dispatch(g, st.document, ev);
}

pub unsafe fn dom_content_loaded(g: *mut c_void) {
    let st = state(g);
    define(g, st.document, "readyState", js_string(g, "interactive"), 0);
    // HTML fires DOMContentLoaded with bubbles: true, so it reaches window.
    let ev = events::new_trusted_event_with(g, "DOMContentLoaded", true, false);
    events::dispatch(g, st.document, ev);
    define(g, st.document, "readyState", js_string(g, "complete"), 0);
    let ev = events::new_trusted_event(g, "load");
    events::dispatch(g, glue::rjs_global(g), ev);
}

pub unsafe fn register_element(g: *mut c_void, id: &str, value: u64) {
    let st = state(g);
    define(g, st.elements, id, value, 0);
}
