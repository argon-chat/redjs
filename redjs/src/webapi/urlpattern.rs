//! `URLPattern` over the `urlpattern` crate. Custom groups use the `regex` crate's syntax:
//! a pattern needing JS-only regex features (lookaround, backreferences) is a TypeError.

use core::ffi::c_void;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use urlpattern::quirks::{self, StringOrInit, UrlPatternInit};
use urlpattern::{UrlPattern, UrlPatternOptions};

use super::util::*;
use crate::capi;
use crate::glue;

struct Classes {
    host: capi::JSClassRef,
    pattern: Class,
}

unsafe impl Send for Classes {}
unsafe impl Sync for Classes {}

fn registry() -> &'static Mutex<HashMap<usize, &'static Classes>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static Classes>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> &'static Classes {
    registry().lock().unwrap().get(&(g as usize)).copied().expect("webapi::urlpattern not installed")
}

pub(crate) fn forget(gone: crate::context::Gone) {
    registry().lock().unwrap().remove(&gone.key);
}

const FIELDS: [&str; 8] = ["protocol", "username", "password", "hostname", "port", "pathname", "search", "hash"];

unsafe fn is_object(g: *mut c_void, v: u64) -> bool {
    super::blob::is_object(g, v)
}

unsafe fn string_member(g: *mut c_void, obj: u64, key: &str) -> Result<Option<String>, ()> {
    let v = try_get(g, obj, key)?;
    if v == UNDEF {
        return Ok(None);
    }
    Ok(Some(read_string(g, to_js_string(g, v)?).unwrap_or_default()))
}

unsafe fn read_input(g: *mut c_void, v: u64) -> Result<StringOrInit<'static>, ()> {
    if !is_object(g, v) {
        let s = if v == UNDEF { String::new() } else { read_string(g, to_js_string(g, v)?).unwrap_or_default() };
        if v == UNDEF {
            return Ok(StringOrInit::Init(UrlPatternInit::default()));
        }
        return Ok(StringOrInit::String(Cow::Owned(s)));
    }
    let mut init = UrlPatternInit::default();
    let slots = [
        &mut init.protocol,
        &mut init.username,
        &mut init.password,
        &mut init.hostname,
        &mut init.port,
        &mut init.pathname,
        &mut init.search,
        &mut init.hash,
    ];
    for (slot, key) in slots.into_iter().zip(FIELDS) {
        *slot = string_member(g, v, key)?;
    }
    init.base_url = string_member(g, v, "baseURL")?;
    Ok(StringOrInit::Init(init))
}

unsafe fn type_error(g: *mut c_void, what: &str, e: impl std::fmt::Display) -> u64 {
    throw_type_error(g, &format!("Failed to {what} 'URLPattern': {e}"))
}

#[crate::host_fn]
unsafe fn construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let (a0, a1, a2) = (cf_arg(cf, 0), cf_arg(cf, 1), cf_arg(cf, 2));
    let Ok(input) = read_input(g, a0) else { return UNDEF };
    let (base, options) = if a1 != UNDEF && !is_object(g, a1) {
        let Ok(s) = to_js_string(g, a1) else { return UNDEF };
        (read_string(g, s), a2)
    } else {
        (None, a1)
    };
    let mut opts = UrlPatternOptions::default();
    if is_object(g, options) {
        match try_get(g, options, "ignoreCase") {
            Ok(v) => opts.ignore_case = v != UNDEF && glue::rjs_to_boolean(g, v) != 0,
            Err(()) => return UNDEF,
        }
    }
    let init = match quirks::process_construct_pattern_input(input, base.as_deref()) {
        Ok(i) => i,
        Err(e) => return type_error(g, "construct", e),
    };
    let parsed: Result<UrlPattern, _> = quirks::parse_pattern_as_lib(init, opts);
    let pattern = match parsed {
        Ok(p) => p,
        Err(e) => return type_error(g, "construct", e),
    };
    let nt = cf_new_target(cf);
    let mut proto = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
    if !is_cell(proto) {
        proto = c.pattern.proto;
    }
    host_create(g, c.host, proto, pattern)
}

unsafe fn this_pattern<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut UrlPattern> {
    host_data::<UrlPattern>(g, classes(g).host, cf_this(cf))
}

macro_rules! component_getter {
    ($name:ident, $field:ident) => {
        #[crate::host_fn]
        unsafe fn $name(g: *mut c_void, cf: *mut c_void) -> u64 {
            match this_pattern(g, cf) {
                Some(p) => js_string(g, p.$field()),
                None => throw_type_error(g, "Illegal invocation"),
            }
        }
    };
}

component_getter!(get_protocol, protocol);
component_getter!(get_username, username);
component_getter!(get_password, password);
component_getter!(get_hostname, hostname);
component_getter!(get_port, port);
component_getter!(get_pathname, pathname);
component_getter!(get_search, search);
component_getter!(get_hash, hash);

#[crate::host_fn]
unsafe fn get_has_regexp_groups(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_pattern(g, cf) {
        Some(p) => enc_bool(p.has_regexp_groups()),
        None => throw_type_error(g, "Illegal invocation"),
    }
}

enum Outcome {
    Thrown,
    NoMatch,
    Matched(Box<urlpattern::UrlPatternResult>),
}

unsafe fn run(g: *mut c_void, cf: *mut c_void, what: &str) -> Outcome {
    let Some(pattern) = this_pattern(g, cf) else {
        throw_type_error(g, "Illegal invocation");
        return Outcome::Thrown;
    };
    let Ok(input) = read_input(g, cf_arg(cf, 0)) else { return Outcome::Thrown };
    let base = match cf_arg(cf, 1) {
        UNDEF => None,
        v => match to_js_string(g, v) {
            Ok(s) => read_string(g, s),
            Err(()) => return Outcome::Thrown,
        },
    };
    let matched = match quirks::process_match_input(input, base.as_deref()) {
        Ok(Some((m, _))) => m,
        Ok(None) => return Outcome::NoMatch,
        Err(e) => {
            type_error(g, what, e);
            return Outcome::Thrown;
        }
    };
    match pattern.exec(matched) {
        Ok(Some(r)) => Outcome::Matched(Box::new(r)),
        Ok(None) => Outcome::NoMatch,
        Err(e) => {
            type_error(g, what, e);
            Outcome::Thrown
        }
    }
}

#[crate::host_fn]
unsafe fn test(g: *mut c_void, cf: *mut c_void) -> u64 {
    match run(g, cf, "execute 'test' on") {
        Outcome::Thrown => UNDEF,
        Outcome::NoMatch => enc_bool(false),
        Outcome::Matched(_) => enc_bool(true),
    }
}

unsafe fn component(g: *mut c_void, r: &urlpattern::UrlPatternComponentResult) -> u64 {
    let obj = new_object(g, UNDEF);
    define(g, obj, "input", js_string(g, &r.input), 0);
    let groups = new_object(g, UNDEF);
    let mut names: Vec<_> = r.groups.keys().collect();
    names.sort_by_key(|n| n.parse::<u64>().ok().map_or((1, 0, (*n).clone()), |i| (0, i, String::new())));
    for name in names {
        let v = match &r.groups[name] {
            Some(s) => js_string(g, s),
            None => UNDEF,
        };
        define(g, groups, name, v, 0);
    }
    define(g, obj, "groups", groups, 0);
    obj
}

#[crate::host_fn]
unsafe fn exec(g: *mut c_void, cf: *mut c_void) -> u64 {
    let r = match run(g, cf, "execute 'exec' on") {
        Outcome::Thrown => return UNDEF,
        Outcome::NoMatch => return NULL,
        Outcome::Matched(r) => r,
    };
    let out = new_object(g, UNDEF);
    let inputs = new_array(g);
    set_i(g, inputs, 0, cf_arg(cf, 0));
    if cf_argc(cf) > 1 && cf_arg(cf, 1) != UNDEF {
        set_i(g, inputs, 1, cf_arg(cf, 1));
    }
    define(g, out, "inputs", inputs, 0);
    let parts = [&r.protocol, &r.username, &r.password, &r.hostname, &r.port, &r.pathname, &r.search, &r.hash];
    for (key, part) in FIELDS.into_iter().zip(parts) {
        define(g, out, key, component(g, part), 0);
    }
    out
}

pub unsafe fn install(g: *mut c_void) {
    let host = make_host_class::<UrlPattern>("URLPattern");
    let pattern = define_class(g, &ClassSpec {
        name: "URLPattern",
        arity: 0,
        construct: Some(construct),
        parent_proto: 0,
        methods: &[("test", 0, test), ("exec", 0, exec)],
        getters: &[
            ("protocol", get_protocol),
            ("username", get_username),
            ("password", get_password),
            ("hostname", get_hostname),
            ("port", get_port),
            ("pathname", get_pathname),
            ("search", get_search),
            ("hash", get_hash),
            ("hasRegExpGroups", get_has_regexp_groups),
        ],
        accessors: &[],
    });
    registry().lock().unwrap().insert(g as usize, Box::leak(Box::new(Classes { host, pattern })));
}

#[cfg(test)]
mod tests {
    use crate::webapi::testing::{ctx, ok, s};

    #[test]
    fn patterns_match_and_capture() {
        let c = ctx();
        ok(&c, "new URLPattern({ pathname: '/books/:id' }).test('https://example.com/books/123')");
        ok(&c, "!new URLPattern({ pathname: '/books/:id' }).test('https://example.com/authors/1')");
        assert_eq!(
            s(&c, "new URLPattern('https://*.example.com/books/:id(\\\\d+)').exec('https://api.example.com/books/42').pathname.groups.id"),
            "42"
        );
        assert_eq!(s(&c, "new URLPattern('/users/:name', 'https://x.test').hostname"), "x.test");
        ok(&c, "new URLPattern({ pathname: '/a/:x(\\\\d+)' }).hasRegExpGroups && !new URLPattern({ pathname: '/a/:x' }).hasRegExpGroups");
        ok(&c, "new URLPattern({ pathname: '/A' }, { ignoreCase: true }).test({ pathname: '/a' })");
        ok(&c, "new URLPattern({ pathname: '/x' }).exec({ pathname: '/y' }) === null");
        ok(&c, "Object.getPrototypeOf(new URLPattern({ pathname: '/x' }).exec({ pathname: '/x' }).pathname) === Object.prototype");
        assert_eq!(
            s(&c, "JSON.stringify(new URLPattern({ pathname: '/:a/:b?' }).exec({ pathname: '/one' }).pathname.groups)"),
            r#"{"a":"one"}"#
        );
        ok(&c, "(() => { const r = new URLPattern({ pathname: '/p' }).exec('/p', 'https://h.test'); return r.inputs.length === 2 && r.inputs[1] === 'https://h.test'; })()");
        ok(&c, "(() => { try { new URLPattern('/relative'); return false; } catch (e) { return e instanceof TypeError; } })()");
        ok(&c, "(() => { try { new URLPattern({ pathname: '/(' }); return false; } catch (e) { return e instanceof TypeError; } })()");
    }
}
