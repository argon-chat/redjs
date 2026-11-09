//! Native `TextEncoder` / `TextDecoder` (WHATWG Encoding).
//!
//! TextDecoder supports every label of the Encoding Standard via
//! `encoding_rs` (the reference implementation — it's what Firefox ships):
//! utf-8, utf-16le/be, windows-125x, koi8-r/u, shift_jis, gbk, …
//! Unknown labels throw RangeError per spec. `fatal` and `ignoreBOM` are
//! honored; streaming (`{stream: true}`) is NOT — each `decode()` call is
//! treated as a complete input (the engine never chunk-decodes).
//!
//! TextEncoder is utf-8 only per spec, including a real `encodeInto`.

use core::ffi::c_void;

use super::util::*;
use crate::glue;

// Prototypes resolve from the globals (no WebState dependency — these
// classes install before the state is published).
unsafe fn proto_of_global_class(g: *mut c_void, name: &str) -> u64 {
    get(g, get(g, crate::glue::rjs_global(g), name), "prototype")
}

// ---- TextEncoder ------------------------------------------------------------

#[crate::host_fn]
unsafe fn encoder_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    ordinary_create(g, cf_new_target(cf), proto_of_global_class(g, "TextEncoder"))
}

#[crate::host_fn]
unsafe fn encoder_get_encoding(g: *mut c_void, _cf: *mut c_void) -> u64 {
    js_string(g, "utf-8")
}

#[crate::host_fn]
unsafe fn encode(g: *mut c_void, cf: *mut c_void) -> u64 {
    let v = cf_arg(cf, 0);
    if v == UNDEF {
        return make_u8(g, &[]);
    }
    let s = read_string(g, v).unwrap_or_default();
    make_u8(g, s.as_bytes())
}

/// encodeInto(source, destination) → { read, written } (spec: write as many
/// full code points as fit into the destination Uint8Array).
#[crate::host_fn]
unsafe fn encode_into(g: *mut c_void, cf: *mut c_void) -> u64 {
    let s = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let mut ta = crate::ffi::RjsTaInfo::default();
    if glue::rjs_typedarray_info(g, cf_arg(cf, 1), &mut ta) != 0 || ta.kind != 2 || ta.detached != 0 {
        return throw_type_error(g, "TextEncoder.encodeInto: destination must be a Uint8Array");
    }
    let dst = if ta.data.is_null() {
        &mut [][..]
    } else {
        core::slice::from_raw_parts_mut(ta.data as *mut u8, ta.length as usize)
    };
    let mut read = 0u64; // UTF-16 code units consumed (spec)
    let mut written = 0usize;
    for ch in s.chars() {
        let n = ch.len_utf8();
        if written + n > dst.len() {
            break;
        }
        ch.encode_utf8(&mut dst[written..written + n]);
        written += n;
        read += ch.len_utf16() as u64;
    }
    let out = new_object(g, UNDEF);
    define(g, out, "read", enc_f64(read as f64), 0);
    define(g, out, "written", enc_f64(written as f64), 0);
    out
}

// ---- TextDecoder ------------------------------------------------------------

#[crate::host_fn]
unsafe fn decoder_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let label_v = cf_arg(cf, 0);
    let label = if label_v == UNDEF { "utf-8".to_string() } else { read_string(g, label_v).unwrap_or_default() };
    let Some(enc) = encoding_rs::Encoding::for_label(label.trim().as_bytes()) else {
        return throw_error(g, 2, &format!("TextDecoder: unknown encoding label \"{label}\""));
    };
    // The Encoding Standard forbids the replacement encoding for TextDecoder.
    if enc == encoding_rs::REPLACEMENT {
        return throw_error(g, 2, &format!("TextDecoder: \"{label}\" is not a supported encoding"));
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
    // Own props (readonly): the spec `encoding` getter returns the canonical
    // lowercase name of the RESOLVED encoding, not the label passed in.
    define(g, obj, "encoding", js_string(g, &enc.name().to_ascii_lowercase()), 1 | 2);
    define(g, obj, "fatal", flag("fatal"), 1 | 2);
    define(g, obj, "ignoreBOM", flag("ignoreBOM"), 1 | 2);
    obj
}

#[crate::host_fn]
unsafe fn decode(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let name = read_string(g, get(g, this, "encoding")).unwrap_or_else(|| "utf-8".into());
    let Some(enc) = encoding_rs::Encoding::for_label(name.as_bytes()) else {
        return throw_type_error(g, "TextDecoder.decode called on wrong this");
    };

    let v = cf_arg(cf, 0);
    if v == UNDEF {
        return js_string(g, "");
    }
    let Some(bytes) = bufferish(g, v) else {
        return throw_type_error(g, "TextDecoder.decode: expected an ArrayBuffer or ArrayBufferView");
    };

    let ignore_bom = truthy_flag(get(g, this, "ignoreBOM"));
    // BOM handling per spec: consume a leading BOM of THIS encoding unless
    // ignoreBOM (never re-sniff the encoding from the BOM).
    let (text, had_errors) = if ignore_bom {
        enc.decode_without_bom_handling(bytes)
    } else {
        let (cow, had) = enc.decode_with_bom_removal(bytes);
        (cow, had)
    };
    if had_errors && truthy_flag(get(g, this, "fatal")) {
        return throw_type_error(g, &format!("TextDecoder.decode: invalid {name} input"));
    }
    let owned = text.into_owned();
    js_string(g, &owned)
}

#[crate::host_fn]
unsafe fn decoder_get_encoding(g: *mut c_void, cf: *mut c_void) -> u64 {
    // Own data prop shadows this getter on real instances; the getter only
    // answers for the bare prototype.
    let _ = cf;
    js_string(g, "utf-8")
}

pub unsafe fn install(g: *mut c_void) {
    define_class(g, &ClassSpec {
        name: "TextEncoder",
        arity: 0,
        construct: Some(encoder_construct),
        parent_proto: 0,
        methods: &[("encode", 1, encode), ("encodeInto", 2, encode_into)],
        getters: &[("encoding", encoder_get_encoding)],
        accessors: &[],
    });
    define_class(g, &ClassSpec {
        name: "TextDecoder",
        arity: 0,
        construct: Some(decoder_construct),
        parent_proto: 0,
        methods: &[("decode", 1, decode)],
        getters: &[("encoding", decoder_get_encoding)],
        accessors: &[],
    });
}
