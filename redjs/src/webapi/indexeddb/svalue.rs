//! A pure-Rust reader of the structured serializer's bytes
//! ([`crate::webapi::serialize`], wire format v1) — what key paths are
//! evaluated on, on either thread, with no JS anywhere.
//!
//! IndexedDB evaluates key paths on the CLONE of a value (§4.6 of the design,
//! WPT `clone-before-keypath-eval`), and the clone is plain data: own data
//! properties, arrays, strings, Blobs. So the evaluator reads the bytes the
//! client is about to send anyway, and the server — which computes every
//! index key — reads the very same bytes. No user code can run, so evaluation
//! cannot throw, and both sides always agree.
//!
//! Parsing is iterative (an explicit stack), so a value nested deeper than
//! any native stack is still read.
//!
//! [`inject`] is the one writer: it adds a generated key at a key path
//! ("inject a key into a value", §2.11) by splicing the bytes and renumbering
//! the object references that the insertion shifted.

use super::key::Key;
use super::keypath::KeyPath;
use crate::webapi::serialize::{tag, FLAG_STORAGE, MAGIC, VERSION};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PKey {
    Name(Vec<u16>),
    Index(u32),
}

#[derive(Debug)]
enum Node {
    Undefined,
    Other,
    Number(f64),
    Str(Vec<u16>),
    /// `end` = byte offset of the object's END tag (where an injected
    /// property goes).
    Obj { props: Vec<(PKey, usize)>, end: usize },
    Arr { len: u32, props: Vec<(PKey, usize)>, end: usize },
    Date(f64),
    Buffer { start: usize, len: usize },
    View { kind: u8, off: u64, len1: u64, buf: usize },
    Blob { mime: Vec<u16>, size: u64 },
    StrObj(Vec<u16>),
    /// An Error's own data properties (message / stack / cause).
    Error { props: Vec<(PKey, usize)> },
    Ref(u32),
}

/// A parsed value: the node arena over the original bytes.
pub struct Parsed<'a> {
    bytes: &'a [u8],
    nodes: Vec<Node>,
    /// node index → byte offset of its tag.
    offsets: Vec<usize>,
    /// object id → node index (ids are handed out at the tag, in byte order).
    id_node: Vec<usize>,
    /// object id → byte offset of its tag.
    id_offset: Vec<usize>,
    /// Every 0x10 reference: (offset of the tag, byte length incl. varint, target id).
    refs: Vec<(usize, usize, u32)>,
}

enum Kind {
    /// Object / Array: (key value)* END.
    Props,
    /// Map / Set: value* END (contents are opaque to key paths).
    Seq,
    /// Exactly `left` more child values.
    Fixed,
}

struct Frame {
    node: usize,
    kind: Kind,
    left: u8,
    key: Option<PKey>,
}

struct Rd<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Rd<'_> {
    fn byte(&mut self) -> Option<u8> {
        let v = *self.b.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }
    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }
    fn varint(&mut self) -> Option<u64> {
        let mut v: u64 = 0;
        for shift in (0..70).step_by(7) {
            let b = self.byte()?;
            if shift == 63 && b > 1 {
                return None;
            }
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
    fn f64(&mut self) -> Option<f64> {
        let s = self.b.get(self.pos..self.pos + 8)?;
        self.pos += 8;
        Some(f64::from_le_bytes(s.try_into().ok()?))
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        if self.pos.checked_add(n)? > self.b.len() {
            return None;
        }
        self.pos += n;
        Some(())
    }
    /// A string tag + payload, as UTF-16 units.
    fn string(&mut self) -> Option<Vec<u16>> {
        let t = self.byte()?;
        self.string_body(t)
    }
    fn string_body(&mut self, t: u8) -> Option<Vec<u16>> {
        let n = self.varint()? as usize;
        match t {
            tag::STRING_LATIN1 => {
                let s = self.b.get(self.pos..self.pos.checked_add(n)?)?;
                self.pos += n;
                Some(s.iter().map(|&c| c as u16).collect())
            }
            tag::STRING_UTF16 => {
                let s = self.b.get(self.pos..self.pos.checked_add(n.checked_mul(2)?)?)?;
                self.pos += n * 2;
                Some(s.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect())
            }
            _ => None,
        }
    }
}

impl<'a> Parsed<'a> {
    /// Parses bytes written by the serializer. `None` = corrupt.
    pub fn parse(bytes: &'a [u8]) -> Option<Parsed<'a>> {
        if bytes.len() < 5 || bytes[..3] != MAGIC || bytes[3] != VERSION || bytes[4] & !FLAG_STORAGE != 0 {
            return None;
        }
        let mut p = Parsed { bytes, nodes: Vec::new(), offsets: Vec::new(), id_node: Vec::new(), id_offset: Vec::new(), refs: Vec::new() };
        let mut r = Rd { b: bytes, pos: 5 };
        let mut stack: Vec<Frame> = Vec::new();
        let mut have_root = false;
        loop {
            // What does the innermost open container expect next?
            if let Some(f) = stack.last_mut() {
                match f.kind {
                    Kind::Props if f.key.is_none() => {
                        let t = r.byte()?;
                        if t == tag::END {
                            let end = r.pos - 1;
                            let f = stack.pop().unwrap();
                            match &mut p.nodes[f.node] {
                                Node::Obj { end: e, .. } | Node::Arr { end: e, .. } => *e = end,
                                _ => {}
                            }
                            continue;
                        }
                        let k = if t == tag::KEY_INDEX {
                            PKey::Index(u32::try_from(r.varint()?).ok()?)
                        } else {
                            PKey::Name(r.string_body(t)?)
                        };
                        f.key = Some(k);
                        continue;
                    }
                    Kind::Seq if r.peek()? == tag::END => {
                        r.pos += 1;
                        stack.pop();
                        continue;
                    }
                    Kind::Fixed if f.left == 0 => {
                        stack.pop();
                        continue;
                    }
                    _ => {}
                }
            } else if have_root {
                break;
            }
            // Read one value and attach it where it belongs.
            let (idx, frame) = p.value(&mut r)?;
            match stack.last_mut() {
                None => have_root = true,
                Some(f) => match f.kind {
                    Kind::Props => {
                        let k = f.key.take()?;
                        match &mut p.nodes[f.node] {
                            Node::Obj { props, .. } | Node::Arr { props, .. } | Node::Error { props } => props.push((k, idx)),
                            _ => return None,
                        }
                    }
                    Kind::Seq => {}
                    Kind::Fixed => {
                        f.left -= 1;
                        match &mut p.nodes[f.node] {
                            Node::View { buf, .. } => *buf = idx,
                            Node::Error { props } => props.push((PKey::Name("cause".encode_utf16().collect()), idx)),
                            _ => {}
                        }
                    }
                },
            }
            if let Some(fr) = frame {
                stack.push(fr);
            }
            if stack.len() > 1_000_000 {
                return None;
            }
        }
        if r.pos != bytes.len() {
            return None;
        }
        Some(p)
    }

    fn push(&mut self, n: Node, at: usize) -> usize {
        self.nodes.push(n);
        self.offsets.push(at);
        self.nodes.len() - 1
    }

    fn object(&mut self, n: Node, at: usize) -> usize {
        let idx = self.push(n, at);
        self.id_node.push(idx);
        self.id_offset.push(at);
        idx
    }

    /// One value at the reader: its node, and a frame if it opens a container.
    fn value(&mut self, r: &mut Rd) -> Option<(usize, Option<Frame>)> {
        let at = r.pos;
        let t = r.byte()?;
        let leaf = |p: &mut Self, n: Node| Some((p.push(n, at), None));
        match t {
            tag::UNDEFINED => leaf(self, Node::Undefined),
            tag::NULL | tag::FALSE | tag::TRUE => leaf(self, Node::Other),
            tag::INT32 => {
                let z = r.varint()? as u32;
                let i = ((z >> 1) as i32) ^ -((z & 1) as i32);
                leaf(self, Node::Number(i as f64))
            }
            tag::DOUBLE => {
                let d = r.f64()?;
                leaf(self, Node::Number(d))
            }
            tag::BIGINT => {
                let n = r.varint()? as usize;
                r.skip(n)?;
                leaf(self, Node::Other)
            }
            tag::STRING_LATIN1 | tag::STRING_UTF16 => {
                let s = r.string_body(t)?;
                leaf(self, Node::Str(s))
            }
            tag::OBJECT_REF => {
                let id = u32::try_from(r.varint()?).ok()?;
                if id as usize >= self.id_node.len() {
                    return None;
                }
                self.refs.push((at, r.pos - at, id));
                leaf(self, Node::Ref(id))
            }
            tag::OBJECT => {
                let idx = self.object(Node::Obj { props: Vec::new(), end: 0 }, at);
                Some((idx, Some(Frame { node: idx, kind: Kind::Props, left: 0, key: None })))
            }
            tag::ARRAY => {
                let len = u32::try_from(r.varint()?).ok()?;
                let idx = self.object(Node::Arr { len, props: Vec::new(), end: 0 }, at);
                Some((idx, Some(Frame { node: idx, kind: Kind::Props, left: 0, key: None })))
            }
            tag::DATE => {
                let d = r.f64()?;
                Some((self.object(Node::Date(d), at), None))
            }
            tag::REGEXP => {
                let idx = self.object(Node::Other, at);
                r.string()?;
                r.string()?;
                Some((idx, None))
            }
            tag::MAP | tag::SET => {
                let idx = self.object(Node::Other, at);
                Some((idx, Some(Frame { node: idx, kind: Kind::Seq, left: 0, key: None })))
            }
            tag::BOOLEAN_OBJECT => {
                let idx = self.object(Node::Other, at);
                r.skip(1)?;
                Some((idx, None))
            }
            tag::NUMBER_OBJECT => {
                let idx = self.object(Node::Other, at);
                r.skip(8)?;
                Some((idx, None))
            }
            tag::STRING_OBJECT => {
                let idx = self.object(Node::Other, at);
                let s = r.string()?;
                self.nodes[idx] = Node::StrObj(s);
                Some((idx, None))
            }
            tag::BIGINT_OBJECT => {
                let idx = self.object(Node::Other, at);
                let n = r.varint()? as usize;
                r.skip(n)?;
                Some((idx, None))
            }
            tag::ERROR => {
                let idx = self.object(Node::Error { props: Vec::new() }, at);
                let _name = r.byte()?;
                let mask = r.byte()?;
                if mask & !7 != 0 {
                    return None;
                }
                let mut props = Vec::new();
                if mask & 1 != 0 {
                    let o = r.pos;
                    let m = r.string()?;
                    let n = self.push(Node::Str(m), o);
                    props.push((PKey::Name("message".encode_utf16().collect()), n));
                }
                if mask & 2 != 0 {
                    let o = r.pos;
                    let s = r.string()?;
                    let n = self.push(Node::Str(s), o);
                    props.push((PKey::Name("stack".encode_utf16().collect()), n));
                }
                self.nodes[idx] = Node::Error { props };
                let frame = (mask & 4 != 0).then_some(Frame { node: idx, kind: Kind::Fixed, left: 1, key: None });
                Some((idx, frame))
            }
            tag::ARRAY_BUFFER | tag::RESIZABLE_ARRAY_BUFFER => {
                let n = r.varint()? as usize;
                if t == tag::RESIZABLE_ARRAY_BUFFER {
                    r.varint()?;
                }
                let start = r.pos;
                r.skip(n)?;
                Some((self.object(Node::Buffer { start, len: n }, at), None))
            }
            tag::VIEW => {
                let kind = r.byte()?;
                let off = r.varint()?;
                let len1 = r.varint()?;
                let idx = self.object(Node::View { kind, off, len1, buf: usize::MAX }, at);
                Some((idx, Some(Frame { node: idx, kind: Kind::Fixed, left: 1, key: None })))
            }
            tag::BLOB => {
                let idx = self.object(Node::Other, at);
                let mime = r.string()?;
                let n = r.varint()?;
                r.skip(usize::try_from(n).ok()?)?;
                self.nodes[idx] = Node::Blob { mime, size: n };
                Some((idx, None))
            }
            tag::IMAGE_DATA => {
                let idx = self.object(Node::Other, at);
                r.varint()?;
                r.varint()?;
                r.skip(1)?;
                Some((idx, Some(Frame { node: idx, kind: Kind::Fixed, left: 1, key: None })))
            }
            tag::DOMPOINT => {
                let idx = self.object(Node::Other, at);
                r.skip(1 + 32)?;
                Some((idx, None))
            }
            tag::DOMMATRIX => {
                let idx = self.object(Node::Other, at);
                r.skip(1)?;
                let is2d = r.byte()?;
                r.skip(if is2d != 0 { 48 } else { 128 })?;
                Some((idx, None))
            }
            tag::DOMEXCEPTION => {
                let idx = self.object(Node::Other, at);
                r.skip(1)?;
                r.string()?;
                r.string()?;
                let mask = r.byte()?;
                if mask & 1 != 0 {
                    r.string()?;
                }
                if mask & 2 != 0 {
                    r.skip(8)?;
                }
                if mask & 4 != 0 {
                    r.skip(8)?;
                }
                Some((idx, None))
            }
            _ => None, // attachment tags and reserved ones: not storage bytes
        }
    }

    fn resolve(&self, mut i: usize) -> usize {
        for _ in 0..4 {
            match self.nodes[i] {
                Node::Ref(id) => i = self.id_node[id as usize],
                _ => return i,
            }
        }
        i
    }

    fn own<'b>(&'b self, props: &'b [(PKey, usize)], name: &[u16]) -> Option<usize> {
        props.iter().find(|(k, _)| matches!(k, PKey::Name(n) if n == name)).map(|(_, v)| *v)
    }

    fn root(&self) -> usize {
        0
    }
}

// ---------------------------------------------------------------------------
// Evaluating a key path (§7.5 "evaluate a key path on a value")
// ---------------------------------------------------------------------------

/// What evaluation produced: a node of the clone, or a value synthesized by
/// one of the special identifiers (`length`, `size`, `type`), or the list a
/// sequence key path builds.
#[derive(Debug)]
pub enum Val {
    Node(usize),
    Num(f64),
    Str(Vec<u16>),
    List(Vec<Val>),
}

const LENGTH: &[u16] = &[0x6c, 0x65, 0x6e, 0x67, 0x74, 0x68];
const SIZE: &[u16] = &[0x73, 0x69, 0x7a, 0x65];
const TYPE: &[u16] = &[0x74, 0x79, 0x70, 0x65];

impl Parsed<'_> {
    /// `None` = failure.
    pub fn eval(&self, kp: &KeyPath) -> Option<Val> {
        match kp {
            KeyPath::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for p in items {
                    out.push(self.eval_one(self.root(), p)?);
                }
                Some(Val::List(out))
            }
            KeyPath::String(p) => self.eval_one(self.root(), p),
        }
    }

    fn eval_one(&self, start: usize, path: &[u16]) -> Option<Val> {
        let mut v = Val::Node(start);
        if path.is_empty() {
            return Some(v);
        }
        for ident in path.split(|&c| c == b'.' as u16) {
            v = match v {
                Val::Str(s) if ident == LENGTH => Val::Num(s.len() as f64),
                Val::Num(_) | Val::Str(_) | Val::List(_) => return None,
                Val::Node(i) => {
                    let i = self.resolve(i);
                    match &self.nodes[i] {
                        Node::Str(s) if ident == LENGTH => Val::Num(s.len() as f64),
                        Node::StrObj(s) if ident == LENGTH => Val::Num(s.len() as f64),
                        Node::Arr { len, .. } if ident == LENGTH => Val::Num(*len as f64),
                        Node::Blob { size, .. } if ident == SIZE => Val::Num(*size as f64),
                        Node::Blob { mime, .. } if ident == TYPE => Val::Str(mime.clone()),
                        Node::Obj { props, .. } | Node::Arr { props, .. } | Node::Error { props } => {
                            let c = self.own(props, ident)?;
                            if matches!(self.nodes[self.resolve(c)], Node::Undefined) {
                                return None;
                            }
                            Val::Node(c)
                        }
                        _ => return None,
                    }
                }
            };
        }
        Some(v)
    }

    /// "Convert a value to a key" on an evaluation result. `None` = invalid.
    pub fn to_key(&self, v: &Val) -> Option<Key> {
        let mut seen = Vec::new();
        self.convert(v, &mut seen, 0)
    }

    fn convert(&self, v: &Val, seen: &mut Vec<usize>, depth: u32) -> Option<Key> {
        if depth > 100_000 {
            return None;
        }
        match v {
            Val::Num(d) => (!d.is_nan()).then_some(Key::Number(*d)),
            Val::Str(s) => Some(Key::String(s.clone())),
            Val::List(items) => {
                let mut keys = Vec::with_capacity(items.len());
                for it in items {
                    keys.push(self.convert(it, seen, depth + 1)?);
                }
                Some(Key::Array(keys))
            }
            Val::Node(i) => self.convert_node(*i, seen, depth),
        }
    }

    fn convert_node(&self, i: usize, seen: &mut Vec<usize>, depth: u32) -> Option<Key> {
        let i = self.resolve(i);
        match &self.nodes[i] {
            Node::Number(d) => (!d.is_nan()).then_some(Key::Number(*d)),
            Node::Date(d) => (!d.is_nan()).then_some(Key::Date(*d)),
            Node::Str(s) => Some(Key::String(s.clone())),
            Node::Buffer { .. } | Node::View { .. } => self.binary(i).map(Key::Binary),
            Node::Arr { len, props, .. } => {
                if seen.contains(&i) {
                    return None;
                }
                seen.push(i);
                let mut keys = Vec::with_capacity(*len as usize);
                for ix in 0..*len {
                    let c = props.iter().find(|(k, _)| *k == PKey::Index(ix)).map(|(_, v)| *v)?;
                    keys.push(self.convert(&Val::Node(c), seen, depth + 1)?);
                }
                Some(Key::Array(keys))
            }
            _ => None,
        }
    }

    /// The bytes a buffer source holds (an ArrayBuffer, or a view's window).
    fn binary(&self, i: usize) -> Option<Vec<u8>> {
        match &self.nodes[i] {
            Node::Buffer { start, len } => Some(self.bytes[*start..*start + *len].to_vec()),
            Node::View { kind, off, len1, buf } => {
                let b = self.resolve(*buf);
                let Node::Buffer { start, len } = self.nodes[b] else { return None };
                let elem: u64 = match kind {
                    1..=3 | 12 => 1,
                    4 | 5 | 13 => 2,
                    6..=8 => 4,
                    9..=11 => 8,
                    _ => return None,
                };
                let n = if *len1 == 0 { (len as u64).saturating_sub(*off) / elem * elem } else { (len1 - 1).saturating_mul(elem) };
                if off.saturating_add(n) > len as u64 {
                    return None;
                }
                let s = start + *off as usize;
                Some(self.bytes[s..s + n as usize].to_vec())
            }
            _ => None,
        }
    }

    /// "Extract a key from a value using a key path": `Ok(Some(key))`,
    /// `Ok(None)` = the path evaluated but is not a valid key (invalid),
    /// `Err(())` = the path did not evaluate (failure).
    pub fn extract(&self, kp: &KeyPath) -> Result<Option<Key>, ()> {
        let v = self.eval(kp).ok_or(())?;
        Ok(self.to_key(&v))
    }

    /// The multiEntry form: an array at the path yields its valid, distinct
    /// members (holes and invalid members skipped); anything else converts as
    /// usual. Same three outcomes as [`Parsed::extract`].
    pub fn extract_multi(&self, kp: &KeyPath) -> Result<Option<Key>, ()> {
        let v = self.eval(kp).ok_or(())?;
        if let Val::Node(i) = v {
            let i = self.resolve(i);
            if let Node::Arr { len, props, .. } = &self.nodes[i] {
                let mut seen = vec![i];
                let mut out: Vec<Key> = Vec::new();
                let mut enc: Vec<Vec<u8>> = Vec::new();
                for ix in 0..*len {
                    let Some(c) = props.iter().find(|(k, _)| *k == PKey::Index(ix)).map(|(_, v)| *v) else { continue };
                    if let Some(k) = self.convert(&Val::Node(c), &mut seen, 1) {
                        let e = k.encode();
                        if !enc.contains(&e) {
                            enc.push(e);
                            out.push(k);
                        }
                    }
                }
                return Ok(Some(Key::Array(out)));
            }
            return Ok(self.to_key(&Val::Node(i)));
        }
        Ok(self.to_key(&v))
    }

    /// "Check that a key could be injected into a value" (§7.6).
    pub fn can_inject(&self, path: &[u16]) -> bool {
        let idents: Vec<&[u16]> = path.split(|&c| c == b'.' as u16).collect();
        let mut v = self.root();
        for ident in &idents[..idents.len() - 1] {
            let i = self.resolve(v);
            match &self.nodes[i] {
                Node::Obj { props, .. } | Node::Arr { props, .. } => match self.own(props, ident) {
                    None => return true,
                    Some(c) => v = c,
                },
                _ => return false,
            }
        }
        matches!(self.nodes[self.resolve(v)], Node::Obj { .. } | Node::Arr { .. })
    }
}

// ---------------------------------------------------------------------------
// Injecting a key (§7.7)
// ---------------------------------------------------------------------------

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn put_name(out: &mut Vec<u8>, name: &[u16]) {
    if name.iter().all(|&c| c <= 0xFF) {
        out.push(tag::STRING_LATIN1);
        put_varint(out, name.len() as u64);
        out.extend(name.iter().map(|&c| c as u8));
    } else {
        out.push(tag::STRING_UTF16);
        put_varint(out, name.len() as u64);
        for &c in name {
            out.extend_from_slice(&c.to_le_bytes());
        }
    }
}

/// The wire form of a number value, as the serializer writes one.
pub fn number_bytes(d: f64) -> Vec<u8> {
    let mut out = Vec::with_capacity(9);
    let i = d as i32;
    if i as f64 == d && !(i == 0 && d.is_sign_negative()) {
        out.push(tag::INT32);
        put_varint(&mut out, ((i << 1) ^ (i >> 31)) as u32 as u64);
    } else {
        out.push(tag::DOUBLE);
        out.extend_from_slice(&d.to_le_bytes());
    }
    out
}

/// "Inject a key into a value using a key path": returns the new bytes with
/// `key` (a generated key — always a number) at `path`. `None` when
/// [`Parsed::can_inject`] would have said no.
pub fn inject(bytes: &[u8], path: &[u16], key: f64) -> Option<Vec<u8>> {
    let p = Parsed::parse(bytes)?;
    let idents: Vec<&[u16]> = path.split(|&c| c == b'.' as u16).collect();
    let (last, mids) = idents.split_last()?;
    let mut v = p.root();
    let mut missing_from: Option<usize> = None;
    for (n, ident) in mids.iter().enumerate() {
        let i = p.resolve(v);
        match &p.nodes[i] {
            Node::Obj { props, .. } | Node::Arr { props, .. } => match p.own(props, ident) {
                None => {
                    v = i;
                    missing_from = Some(n);
                    break;
                }
                Some(c) => v = c,
            },
            _ => return None,
        }
    }
    let target = p.resolve(v);
    let (props, end) = match &p.nodes[target] {
        Node::Obj { props, end } | Node::Arr { props, end, .. } => (props, *end),
        _ => return None,
    };
    let value = number_bytes(key);
    // The edit: replace [at, at + remove) with `insert`, creating `created`
    // new objects there.
    let (at, remove, insert, created) = match missing_from {
        Some(n) => {
            // { …, mids[n]: { mids[n+1]: { … last: key } } }
            let mut ins = Vec::new();
            let chain = &mids[n..];
            for ident in chain {
                put_name(&mut ins, ident);
                ins.push(tag::OBJECT);
            }
            put_name(&mut ins, last);
            ins.extend_from_slice(&value);
            ins.extend(core::iter::repeat_n(tag::END, chain.len()));
            (end, 0usize, ins, chain.len() as u32)
        }
        None => match p.own(props, last) {
            // An own property that is `undefined` (that is why extraction
            // failed): CreateDataProperty replaces its value in place.
            Some(c) => {
                if !matches!(p.nodes[c], Node::Undefined) {
                    return None;
                }
                (p.offsets[c], 1usize, value, 0)
            }
            None => {
                let mut ins = Vec::new();
                put_name(&mut ins, last);
                ins.extend_from_slice(&value);
                (end, 0usize, ins, 0)
            }
        },
    };
    // Objects whose tag is at or after `at` move up by `created` ids.
    let first_shifted = p.id_offset.iter().position(|&o| o >= at).unwrap_or(p.id_offset.len()) as u32;
    let mut out = Vec::with_capacity(bytes.len() + insert.len() + 8);
    let mut pos = 0usize;
    let mut refs = p.refs.iter().peekable();
    let mut inserted = false;
    while pos < bytes.len() || !inserted {
        if !inserted && pos == at {
            out.extend_from_slice(&insert);
            pos += remove;
            inserted = true;
            continue;
        }
        if let Some(&&(ro, rl, id)) = refs.peek() {
            if ro == pos {
                refs.next();
                out.push(tag::OBJECT_REF);
                let id = if created > 0 && id >= first_shifted { id + created } else { id };
                put_varint(&mut out, id as u64);
                pos += rl;
                continue;
            }
        }
        if pos >= bytes.len() {
            break;
        }
        // Copy up to the next point of interest.
        let mut next = bytes.len();
        if !inserted {
            next = next.min(at);
        }
        if let Some(&&(ro, _, _)) = refs.peek() {
            next = next.min(ro);
        }
        out.extend_from_slice(&bytes[pos..next]);
        pos = next;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    /// Hand-built storage bytes.
    fn hdr() -> Vec<u8> {
        let mut v = MAGIC.to_vec();
        v.push(VERSION);
        v.push(FLAG_STORAGE);
        v
    }

    fn name(out: &mut Vec<u8>, s: &str) {
        put_name(out, &u(s));
    }

    #[test]
    fn evaluates_nested_paths_lengths_and_lists() {
        // { a: { b: 5 }, s: "hey", arr: [1, 2] }
        let mut b = hdr();
        b.push(tag::OBJECT);
        name(&mut b, "a");
        b.push(tag::OBJECT);
        name(&mut b, "b");
        b.extend(number_bytes(5.0));
        b.push(tag::END);
        name(&mut b, "s");
        name(&mut b, "hey");
        name(&mut b, "arr");
        b.push(tag::ARRAY);
        put_varint(&mut b, 2);
        b.push(tag::KEY_INDEX);
        put_varint(&mut b, 0);
        b.extend(number_bytes(1.0));
        b.push(tag::KEY_INDEX);
        put_varint(&mut b, 1);
        b.extend(number_bytes(2.0));
        b.push(tag::END);
        b.push(tag::END);
        let p = Parsed::parse(&b).expect("parses");
        let kp = |s: &str| KeyPath::String(u(s));
        assert_eq!(p.extract(&kp("a.b")), Ok(Some(Key::Number(5.0))));
        assert_eq!(p.extract(&kp("s.length")), Ok(Some(Key::Number(3.0))));
        assert_eq!(p.extract(&kp("arr.length")), Ok(Some(Key::Number(2.0))));
        assert_eq!(p.extract(&kp("arr")), Ok(Some(Key::Array(vec![Key::Number(1.0), Key::Number(2.0)]))));
        assert_eq!(p.extract(&kp("missing")), Err(()));
        assert_eq!(p.extract(&kp("a")), Ok(None), "an object is not a key");
        assert_eq!(
            p.extract(&KeyPath::List(vec![u("s"), u("a.b")])),
            Ok(Some(Key::Array(vec![Key::String(u("hey")), Key::Number(5.0)])))
        );
        assert!(p.can_inject(&u("x")));
        assert!(p.can_inject(&u("q.r.s")));
        assert!(p.can_inject(&u("a.c")));
        assert!(!p.can_inject(&u("s.x")), "cannot inject into a string");
    }

    #[test]
    fn injection_splices_and_renumbers_references() {
        // [ {}, ref→1 ] : the inner object is id 1; injecting a new object at
        // the root's END... use a root object with a later shared child.
        // { o: {}, again: ref(1) }, inject "n.id" = 7 → { o: {}, again: ref(1), n: { id: 7 } }
        let mut b = hdr();
        b.push(tag::OBJECT); // id 0
        name(&mut b, "o");
        b.push(tag::OBJECT); // id 1
        b.push(tag::END);
        name(&mut b, "again");
        b.push(tag::OBJECT_REF);
        put_varint(&mut b, 1);
        b.push(tag::END);
        let out = inject(&b, &u("n.id"), 7.0).expect("injects");
        let p = Parsed::parse(&out).expect("still parses");
        assert_eq!(p.extract(&KeyPath::String(u("n.id"))), Ok(Some(Key::Number(7.0))));
        // The reference still points at `o` (id 1, before the insertion).
        let again = p.eval(&KeyPath::String(u("again"))).unwrap();
        let o = p.eval(&KeyPath::String(u("o"))).unwrap();
        match (again, o) {
            (Val::Node(a), Val::Node(o)) => assert_eq!(p.resolve(a), p.resolve(o)),
            _ => panic!(),
        }

        // Insertion BEFORE a referenced object shifts its id: { a: {}, b: {}, c: ref(2) }
        // inject into a (target "a.x.y") → a new object inside `a` (id 2), so
        // `b` becomes id 3 and `c` must follow it.
        let mut b2 = hdr();
        b2.push(tag::OBJECT); // 0
        name(&mut b2, "a");
        b2.push(tag::OBJECT); // 1
        b2.push(tag::END);
        name(&mut b2, "b");
        b2.push(tag::OBJECT); // 2
        name(&mut b2, "tag");
        name(&mut b2, "B");
        b2.push(tag::END);
        name(&mut b2, "c");
        b2.push(tag::OBJECT_REF);
        put_varint(&mut b2, 2);
        b2.push(tag::END);
        let out = inject(&b2, &u("a.x.y"), 3.0).expect("injects");
        let p = Parsed::parse(&out).expect("parses");
        assert_eq!(p.extract(&KeyPath::String(u("a.x.y"))), Ok(Some(Key::Number(3.0))));
        assert_eq!(p.extract(&KeyPath::String(u("c.tag"))), Ok(Some(Key::String(u("B")))));

        // An own `undefined` is replaced in place.
        let mut b3 = hdr();
        b3.push(tag::OBJECT);
        name(&mut b3, "id");
        b3.push(tag::UNDEFINED);
        name(&mut b3, "z");
        b3.extend(number_bytes(1.0));
        b3.push(tag::END);
        let pb = Parsed::parse(&b3).unwrap();
        assert_eq!(pb.extract(&KeyPath::String(u("id"))), Err(()), "undefined is failure");
        let out = inject(&b3, &u("id"), 1e10).expect("injects");
        let p = Parsed::parse(&out).unwrap();
        assert_eq!(p.extract(&KeyPath::String(u("id"))), Ok(Some(Key::Number(1e10))));
        assert_eq!(p.extract(&KeyPath::String(u("z"))), Ok(Some(Key::Number(1.0))));
    }

    #[test]
    fn deep_values_parse_without_recursion() {
        let depth = 200_000;
        let mut b = hdr();
        for _ in 0..depth {
            b.push(tag::OBJECT);
            name(&mut b, "a");
        }
        b.extend(number_bytes(1.0));
        b.extend(core::iter::repeat_n(tag::END, depth));
        assert!(Parsed::parse(&b).is_some());
        // Corrupt: one END short.
        b.pop();
        assert!(Parsed::parse(&b).is_none());
    }
}
