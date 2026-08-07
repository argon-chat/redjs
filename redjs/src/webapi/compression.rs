//! The codec behind `CompressionStream` / `DecompressionStream` (Compression
//! Streams Standard) — `RedZip`, a streaming gzip/zlib/raw-deflate host class.
//!
//! ## The decision this file records
//!
//! These two classes are gzip and deflate. There is no way to implement them
//! honestly without a compressor, and the three options were: bring a JS
//! dependency, expose a host hook, or refuse. This takes the fourth, which is
//! the right one here — a Rust compressor crate, `flate2` on its `rust_backend`
//! (miniz_oxide): pure Rust, no C toolchain, no zlib to link, and it compiles
//! into the player like everything else in this tree.
//!
//! What is explicitly NOT here is a stub that returns its input. A passthrough
//! `CompressionStream` produces bytes that every gzip reader in the world
//! rejects, and it does so *silently*, at the far end, in whatever consumed the
//! output — which is exactly the failure mode this repo's ground rules exist to
//! prevent.
//!
//! ## Streaming shape
//!
//! `push(bytes)` returns whatever output is available so far (often empty — a
//! deflate block is not emitted until it is worth emitting) and `finish()`
//! flushes the tail plus the trailer. Truncated input on the decompress side is
//! an ERROR from `finish()`, never a short read: the spec requires the stream to
//! be errored when the compressed data ends early (§"decompress and enqueue a
//! chunk" / §"decompress flush and enqueue"), and a silent short read is a
//! truncated file that looks like a complete one.

use core::ffi::c_void;
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

use flate2::write::{DeflateEncoder, GzEncoder, ZlibEncoder};
use flate2::{Compression, Crc, Decompress, FlushDecompress, Status};

use super::util::*;
use crate::capi;

/// The three formats the Compression Streams Standard defines. Anything else is
/// a TypeError from the constructor (spec §"CompressionStream(format)").
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Format {
    /// zlib-wrapped deflate (RFC 1950).
    Deflate,
    /// bare deflate (RFC 1951).
    DeflateRaw,
    /// gzip (RFC 1952).
    Gzip,
}

impl Format {
    pub fn parse(s: &str) -> Option<Format> {
        match s {
            "deflate" => Some(Format::Deflate),
            "deflate-raw" => Some(Format::DeflateRaw),
            "gzip" => Some(Format::Gzip),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Decompression
//
// Built on the LOW-LEVEL `Decompress`, not on `flate2::write::*Decoder`.
// The writer decoders were the first implementation here and their `finish()`
// returns Ok on a TRUNCATED stream — it flushes what it managed to inflate and
// says nothing. That is the exact silent short read the spec forbids and the
// one this file's tests exist to catch (they did). `Decompress` reports
// `Status::StreamEnd`, so "did the stream actually end" is answerable.
//
// gzip framing is handled here for the same reason: `Compress::new_gzip` /
// `Decompress::new_gzip` are zlib-backend-only, and this crate is on the pure
// Rust backend.
// ---------------------------------------------------------------------------

/// Incremental gzip member header (RFC 1952 §2.3). Re-parses the accumulated
/// bytes after each byte — headers are tens of bytes, and a state machine here
/// would be more code than the thing it parses.
#[derive(Default)]
struct GzHeader {
    buf: Vec<u8>,
    done: bool,
}

impl GzHeader {
    /// Bytes consumed from `input`.
    fn feed(&mut self, input: &[u8]) -> Result<usize, String> {
        if self.done {
            return Ok(0);
        }
        let mut taken = 0;
        for &b in input {
            self.buf.push(b);
            taken += 1;
            if self.try_parse()? {
                self.done = true;
                break;
            }
        }
        Ok(taken)
    }

    /// `true` once the header is complete.
    fn try_parse(&self) -> Result<bool, String> {
        let b = &self.buf;
        if !b.is_empty() && b[0] != 0x1f {
            return Err("not a gzip stream (bad magic)".into());
        }
        if b.len() >= 2 && b[1] != 0x8b {
            return Err("not a gzip stream (bad magic)".into());
        }
        if b.len() >= 3 && b[2] != 8 {
            return Err(format!("unsupported gzip compression method {}", b[2]));
        }
        if b.len() < 10 {
            return Ok(false);
        }
        let flg = b[3];
        let mut i = 10usize;
        if flg & 0x04 != 0 {
            // FEXTRA
            if b.len() < i + 2 {
                return Ok(false);
            }
            let xlen = u16::from_le_bytes([b[i], b[i + 1]]) as usize;
            i += 2 + xlen;
            if b.len() < i {
                return Ok(false);
            }
        }
        for bit in [0x08u8, 0x10u8] {
            // FNAME, FCOMMENT — NUL-terminated
            if flg & bit != 0 {
                loop {
                    if i >= b.len() {
                        return Ok(false);
                    }
                    let c = b[i];
                    i += 1;
                    if c == 0 {
                        break;
                    }
                }
            }
        }
        if flg & 0x02 != 0 {
            // FHCRC
            if b.len() < i + 2 {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

struct Inflate {
    d: Decompress,
    ended: bool,
}

impl Inflate {
    fn new(zlib_header: bool) -> Inflate {
        Inflate { d: Decompress::new(zlib_header), ended: false }
    }

    /// Inflate as much of `input` as possible. Returns the bytes consumed; the
    /// output is appended to `out`.
    fn push(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<usize, String> {
        let mut consumed = 0usize;
        loop {
            if self.ended {
                // Trailing bytes after the stream ended belong to the caller
                // (gzip's trailer); stop rather than feeding them to inflate.
                return Ok(consumed);
            }
            let before_in = self.d.total_in();
            let before_out = self.d.total_out();
            out.reserve(8192);
            let status = self
                .d
                .decompress_vec(&input[consumed..], out, FlushDecompress::None)
                .map_err(|e| format!("the compressed data is corrupt: {e}"))?;
            consumed += (self.d.total_in() - before_in) as usize;
            let produced = self.d.total_out() - before_out;
            match status {
                Status::StreamEnd => {
                    self.ended = true;
                    return Ok(consumed);
                }
                // No progress and nothing left to give: wait for more input.
                _ if consumed >= input.len() && produced == 0 => return Ok(consumed),
                _ => {}
            }
        }
    }
}

enum Enc {
    Deflate(ZlibEncoder<Vec<u8>>),
    DeflateRaw(DeflateEncoder<Vec<u8>>),
    Gzip(GzEncoder<Vec<u8>>),
}

enum Dec {
    /// zlib and raw deflate: `Decompress` handles (or omits) the header and the
    /// adler32 check itself.
    Plain(Inflate),
    Gzip { header: GzHeader, inflate: Inflate, crc: Crc, trailer: Vec<u8> },
}

enum Codec {
    Encode(Enc),
    Decode(Dec),
}

pub struct ZipData {
    codec: Option<Codec>,
    /// Set once `finish` has run; a second `push` after that is a programming
    /// error rather than silently-dropped data.
    done: bool,
}

impl ZipData {
    pub fn new(format: Format, decompress: bool) -> ZipData {
        let level = Compression::default();
        let sink = Vec::new();
        let codec = if decompress {
            Codec::Decode(match format {
                Format::Deflate => Dec::Plain(Inflate::new(true)),
                Format::DeflateRaw => Dec::Plain(Inflate::new(false)),
                Format::Gzip => Dec::Gzip {
                    header: GzHeader::default(),
                    inflate: Inflate::new(false),
                    crc: Crc::new(),
                    trailer: Vec::new(),
                },
            })
        } else {
            Codec::Encode(match format {
                Format::Deflate => Enc::Deflate(ZlibEncoder::new(sink, level)),
                Format::DeflateRaw => Enc::DeflateRaw(DeflateEncoder::new(sink, level)),
                Format::Gzip => Enc::Gzip(GzEncoder::new(sink, level)),
            })
        };
        ZipData { codec: Some(codec), done: false }
    }

    /// Feed bytes; returns whatever output is ready. `Err` carries a message
    /// for the TypeError that errors the stream.
    pub fn push(&mut self, input: &[u8]) -> Result<Vec<u8>, String> {
        if self.done {
            return Err("the stream is already finished".into());
        }
        let Some(codec) = self.codec.as_mut() else {
            return Err("the codec is gone".into());
        };
        match codec {
            Codec::Encode(enc) => {
                let sink = match enc {
                    Enc::Deflate(c) => {
                        c.write_all(input).map_err(|e| format!("{e}"))?;
                        core::mem::take(c.get_mut())
                    }
                    Enc::DeflateRaw(c) => {
                        c.write_all(input).map_err(|e| format!("{e}"))?;
                        core::mem::take(c.get_mut())
                    }
                    Enc::Gzip(c) => {
                        c.write_all(input).map_err(|e| format!("{e}"))?;
                        core::mem::take(c.get_mut())
                    }
                };
                Ok(sink)
            }
            Codec::Decode(Dec::Plain(inf)) => {
                let mut out = Vec::new();
                inf.push(input, &mut out)?;
                Ok(out)
            }
            Codec::Decode(Dec::Gzip { header, inflate, crc, trailer }) => {
                let mut rest = input;
                let taken = header.feed(rest)?;
                rest = &rest[taken..];
                if !header.done {
                    return Ok(Vec::new());
                }
                let mut out = Vec::new();
                let consumed = inflate.push(rest, &mut out)?;
                crc.update(&out);
                // Anything past the deflate stream is the 8-byte trailer.
                trailer.extend_from_slice(&rest[consumed..]);
                Ok(out)
            }
        }
    }

    /// Flush the tail. On the decompress side an incomplete stream errors here
    /// rather than returning what it managed to read.
    pub fn finish(&mut self) -> Result<Vec<u8>, String> {
        if self.done {
            return Err("the stream is already finished".into());
        }
        self.done = true;
        let Some(codec) = self.codec.take() else {
            return Err("the codec is gone".into());
        };
        match codec {
            Codec::Encode(Enc::Deflate(c)) => c.finish().map_err(|e| format!("{e}")),
            Codec::Encode(Enc::DeflateRaw(c)) => c.finish().map_err(|e| format!("{e}")),
            Codec::Encode(Enc::Gzip(c)) => c.finish().map_err(|e| format!("{e}")),
            Codec::Decode(Dec::Plain(inf)) => {
                if !inf.ended {
                    return Err(TRUNCATED.into());
                }
                Ok(Vec::new())
            }
            Codec::Decode(Dec::Gzip { header, inflate, crc, trailer }) => {
                if !header.done || !inflate.ended {
                    return Err(TRUNCATED.into());
                }
                if trailer.len() < 8 {
                    return Err(format!("{TRUNCATED} (the gzip trailer is missing)"));
                }
                let want_crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
                let want_len = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
                if want_crc != crc.sum() {
                    return Err("the gzip checksum does not match: the data is corrupt".into());
                }
                if want_len != (crc.amount() & 0xffff_ffff) {
                    return Err("the gzip length does not match: the data is corrupt".into());
                }
                Ok(Vec::new())
            }
        }
    }
}

const TRUNCATED: &str =
    "the compressed data ends before the stream is complete: it is truncated or corrupt";

// ---------------------------------------------------------------------------
// The JS host class
// ---------------------------------------------------------------------------

struct ZipClasses {
    cls: capi::JSClassRef,
    class: Class,
}
unsafe impl Send for ZipClasses {}
unsafe impl Sync for ZipClasses {}

fn registry() -> &'static Mutex<HashMap<usize, &'static ZipClasses>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static ZipClasses>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> &'static ZipClasses {
    registry().lock().unwrap().get(&(g as usize)).copied().expect("webapi::compression not installed")
}

unsafe fn this_zip<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut ZipData> {
    host_data::<ZipData>(g, classes(g).cls, cf_this(cf))
}

unsafe extern "sysv64" fn construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let name = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let Some(format) = Format::parse(&name) else {
        return throw_type_error(
            g,
            &format!(
                "Unsupported compression format \"{name}\" — the Compression Streams Standard \
                 defines exactly \"deflate\", \"deflate-raw\" and \"gzip\"."
            ),
        );
    };
    let decompress = truthy_flag(cf_arg(cf, 1));
    let proto = {
        let nt = cf_new_target(cf);
        let p = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
        if is_cell(p) { p } else { c.class.proto }
    };
    host_create(g, c.cls, proto, ZipData::new(format, decompress))
}

unsafe extern "sysv64" fn zip_push(g: *mut c_void, cf: *mut c_void) -> u64 {
    let arg = cf_arg(cf, 0);
    let Some(input) = bufferish(g, arg) else {
        return throw_type_error(
            g,
            "CompressionStream/DecompressionStream can only transform BufferSource chunks \
             (ArrayBuffer or a typed-array view).",
        );
    };
    // The slice points into JS memory and `make_u8` allocates, so copy first.
    let input = input.to_vec();
    let Some(z) = this_zip(g, cf) else {
        return throw_type_error(g, "RedZip.push called on a non-RedZip");
    };
    match z.push(&input) {
        Ok(out) => make_u8(g, &out),
        Err(e) => throw_type_error(g, &e),
    }
}

unsafe extern "sysv64" fn zip_finish(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(z) = this_zip(g, cf) else {
        return throw_type_error(g, "RedZip.finish called on a non-RedZip");
    };
    match z.finish() {
        Ok(out) => make_u8(g, &out),
        Err(e) => throw_type_error(g, &e),
    }
}

pub unsafe fn install(g: *mut c_void) {
    let cls = make_host_class::<ZipData>("RedZip");
    let class = define_class(g, &ClassSpec {
        name: "RedZip",
        arity: 2,
        construct: Some(construct),
        parent_proto: 0,
        methods: &[("push", 1, zip_push), ("finish", 0, zip_finish)],
        getters: &[],
        accessors: &[],
    });
    let boxed: &'static ZipClasses = Box::leak(Box::new(ZipClasses { cls, class }));
    registry().lock().unwrap().insert(g as usize, boxed);
}

// ---------------------------------------------------------------------------
// Tests — the codec, no JS context needed
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(format: Format, data: &[u8], chunk: usize) -> Vec<u8> {
        let mut enc = ZipData::new(format, false);
        let mut compressed = Vec::new();
        for part in data.chunks(chunk.max(1)) {
            compressed.extend_from_slice(&enc.push(part).unwrap());
        }
        compressed.extend_from_slice(&enc.finish().unwrap());

        let mut dec = ZipData::new(format, true);
        let mut out = Vec::new();
        for part in compressed.chunks(chunk.max(1)) {
            out.extend_from_slice(&dec.push(part).unwrap());
        }
        out.extend_from_slice(&dec.finish().unwrap());
        out
    }

    fn corpus() -> Vec<u8> {
        // Compressible, but with enough variety that a broken window still shows.
        let mut v = Vec::new();
        for i in 0..4000u32 {
            v.extend_from_slice(format!("line {i}: the quick brown fox\n").as_bytes());
        }
        v
    }

    #[test]
    fn every_format_round_trips() {
        let data = corpus();
        for format in [Format::Deflate, Format::DeflateRaw, Format::Gzip] {
            assert_eq!(round_trip(format, &data, 1 << 20), data, "{format:?} whole-buffer");
            assert_eq!(round_trip(format, &data, 97), data, "{format:?} in 97-byte chunks");
        }
    }

    #[test]
    fn the_output_is_actually_compressed_and_is_not_the_input() {
        // The test that a passthrough stub cannot pass. Both halves matter: the
        // bytes must differ from the input, AND they must be smaller.
        let data = corpus();
        let mut enc = ZipData::new(Format::Gzip, false);
        let mut out = enc.push(&data).unwrap();
        out.extend_from_slice(&enc.finish().unwrap());
        assert_ne!(out, data, "the output is the input — this is a passthrough");
        assert!(out.len() < data.len() / 4, "{} vs {}", out.len(), data.len());
    }

    #[test]
    fn gzip_output_carries_the_gzip_magic() {
        // A consumer identifies the format by these two bytes; the wrong (or
        // missing) wrapper is a file every gzip reader rejects.
        let mut enc = ZipData::new(Format::Gzip, false);
        let mut out = enc.push(b"hello").unwrap();
        out.extend_from_slice(&enc.finish().unwrap());
        assert_eq!(&out[..2], &[0x1f, 0x8b], "not a gzip member: {:?}", &out[..4.min(out.len())]);
    }

    #[test]
    fn zlib_and_raw_deflate_are_different_wrappers_of_the_same_data() {
        let mut a = ZipData::new(Format::Deflate, false);
        let mut wrapped = a.push(b"hello hello hello").unwrap();
        wrapped.extend_from_slice(&a.finish().unwrap());
        let mut b = ZipData::new(Format::DeflateRaw, false);
        let mut raw = b.push(b"hello hello hello").unwrap();
        raw.extend_from_slice(&b.finish().unwrap());
        // zlib = 2-byte header + deflate + 4-byte adler32.
        assert_eq!(wrapped.len(), raw.len() + 6, "the zlib wrapper is missing or doubled");
        assert_eq!(&wrapped[2..2 + raw.len()], &raw[..]);
    }

    #[test]
    fn truncated_input_errors_rather_than_returning_a_short_read() {
        // The failure this guards: a half-written file decompressing "fine" to
        // half its content, which looks exactly like a complete small file.
        let data = corpus();
        for format in [Format::Deflate, Format::DeflateRaw, Format::Gzip] {
            let mut enc = ZipData::new(format, false);
            let mut compressed = enc.push(&data).unwrap();
            compressed.extend_from_slice(&enc.finish().unwrap());

            let mut dec = ZipData::new(format, true);
            let _ = dec.push(&compressed[..compressed.len() / 2]);
            assert!(dec.finish().is_err(), "{format:?} accepted a truncated stream");
        }
    }

    #[test]
    fn corrupt_input_is_rejected() {
        let mut enc = ZipData::new(Format::Gzip, false);
        let mut compressed = enc.push(&corpus()).unwrap();
        compressed.extend_from_slice(&enc.finish().unwrap());
        // Flip a byte in the middle of the deflate data.
        let mid = compressed.len() / 2;
        compressed[mid] ^= 0xff;

        let mut dec = ZipData::new(Format::Gzip, true);
        let pushed = dec.push(&compressed);
        let finished = dec.finish();
        assert!(pushed.is_err() || finished.is_err(), "corruption went unnoticed");
    }

    #[test]
    fn an_empty_stream_still_produces_a_valid_member() {
        for format in [Format::Deflate, Format::DeflateRaw, Format::Gzip] {
            let mut enc = ZipData::new(format, false);
            let compressed = enc.finish().unwrap();
            let mut dec = ZipData::new(format, true);
            let mut out = dec.push(&compressed).unwrap();
            out.extend_from_slice(&dec.finish().unwrap());
            assert!(out.is_empty(), "{format:?} produced {out:?} from nothing");
        }
    }

    #[test]
    fn only_the_three_specified_format_names_are_accepted() {
        assert_eq!(Format::parse("gzip"), Some(Format::Gzip));
        assert_eq!(Format::parse("deflate"), Some(Format::Deflate));
        assert_eq!(Format::parse("deflate-raw"), Some(Format::DeflateRaw));
        // Case matters, and neither of these is a format.
        assert_eq!(Format::parse("GZIP"), None);
        assert_eq!(Format::parse("br"), None);
        assert_eq!(Format::parse(""), None);
    }

    #[test]
    fn pushing_after_finish_is_an_error_not_a_silent_drop() {
        let mut enc = ZipData::new(Format::Gzip, false);
        enc.finish().unwrap();
        assert!(enc.push(b"more").is_err());
        assert!(enc.finish().is_err());
    }
}

/// Through the real `CompressionStream` / `DecompressionStream` classes, which
/// are TransformStreams over the codec above.
#[cfg(test)]
mod jsc_tests {
    use crate::api::Context;
    use crate::webapi::testing::*;

    fn run(c: &Context, body: &str) -> String {
        s(
            c,
            &format!(
                "globalThis.__out = '<pending>'; (async () => {{ {body} }})().then(\
                   v => {{ globalThis.__out = String(v); }}, \
                   e => {{ globalThis.__out = 'ERROR: ' + ((e && e.message) || e); }}); 'started'"
            ),
        );
        for _ in 0..64 {
            c.drain_microtasks();
        }
        s(c, "globalThis.__out")
    }

    const HELPERS: &str = "
      globalThis.collect = async (stream) => {
        const r = stream.getReader(); const parts = []; let total = 0;
        for (;;) { const { value, done } = await r.read(); if (done) break; parts.push(value); total += value.byteLength; }
        const out = new Uint8Array(total); let at = 0;
        for (const p of parts) { out.set(p, at); at += p.byteLength; }
        return out;
      };
      globalThis.through = async (bytes, stream) => {
        const w = stream.writable.getWriter();
        w.write(bytes); w.close();
        return await collect(stream.readable);
      };
      globalThis.sample = () => {
        const s = 'the quick brown fox jumps over the lazy dog. '.repeat(200);
        return new TextEncoder().encode(s);
      };
    ";

    #[test]
    fn every_format_round_trips_through_the_stream_classes() {
        let c = ctx();
        s(&c, HELPERS);
        for format in ["gzip", "deflate", "deflate-raw"] {
            let out = run(
                &c,
                &format!(
                    "const src = sample();
                     const packed = await through(src, new CompressionStream('{format}'));
                     const back = await through(packed, new DecompressionStream('{format}'));
                     return (packed.byteLength < src.byteLength) + ',' +
                            (back.byteLength === src.byteLength) + ',' +
                            (new TextDecoder().decode(back) === new TextDecoder().decode(src));"
                ),
            );
            assert_eq!(out, "true,true,true", "format {format}");
        }
    }

    #[test]
    fn the_output_is_not_the_input() {
        // The assertion a passthrough stub fails. Byte-for-byte identity is the
        // signature of "compression" that compressed nothing.
        let c = ctx();
        s(&c, HELPERS);
        let out = run(
            &c,
            "const src = sample();
             const packed = await through(src, new CompressionStream('gzip'));
             const same = packed.byteLength === src.byteLength &&
                          packed.every((b, i) => b === src[i]);
             return same + ',' + (packed[0] === 0x1f && packed[1] === 0x8b);",
        );
        assert_eq!(out, "false,true");
    }

    #[test]
    fn a_pipe_through_a_compression_stream_works_like_any_other_transform() {
        let c = ctx();
        s(&c, HELPERS);
        let out = run(
            &c,
            "const src = new TextEncoder().encode('hello '.repeat(100));
             const rs = new ReadableStream({ start(c) { c.enqueue(src); c.close(); } }, { highWaterMark: 4 });
             const round = rs.pipeThrough(new CompressionStream('gzip'))
                             .pipeThrough(new DecompressionStream('gzip'));
             const back = await collect(round);
             return new TextDecoder().decode(back) === 'hello '.repeat(100);",
        );
        assert_eq!(out, "true");
    }

    #[test]
    fn truncated_input_errors_the_stream_instead_of_yielding_a_short_read() {
        let c = ctx();
        s(&c, HELPERS);
        let out = run(
            &c,
            "const src = sample();
             const packed = await through(src, new CompressionStream('gzip'));
             const half = packed.slice(0, Math.floor(packed.byteLength / 2));
             try { await through(half, new DecompressionStream('gzip')); return 'accepted'; }
             catch (e) { return 'errored'; }",
        );
        assert_eq!(out, "errored", "a truncated stream must not decompress 'successfully'");
    }

    #[test]
    fn an_unsupported_format_refuses_and_names_the_three_that_work() {
        let c = ctx();
        let msg = throws(&c, "new CompressionStream('brotli')");
        assert!(msg.contains("deflate-raw"), "{msg}");
        assert!(msg.contains("gzip"), "{msg}");
        // Case is significant per the spec's enumeration.
        assert!(!throws(&c, "new DecompressionStream('GZIP')").is_empty());
    }

    #[test]
    fn a_non_buffer_chunk_is_refused() {
        let c = ctx();
        s(&c, HELPERS);
        let out = run(
            &c,
            "const cs = new CompressionStream('gzip');
             const w = cs.writable.getWriter();
             try { await w.write('a string, not a BufferSource'); return 'accepted'; }
             catch (e) { return 'refused'; }",
        );
        assert_eq!(out, "refused");
    }

    #[test]
    fn the_pair_exposes_readable_and_writable_like_a_transform_stream() {
        let c = ctx();
        ok(&c, "new CompressionStream('gzip').readable instanceof ReadableStream");
        ok(&c, "new CompressionStream('gzip').writable instanceof WritableStream");
        ok(&c, "new DecompressionStream('deflate').readable instanceof ReadableStream");
    }
}
