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
    if is_cell(v) {
        v
    } else {
        NULL
    }
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
    let tag = read_string(g, cf_arg(cf, 0))
        .unwrap_or_default()
        .to_ascii_lowercase();
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
    let el = new_object(
        g,
        if is_cell(html_proto) {
            html_proto
        } else {
            st.event_target.proto
        },
    );
    define(g, el, "tagName", js_string(g, &tag.to_ascii_uppercase()), 0);
    define(g, el, "style", new_object(g, UNDEF), 0);
    let noop = {
        let n16: Vec<u16> = "noop".encode_utf16().collect();
        let mut f = UNDEF;
        glue::rjs_function_create(
            g,
            n16.as_ptr(),
            n16.len(),
            0,
            dom_noop as *const c_void,
            &mut f,
        );
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
        let p = if is_cell(nt) {
            get(g, nt, "prototype")
        } else {
            UNDEF
        };
        if is_cell(p) {
            p
        } else {
            get(
                g,
                get(g, glue::rjs_global(g), "IntersectionObserver"),
                "prototype",
            )
        }
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
    let element = define_class(
        g,
        &ClassSpec {
            name: "Element",
            arity: 0,
            construct: None,
            parent_proto: event_target_proto,
            methods: &[],
            getters: &[],
            accessors: &[],
        },
    );
    // Node alias: some libs check `instanceof Node`.
    define(
        g,
        glue::rjs_global(g),
        "Node",
        get(g, glue::rjs_global(g), "Element"),
        2,
    );

    let html_element = define_class(
        g,
        &ClassSpec {
            name: "HTMLElement",
            arity: 0,
            construct: None,
            parent_proto: element.proto,
            methods: &[],
            getters: &[],
            accessors: &[],
        },
    );
    define_class(
        g,
        &ClassSpec {
            name: "SVGElement",
            arity: 0,
            construct: None,
            parent_proto: element.proto,
            methods: &[],
            getters: &[],
            accessors: &[],
        },
    );
    html_element.proto
}

const NODE_FILTER: &[(&str, u32)] = &[
    ("FILTER_ACCEPT", 1),
    ("FILTER_REJECT", 2),
    ("FILTER_SKIP", 3),
    ("SHOW_ALL", 0xffff_ffff),
    ("SHOW_ELEMENT", 0x1),
    ("SHOW_ATTRIBUTE", 0x2),
    ("SHOW_TEXT", 0x4),
    ("SHOW_CDATA_SECTION", 0x8),
    ("SHOW_ENTITY_REFERENCE", 0x10),
    ("SHOW_ENTITY", 0x20),
    ("SHOW_PROCESSING_INSTRUCTION", 0x40),
    ("SHOW_COMMENT", 0x80),
    ("SHOW_DOCUMENT", 0x100),
    ("SHOW_DOCUMENT_TYPE", 0x200),
    ("SHOW_DOCUMENT_FRAGMENT", 0x400),
    ("SHOW_NOTATION", 0x800),
];

pub unsafe fn install(g: *mut c_void, event_target_proto: u64) -> (u64, u64) {
    let global = glue::rjs_global(g);

    let html_proto = install_element_classes(g, event_target_proto);
    let _ = html_proto;

    let node_filter = define_class(
        g,
        &ClassSpec {
            name: "NodeFilter",
            arity: 0,
            construct: None,
            parent_proto: 0,
            methods: &[],
            getters: &[],
            accessors: &[],
        },
    );
    for (name, v) in NODE_FILTER {
        define(g, node_filter.ctor, name, enc_f64(*v as f64), 1 | 4);
    }

    // IntersectionObserver (inert) + matchMedia.
    define_class(
        g,
        &ClassSpec {
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
        },
    );

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
        glue::rjs_function_create(
            g,
            n16.as_ptr(),
            n16.len(),
            arity,
            f as *const c_void,
            &mut func,
        );
        define(g, target, name, func, 2);
    };
    def_fn(document, "getElementById", 1, get_element_by_id);
    def_fn(document, "querySelector", 1, query_selector);
    def_fn(document, "createElement", 1, create_element);

    // `document.cookie` (cookies.rs C-4): the document's URL is
    // `app://redjs/`, a scheme with no cookies, so — as for a browser
    // document with an opaque origin — it reads "" and a write is dropped.
    // The jar itself is never reachable from script (HttpOnly or not).
    {
        let getter = make_fn(g, "get cookie", 0, document_cookie_get);
        let setter = make_fn(g, "set cookie", 1, document_cookie_set);
        let a = atom(g, "cookie");
        glue::rjs_define_accessor(g, document, a, getter, setter, 0);
        glue::rjs_atom_free(g, a);
    }

    for (name, tag) in [
        ("body", "body"),
        ("head", "head"),
        ("documentElement", "html"),
    ] {
        define(
            g,
            document,
            name,
            dumb_element(g, event_target_proto, document, tag),
            0,
        );
    }

    crate::ffi::redjs_protect(g, document);
    define(g, global, "document", document, 2);

    // window surface on globalThis. `window` must BE the globalThis proxy
    // (=== globalThis), not the JSGlobalObject.
    let global_proxy = get(g, global, "globalThis");
    define(g, global, "window", global_proxy, 2);
    define(g, global, "self", global_proxy, 2);
    define(g, document, "defaultView", global_proxy, 2);
    install_viewport(g, global);
    // add/remove/dispatchEventListener come from EventTarget.prototype methods
    // defined as bare globals (their `this` defaults to the global object).
    def_fn(global, "addEventListener", 2, events::add_event_listener);
    def_fn(
        global,
        "removeEventListener",
        2,
        events::remove_event_listener,
    );
    def_fn(global, "dispatchEvent", 1, events::dispatch_event);
    def_fn(global, "matchMedia", 1, match_media);

    (document, elements)
}

// ---------------------------------------------------------------------------
// The dumb elements' members
// ---------------------------------------------------------------------------

#[crate::host_fn]
unsafe fn answer_null(_g: *mut c_void, _cf: *mut c_void) -> u64 {
    NULL
}

#[crate::host_fn]
unsafe fn answer_false(_g: *mut c_void, _cf: *mut c_void) -> u64 {
    FALSE
}

#[crate::host_fn]
unsafe fn answer_undefined(_g: *mut c_void, _cf: *mut c_void) -> u64 {
    UNDEF
}

#[crate::host_fn]
unsafe fn answer_empty(g: *mut c_void, _cf: *mut c_void) -> u64 {
    new_array(g)
}

#[crate::host_fn]
unsafe fn answer_first(_g: *mut c_void, cf: *mut c_void) -> u64 {
    cf_arg(cf, 0)
}

unsafe fn bag_key(g: *mut c_void, cf: *mut c_void) -> Option<(u64, String)> {
    let this = cf_this(cf);
    if !is_cell(this) {
        return None;
    }
    let key = to_js_string(g, cf_arg(cf, 0)).ok()?;
    Some((this, read_string(g, key)?))
}

#[crate::host_fn]
unsafe fn bag_set_property(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Some((bag, key)) = bag_key(g, cf) {
        let v = cf_arg(cf, 1);
        let text = if v == UNDEF || v == NULL {
            js_string(g, "")
        } else {
            to_js_string(g, v).unwrap_or(UNDEF)
        };
        define(g, bag, &key, text, 0);
    }
    UNDEF
}

#[crate::host_fn]
unsafe fn bag_get_property_value(g: *mut c_void, cf: *mut c_void) -> u64 {
    match bag_key(g, cf) {
        Some((bag, key)) => match get(g, bag, &key) {
            UNDEF => js_string(g, ""),
            v => v,
        },
        None => js_string(g, ""),
    }
}

#[crate::host_fn]
unsafe fn bag_remove_property(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some((bag, key)) = bag_key(g, cf) else {
        return js_string(g, "");
    };
    let prev = match get(g, bag, &key) {
        UNDEF => js_string(g, ""),
        v => v,
    };
    let a = atom(g, &key);
    glue::rjs_delete(g, bag, a);
    glue::rjs_atom_free(g, a);
    prev
}

/// `<head>`, `<body>` and `<html>` before anything replaces them: not boxes, so lookups find nothing and
/// insertions keep nothing, and `style` is a property bag with the CSSOM methods.
unsafe fn dumb_element(g: *mut c_void, proto: u64, document: u64, tag: &str) -> u64 {
    let el = new_object(g, proto);
    define(g, el, "tagName", js_string(g, &tag.to_ascii_uppercase()), 0);
    let style = new_object(g, UNDEF);
    let members: [(u64, &str, u32, HostFn); 11] = [
        (style, "setProperty", 2, bag_set_property),
        (style, "removeProperty", 1, bag_remove_property),
        (style, "getPropertyValue", 1, bag_get_property_value),
        (el, "querySelector", 1, answer_null),
        (el, "querySelectorAll", 1, answer_empty),
        (el, "contains", 1, answer_false),
        (el, "getAttribute", 1, answer_null),
        (el, "setAttribute", 2, answer_undefined),
        (el, "appendChild", 1, answer_first),
        (el, "removeChild", 1, answer_first),
        (el, "insertBefore", 2, answer_first),
    ];
    for (target, name, arity, f) in members {
        define(g, target, name, make_fn(g, name, arity, f), 2);
    }
    define(g, el, "style", style, 0);
    define(g, el, "ownerDocument", document, 2);
    el
}

// ---------------------------------------------------------------------------
// Host hooks (called from the C ABI)
// ---------------------------------------------------------------------------

pub unsafe fn resize(g: *mut c_void, w: i32, h: i32, dpr: f64) {
    VIEWPORT.with(|c| {
        let mut v = c.get();
        v.inner = (w, h);
        if dpr > 0.0 {
            v.dpr = dpr;
        }
        c.set(v);
    });
    let global = glue::rjs_global(g);
    let ev = events::new_trusted_event(g, "resize");
    events::dispatch(g, global, ev);
}

/// Publishes the viewport, the window frame and the device pixel ratio without a `resize` event;
/// `dpr` <= 0 keeps the current value.
pub fn set_viewport(inner: (i32, i32), outer: (i32, i32), dpr: f64) {
    VIEWPORT.with(|c| {
        let mut v = c.get();
        v.inner = inner;
        v.outer = Some(outer);
        if dpr > 0.0 {
            v.dpr = dpr;
        }
        c.set(v);
    });
}

pub unsafe fn set_visibility(g: *mut c_void, hidden: bool) {
    let st = state(g);
    define(g, st.document, "hidden", enc_bool(hidden), 0);
    define(
        g,
        st.document,
        "visibilityState",
        js_string(g, if hidden { "hidden" } else { "visible" }),
        0,
    );
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

// ---------------------------------------------------------------------------
// Viewport metrics
// ---------------------------------------------------------------------------

/// What the host last published: the viewport (`inner*`), the window frame (`outer*`, the viewport until
/// a host says otherwise) and the device pixel ratio.
#[derive(Clone, Copy)]
struct Viewport {
    inner: (i32, i32),
    outer: Option<(i32, i32)>,
    dpr: f64,
}

std::thread_local! {
    static VIEWPORT: std::cell::Cell<Viewport> =
        const { std::cell::Cell::new(Viewport { inner: (0, 0), outer: None, dpr: 1.0 }) };
}

macro_rules! viewport_getter {
    ($name:ident, |$v:ident| $value:expr) => {
        #[crate::host_fn]
        unsafe fn $name(_g: *mut c_void, _cf: *mut c_void) -> u64 {
            let $v = VIEWPORT.with(|c| c.get());
            $value
        }
    };
}
viewport_getter!(inner_width, |v| enc_i32(v.inner.0));
viewport_getter!(inner_height, |v| enc_i32(v.inner.1));
viewport_getter!(outer_width, |v| enc_i32(v.outer.unwrap_or(v.inner).0));
viewport_getter!(outer_height, |v| enc_i32(v.outer.unwrap_or(v.inner).1));
viewport_getter!(device_pixel_ratio, |v| enc_f64(v.dpr));
viewport_getter!(no_scroll, |_v| enc_i32(0));

/// WebIDL `[Replaceable]`: an assignment replaces the accessor with a plain data property.
macro_rules! replaceable {
    ($name:ident, $prop:literal) => {
        #[crate::host_fn]
        unsafe fn $name(g: *mut c_void, cf: *mut c_void) -> u64 {
            define(g, glue::rjs_global(g), $prop, cf_arg(cf, 0), 0);
            UNDEF
        }
    };
}
replaceable!(replace_inner_width, "innerWidth");
replaceable!(replace_inner_height, "innerHeight");
replaceable!(replace_outer_width, "outerWidth");
replaceable!(replace_outer_height, "outerHeight");
replaceable!(replace_device_pixel_ratio, "devicePixelRatio");
replaceable!(replace_scroll_x, "scrollX");
replaceable!(replace_scroll_y, "scrollY");
replaceable!(replace_page_x_offset, "pageXOffset");
replaceable!(replace_page_y_offset, "pageYOffset");

unsafe fn install_viewport(g: *mut c_void, global: u64) {
    let members: [(&str, HostFn, HostFn); 9] = [
        ("innerWidth", inner_width, replace_inner_width),
        ("innerHeight", inner_height, replace_inner_height),
        ("outerWidth", outer_width, replace_outer_width),
        ("outerHeight", outer_height, replace_outer_height),
        (
            "devicePixelRatio",
            device_pixel_ratio,
            replace_device_pixel_ratio,
        ),
        // The window itself never scrolls: the app root is the scroll container.
        ("scrollX", no_scroll, replace_scroll_x),
        ("scrollY", no_scroll, replace_scroll_y),
        ("pageXOffset", no_scroll, replace_page_x_offset),
        ("pageYOffset", no_scroll, replace_page_y_offset),
    ];
    for (name, get_fn, set_fn) in members {
        let a = atom(g, name);
        glue::rjs_define_accessor(
            g,
            global,
            a,
            make_fn(g, &format!("get {name}"), 0, get_fn),
            make_fn(g, &format!("set {name}"), 1, set_fn),
            0,
        );
        glue::rjs_atom_free(g, a);
    }
}
