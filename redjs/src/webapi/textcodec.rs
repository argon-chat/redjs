//! Native `TextEncoder` / `TextDecoder` (WHATWG Encoding).
//!
//! TextDecoder supports every label of the Encoding Standard via
//! `encoding_rs` (the reference implementation — it's what Firefox ships):
//! utf-8, utf-16le/be, windows-125x, koi8-r/u, shift_jis, gbk, …
//! Unknown labels throw RangeError per spec. `fatal`, `ignoreBOM` and
//! `decode(input, { stream: true })` are honored.
//!
//! TextEncoder is utf-8 only per spec, including a real `encodeInto`.

use core::ffi::c_void;
use std::sync::OnceLock;

use encoding_rs::{CoderResult, Decoder, DecoderResult, Encoding};

use super::util::*;
use crate::{capi, glue};

// Prototypes resolve from the globals (no WebState dependency — these
// classes install before the state is published).
unsafe fn proto_of_global_class(g: *mut c_void, name: &str) -> u64 {
    get(g, get(g, crate::glue::rjs_global(g), name), "prototype")
}

// ---- TextEncoder ------------------------------------------------------------

#[crate::host_fn]
unsafe fn encoder_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    ordinary_create(
        g,
        cf_new_target(cf),
        proto_of_global_class(g, "TextEncoder"),
    )
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
    if glue::rjs_typedarray_info(g, cf_arg(cf, 1), &mut ta) != 0 || ta.kind != 2 || ta.detached != 0
    {
        return throw_type_error(
            g,
            "TextEncoder.encodeInto: destination must be a Uint8Array",
        );
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

/// The decoder of one `TextDecoder`: `encoding_rs`'s stateful decoder, kept
/// between `decode(…, { stream: true })` calls and dropped by a flushing one.
struct Streaming {
    encoding: &'static Encoding,
    ignore_bom: bool,
    fatal: bool,
    decoder: Option<Decoder>,
}

impl Streaming {
    fn new(encoding: &'static Encoding, ignore_bom: bool, fatal: bool) -> Streaming {
        Streaming {
            encoding,
            ignore_bom,
            fatal,
            decoder: None,
        }
    }

    /// `None` when `fatal` and the input is malformed.
    fn decode(&mut self, input: &[u8], stream: bool) -> Option<Vec<u16>> {
        let (encoding, ignore_bom) = (self.encoding, self.ignore_bom);
        let decoder = self.decoder.get_or_insert_with(|| {
            if ignore_bom {
                encoding.new_decoder_without_bom_handling()
            } else {
                encoding.new_decoder_with_bom_removal()
            }
        });
        let last = !stream;
        let mut out = vec![
            0u16;
            decoder
                .max_utf16_buffer_length(input.len())
                .unwrap_or(input.len() * 2 + 4)
        ];
        let (mut read, mut written) = (0, 0);
        let ok = loop {
            let full = if self.fatal {
                let (result, r, w) = decoder.decode_to_utf16_without_replacement(
                    &input[read..],
                    &mut out[written..],
                    last,
                );
                read += r;
                written += w;
                match result {
                    DecoderResult::InputEmpty => break true,
                    DecoderResult::OutputFull => true,
                    DecoderResult::Malformed(..) => break false,
                }
            } else {
                let (result, r, w, _) =
                    decoder.decode_to_utf16(&input[read..], &mut out[written..], last);
                read += r;
                written += w;
                result == CoderResult::OutputFull
            };
            if !full {
                break true;
            }
            out.resize(out.len() * 2 + 4, 0);
        };
        if last || !ok {
            self.decoder = None;
        }
        ok.then(|| {
            out.truncate(written);
            out
        })
    }
}

fn decoder_class() -> capi::JSClassRef {
    static CLASS: OnceLock<usize> = OnceLock::new();
    *CLASS.get_or_init(|| unsafe { make_host_class::<Streaming>("TextDecoder") } as usize)
        as capi::JSClassRef
}

#[crate::host_fn]
unsafe fn decoder_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let label_v = cf_arg(cf, 0);
    let label = if label_v == UNDEF {
        "utf-8".to_string()
    } else {
        read_string(g, label_v).unwrap_or_default()
    };
    let Some(enc) = encoding_rs::Encoding::for_label(label.trim().as_bytes()) else {
        return throw_error(
            g,
            2,
            &format!("TextDecoder: unknown encoding label \"{label}\""),
        );
    };
    // The Encoding Standard forbids the replacement encoding for TextDecoder.
    if enc == encoding_rs::REPLACEMENT {
        return throw_error(
            g,
            2,
            &format!("TextDecoder: \"{label}\" is not a supported encoding"),
        );
    }
    let opts = cf_arg(cf, 1);
    let flag = |name: &str| -> bool { is_cell(opts) && to_bool(g, get(g, opts, name)) };
    let (fatal, ignore_bom) = (flag("fatal"), flag("ignoreBOM"));
    let new_target = cf_new_target(cf);
    let mut proto = if is_cell(new_target) {
        get(g, new_target, "prototype")
    } else {
        UNDEF
    };
    if !is_cell(proto) {
        proto = proto_of_global_class(g, "TextDecoder");
    }
    let obj = host_create(
        g,
        decoder_class(),
        proto,
        Streaming::new(enc, ignore_bom, fatal),
    );
    // Own props (readonly): the spec `encoding` getter returns the canonical
    // lowercase name of the RESOLVED encoding, not the label passed in.
    define(
        g,
        obj,
        "encoding",
        js_string(g, &enc.name().to_ascii_lowercase()),
        1 | 2,
    );
    define(g, obj, "fatal", enc_bool(fatal), 1 | 2);
    define(g, obj, "ignoreBOM", enc_bool(ignore_bom), 1 | 2);
    obj
}

#[crate::host_fn]
unsafe fn decode(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    if host_data::<Streaming>(g, decoder_class(), this).is_none() {
        return throw_type_error(
            g,
            "TextDecoder.decode called on an object that is not a TextDecoder",
        );
    }
    let opts = cf_arg(cf, 1);
    let stream = is_cell(opts) && to_bool(g, get(g, opts, "stream"));
    let v = cf_arg(cf, 0);
    let bytes: &[u8] = if v == UNDEF {
        &[]
    } else {
        match bufferish(g, v) {
            Some(bytes) => bytes,
            None => {
                return throw_type_error(
                    g,
                    "TextDecoder.decode: expected an ArrayBuffer or ArrayBufferView",
                );
            }
        }
    };
    // Borrowed only now: the `stream` getter can call decode() on the same decoder.
    let Some(state) = host_data::<Streaming>(g, decoder_class(), this) else {
        return UNDEF;
    };
    match state.decode(bytes, stream) {
        Some(units) => js_string16(g, &units),
        None => throw_type_error(
            g,
            &format!(
                "TextDecoder.decode: the input is not valid {}",
                state.encoding.name()
            ),
        ),
    }
}

#[crate::host_fn]
unsafe fn decoder_get_encoding(g: *mut c_void, cf: *mut c_void) -> u64 {
    // Own data prop shadows this getter on real instances; the getter only
    // answers for the bare prototype.
    let _ = cf;
    js_string(g, "utf-8")
}

pub unsafe fn install(g: *mut c_void) {
    define_class(
        g,
        &ClassSpec {
            name: "TextEncoder",
            arity: 0,
            construct: Some(encoder_construct),
            parent_proto: 0,
            methods: &[("encode", 1, encode), ("encodeInto", 2, encode_into)],
            getters: &[("encoding", encoder_get_encoding)],
            accessors: &[],
        },
    );
    define_class(
        g,
        &ClassSpec {
            name: "TextDecoder",
            arity: 0,
            construct: Some(decoder_construct),
            parent_proto: 0,
            methods: &[("decode", 1, decode)],
            getters: &[("encoding", decoder_get_encoding)],
            accessors: &[],
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(units: Vec<u16>) -> String {
        String::from_utf16(&units).unwrap()
    }

    fn decoder(label: &str, ignore_bom: bool, fatal: bool) -> Streaming {
        Streaming::new(
            Encoding::for_label(label.as_bytes()).unwrap(),
            ignore_bom,
            fatal,
        )
    }

    fn utf16(s: &str, little: bool) -> Vec<u8> {
        s.encode_utf16()
            .flat_map(|u| {
                if little {
                    u.to_le_bytes()
                } else {
                    u.to_be_bytes()
                }
            })
            .collect()
    }

    #[test]
    fn a_character_split_anywhere_decodes_whole_in_every_encoding() {
        let mixed = "aé€😀z";
        let (sjis, _, _) = encoding_rs::SHIFT_JIS.encode("aあ漢ｱz");
        let (gbk, _, _) = encoding_rs::GBK.encode("a汉字z");
        let cases: [(&str, Vec<u8>, &str); 5] = [
            ("utf-8", mixed.as_bytes().to_vec(), mixed),
            ("utf-16le", utf16(mixed, true), mixed),
            ("utf-16be", utf16(mixed, false), mixed),
            ("shift_jis", sjis.into_owned(), "aあ漢ｱz"),
            ("gbk", gbk.into_owned(), "a汉字z"),
        ];
        for (label, bytes, expected) in cases {
            for fatal in [false, true] {
                let mut d = decoder(label, false, fatal);
                for at in 0..=bytes.len() {
                    let mut out = text(d.decode(&bytes[..at], true).unwrap());
                    out += &text(d.decode(&bytes[at..], false).unwrap());
                    assert_eq!(out, expected, "{label} split at {at}");
                }
                let mut out = String::new();
                for byte in &bytes {
                    out += &text(d.decode(std::slice::from_ref(byte), true).unwrap());
                }
                out += &text(d.decode(&[], false).unwrap());
                assert_eq!(out, expected, "{label} a byte at a time");
            }
        }
    }

    #[test]
    fn a_truncated_tail_is_replaced_or_thrown_on_flush_and_the_decoder_starts_over() {
        let mut d = decoder("utf-8", false, false);
        assert_eq!(text(d.decode(&[0x61, 0xE2, 0x82], true).unwrap()), "a");
        assert_eq!(text(d.decode(&[], false).unwrap()), "\u{FFFD}");
        assert_eq!(text(d.decode(&[0xAC], false).unwrap()), "\u{FFFD}");

        let mut d = decoder("utf-8", false, true);
        assert_eq!(text(d.decode(&[0xE2, 0x82], true).unwrap()), "");
        assert_eq!(d.decode(&[], false), None);
        assert_eq!(text(d.decode(b"ok", false).unwrap()), "ok");

        let mut d = decoder("utf-16le", false, true);
        assert_eq!(text(d.decode(&[0x3D, 0xD8], true).unwrap()), "");
        assert_eq!(d.decode(&[], false), None);
    }

    #[test]
    fn a_bom_is_dropped_once_per_run_even_when_split() {
        let mut d = decoder("utf-8", false, false);
        assert_eq!(text(d.decode(&[0xEF], true).unwrap()), "");
        assert_eq!(text(d.decode(&[0xBB], true).unwrap()), "");
        assert_eq!(
            text(d.decode(&[0xBF, 0x61, 0xEF, 0xBB, 0xBF], false).unwrap()),
            "a\u{FEFF}"
        );
        assert_eq!(
            text(d.decode(&[0xEF, 0xBB, 0xBF, 0x62], false).unwrap()),
            "b"
        );

        let mut d = decoder("utf-16le", false, false);
        assert_eq!(text(d.decode(&[0xFF], true).unwrap()), "");
        assert_eq!(text(d.decode(&[0xFE, 0x41, 0x00], false).unwrap()), "A");

        let mut d = decoder("utf-8", true, false);
        assert_eq!(text(d.decode(&[0xEF, 0xBB], true).unwrap()), "");
        assert_eq!(text(d.decode(&[0xBF, 0x61], false).unwrap()), "\u{FEFF}a");
    }
}
