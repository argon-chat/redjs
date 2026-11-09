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
//! Surface — stateless, so every entry point is a pure function of its argument:
//! ```js
//! RedCss.parseSheet(cssText)      -> jsonText  // rules, selectors, specificity, @keyframes, diags
//! RedCss.parseInline(declBody)    -> jsonText  // an inline `style` prop
//! RedCss.parseDecl(prop, value)   -> jsonText  // ONE declaration (var() substitution at runtime)
//! RedCss.props()                  -> jsonText  // the property registry
//! ```
//! Each declaration arrives with its value already resolved, so the JS side does no CSS parsing at
//! all. Whole strings cross rather than per-declaration ones: a stylesheet is parsed once at import
//! and an inline style once per change, which is the cheap direction — unlike the per-node layout
//! traffic that forced `RedYoga`'s batched scratch protocol.

use core::ffi::c_void;

use crate::glue;
use crate::webapi::util::{cf_arg, define, js_string, new_object, read_string, throw_type_error, HostFn, UNDEF};

/// `RedCss.parseSheet(css: string): string` — the compiled-sheet JSON.
#[crate::host_fn]
unsafe fn parse_sheet(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(src) = read_string(g, cf_arg(cf, 0)) else {
        return throw_type_error(g, "RedCss.parseSheet expects a string");
    };
    js_string(g, &redcss::sheet::to_json(&redcss::sheet::parse_sheet(&src)))
}

/// `RedCss.parseInline(body: string): string` — an inline `style` prop's declaration list.
#[crate::host_fn]
unsafe fn parse_inline(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(src) = read_string(g, cf_arg(cf, 0)) else {
        return throw_type_error(g, "RedCss.parseInline expects a string");
    };
    js_string(g, &redcss::sheet::decls_to_json(&redcss::sheet::parse_inline(&src)))
}

/// `RedCss.parseDecl(prop: string, value: string): string` — one declaration's patch.
///
/// Two arguments here rather than the wasm build's `prop\0value` packing: the wasm form exists to
/// avoid a second `alloc` across the linear-memory boundary, which is not a cost that exists when
/// the call is a direct one.
#[crate::host_fn]
unsafe fn parse_decl(g: *mut c_void, cf: *mut c_void) -> u64 {
    let (Some(prop), Some(value)) = (read_string(g, cf_arg(cf, 0)), read_string(g, cf_arg(cf, 1))) else {
        return throw_type_error(g, "RedCss.parseDecl expects (prop, value)");
    };
    let out = match redcss::value::parse_decl(&prop, &value) {
        redcss::value::Coverage::Patch(json) => json,
        // A knowingly-ignored property (`redcss::value::IGNORED`) parses successfully and contributes
        // nothing — an empty patch, never a rejection sentinel. Must match the wasm build's
        // `redcss_parse_decl` exactly: the two spellings of this ABI have to agree, which is the
        // whole reason redcss is one crate rather than two implementations.
        redcss::value::Coverage::Ignored => "{}".to_string(),
        redcss::value::Coverage::Invalid => "\"invalid\"".to_string(),
        redcss::value::Coverage::NotPorted => "\"notported\"".to_string(),
    };
    js_string(g, &out)
}

/// `RedCss.props(): string` — the property registry as a sorted JSON array.
#[crate::host_fn]
unsafe fn props(g: *mut c_void, _cf: *mut c_void) -> u64 {
    let mut names: Vec<&str> = redcss::value::PORTED.to_vec();
    names.sort_unstable();
    let body: Vec<String> = names.iter().map(|n| format!("\"{n}\"")).collect();
    js_string(g, &format!("[{}]", body.join(",")))
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
    def_fn(obj, "parseInline", 1, parse_inline);
    def_fn(obj, "parseDecl", 2, parse_decl);
    def_fn(obj, "props", 0, props);

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
            assert_eq!(crate::ffi::redjs_web_install(ctx.raw()), 0);
            assert_eq!(crate::ffi::redjs_webshim_install(ctx.raw(), core::ptr::null(), 0, core::ptr::null(), 0), 0);
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
        assert!(json.contains("\"important\":true"), "{json}");
        // declarations now carry a TYPED patch next to the raw value
        assert!(json.contains("\"p\":{\"color\":[1,0,0,1]}"), "{json}");
    }

    /// `@media` bodies are not dropped and do not leak as unconditional rules:
    /// they surface as ordinary rules tagged with `"conds"` indices into the
    /// sheet-level `"conditions"` table (media evaluation is the consumer's job).
    #[test]
    fn media_rule_bodies_are_condition_tagged() {
        let ctx = Context::new();
        unsafe {
            assert_eq!(crate::ffi::redjs_web_install(ctx.raw()), 0);
            assert_eq!(crate::ffi::redjs_webshim_install(ctx.raw(), core::ptr::null(), 0, core::ptr::null(), 0), 0);
        }
        let v = ctx
            .eval("RedCss.parseSheet('@media screen { .a { color: red } } .b { color: blue }')")
            .unwrap();
        let json = ctx.to_rust_string(v).unwrap();
        assert!(json.contains("\"classes\":[\"b\"]"), "{json}");
        assert!(json.contains("\"classes\":[\"a\"]"), "{json}");
        assert!(json.contains("\"conds\":[0]"), "media rule not condition-tagged: {json}");
        assert!(json.contains("\"conditions\":[{"), "conditions table missing: {json}");
    }
}
