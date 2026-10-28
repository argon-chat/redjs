//! IndexedDB keys (Indexed Database API 3.0 §2.5): conversion from JS values,
//! the byte encoding everything else compares, and the way back.
//!
//! # One encoding, ordered by `memcmp`
//!
//! A key's encoding sorts exactly as IndexedDB orders keys, so the server,
//! the backends, `indexedDB.cmp()`, range checks and index uniqueness are all
//! one byte comparison:
//!
//! ```text
//! number  0x10 ‖ f64 sortable, 8 bytes   (-0 is written as +0)
//! date    0x20 ‖ f64 sortable, 8 bytes
//! string  0x30 ‖ unit* ‖ 0x00            (UTF-16 code units, lone surrogates kept)
//! binary  0x40 ‖ byte* ‖ 0x00            (each byte escaped like a unit)
//! array   0x50 ‖ key*  ‖ 0x00
//! ```
//!
//! A unit `u` is written as one byte `u + 1` below 0x7F, two bytes
//! `0x80 | (u − 0x7F) >> 8, (u − 0x7F) & 0xFF` up to 0x407E, and three bytes
//! `0xC0 | u >> 10, u >> 2, u << 6` above — Firefox's `Key.cpp` scheme. No
//! unit's FIRST byte is 0x00, so the terminator sorts before any unit: a
//! prefix sorts first (a continuation byte may be 0x00, but it is only ever
//! compared with a continuation byte of the same width, because the first
//! bytes were equal). Type tags sit far apart, giving number < date < string <
//! binary < array, and a shorter array sorts first because its terminator
//! (0x00) is below every element tag.
//!
//! The encoding is PREFIX-FREE: no key's bytes are a proper prefix of
//! another's. So an index entry is stored as `index key ‖ primary key` and the
//! concatenations sort by (index key, primary key). [`AFTER`] appended to an
//! encoded key is a bound strictly above every concatenation that starts with
//! that key and below every larger key.

use core::ffi::c_void;

use crate::glue::{self, brand, RjsBrandInfo};
use crate::webapi::util::*;

pub const T_NUMBER: u8 = 0x10;
pub const T_DATE: u8 = 0x20;
pub const T_STRING: u8 = 0x30;
pub const T_BINARY: u8 = 0x40;
pub const T_ARRAY: u8 = 0x50;
pub const END: u8 = 0x00;
/// Appended to an encoded key: above every `key ‖ anything`.
pub const AFTER: u8 = 0xFF;

/// A valid key.
#[derive(Clone, Debug, PartialEq)]
pub enum Key {
    Number(f64),
    Date(f64),
    String(Vec<u16>),
    Binary(Vec<u8>),
    Array(Vec<Key>),
}

/// 2^53: the largest integer the key generator can still hand out is 2^53 − 1
/// beyond which a generated key would fail (§2.11).
pub const KEYGEN_MAX: f64 = 9_007_199_254_740_992.0;

fn sortable(d: f64) -> [u8; 8] {
    let d = if d == 0.0 { 0.0 } else { d }; // -0 → +0
    let bits = d.to_bits();
    let b = if bits & (1 << 63) != 0 { !bits } else { bits ^ (1 << 63) };
    b.to_be_bytes()
}

fn unsortable(b: [u8; 8]) -> f64 {
    let v = u64::from_be_bytes(b);
    let bits = if v & (1 << 63) != 0 { v ^ (1 << 63) } else { !v };
    f64::from_bits(bits)
}

fn put_unit(out: &mut Vec<u8>, u: u16) {
    let u = u as u32;
    if u < 0x7F {
        out.push((u + 1) as u8);
    } else if u <= 0x3FFF + 0x7F {
        let c = u - 0x7F;
        out.push(0x80 | (c >> 8) as u8);
        out.push((c & 0xFF) as u8);
    } else {
        out.push(0xC0 | (u >> 10) as u8);
        out.push(((u >> 2) & 0xFF) as u8);
        out.push(((u & 0x3) << 6) as u8);
    }
}

impl Key {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        self.encode_into(&mut out);
        out
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Key::Number(d) => {
                out.push(T_NUMBER);
                out.extend_from_slice(&sortable(*d));
            }
            Key::Date(d) => {
                out.push(T_DATE);
                out.extend_from_slice(&sortable(*d));
            }
            Key::String(units) => {
                out.push(T_STRING);
                for &u in units {
                    put_unit(out, u);
                }
                out.push(END);
            }
            Key::Binary(bytes) => {
                out.push(T_BINARY);
                for &b in bytes {
                    put_unit(out, b as u16);
                }
                out.push(END);
            }
            Key::Array(items) => {
                out.push(T_ARRAY);
                for k in items {
                    k.encode_into(out);
                }
                out.push(END);
            }
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self {
            Key::Number(d) => Some(*d),
            _ => None,
        }
    }
}

/// Decodes one key from the front of `b`: the key and the bytes it used.
pub fn decode_prefix(b: &[u8]) -> Option<(Key, usize)> {
    let mut p = Parser { b, pos: 0 };
    let k = p.key(0)?;
    Some((k, p.pos))
}

/// Decodes a whole buffer that holds exactly one key.
pub fn decode(b: &[u8]) -> Option<Key> {
    let (k, n) = decode_prefix(b)?;
    (n == b.len()).then_some(k)
}

/// Length of the first encoded key in `b` (index entries: where the index key
/// ends and the primary key starts).
pub fn key_len(b: &[u8]) -> Option<usize> {
    let mut i = 0usize;
    let mut depth = 0u32;
    loop {
        let t = *b.get(i)?;
        i += 1;
        match t {
            T_NUMBER | T_DATE => i += 8,
            T_STRING | T_BINARY => {
                // Unit boundaries come from first bytes: a continuation byte
                // may be 0x00, a first byte never is.
                loop {
                    let c = *b.get(i)?;
                    i += match c {
                        0 => 1,
                        0x01..=0x7F => 1,
                        0x80..=0xBF => 2,
                        _ => 3,
                    };
                    if c == 0 {
                        break;
                    }
                }
            }
            T_ARRAY => {
                depth += 1;
                continue;
            }
            END if depth > 0 => {
                depth -= 1;
            }
            _ => return None,
        }
        if i > b.len() {
            return None;
        }
        // Inside an array: keep reading until its terminator.
        if depth == 0 {
            return Some(i);
        }
    }
}

struct Parser<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn byte(&mut self) -> Option<u8> {
        let v = *self.b.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }

    fn f64(&mut self) -> Option<f64> {
        let s = self.b.get(self.pos..self.pos + 8)?;
        self.pos += 8;
        Some(unsortable(s.try_into().ok()?))
    }

    fn units(&mut self) -> Option<Vec<u16>> {
        let mut out = Vec::new();
        loop {
            let c = self.byte()? as u32;
            if c == 0 {
                return Some(out);
            }
            if c < 0x80 {
                out.push((c - 1) as u16);
            } else if c < 0xC0 {
                let lo = self.byte()? as u32;
                out.push((((c & 0x3F) << 8 | lo) + 0x7F) as u16);
            } else {
                let b1 = self.byte()? as u32;
                let b2 = self.byte()? as u32;
                out.push((((c & 0x3F) << 10) | (b1 << 2) | (b2 >> 6)) as u16);
            }
        }
    }

    fn key(&mut self, depth: u32) -> Option<Key> {
        if depth > 10_000 {
            return None;
        }
        Some(match self.byte()? {
            T_NUMBER => Key::Number(self.f64()?),
            T_DATE => Key::Date(self.f64()?),
            T_STRING => Key::String(self.units()?),
            T_BINARY => Key::Binary(self.units()?.into_iter().map(|u| u as u8).collect()),
            T_ARRAY => {
                let mut items = Vec::new();
                loop {
                    if *self.b.get(self.pos)? == END {
                        self.pos += 1;
                        break;
                    }
                    items.push(self.key(depth + 1)?);
                }
                Key::Array(items)
            }
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Ranges over encoded keys
// ---------------------------------------------------------------------------

/// One end of a range, over encoded keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bound {
    Unbounded,
    Incl(Vec<u8>),
    Excl(Vec<u8>),
}

/// A key range with encoded bounds — what crosses to the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Range {
    pub lower: Bound,
    pub upper: Bound,
}

impl Range {
    pub fn all() -> Range {
        Range { lower: Bound::Unbounded, upper: Bound::Unbounded }
    }

    pub fn only(k: Vec<u8>) -> Range {
        Range { lower: Bound::Incl(k.clone()), upper: Bound::Incl(k) }
    }

    /// Is `k` (encoded) inside?
    pub fn contains(&self, k: &[u8]) -> bool {
        let lo = match &self.lower {
            Bound::Unbounded => true,
            Bound::Incl(l) => k >= l.as_slice(),
            Bound::Excl(l) => k > l.as_slice(),
        };
        let hi = match &self.upper {
            Bound::Unbounded => true,
            Bound::Incl(u) => k <= u.as_slice(),
            Bound::Excl(u) => k < u.as_slice(),
        };
        lo && hi
    }

    /// Is this range exactly one key?
    pub fn single(&self) -> Option<&[u8]> {
        match (&self.lower, &self.upper) {
            (Bound::Incl(a), Bound::Incl(b)) if a == b => Some(a),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// JS → key ("convert a value to a key", §7.1)
// ---------------------------------------------------------------------------

/// The result of a conversion that did not throw: a key, or one of the
/// spec's two failures (callers treat them alike: a DataError, or "absent").
pub enum Conv {
    Key(Key),
    Invalid,
}

/// The bytes of a buffer source, or `None` when detached. `v` must be an
/// ArrayBuffer or a view (per `info`).
pub(crate) unsafe fn buffer_source_bytes(g: *mut c_void, v: u64, info: &RjsBrandInfo) -> Option<Vec<u8>> {
    if info.flags & brand::F_DETACHED != 0 {
        return None;
    }
    if info.brand == brand::ARRAY_BUFFER {
        let n = info.byte_length as usize;
        return Some(if n == 0 || info.data.is_null() { Vec::new() } else { core::slice::from_raw_parts(info.data as *const u8, n).to_vec() });
    }
    // A view: its window of the buffer.
    let mut buf = UNDEF;
    if glue::rjs_view_buffer(g, v, &mut buf) != 0 {
        return None;
    }
    let mut b = RjsBrandInfo::default();
    glue::rjs_brand(g, buf, &mut b);
    if b.flags & brand::F_DETACHED != 0 {
        return None;
    }
    let elem: u64 = match info.ta_kind {
        1..=3 | 12 => 1,
        4 | 5 | 13 => 2,
        6..=8 => 4,
        9..=11 => 8,
        _ => 1,
    };
    let blen = b.byte_length;
    let off = info.byte_offset;
    let len = if info.flags & brand::F_LENGTH_TRACKING != 0 {
        blen.saturating_sub(off) / elem * elem
    } else {
        info.length.saturating_mul(elem)
    };
    if off.saturating_add(len) > blen {
        return None; // out of bounds: treated like detached
    }
    if len == 0 || b.data.is_null() {
        return Some(Vec::new());
    }
    Some(core::slice::from_raw_parts((b.data as *const u8).add(off as usize), len as usize).to_vec())
}

/// "Convert a value to a key". `Err(())` = an exception is pending (a getter
/// threw); rethrow it.
pub unsafe fn from_js(g: *mut c_void, v: u64) -> Result<Conv, ()> {
    let mut seen = Vec::new();
    convert(g, v, &mut seen, 0)
}

unsafe fn convert(g: *mut c_void, v: u64, seen: &mut Vec<u64>, depth: u32) -> Result<Conv, ()> {
    if let Some(d) = dec_f64(v) {
        return Ok(if d.is_nan() { Conv::Invalid } else { Conv::Key(Key::Number(d)) });
    }
    if !is_cell(v) {
        return Ok(Conv::Invalid);
    }
    match glue::rjs_kind(g, v) {
        5 => {
            return Ok(match read_string16(g, v) {
                Some(u) => Conv::Key(Key::String(u)),
                None => Conv::Invalid,
            })
        }
        k if k < 8 => return Ok(Conv::Invalid),
        _ => {}
    }
    let mut info = RjsBrandInfo::default();
    match glue::rjs_brand(g, v, &mut info) {
        brand::DATE => Ok(if info.number.is_nan() { Conv::Invalid } else { Conv::Key(Key::Date(info.number)) }),
        brand::ARRAY_BUFFER | brand::VIEW => Ok(match buffer_source_bytes(g, v, &info) {
            Some(b) => Conv::Key(Key::Binary(b)),
            None => Conv::Invalid,
        }),
        brand::ARRAY => {
            if seen.contains(&v) || depth > 4096 {
                return Ok(Conv::Invalid);
            }
            let len = array_len(g, v);
            seen.push(v);
            let mut keys = Vec::with_capacity(len as usize);
            for i in 0..len {
                let mut entry = UNDEF;
                match glue::rjs_own_get_index(g, v, i, &mut entry) {
                    1 => {}
                    0 => return Ok(Conv::Invalid), // a hole
                    _ => {
                        glue::rjs_throw(g, entry);
                        return Err(());
                    }
                }
                match convert(g, entry, seen, depth + 1)? {
                    Conv::Key(k) => keys.push(k),
                    Conv::Invalid => return Ok(Conv::Invalid),
                }
            }
            Ok(Conv::Key(Key::Array(keys)))
        }
        _ => Ok(Conv::Invalid),
    }
}

/// `Ok(Some(key))`, `Ok(None)` for invalid, `Err` = exception pending.
pub unsafe fn valid_from_js(g: *mut c_void, v: u64) -> Result<Option<Key>, ()> {
    Ok(match from_js(g, v)? {
        Conv::Key(k) => Some(k),
        Conv::Invalid => None,
    })
}

// ---------------------------------------------------------------------------
// key → JS ("convert a key to a value", §7.2)
// ---------------------------------------------------------------------------

pub unsafe fn to_js(g: *mut c_void, k: &Key) -> u64 {
    match k {
        Key::Number(d) => enc_f64(*d),
        Key::Date(d) => {
            let global = glue::rjs_global(g);
            let date = get(g, global, "Date");
            let mut out = UNDEF;
            let arg = enc_f64(*d);
            glue::rjs_construct(g, date, &arg, 1, &mut out);
            out
        }
        Key::String(u) => js_string16(g, u),
        Key::Binary(b) => {
            let ta = make_u8(g, b);
            let mut buf = UNDEF;
            glue::rjs_view_buffer(g, ta, &mut buf);
            buf
        }
        Key::Array(items) => {
            let arr = new_array(g);
            for (i, k) in items.iter().enumerate() {
                let v = to_js(g, k);
                glue::rjs_define_index(g, arr, i as u32, v);
            }
            arr
        }
    }
}

/// Decodes and converts in one step; `undefined` for bytes that are not a key.
pub unsafe fn bytes_to_js(g: *mut c_void, b: &[u8]) -> u64 {
    match decode(b) {
        Some(k) => to_js(g, &k),
        None => UNDEF,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> Key {
        Key::String(x.encode_utf16().collect())
    }

    /// A table of keys in IndexedDB order — every type pair, the numeric edge
    /// cases, code-unit (not code-point) string order, binary, nested arrays.
    fn ordered() -> Vec<Key> {
        vec![
            Key::Number(f64::NEG_INFINITY),
            Key::Number(-f64::MAX),
            Key::Number(-1.5),
            Key::Number(-f64::MIN_POSITIVE),
            Key::Number(0.0),
            Key::Number(f64::MIN_POSITIVE),
            Key::Number(1.0),
            Key::Number(2.5),
            Key::Number(f64::MAX),
            Key::Number(f64::INFINITY),
            Key::Date(-1e15),
            Key::Date(0.0),
            Key::Date(1e15),
            s(""),
            Key::String(vec![0]),
            s("\u{1}"),
            s("A"),
            s("a"),
            s("aa"),
            s("ab"),
            s("b"),
            Key::String(vec![0x7E]),
            Key::String(vec![0x7F]),
            Key::String(vec![0x80]),
            Key::String(vec![0x407E]),
            Key::String(vec![0x407F]),
            // A lone high surrogate, then a pair: code-unit order puts both
            // below U+E000 even though U+1F600 is a higher code point.
            Key::String(vec![0xD800]),
            Key::String(vec![0xD83D, 0xDE00]),
            Key::String(vec![0xE000]),
            Key::String(vec![0xFFFF]),
            Key::Binary(vec![]),
            Key::Binary(vec![0]),
            Key::Binary(vec![0, 0]),
            Key::Binary(vec![1]),
            Key::Binary(vec![0x7F]),
            Key::Binary(vec![0xFF]),
            Key::Array(vec![]),
            Key::Array(vec![Key::Number(-1.0)]),
            Key::Array(vec![Key::Number(0.0)]),
            Key::Array(vec![Key::Number(0.0), Key::Number(0.0)]),
            Key::Array(vec![Key::Number(1.0)]),
            Key::Array(vec![s("a")]),
            Key::Array(vec![Key::Array(vec![])]),
            Key::Array(vec![Key::Array(vec![Key::Number(1.0)])]),
            Key::Array(vec![Key::Array(vec![Key::Array(vec![])])]),
        ]
    }

    #[test]
    fn encoding_order_is_idb_order() {
        let keys = ordered();
        let enc: Vec<Vec<u8>> = keys.iter().map(|k| k.encode()).collect();
        for i in 0..enc.len() {
            for j in 0..enc.len() {
                assert_eq!(enc[i].cmp(&enc[j]), i.cmp(&j), "{:?} vs {:?}", keys[i], keys[j]);
            }
        }
    }

    #[test]
    fn round_trip_and_length() {
        for k in ordered() {
            let e = k.encode();
            assert_eq!(decode(&e).as_ref(), Some(&k), "{k:?}");
            assert_eq!(key_len(&e), Some(e.len()), "{k:?}");
            // An index entry: key ‖ primary key splits at key_len.
            let mut cat = e.clone();
            cat.extend(s("pk").encode());
            assert_eq!(key_len(&cat), Some(e.len()));
        }
    }

    #[test]
    fn negative_zero_is_zero_and_nan_bytes_never_decode_to_a_number_key() {
        assert_eq!(Key::Number(-0.0).encode(), Key::Number(0.0).encode());
        assert_eq!(decode(&Key::Number(-0.0).encode()), Some(Key::Number(0.0)));
        assert!(decode(&[T_STRING, 0x05]).is_none(), "unterminated");
        assert!(decode(&[0x99]).is_none());
    }

    #[test]
    fn prefix_free_with_the_after_sentinel() {
        // Every concatenation `a ‖ x` sorts below `a ‖ AFTER` and above `a`,
        // and below any larger key `b`.
        let keys = ordered();
        for w in keys.windows(2) {
            let (a, b) = (w[0].encode(), w[1].encode());
            let mut a_after = a.clone();
            a_after.push(AFTER);
            for pk in [Key::Number(0.0), s("zzz"), Key::Array(vec![s("q")])] {
                let mut cat = a.clone();
                cat.extend(pk.encode());
                assert!(cat > a && cat < a_after, "{:?}", w[0]);
            }
            assert!(a_after < b, "{:?} AFTER vs {:?}", w[0], w[1]);
        }
    }

    #[test]
    fn ranges() {
        let r = Range { lower: Bound::Excl(Key::Number(1.0).encode()), upper: Bound::Incl(Key::Number(3.0).encode()) };
        assert!(!r.contains(&Key::Number(1.0).encode()));
        assert!(r.contains(&Key::Number(2.0).encode()));
        assert!(r.contains(&Key::Number(3.0).encode()));
        assert!(!r.contains(&s("a").encode()));
        assert!(Range::all().contains(&s("a").encode()));
    }
}
