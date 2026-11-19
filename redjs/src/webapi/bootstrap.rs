//! Realm globals installed after the native Web API constructors.

use super::util::*;
use crate::glue;
use core::ffi::c_void;

type JsResult = Result<u64, u64>;

unsafe fn property(g: *mut c_void, obj: u64, name: &str) -> JsResult {
    let mut value = UNDEF;
    match glue::rjs_get(g, obj, key(g, name), &mut value) {
        0 => Ok(value),
        _ => Err(value),
    }
}

unsafe fn invoke(g: *mut c_void, obj: u64, method: &str, args: &[u64]) -> JsResult {
    call(g, property(g, obj, method)?, obj, args)
}

unsafe fn string_units(g: *mut c_void, value: u64) -> Result<Vec<u16>, u64> {
    let string = if glue::rjs_kind(g, value) == 5 {
        value
    } else {
        invoke(g, state(g).global_proxy, "String", &[value])?
    };
    read_string16(g, string).ok_or(UNDEF)
}

unsafe fn is_error(g: *mut c_void, value: u64) -> Result<bool, u64> {
    let kind = glue::rjs_kind(g, value);
    if kind < 8 || kind == 9 {
        return Ok(false);
    }
    let global = state(g).global_proxy;
    let error = property(g, global, "Error")?;
    let instance = property(g, property(g, global, "Symbol")?, "hasInstance")?;
    let has_instance = invoke(g, property(g, global, "Reflect")?, "get", &[error, instance])?;
    if to_bool(g, call(g, has_instance, error, &[value])?) {
        return Ok(true);
    }
    let proto = property(g, property(g, global, "Object")?, "prototype")?;
    let tag = call(g, property(g, proto, "toString")?, value, &[])?;
    Ok(matches!(read_string(g, tag).as_deref(), Some("[object Error]" | "[object DOMException]")))
}

unsafe fn error_head(g: *mut c_void, error: u64) -> Result<Vec<u16>, u64> {
    let name = property(g, error, "name")?;
    let mut units = if to_bool(g, name) { string_units(g, name)? } else { "Error".encode_utf16().collect() };
    let message = property(g, error, "message")?;
    if to_bool(g, message) {
        units.extend_from_slice(&[b':' as u16, b' ' as u16]);
        units.extend(string_units(g, message)?);
    }
    Ok(units)
}

#[crate::host_fn]
unsafe fn json_replacer(g: *mut c_void, cf: *mut c_void) -> u64 {
    let value = cf_arg(cf, 1);
    let result = (|| -> JsResult {
        if glue::rjs_kind(g, value) == 7 {
            let mut units = string_units(g, value)?;
            units.push(b'n' as u16);
            Ok(js_string16(g, &units))
        } else if is_error(g, value)? {
            Ok(js_string16(g, &error_head(g, value)?))
        } else {
            Ok(value)
        }
    })();
    result.unwrap_or_else(|exc| {
        glue::rjs_throw(g, exc);
        UNDEF
    })
}

unsafe fn show(g: *mut c_void, value: u64, replacer: u64) -> Result<Vec<u16>, u64> {
    if glue::rjs_kind(g, value) == 5 {
        return string_units(g, value);
    }
    if is_error(g, value)? {
        let mut units = error_head(g, value)?;
        let stack = property(g, value, "stack")?;
        if to_bool(g, stack) {
            // JSC stack strings contain frames without a "Name: message" header.
            units.push(b'\n' as u16);
            units.extend(string_units(g, stack)?);
        }
        return Ok(units);
    }
    let json = property(g, state(g).global_proxy, "JSON")?;
    match invoke(g, json, "stringify", &[value, replacer]) {
        Ok(serialized) if serialized != UNDEF => string_units(g, serialized),
        _ => string_units(g, value),
    }
}

unsafe fn format(g: *mut c_void, args: &[u64], replacer: u64) -> Result<Vec<u16>, u64> {
    let mut out = Vec::new();
    let mut next = 0;
    if args.len() > 1 && glue::rjs_kind(g, args[0]) == 5 {
        next = 1;
        let format = string_units(g, args[0])?;
        let mut chars = format.iter().copied().peekable();
        while let Some(ch) = chars.next() {
            if ch != b'%' as u16 {
                out.push(ch);
                continue;
            }
            let spec = chars.peek().copied().and_then(|ch| u8::try_from(ch).ok());
            let Some(spec) = spec.filter(|c| b"sdifoOjc%".contains(c)) else {
                out.push(ch);
                continue;
            };
            chars.next();
            if spec == b'%' {
                out.push(ch);
                continue;
            }
            let Some(&value) = args.get(next) else {
                out.extend_from_slice(&[ch, spec as u16]);
                continue;
            };
            next += 1;
            match spec {
                b'c' => {}
                b'd' | b'i' | b'f' => {
                    let kind = glue::rjs_kind(g, value);
                    let parsed = if kind == 6 || (kind == 7 && spec != b'f') {
                        value
                    } else if spec == b'f' {
                        invoke(g, state(g).global_proxy, "parseFloat", &[value])?
                    } else {
                        invoke(g, state(g).global_proxy, "parseInt", &[value, enc_i32(10)])?
                    };
                    out.extend(string_units(g, parsed)?);
                }
                _ => out.extend(show(g, value, replacer)?),
            }
        }
    }
    for (index, &value) in args.iter().enumerate().skip(next) {
        if index != 0 {
            out.push(b' ' as u16);
        }
        out.extend(show(g, value, replacer)?);
    }
    Ok(out)
}

#[crate::host_fn]
unsafe fn forward(g: *mut c_void, cf: *mut c_void) -> u64 {
    let f = crate::api::HostArgs::new(cf).callee().0;
    let capture = get_a(g, f, state(g).s_console);
    let args: Vec<_> = (0..cf_argc(cf)).map(|i| cf_arg(cf, i)).collect();
    let host = get_i(g, capture, 0);
    if to_bool(g, host) {
        if let Some(method) = read_string(g, get_i(g, capture, 1)) {
            let _ = invoke(g, host, &method, &args);
        }
    } else if let Ok(units) = format(g, &args, get_i(g, capture, 3)) {
        let _ =
            invoke(g, state(g).global_proxy, "__rjs_print", &[get_i(g, capture, 2), js_string16(g, &units)]);
    }
    UNDEF
}

#[crate::host_fn]
unsafe fn console_assert(g: *mut c_void, cf: *mut c_void) -> u64 {
    if !to_bool(g, cf_arg(cf, 0)) {
        let mut args = vec![js_string(g, "Assertion failed:")];
        args.extend((1..cf_argc(cf)).map(|i| cf_arg(cf, i)));
        let result = property(g, state(g).global_proxy, "console").and_then(|c| invoke(g, c, "error", &args));
        if let Err(exc) = result {
            glue::rjs_throw(g, exc);
        }
    }
    UNDEF
}

#[crate::host_fn]
unsafe fn noop(_g: *mut c_void, _cf: *mut c_void) -> u64 {
    UNDEF
}

unsafe fn console(g: *mut c_void, global: u64) -> JsResult {
    let obj = new_object(g, UNDEF);
    let host = property(g, global, "__hostConsole")?;
    let replacer = make_fn(g, "", 2, json_replacer);
    let noop = make_fn(g, "noop", 0, noop);
    for name in [
        "log",
        "info",
        "warn",
        "error",
        "debug",
        "trace",
        "clear",
        "assert",
        "dir",
        "table",
        "group",
        "groupCollapsed",
        "groupEnd",
        "count",
        "countReset",
        "time",
        "timeEnd",
        "timeLog",
    ] {
        let method = match name {
            "assert" => make_fn(g, name, 1, console_assert),
            "groupEnd" | "count" | "countReset" | "time" | "timeEnd" | "timeLog" => noop,
            _ => {
                let target = match name {
                    "dir" | "table" | "group" | "groupCollapsed" => "log",
                    _ => name,
                };
                let level = match target {
                    "error" => 3,
                    "warn" => 2,
                    _ => 1,
                };
                let capture = new_array(g);
                for (i, value) in
                    [host, js_string(g, target), enc_i32(level), replacer].into_iter().enumerate()
                {
                    set_i(g, capture, i as u32, value);
                }
                let f = make_fn(g, "", 0, forward);
                glue::rjs_define(g, f, state(g).s_console, capture, 0);
                f
            }
        };
        define(g, obj, name, method, 0);
    }
    Ok(obj)
}

#[crate::host_fn]
unsafe fn construct_element(g: *mut c_void, cf: *mut c_void) -> u64 {
    ordinary_create(g, cf_new_target(cf), UNDEF)
}

#[crate::host_fn]
unsafe fn is_canvas(g: *mut c_void, cf: *mut c_void) -> u64 {
    let brand = get(g, cf_arg(cf, 0), "__brand");
    enc_bool(glue::rjs_strict_equals(g, brand, js_string(g, "HTMLCanvasElement")) == 1)
}

unsafe fn element(g: *mut c_void, canvas: bool) -> JsResult {
    let (name, function_name, call_hook): (_, _, HostFn) = if canvas {
        ("HTMLCanvasElement", "fn", noop)
    } else {
        ("HTMLVideoElement", "HTMLVideoElement", ctor_requires_new)
    };
    let name16: Vec<_> = function_name.encode_utf16().collect();
    let mut ctor = UNDEF;
    glue::rjs_function_create_full(
        g,
        name16.as_ptr(),
        name16.len(),
        0,
        call_hook as *const c_void,
        construct_element as *const c_void,
        &mut ctor,
    );
    let proto = new_object(g, UNDEF);
    to_string_tag(g, proto, name);
    define(g, proto, "constructor", ctor, 2);
    define(g, ctor, "prototype", proto, if canvas { 2 | 4 } else { 1 | 2 | 4 });
    if canvas {
        let symbol = glue::rjs_atom_wellknown(g, 3); // Symbol.hasInstance
        let method = make_fn(g, "value", 1, is_canvas);
        let rc = glue::rjs_define(g, ctor, symbol, method, 1 | 2 | 4);
        glue::rjs_atom_free(g, symbol);
        if rc != 0 {
            return Err(js_string(g, "Cannot install HTMLCanvasElement[Symbol.hasInstance]"));
        }
    }
    Ok(ctor)
}

unsafe fn publish(g: *mut c_void, name: &str, value: u64) -> Result<(), u64> {
    let global = state(g).global_proxy;
    let reflect = property(g, global, "Reflect")?;
    let applied = invoke(g, reflect, "set", &[global, js_string(g, name), value])?;
    if to_bool(g, applied) {
        Ok(())
    } else {
        Err(js_string(g, &format!("Cannot assign {name}")))
    }
}

pub(super) unsafe fn install(g: *mut c_void) -> Result<(), u64> {
    let global = state(g).global_proxy;
    publish(g, "console", console(g, global)?)?;
    publish(g, "HTMLCanvasElement", element(g, true)?)?;
    if glue::rjs_kind(g, property(g, global, "HTMLVideoElement")?) != 9 {
        publish(g, "HTMLVideoElement", element(g, false)?)?;
    }
    if property(g, global, "location")? == UNDEF {
        let location = new_object(g, UNDEF);
        for (name, value) in [
            ("href", "app://redjs/"),
            ("origin", "app://redjs"),
            ("protocol", "app:"),
            ("host", "redjs"),
            ("pathname", "/"),
            ("search", ""),
            ("hash", ""),
        ] {
            define(g, location, name, js_string(g, value), 0);
        }
        define(g, location, "reload", make_fn(g, "reload", 0, noop), 0);
        publish(g, "location", location)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webapi::testing::{ctx, ok, s};

    #[test]
    fn host_console_keeps_its_receiver_and_the_original_arguments() {
        let c = ctx();
        s(&c, "globalThis.original = { log(...args) { this.saved = args; }, warn() { throw Error('ignored'); } }; \
               globalThis.__hostConsole = original; 0");
        unsafe { install(c.raw()).unwrap() };
        ok(&c, "globalThis.__hostConsole = null; const arg = {}; const detached = console.table; detached(arg, 3); \
                console.warn(); original.saved[0] === arg && original.saved[1] === 3");
        ok(
            &c,
            "let assertion; console.error = (...args) => { assertion = args; }; console.assert(false, arg); \
                assertion[0] === 'Assertion failed:' && assertion[1] === arg",
        );
    }

    #[test]
    fn fallback_console_preserves_utf16_and_non_numeric_format_arguments() {
        let c = ctx();
        ok(
            &c,
            "globalThis.__rjs_print = (_level, line) => { globalThis.line = line; }; \
                console.log('%s %d %i %f %c%%', '\\ud800', Symbol('x'), 3n, '1.25x', 'css'); \
                line === '\\ud800 Symbol(x) 3 1.25 %'",
        );
        ok(&c, "let cycle = {}; cycle.self = cycle; console.log(cycle); line === '[object Object]'");
    }

    #[test]
    fn host_element_brands_and_location_keep_their_public_shape() {
        let c = ctx();
        ok(
            &c,
            "({ __brand: 'HTMLCanvasElement' }) instanceof HTMLCanvasElement \
                && !(new HTMLCanvasElement() instanceof HTMLCanvasElement) \
                && !({ get __brand() { throw Error('ignored'); } } instanceof HTMLCanvasElement) \
                && Object.prototype.toString.call(new HTMLCanvasElement()) === '[object HTMLCanvasElement]'",
        );
        ok(
            &c,
            "class Video extends HTMLVideoElement {}; new Video() instanceof Video \
                && Object.prototype.toString.call(new Video()) === '[object HTMLVideoElement]'",
        );
        ok(
            &c,
            "location.href === 'app://redjs/' && location.reload() === undefined \
                && Object.getOwnPropertyDescriptor(globalThis, 'HTMLCanvasElement').enumerable",
        );
        s(&c, "globalThis.location = { custom: true }; globalThis.HTMLVideoElement = class Existing {}; 0");
        unsafe { install(c.raw()).unwrap() };
        ok(&c, "location.custom && HTMLVideoElement.name === 'Existing'");
    }

    #[cfg(not(feature = "canvas"))]
    #[test]
    fn canvas_fallback_is_available_without_the_canvas_feature() {
        let c = ctx();
        ok(
            &c,
            "const canvas = new OffscreenCanvas(2, 3), ctx2d = canvas.getContext('2d'); \
                ctx2d.getImageData(0, 0, 2, 3).data.length === 24 && canvas.getContext('wrong') === null",
        );
    }
}
