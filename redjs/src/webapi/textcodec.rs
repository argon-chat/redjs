//! Native `TextEncoder` / `TextDecoder` (WHATWG Encoding subset: UTF-8 only,
//! which is what the engine bundle uses). Non-UTF-8 labels throw RangeError
//! per spec instead of silently mis-decoding.

use core::ffi::c_void;

use super::util::*;

// Prototypes resolve from the globals (no WebState dependency — these
// classes install before the state is published).
unsafe fn proto_of_global_class(g: *mut c_void, name: &str) -> u64 {
    get(g, get(g, crate::glue::rjs_global(g), name), "prototype")
}

unsafe extern "sysv64" fn encoder_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    ordinary_create(g, cf_new_target(cf), proto_of_global_class(g, "TextEncoder"))
}

unsafe extern "sysv64" fn get_encoding(g: *mut c_void, _cf: *mut c_void) -> u64 {
    js_string(g, "utf-8")
}

unsafe extern "sysv64" fn encode(g: *mut c_void, cf: *mut c_void) -> u64 {
    let v = cf_arg(cf, 0);
    if v == UNDEF {
        return make_u8(g, &[]);
    }
    let s = read_string(g, v).unwrap_or_default();
    make_u8(g, s.as_bytes())
}

unsafe extern "sysv64" fn encode_into(g: *mut c_void, _cf: *mut c_void) -> u64 {
    throw_type_error(g, "TextEncoder.encodeInto is not supported in the player")
}

unsafe extern "sysv64" fn decoder_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let label_v = cf_arg(cf, 0);
    let label = if label_v == UNDEF {
        "utf-8".to_string()
    } else {
        read_string(g, label_v).unwrap_or_default().trim().to_ascii_lowercase()
    };
    if !matches!(label.as_str(), "utf-8" | "utf8" | "unicode-1-1-utf-8") {
        return throw_error(g, 2, &format!("TextDecoder: unsupported encoding label \"{label}\""));
    }
    let obj = ordinary_create(g, cf_new_target(cf), proto_of_global_class(g, "TextDecoder"));
    let opts = cf_arg(cf, 1);
    let flag = |name: &str| -> u64 {
        if is_cell(opts) {
            let v = get(g, opts, name);
            enc_bool(v != UNDEF && crate::glue::rjs_to_boolean(g, v) != 0)
        } else {
            FALSE
        }
    };
    define(g, obj, "fatal", flag("fatal"), 1 | 2);
    define(g, obj, "ignoreBOM", flag("ignoreBOM"), 1 | 2);
    obj
}

unsafe extern "sysv64" fn decode(g: *mut c_void, cf: *mut c_void) -> u64 {
    let v = cf_arg(cf, 0);
    if v == UNDEF {
        return js_string(g, "");
    }
    let Some(bytes) = bufferish(g, v) else {
        return throw_type_error(g, "TextDecoder.decode: expected an ArrayBuffer or ArrayBufferView");
    };
    // Strip the BOM (spec default: ignoreBOM=false → BOM is consumed).
    let this = cf_this(cf);
    let ignore_bom = truthy_flag(get(g, this, "ignoreBOM"));
    let bytes = if !ignore_bom && bytes.starts_with(&[0xEF, 0xBB, 0xBF]) { &bytes[3..] } else { bytes };
    if truthy_flag(get(g, this, "fatal")) {
        match core::str::from_utf8(bytes) {
            Ok(s) => {
                let owned = s.to_owned();
                return js_string(g, &owned);
            }
            Err(_) => return throw_type_error(g, "TextDecoder.decode: invalid UTF-8"),
        }
    }
    let s = String::from_utf8_lossy(bytes).into_owned();
    js_string(g, &s)
}

pub unsafe fn install(g: *mut c_void) {
    define_class(g, &ClassSpec {
        name: "TextEncoder",
        arity: 0,
        construct: Some(encoder_construct),
        parent_proto: 0,
        methods: &[("encode", 1, encode), ("encodeInto", 2, encode_into)],
        getters: &[("encoding", get_encoding)],
        accessors: &[],
    });
    define_class(g, &ClassSpec {
        name: "TextDecoder",
        arity: 0,
        construct: Some(decoder_construct),
        parent_proto: 0,
        methods: &[("decode", 1, decode)],
        getters: &[("encoding", get_encoding)],
        accessors: &[],
    });
}
