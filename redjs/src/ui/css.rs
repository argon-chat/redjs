//! `RedCss` — the CSS front end (`packages/redcss`) exposed to JS as a host object.
//!
//! Same shape as the wasm build the editor and the web player load, and deliberately the same
//! *source*: `redcss` is a plain path dependency here, so the .NET player links the parser in
//! natively while the browser targets compile the identical crate to `wasm32-unknown-unknown`. One
//! implementation, three deployments — the property that made `RedYoga` safe to swap, applied to
//! the thing whose divergence would be far harder to spot (a stylesheet that parses *slightly*
//! differently changes how a shipped game looks, silently).
//!
//! No wasm is involved on this path, which is the point: removing the last wasm consumer is what
//! makes `JSC_useWasm=0` reachable (`.refactor/UI-LAYOUT-NATIVE-DESIGN.md` §3 "Measured — memory"),
//! and re-introducing a wasm CSS parser here would undo it.
//!
//! Surface — one function, because the parser is stateless:
//! ```js
//! RedCss.parseSheet(cssText) -> jsonText   // rules + selectors + specificity + diagnostics
//! ```
//! The JSON shape is `redcss::sheet::to_json`; `packages/ui` parses it once per stylesheet at
//! import. Strings cross whole rather than per-declaration: a stylesheet is parsed once, so this is
//! the cheap direction — unlike the per-node layout traffic that forced `RedYoga`'s batched
//! scratch protocol.

use core::ffi::c_void;

use crate::glue;
use crate::webapi::util::{cf_arg, define, js_string, new_object, read_string, throw_type_error, HostFn, UNDEF};

/// `RedCss.parseSheet(css: string): string` — the compiled-sheet JSON.
unsafe extern "sysv64" fn parse_sheet(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(src) = read_string(g, cf_arg(cf, 0)) else {
        return throw_type_error(g, "RedCss.parseSheet expects a string");
    };
    js_string(g, &redcss::sheet::to_json(&redcss::sheet::parse_sheet(&src)))
}

/// Installs the `RedCss` global. Called from `webapi::install` next to the other host classes.
pub unsafe fn install(g: *mut c_void) {
    let global = glue::rjs_global(g);
    let obj = new_object(g, UNDEF);

    let def_fn = |target: u64, name: &str, arity: u32, f: HostFn| {
        let n16: Vec<u16> = name.encode_utf16().collect();
        let mut func = UNDEF;
        glue::rjs_function_create(g, n16.as_ptr(), n16.len(), arity, f as *const c_void, &mut func);
        define(g, target, name, func, 2);
    };
    def_fn(obj, "parseSheet", 1, parse_sheet);

    define(g, global, "RedCss", obj, 2);
}

#[cfg(test)]
mod tests {
    use crate::api::Context;

    /// End-to-end through a real JSC context: the host object exists, takes a string, and returns
    /// the compiled-sheet JSON. This is the half the wasm build cannot prove — the editor and the
    /// web player exercise the same `redcss` code, but only here does it go through JSC.
    #[test]
    fn parse_sheet_round_trips_through_jsc() {
        let ctx = Context::new();
        unsafe {
            crate::ffi::redjs_webshim_install(ctx.raw(), core::ptr::null(), 0, core::ptr::null(), 0);
        }
        let v = ctx
            .eval("RedCss.parseSheet('.card .title:hover { color: red !important }')")
            .expect("RedCss.parseSheet threw");
        let json = ctx.to_rust_string(v).expect("not a string");

        // selector shape, real (a,b,c,d) specificity, and !important split off the value
        assert!(json.contains("\"classes\":[\"card\"]"), "{json}");
        assert!(json.contains("\"classes\":[\"title\"]"), "{json}");
        assert!(json.contains("\"pseudo\":[\"hover\"]"), "{json}");
        assert!(json.contains("\"spec\":[0,0,3,0]"), "{json}");
        assert!(json.contains("\"v\":\"red\""), "{json}");
        assert!(json.contains("\"imp\":true"), "{json}");
    }

    #[test]
    fn an_at_rule_body_does_not_leak_through_the_host_class() {
        let ctx = Context::new();
        unsafe {
            crate::ffi::redjs_webshim_install(ctx.raw(), core::ptr::null(), 0, core::ptr::null(), 0);
        }
        let v = ctx
            .eval("RedCss.parseSheet('@media screen { .a { color: red } } .b { color: blue }')")
            .unwrap();
        let json = ctx.to_rust_string(v).unwrap();
        assert!(json.contains("\"classes\":[\"b\"]"), "{json}");
        assert!(!json.contains("\"classes\":[\"a\"]"), "@media body leaked: {json}");
        assert!(json.contains("\"name\":\"media\""), "{json}");
    }
}
