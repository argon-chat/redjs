//! Native `WebSocket` (WHATWG HTML §9.3) over a hand-rolled RFC 6455 client.
//!
//! ## Why the framing is ours
//!
//! The obvious answer is `tokio-tungstenite`, and it is the wrong one *here*,
//! for three reasons that are specific to this crate:
//!
//! 1. **It brings a tokio runtime into a runtime that has no executor.** redjs
//!    is a single-threaded pump that holds the VM lock across a frame; every
//!    async-shaped API in it (`fetch`, image decode) is already "blocking work
//!    on a worker thread, result handed back through the pump". A second
//!    concurrency model — a multi-thread reactor, its own thread pool, its own
//!    shutdown semantics — would be the only one of its kind in the crate, and
//!    a shipped game player would link it for one API.
//! 2. **TLS would become a second trust store, or a landmine.**
//!    `tokio-tungstenite` pulls either `native-tls` (fine, but then the runtime
//!    is the only thing gained) or `rustls`. rustls 0.23 has no default crypto
//!    provider and *panics* when its features name two — and in the embedder
//!    this crate ships into, `ring` and `aws-lc-rs` both arrive transitively.
//!    That defect cost the host workspace a milestone (its `vn-voice/src/tls.rs`
//!    exists solely to pin one provider). `wss:` here goes through the very same
//!    `native_tls` connector `https:` already uses: platform TLS, no bundled
//!    root store, no C toolchain, no provider ambiguity, and trust that follows
//!    Windows Update rather than our release cadence.
//! 3. **The upgrade is an HTTP/1.1 request, and we own an HTTP/1.1 client.**
//!    [`super::http`] already does connect-with-timeout, TLS handshake with a
//!    cancellable poll loop, and strict head parsing. A WebSocket handshake is
//!    that, plus one header check. Reusing it is ~40 lines; the framing on top
//!    (§5 of RFC 6455) is a few hundred more and is *pure* — which is why most
//!    of the test count below needs no socket at all.
//!
//! ## Runtime ownership: none, on purpose
//!
//! This module owns no async runtime and accepts no injected one. Each
//! connection gets **one dedicated OS thread** that does blocking I/O in
//! [`WS_POLL`]-millisecond slices — the same shape, and the same cancellation
//! discipline, as `http.rs`'s `Cancellable`. One thread means one owner of the
//! TLS session (a `native_tls::TlsStream` is a single state machine and cannot
//! be read and written from two threads), so the loop alternates: flush what JS
//! queued, then read for up to `WS_POLL`. The cost of that is a ~5 ms ceiling on
//! send latency and ~200 no-op `recv` wakeups per second per open socket (about
//! 0.1 % of one core); the benefit is that plaintext and TLS are one code path
//! and nothing in the crate has to grow a reactor.
//!
//! The host workspace's voice stack (`vn-voice`) does run a tokio multi-thread
//! runtime when voice is on. Borrowing it was considered and rejected: it would
//! make `WebSocket` — a *core* web API, available before any media exists —
//! conditional on an optional subsystem of one particular embedder, and would
//! hand redjs a lifetime it does not control. Two independent I/O worlds in one
//! process is the correct outcome, not an accident.
//!
//! ## Threading and delivery
//!
//! The I/O thread **never touches JSC**. It parses frames and pushes
//! [`Incoming`] values into a mutex-guarded inbox; [`pump`] drains every live
//! socket's inbox on the frame thread, with the VM lock held, and dispatches
//! real `Event` / `MessageEvent` / `CloseEvent` objects through `events.rs`'s
//! one dispatch path. This is the `pcm.rs` seam (a `Send + Sync` registry that
//! cannot reach the VM) with `permissions.rs`'s drain-once-per-pump delivery.
//!
//! `bufferedAmount` is **real**: bytes go on when `send()` queues them and come
//! off when the socket has actually taken them. A caller that watches it to
//! decide whether to push more gets a true answer, not a constant 0.
//!
//! ## Deliberate narrowings
//!
//! - **No extensions.** `Sec-WebSocket-Extensions` is not offered, so
//!   `permessage-deflate` is not negotiated and `ws.extensions` is `""` — which
//!   is exactly what a browser reports when the server declines. A server that
//!   answers with an extension we never asked for **fails the connection**
//!   (RFC 6455 §4.1) rather than being ignored, because ignoring it would mean
//!   decoding its frames wrong.
//! - **No `Origin` header.** There is no document origin in this runtime;
//!   `bootstrap.js` publishes a synthetic `app://redpew` `location`, and sending
//!   that as an `Origin` would claim a web origin that does not exist. A server
//!   with an origin allowlist therefore has to allowlist "no origin", which is
//!   the honest description of this client.
//! - **No automatic reconnection, no heartbeat.** Pings from the server are
//!   answered with pongs (RFC 6455 §5.5.2 requires it); we never ping first.

use core::ffi::c_void;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::Shutdown;
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use sha1::Digest;

use super::util::*;
use super::{blob, events, http};
use crate::capi;
use crate::glue;

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Read/write timeout slice on an established socket. Doubles as the ceiling on
/// how long a `send()` waits for the I/O thread to notice it. See the module
/// header for why one thread polls instead of two threads blocking.
pub(crate) const WS_POLL: Duration = Duration::from_millis(5);
/// TCP + TLS ceiling for the opening handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Stall budget for reading the 101 response.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for the peer's Close echo before dropping the TCP.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Ceiling on one reassembled message. A cap that fails loudly beats an
/// allocation that takes the process with it.
pub const MAX_MESSAGE: usize = 64 * 1024 * 1024;
/// Simultaneous open sockets. One thread each; past this a constructor throws
/// rather than quietly exhausting the process's threads.
const MAX_SOCKETS: usize = 64;

const RFC6455_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

// readyState
const CONNECTING: u8 = 0;
const OPEN: u8 = 1;
const CLOSING: u8 = 2;
const CLOSED: u8 = 3;

// ---------------------------------------------------------------------------
// RFC 6455 §5 — framing. Pure; no socket, no JS.
// ---------------------------------------------------------------------------

pub const OP_CONT: u8 = 0x0;
pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
pub const OP_PONG: u8 = 0xa;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub fin: bool,
    pub opcode: u8,
    pub payload: Vec<u8>,
}

/// A violation that must "fail the WebSocket connection" with a specific close
/// code (RFC 6455 §7.4.1). The code is what we send in our Close frame; the
/// message is what goes to stderr, never to JS (a browser's `error` event
/// carries no detail, and neither does ours).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtoError {
    pub code: u16,
    pub message: String,
}

impl ProtoError {
    fn new(code: u16, message: impl Into<String>) -> Self {
        ProtoError { code, message: message.into() }
    }
}

/// Decodes one frame from the front of `buf`.
///
/// `Ok(None)` means "not enough bytes yet" — the caller reads more and retries.
/// `Ok(Some((frame, n)))` consumed `n` bytes.
pub fn parse_frame(buf: &[u8], max_message: usize) -> Result<Option<(Frame, usize)>, ProtoError> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let (b0, b1) = (buf[0], buf[1]);
    let fin = b0 & 0x80 != 0;
    // We negotiate no extensions, so a set RSV bit means the peer is framing
    // something we would decode as garbage.
    if b0 & 0x70 != 0 {
        return Err(ProtoError::new(1002, "a reserved frame bit is set but no extension was negotiated"));
    }
    let opcode = b0 & 0x0f;
    // §5.1: a server MUST NOT mask. Accepting a masked frame would mean
    // silently reading the mask key as payload.
    if b1 & 0x80 != 0 {
        return Err(ProtoError::new(1002, "the server masked a frame"));
    }
    let (len, header) = match b1 & 0x7f {
        126 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            (u16::from_be_bytes([buf[2], buf[3]]) as u64, 4usize)
        }
        127 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            let n = u64::from_be_bytes([buf[2], buf[3], buf[4], buf[5], buf[6], buf[7], buf[8], buf[9]]);
            if n & 0x8000_0000_0000_0000 != 0 {
                return Err(ProtoError::new(1002, "frame length has the reserved high bit set"));
            }
            (n, 10usize)
        }
        n => (n as u64, 2usize),
    };

    let is_control = opcode & 0x08 != 0;
    if is_control {
        // §5.5: control frames are never fragmented and never exceed 125 bytes.
        if !fin {
            return Err(ProtoError::new(1002, "a control frame was fragmented"));
        }
        if len > 125 {
            return Err(ProtoError::new(1002, format!("a control frame carries {len} bytes (max 125)")));
        }
    }
    if !matches!(opcode, OP_CONT | OP_TEXT | OP_BINARY | OP_CLOSE | OP_PING | OP_PONG) {
        return Err(ProtoError::new(1002, format!("unknown opcode 0x{opcode:x}")));
    }
    if len > max_message as u64 {
        return Err(ProtoError::new(1009, format!("a frame of {len} bytes exceeds the {max_message}-byte limit")));
    }
    let len = len as usize;
    if buf.len() < header + len {
        return Ok(None);
    }
    Ok(Some((Frame { fin, opcode, payload: buf[header..header + len].to_vec() }, header + len)))
}

/// Encodes a client frame. Client frames are ALWAYS masked (§5.3) — an unmasked
/// one is a protocol error the server must close on.
pub fn encode_client_frame(fin: bool, opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let n = payload.len();
    let mut out = Vec::with_capacity(n + 14);
    out.push(if fin { 0x80 | opcode } else { opcode });
    if n < 126 {
        out.push(0x80 | n as u8);
    } else if n <= u16::MAX as usize {
        out.push(0x80 | 126);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(n as u64).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    let start = out.len();
    out.extend_from_slice(payload);
    for (i, b) in out[start..].iter_mut().enumerate() {
        *b ^= mask[i & 3];
    }
    out
}

fn random_mask() -> [u8; 4] {
    let mut m = [0u8; 4];
    // A failure here would mean a predictable mask, which is a security
    // property of the protocol (it exists to defeat proxy cache poisoning),
    // not a cosmetic one — so fall back to something that still varies.
    if getrandom::fill(&mut m).is_err() {
        let t = crate::web::now_ms().to_bits();
        m.copy_from_slice(&t.to_le_bytes()[..4]);
    }
    m
}

/// §7.4.1: which close codes may appear on the wire. 1005/1006/1015 are
/// "not sent" sentinels, 1004 is unassigned, and everything under 1000 or
/// above 4999 is out of range.
pub fn is_wire_close_code(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1014) || (3000..=4999).contains(&code)
}

/// Decodes a Close frame body into `(code, reason)`.
///
/// An empty body is "no status received" (1005), which is a legal close, not an
/// error. Anything else malformed fails the connection.
pub fn parse_close_payload(payload: &[u8]) -> Result<(u16, String), ProtoError> {
    if payload.is_empty() {
        return Ok((1005, String::new()));
    }
    if payload.len() == 1 {
        return Err(ProtoError::new(1002, "a Close frame carried a one-byte body"));
    }
    let code = u16::from_be_bytes([payload[0], payload[1]]);
    if !is_wire_close_code(code) {
        return Err(ProtoError::new(1002, format!("a Close frame carried the reserved status code {code}")));
    }
    let reason = std::str::from_utf8(&payload[2..])
        .map_err(|_| ProtoError::new(1007, "a Close frame reason was not valid UTF-8"))?;
    Ok((code, reason.to_string()))
}

fn close_payload(code: u16, reason: &str) -> Vec<u8> {
    let mut p = Vec::with_capacity(2 + reason.len());
    p.extend_from_slice(&code.to_be_bytes());
    p.extend_from_slice(reason.as_bytes());
    p
}

/// Reassembles fragmented messages (§5.4). Control frames pass around it.
#[derive(Default)]
struct Assembler {
    /// The opcode of the message in progress, if any.
    kind: Option<u8>,
    buf: Vec<u8>,
}

#[derive(Debug)]
enum Assembled {
    None,
    Text(String),
    Binary(Vec<u8>),
}

impl Assembler {
    fn push(&mut self, f: &Frame, max_message: usize) -> Result<Assembled, ProtoError> {
        match f.opcode {
            OP_TEXT | OP_BINARY => {
                if self.kind.is_some() {
                    return Err(ProtoError::new(1002, "a new data frame arrived inside a fragmented message"));
                }
                self.kind = Some(f.opcode);
                self.buf.clear();
                self.buf.extend_from_slice(&f.payload);
            }
            OP_CONT => {
                if self.kind.is_none() {
                    return Err(ProtoError::new(1002, "a continuation frame arrived with no message in progress"));
                }
                self.buf.extend_from_slice(&f.payload);
            }
            _ => return Ok(Assembled::None),
        }
        if self.buf.len() > max_message {
            return Err(ProtoError::new(1009, format!("a message exceeds the {max_message}-byte limit")));
        }
        if !f.fin {
            return Ok(Assembled::None);
        }
        let kind = self.kind.take().unwrap_or(OP_BINARY);
        let bytes = core::mem::take(&mut self.buf);
        if kind == OP_TEXT {
            // §8.1: a text message that is not valid UTF-8 fails the connection.
            // Lossy decoding here would hand JS a plausible wrong string.
            let s = String::from_utf8(bytes)
                .map_err(|_| ProtoError::new(1007, "a text message was not valid UTF-8"))?;
            Ok(Assembled::Text(s))
        } else {
            Ok(Assembled::Binary(bytes))
        }
    }
}

// ---------------------------------------------------------------------------
// Base64 (handshake key only — no dependency worth adding for 40 lines)
// ---------------------------------------------------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

// ---------------------------------------------------------------------------
// The opening handshake (RFC 6455 §4). Pure.
// ---------------------------------------------------------------------------

/// `Sec-WebSocket-Accept` for a given `Sec-WebSocket-Key`.
pub fn accept_key(key_b64: &str) -> String {
    let mut h = sha1::Sha1::new();
    h.update(key_b64.as_bytes());
    h.update(RFC6455_GUID);
    b64_encode(&h.finalize())
}

/// The upgrade request exactly as it goes on the wire. `u` is the *http/https*
/// form of the URL (see [`to_http_url`]) so the `Host` header and default ports
/// come out of the same code paths `fetch` uses.
pub fn handshake_request(u: &url::Url, key_b64: &str, protocols: &[String]) -> String {
    let mut target = u.path().to_string();
    if target.is_empty() {
        target.push('/');
    }
    if let Some(q) = u.query() {
        target.push('?');
        target.push_str(q);
    }
    let host = u.host_str().unwrap_or("");
    let host_header = match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    };
    let mut s = String::with_capacity(256);
    s.push_str(&format!("GET {target} HTTP/1.1\r\n"));
    s.push_str(&format!("Host: {host_header}\r\n"));
    s.push_str("Upgrade: websocket\r\n");
    s.push_str("Connection: Upgrade\r\n");
    s.push_str(&format!("Sec-WebSocket-Key: {key_b64}\r\n"));
    s.push_str("Sec-WebSocket-Version: 13\r\n");
    if !protocols.is_empty() {
        s.push_str(&format!("Sec-WebSocket-Protocol: {}\r\n", protocols.join(", ")));
    }
    s.push_str("User-Agent: RedPew\r\n");
    s.push_str("\r\n");
    s
}

/// Checks the 101 response. Returns the negotiated `(protocol, extensions)`.
///
/// Every branch here is a connection failure rather than a warning: a peer that
/// answers a WebSocket upgrade with anything else is not speaking the protocol,
/// and continuing would mean framing its bytes as if it were.
pub fn validate_handshake(head: &http::Head, key_b64: &str, offered: &[String]) -> Result<(String, String), String> {
    if head.status != 101 {
        return Err(format!(
            "the server answered {} {} instead of 101 Switching Protocols",
            head.status,
            head.reason.trim()
        ));
    }
    let h = |n: &str| http::header(&head.headers, n).unwrap_or("");
    if !h("upgrade").eq_ignore_ascii_case("websocket") {
        return Err(format!("the 101 response has Upgrade: {:?}, expected \"websocket\"", h("upgrade")));
    }
    if !h("connection").split(',').any(|t| t.trim().eq_ignore_ascii_case("upgrade")) {
        return Err(format!("the 101 response has Connection: {:?}, expected \"Upgrade\"", h("connection")));
    }
    let expected = accept_key(key_b64);
    if h("sec-websocket-accept") != expected {
        return Err("the 101 response carried a wrong Sec-WebSocket-Accept".to_string());
    }
    // §4.1: an extension the client did not offer fails the connection. We
    // offer none, so any non-empty value is a violation.
    let ext = h("sec-websocket-extensions").trim().to_string();
    if !ext.is_empty() {
        return Err(format!("the server selected the extension {ext:?}, which was never offered"));
    }
    let proto = h("sec-websocket-protocol").trim().to_string();
    if !proto.is_empty() && !offered.iter().any(|p| p == &proto) {
        return Err(format!("the server selected the subprotocol {proto:?}, which was never offered"));
    }
    Ok((proto, ext))
}

/// `ws://x` → `http://x`, `wss://x` → `https://x`. The two pairs share default
/// ports and host syntax, so this is a rename, not a reinterpretation — and it
/// lets the connect/TLS path be literally `fetch`'s.
pub fn to_http_url(ws: &url::Url) -> Result<url::Url, String> {
    let s = ws.as_str();
    let mapped = match ws.scheme() {
        "ws" | "wss" => format!("http{}", &s[2..]),
        other => return Err(format!("{other}: is not a WebSocket scheme")),
    };
    url::Url::parse(&mapped).map_err(|e| format!("invalid URL {s:?}: {e}"))
}

// ---------------------------------------------------------------------------
// The URL and protocol rules the constructor enforces (HTML §9.3.1). Pure.
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum UrlError {
    /// SyntaxError — the URL or the protocol list is malformed.
    Syntax(String),
}

/// HTML "The WebSocket(url, protocols) constructor": parse, map http(s) onto
/// ws(s), refuse any other scheme, refuse a fragment.
pub fn parse_ws_url(raw: &str) -> Result<url::Url, UrlError> {
    // No base URL exists in this runtime, so a relative reference cannot be
    // resolved — which the spec also treats as a SyntaxError.
    let mut u = url::Url::parse(raw).map_err(|e| UrlError::Syntax(format!("invalid URL {raw:?}: {e}")))?;
    match u.scheme() {
        "ws" | "wss" => {}
        "http" => {
            let _ = u.set_scheme("ws");
        }
        "https" => {
            let _ = u.set_scheme("wss");
        }
        other => {
            return Err(UrlError::Syntax(format!(
                "the URL's scheme must be \"ws\" or \"wss\", not {other:?}"
            )))
        }
    }
    if u.fragment().is_some() {
        return Err(UrlError::Syntax(format!("the URL {raw:?} has a fragment, which a WebSocket URL may not")));
    }
    if u.host_str().unwrap_or("").is_empty() {
        return Err(UrlError::Syntax(format!("the URL {raw:?} has no host")));
    }
    Ok(u)
}

/// RFC 7230 token characters — the grammar a subprotocol name must match.
fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

pub fn validate_protocols(list: &[String]) -> Result<(), UrlError> {
    for (i, p) in list.iter().enumerate() {
        if !is_token(p) {
            return Err(UrlError::Syntax(format!("{p:?} is not a valid WebSocket subprotocol name")));
        }
        if list[..i].iter().any(|q| q.eq_ignore_ascii_case(p)) {
            return Err(UrlError::Syntax(format!("the subprotocol {p:?} is listed twice")));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The Send + Sync half: connection state the I/O thread and the pump share.
// Nothing here can reach the VM.
// ---------------------------------------------------------------------------

/// What the I/O thread hands the pump. One variant per event JS will see.
pub enum Incoming {
    Open,
    Text(String),
    Binary(Vec<u8>),
    /// The `error` event. `detail` never reaches JS (browsers expose nothing
    /// either); it goes to stderr so a failure is diagnosable.
    Error(String),
    Closed { code: u16, reason: String, clean: bool },
}

enum Out {
    /// A data frame from `send()`. `billed` is what `bufferedAmount` was
    /// charged and must be refunded once the bytes are on the socket.
    Data { opcode: u8, payload: Vec<u8>, billed: u64 },
    /// A control frame, already complete. Jumps the data queue.
    Control { opcode: u8, payload: Vec<u8> },
    /// The closing handshake. Sent after everything queued before it.
    Close { code: u16, reason: String },
}

#[derive(Default)]
struct Meta {
    protocol: String,
    extensions: String,
}

pub struct Shared {
    state: AtomicU8,
    buffered: AtomicU64,
    out: Mutex<VecDeque<Out>>,
    inbox: Mutex<Vec<Incoming>>,
    meta: Mutex<Meta>,
    /// Reused from `http.rs`: holds a dup of the socket so the JS thread can
    /// cut a blocked connect or handshake short.
    cancel: Arc<http::Cancel>,
}

impl Shared {
    fn new() -> Arc<Shared> {
        Arc::new(Shared {
            state: AtomicU8::new(CONNECTING),
            buffered: AtomicU64::new(0),
            out: Mutex::new(VecDeque::new()),
            inbox: Mutex::new(Vec::new()),
            meta: Mutex::new(Meta::default()),
            cancel: http::Cancel::new(),
        })
    }

    fn ready_state(&self) -> u8 {
        self.state.load(Ordering::SeqCst)
    }

    fn set_state(&self, s: u8) {
        self.state.store(s, Ordering::SeqCst);
    }

    fn post(&self, ev: Incoming) {
        self.inbox.lock().unwrap().push(ev);
    }

    fn drain_inbox(&self) -> Vec<Incoming> {
        core::mem::take(&mut *self.inbox.lock().unwrap())
    }
}

static LIVE_COUNT: AtomicUsize = AtomicUsize::new(0);

fn conns() -> &'static Mutex<HashMap<u64, Arc<Shared>>> {
    static C: OnceLock<Mutex<HashMap<u64, Arc<Shared>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn shared_of(id: u64) -> Option<Arc<Shared>> {
    conns().lock().unwrap().get(&id).cloned()
}

/// Every socket id that is not yet CLOSED. Diagnostics, and what `MAX_SOCKETS`
/// counts — a closed socket's entry lives on until its JS object is collected
/// (so `ws.protocol` still reads back afterwards) but costs no thread.
pub fn live_sockets() -> Vec<u64> {
    let mut v: Vec<u64> = conns()
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, s)| s.ready_state() != CLOSED)
        .map(|(id, _)| *id)
        .collect();
    v.sort_unstable();
    v
}

/// `*mut c_void` is not `Send`; the I/O thread carries the context key only to
/// name which pump queue it belongs to and never dereferences it off-thread.
#[derive(Clone, Copy)]
struct Ctx(*mut c_void);
unsafe impl Send for Ctx {}

impl Ctx {
    fn key(self) -> usize {
        self.0 as usize
    }
}

// ---------------------------------------------------------------------------
// The I/O thread
// ---------------------------------------------------------------------------

/// Writes `buf` in full, retrying across `WS_POLL` slices. Always resumes from
/// the same offset of the same buffer — a TLS write that reports a partial
/// accept must never be retried with different bytes.
fn write_all_polling(s: &mut http::Stream, buf: &[u8], sh: &Shared) -> Result<(), String> {
    let mut at = 0usize;
    let mut stalled = Duration::ZERO;
    while at < buf.len() {
        if sh.cancel.is_aborted() {
            return Err("the connection was closed locally".into());
        }
        match s.write(&buf[at..]) {
            Ok(0) => return Err("the connection accepted no more bytes".into()),
            Ok(n) => {
                at += n;
                stalled = Duration::ZERO;
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                stalled += WS_POLL;
                if stalled >= HANDSHAKE_TIMEOUT {
                    return Err("the connection stalled while sending".into());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("send failed: {e}")),
        }
    }
    s.flush().map_err(|e| format!("send failed: {e}"))
}

struct LoopEnd {
    code: u16,
    reason: String,
    clean: bool,
    /// Set when the connection *failed* rather than closed — an `error` event
    /// precedes the `close` event, as in a browser.
    failure: Option<String>,
}

fn io_thread(ctx: Ctx, id: u64, sh: Arc<Shared>, ws_url: url::Url, protocols: Vec<String>) {
    let end = match connect_and_run(&sh, ws_url, protocols) {
        Ok(end) => end,
        Err(msg) => LoopEnd { code: 1006, reason: String::new(), clean: false, failure: Some(msg) },
    };
    if let Some(ref why) = end.failure {
        // The JS-visible `error` event carries no detail, by specification. The
        // reason still has to be findable, so it goes here.
        eprintln!("[js:ws] {why}");
        sh.post(Incoming::Error(why.clone()));
    }
    sh.set_state(CLOSED);
    sh.post(Incoming::Closed { code: end.code, reason: end.reason, clean: end.clean });
    let _ = (ctx, id);
}

fn connect_and_run(sh: &Arc<Shared>, ws_url: url::Url, protocols: Vec<String>) -> Result<LoopEnd, String> {
    let http_url = to_http_url(&ws_url)?;
    let stream = http::connect(&http_url, CONNECT_TIMEOUT, &sh.cancel).map_err(|e| e.message)?;

    let mut key = [0u8; 16];
    let _ = getrandom::fill(&mut key);
    let key_b64 = b64_encode(&key);
    let req = handshake_request(&http_url, &key_b64, &protocols);

    let mut c = http::Cancellable::new(stream, &sh.cancel, HANDSHAKE_TIMEOUT);
    c.write_all(req.as_bytes()).map_err(|e| format!("could not send the WebSocket upgrade: {e}"))?;
    c.flush().map_err(|e| format!("could not send the WebSocket upgrade: {e}"))?;
    let (head, rest) = http::read_head(&mut c, &sh.cancel).map_err(|e| e.message)?;
    let (protocol, extensions) = validate_handshake(&head, &key_b64, &protocols)?;
    let mut stream = c.into_inner();

    // From here on the loop owns the cadence, not the 200 ms fetch slice.
    let _ = stream.tcp().set_read_timeout(Some(WS_POLL));
    let _ = stream.tcp().set_write_timeout(Some(WS_POLL));

    {
        let mut m = sh.meta.lock().unwrap();
        m.protocol = protocol;
        m.extensions = extensions;
    }
    sh.set_state(OPEN);
    sh.post(Incoming::Open);

    Ok(run_loop(sh, &mut stream, rest))
}

fn run_loop(sh: &Arc<Shared>, stream: &mut http::Stream, prefix: Vec<u8>) -> LoopEnd {
    let mut acc = prefix;
    let mut asm = Assembler::default();
    let mut close_sent = false;
    let mut closing_deadline: Option<Instant> = None;
    let mut buf = [0u8; 16 * 1024];

    macro_rules! fail {
        ($pe:expr) => {{
            let pe: ProtoError = $pe;
            if !close_sent {
                let f = encode_client_frame(true, OP_CLOSE, &close_payload(pe.code, ""), random_mask());
                let _ = write_all_polling(stream, &f, sh);
            }
            let _ = stream.tcp().shutdown(Shutdown::Both);
            return LoopEnd { code: 1006, reason: String::new(), clean: false, failure: Some(pe.message) };
        }};
    }

    // Parse FIRST, every time around. The handshake's head read over-reads,
    // so `prefix` routinely already holds whole frames — and if the peer then
    // waits for us (a server that sent Close and is waiting for the echo), no
    // further byte ever arrives to trigger a parse. Reading before parsing
    // deadlocked exactly that case until the socket timed out.
    loop {
        // -- 1. everything already buffered ---------------------------------
        loop {
            let parsed = match parse_frame(&acc, MAX_MESSAGE) {
                Ok(p) => p,
                Err(pe) => fail!(pe),
            };
            let Some((f, used)) = parsed else { break };
            acc.drain(..used);
            match f.opcode {
                OP_PING => {
                    // §5.5.2: answer with a pong carrying the same payload.
                    sh.out.lock().unwrap().push_back(Out::Control { opcode: OP_PONG, payload: f.payload });
                }
                OP_PONG => {}
                OP_CLOSE => {
                    let (code, reason) = match parse_close_payload(&f.payload) {
                        Ok(v) => v,
                        Err(pe) => fail!(pe),
                    };
                    sh.set_state(CLOSING);
                    if !close_sent {
                        // Echo the peer's code (1005 means "no body", which we
                        // must not put on the wire — echo an empty body).
                        let body = if code == 1005 { Vec::new() } else { close_payload(code, "") };
                        let wire = encode_client_frame(true, OP_CLOSE, &body, random_mask());
                        let _ = write_all_polling(stream, &wire, sh);
                    }
                    let _ = stream.tcp().shutdown(Shutdown::Both);
                    return LoopEnd { code, reason, clean: true, failure: None };
                }
                _ => match asm.push(&f, MAX_MESSAGE) {
                    Ok(Assembled::None) => {}
                    Ok(Assembled::Text(s)) => sh.post(Incoming::Text(s)),
                    Ok(Assembled::Binary(b)) => sh.post(Incoming::Binary(b)),
                    Err(pe) => fail!(pe),
                },
            }
        }

        // -- 2. flush whatever JS queued -----------------------------------
        loop {
            let next = {
                let mut q = sh.out.lock().unwrap();
                // Control frames (pong, and our own close-echo) jump the queue.
                let ctrl = q.iter().position(|o| matches!(o, Out::Control { .. }));
                match ctrl {
                    Some(i) => q.remove(i),
                    None => q.pop_front(),
                }
            };
            let Some(item) = next else { break };
            let (wire, billed) = match item {
                Out::Data { opcode, payload, billed } => {
                    (encode_client_frame(true, opcode, &payload, random_mask()), billed)
                }
                Out::Control { opcode, payload } => {
                    (encode_client_frame(true, opcode, &payload, random_mask()), 0)
                }
                Out::Close { code, reason } => {
                    if close_sent {
                        continue;
                    }
                    close_sent = true;
                    closing_deadline = Some(Instant::now() + CLOSE_TIMEOUT);
                    (encode_client_frame(true, OP_CLOSE, &close_payload(code, &reason), random_mask()), 0)
                }
            };
            if let Err(e) = write_all_polling(stream, &wire, sh) {
                let _ = stream.tcp().shutdown(Shutdown::Both);
                return LoopEnd { code: 1006, reason: String::new(), clean: false, failure: Some(e) };
            }
            // Only now are the bytes really gone: `bufferedAmount` drops here,
            // not when send() returned.
            if billed > 0 {
                sh.buffered.fetch_sub(billed, Ordering::SeqCst);
            }
        }

        // -- 3. a local abort ----------------------------------------------
        // Reachable through one race: `close()` read CONNECTING (and so took
        // the "fail the connection" branch) in the instant after this thread
        // had already published OPEN.
        if sh.cancel.is_aborted() {
            let _ = stream.tcp().shutdown(Shutdown::Both);
            return LoopEnd {
                code: 1006,
                reason: String::new(),
                clean: false,
                failure: Some("the connection was cut locally before the closing handshake".into()),
            };
        }

        // -- 4. read; the next pass parses whatever it brought -------------
        match stream.read(&mut buf) {
            Ok(0) => {
                // EOF. Clean only if both close frames crossed the wire.
                let _ = stream.tcp().shutdown(Shutdown::Both);
                return LoopEnd {
                    code: 1006,
                    reason: String::new(),
                    clean: false,
                    failure: Some("the connection closed without a closing handshake".into()),
                };
            }
            Ok(n) => acc.extend_from_slice(&buf[..n]),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                if let Some(d) = closing_deadline {
                    if Instant::now() >= d {
                        let _ = stream.tcp().shutdown(Shutdown::Both);
                        return LoopEnd {
                            code: 1006,
                            reason: String::new(),
                            clean: false,
                            failure: Some("the peer never answered the closing handshake".into()),
                        };
                    }
                }
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                let _ = stream.tcp().shutdown(Shutdown::Both);
                return LoopEnd {
                    code: 1006,
                    reason: String::new(),
                    clean: false,
                    failure: Some(format!("read failed: {e}")),
                };
            }
        }
    }
}

// ---------------------------------------------------------------------------
// JS side: per-context classes
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum BinaryType {
    Blob,
    ArrayBuffer,
}

pub struct WsData {
    id: u64,
    url: String,
    /// `MessageEvent.origin` — the serialization of the URL's origin, which is
    /// what a browser puts there for a socket message.
    origin: String,
    /// Browser default is `"blob"`, and this runtime has a real `Blob`, so the
    /// default is `"blob"` here too. Choosing `"arraybuffer"` because it is
    /// cheaper would be a silent divergence in the one place an app is most
    /// likely to feel it (`typeof event.data`).
    binary_type: BinaryType,
}

/// The connection record outlives the close event on purpose — `ws.protocol`
/// and `ws.readyState` must still read back afterwards, as they do in a
/// browser. It is released when the JS object is collected, which is what this
/// finalizer-driven drop is for. Runs on a GC thread; touches no JS, and the
/// registry lock is never held across an allocation, so it cannot deadlock the
/// VM thread.
impl Drop for WsData {
    fn drop(&mut self) {
        conns().lock().unwrap().remove(&self.id);
    }
}

struct WsClasses {
    socket: Class,
    socket_class: capi::JSClassRef,
    message_event: Class,
    close_event: Class,
    /// Bag holding `onopen`/`onmessage`/`onerror`/`onclose`, so an unset
    /// handler reads back as `null` the way it does in a browser.
    s_handlers: *mut c_void,
}

unsafe impl Send for WsClasses {}
unsafe impl Sync for WsClasses {}

fn classes_reg() -> &'static Mutex<HashMap<usize, &'static WsClasses>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static WsClasses>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> &'static WsClasses {
    classes_reg().lock().unwrap().get(&(g as usize)).copied().expect("webapi::websocket not installed")
}

/// `crate::context` fan-out. The protections died with the gone heap; only the
/// bookkeeping is dropped (no unprotect — there is no VM to call into). The
/// connection threads run on until the peer closes, exactly as a context
/// release left them before this hook existed.
pub(crate) fn forget(gone: crate::context::Gone) {
    classes_reg().lock().unwrap().remove(&gone.key);
    if let Some(v) = live().lock().unwrap().remove(&gone.key) {
        LIVE_COUNT.fetch_sub(v.len(), Ordering::SeqCst);
    }
}

/// The JS objects the pump must be able to reach: `(socket id, protected
/// instance)`. Protected for exactly as long as the connection can still fire
/// an event — a browser keeps an open socket alive with no references too.
fn live() -> &'static Mutex<HashMap<usize, Vec<(u64, u64)>>> {
    static L: OnceLock<Mutex<HashMap<usize, Vec<(u64, u64)>>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(HashMap::new()))
}

unsafe fn this_ws<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut WsData> {
    host_data::<WsData>(g, classes(g).socket_class, cf_this(cf))
}

// ---------------------------------------------------------------------------
// MessageEvent / CloseEvent
// ---------------------------------------------------------------------------

/// Slot bag of an Event subclass — the same `[[s_event]]` object `events.rs`
/// dispatches against, never a second store.
unsafe fn slots(g: *mut c_void, obj: u64) -> Option<u64> {
    let s = get_a(g, obj, state(g).s_event);
    if is_cell(s) {
        Some(s)
    } else {
        None
    }
}

unsafe fn slot_get(g: *mut c_void, cf: *mut c_void, key: &str, label: &str) -> u64 {
    match slots(g, cf_this(cf)) {
        Some(s) => {
            let v = get(g, s, key);
            if v == UNDEF {
                NULL
            } else {
                v
            }
        }
        None => throw_type_error(g, &format!("{label} called on a non-Event")),
    }
}

macro_rules! slot_getters {
    ($($fn_name:ident, $key:literal, $label:literal;)*) => {
        $(
            unsafe extern "sysv64" fn $fn_name(g: *mut c_void, cf: *mut c_void) -> u64 {
                slot_get(g, cf, $key, $label)
            }
        )*
    };
}

slot_getters! {
    me_data, "data", "MessageEvent.data";
    me_origin, "origin", "MessageEvent.origin";
    me_last_event_id, "lastEventId", "MessageEvent.lastEventId";
    me_source, "source", "MessageEvent.source";
    ce_code, "code", "CloseEvent.code";
    ce_reason, "reason", "CloseEvent.reason";
    ce_was_clean, "wasClean", "CloseEvent.wasClean";
}

unsafe extern "sysv64" fn me_ports(g: *mut c_void, _cf: *mut c_void) -> u64 {
    // There is no MessagePort in this runtime; the spec's own default for a
    // MessageEvent that carries none is an empty list.
    new_array(g)
}

unsafe fn init_message_event(g: *mut c_void, obj: u64, ty: u64, init: u64) {
    events::init_event(g, obj, ty, init, false);
    let Some(s) = slots(g, obj) else { return };
    let from = |k: &str, d: u64| -> u64 {
        if is_cell(init) {
            let v = get(g, init, k);
            if v != UNDEF {
                return v;
            }
        }
        d
    };
    define(g, s, "data", from("data", NULL), 0);
    define(g, s, "origin", from("origin", js_string(g, "")), 0);
    define(g, s, "lastEventId", from("lastEventId", js_string(g, "")), 0);
    define(g, s, "source", from("source", NULL), 0);
}

unsafe extern "sysv64" fn message_event_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "MessageEvent constructor requires a type argument");
    }
    let obj = ordinary_create(g, cf_new_target(cf), classes(g).message_event.proto);
    init_message_event(g, obj, cf_arg(cf, 0), cf_arg(cf, 1));
    obj
}

unsafe extern "sysv64" fn close_event_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "CloseEvent constructor requires a type argument");
    }
    let obj = ordinary_create(g, cf_new_target(cf), classes(g).close_event.proto);
    events::init_event(g, obj, cf_arg(cf, 0), cf_arg(cf, 1), false);
    let init = cf_arg(cf, 1);
    if let Some(s) = slots(g, obj) {
        let code = if is_cell(init) { get(g, init, "code") } else { UNDEF };
        let reason = if is_cell(init) { get(g, init, "reason") } else { UNDEF };
        let clean = if is_cell(init) { get(g, init, "wasClean") } else { UNDEF };
        define(g, s, "code", if code == UNDEF { enc_i32(0) } else { enc_f64(read_number(g, code)) }, 0);
        define(g, s, "reason", if reason == UNDEF { js_string(g, "") } else { js_string(g, &read_string(g, reason).unwrap_or_default()) }, 0);
        define(g, s, "wasClean", enc_bool(clean != UNDEF && glue::rjs_to_boolean(g, clean) != 0), 0);
    }
    obj
}

/// A trusted `MessageEvent` carrying `data`, dispatched from the pump.
unsafe fn new_message_event(g: *mut c_void, data: u64, origin: &str) -> u64 {
    new_message_event_of(g, "message", data, origin)
}

/// A trusted `MessageEvent` of any type (`message` / `messageerror`) — also
/// what BroadcastChannel delivers.
pub(crate) unsafe fn new_message_event_of(g: *mut c_void, ty: &str, data: u64, origin: &str) -> u64 {
    let c = classes(g);
    let obj = new_object(g, c.message_event.proto);
    events::init_event(g, obj, js_string(g, ty), UNDEF, false);
    if let Some(s) = slots(g, obj) {
        define(g, s, "isTrusted", TRUE, 0);
        define(g, s, "data", data, 0);
        define(g, s, "origin", js_string(g, origin), 0);
        define(g, s, "lastEventId", js_string(g, ""), 0);
        define(g, s, "source", NULL, 0);
    }
    obj
}

unsafe fn new_close_event(g: *mut c_void, code: u16, reason: &str, clean: bool) -> u64 {
    let c = classes(g);
    let obj = new_object(g, c.close_event.proto);
    events::init_event(g, obj, js_string(g, "close"), UNDEF, false);
    if let Some(s) = slots(g, obj) {
        define(g, s, "isTrusted", TRUE, 0);
        define(g, s, "code", enc_i32(code as i32), 0);
        define(g, s, "reason", js_string(g, reason), 0);
        define(g, s, "wasClean", enc_bool(clean), 0);
    }
    obj
}

// ---------------------------------------------------------------------------
// on<event> handler IDL attributes
// ---------------------------------------------------------------------------

unsafe fn handler_bag(g: *mut c_void, this: u64, create: bool) -> u64 {
    let slot = classes(g).s_handlers;
    let bag = get_a(g, this, slot);
    if is_cell(bag) {
        return bag;
    }
    if !create {
        return UNDEF;
    }
    let fresh = new_object(g, UNDEF);
    glue::rjs_define(g, this, slot, fresh, 0);
    fresh
}

macro_rules! handler_accessor {
    ($get_fn:ident, $set_fn:ident, $name:literal) => {
        unsafe extern "sysv64" fn $get_fn(g: *mut c_void, cf: *mut c_void) -> u64 {
            let bag = handler_bag(g, cf_this(cf), false);
            if !is_cell(bag) {
                return NULL;
            }
            let v = get(g, bag, $name);
            if is_cell(v) {
                v
            } else {
                NULL
            }
        }
        unsafe extern "sysv64" fn $set_fn(g: *mut c_void, cf: *mut c_void) -> u64 {
            let this = cf_this(cf);
            let bag = handler_bag(g, this, true);
            if is_cell(bag) {
                let v = cf_arg(cf, 0);
                define(g, bag, $name, if is_cell(v) { v } else { NULL }, 0);
            }
            UNDEF
        }
    };
}

handler_accessor!(get_onopen, set_onopen, "onopen");
handler_accessor!(get_onmessage, set_onmessage, "onmessage");
handler_accessor!(get_onerror, set_onerror, "onerror");
handler_accessor!(get_onclose, set_onclose, "onclose");

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------

/// "Throw a `name` DOMException" (SyntaxError, InvalidStateError,
/// InvalidAccessError — the spec's own names for each refusal).
unsafe fn throw_named(g: *mut c_void, name: &str, msg: &str) -> u64 {
    super::domexception::throw(g, name, msg)
}

/// `close(optional [Clamp] unsigned short code, ...)`.
///
/// `[Clamp]`, not the default modulo wrap, and this distinction is load-bearing:
/// Rust's `as u16` truncates, so `close(70000)` would come out as **4464** — a
/// value inside the application range that the check below would happily accept
/// and put on the wire. Clamping makes it 65535, which is refused by name, which
/// is what a browser does.
fn clamp_close_code(n: f64) -> u16 {
    if n.is_nan() {
        return 0;
    }
    n.round().clamp(0.0, 65535.0) as u16
}

/// `protocols` argument: a single string, or a sequence of strings.
unsafe fn read_protocols(g: *mut c_void, v: u64) -> Vec<String> {
    if v == UNDEF || v == NULL {
        return Vec::new();
    }
    if glue::rjs_kind(g, v) == 5 {
        return vec![read_string(g, v).unwrap_or_default()];
    }
    if is_cell(v) {
        let n = array_len(g, v);
        // An object that is not an array has length 0 and yields nothing, which
        // is what `sequence<DOMString>` conversion does with a non-iterable.
        return (0..n).filter_map(|i| read_string(g, get_i(g, v, i))).collect();
    }
    vec![read_string(g, v).unwrap_or_default()]
}

unsafe extern "sysv64" fn ws_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "WebSocket constructor requires a url argument");
    }
    let raw = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let url = match parse_ws_url(&raw) {
        Ok(u) => u,
        Err(UrlError::Syntax(m)) => return throw_named(g, "SyntaxError", &m),
    };
    let protocols = read_protocols(g, cf_arg(cf, 1));
    if let Err(UrlError::Syntax(m)) = validate_protocols(&protocols) {
        return throw_named(g, "SyntaxError", &m);
    }
    if live_sockets().len() >= MAX_SOCKETS {
        return throw_named(
            g,
            "InvalidStateError",
            &format!("more than {MAX_SOCKETS} WebSockets are already open"),
        );
    }

    let c = classes(g);
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    let sh = Shared::new();
    conns().lock().unwrap().insert(id, Arc::clone(&sh));

    let proto = {
        let nt = cf_new_target(cf);
        let mut p = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
        if !is_cell(p) {
            p = c.socket.proto;
        }
        p
    };
    let obj = host_create(
        g,
        c.socket_class,
        proto,
        WsData {
            id,
            url: url.to_string(),
            origin: url.origin().ascii_serialization(),
            binary_type: BinaryType::Blob,
        },
    );

    let ctx = Ctx(g);
    let spawned = std::thread::Builder::new()
        .name("redjs-ws".into())
        .spawn({
            let sh = Arc::clone(&sh);
            move || io_thread(ctx, id, sh, url, protocols)
        });

    if let Err(e) = spawned {
        conns().lock().unwrap().remove(&id);
        return throw_named(g, "InvalidStateError", &format!("could not start a WebSocket thread: {e}"));
    }

    // Protected until the close event has been delivered: an open socket must
    // stay reachable for its own events even with no references left.
    crate::ffi::redjs_protect(g, obj);
    live().lock().unwrap().entry(ctx.key()).or_default().push((id, obj));
    LIVE_COUNT.fetch_add(1, Ordering::SeqCst);
    obj
}

macro_rules! ws_getter {
    ($fn_name:ident, |$d:ident, $sh:ident, $g:ident| $body:expr) => {
        unsafe extern "sysv64" fn $fn_name($g: *mut c_void, cf: *mut c_void) -> u64 {
            let Some($d) = this_ws($g, cf) else {
                return throw_type_error($g, "WebSocket getter called on a non-WebSocket");
            };
            match shared_of($d.id) {
                Some($sh) => $body,
                // Unreachable while the instance is alive — the record is
                // released by `Drop for WsData`, i.e. after this object is
                // gone. Answering as a closed socket beats throwing.
                None => terminal_getter($g, stringify!($fn_name)),
            }
        }
    };
}

unsafe fn terminal_getter(g: *mut c_void, which: &str) -> u64 {
    match which {
        "ws_ready_state" => enc_i32(CLOSED as i32),
        "ws_buffered" => enc_i32(0),
        _ => js_string(g, ""),
    }
}

ws_getter!(ws_ready_state, |_d, sh, g| {
    let _ = g;
    enc_i32(sh.ready_state() as i32)
});
ws_getter!(ws_buffered, |_d, sh, g| {
    let _ = g;
    enc_f64(sh.buffered.load(Ordering::SeqCst) as f64)
});
ws_getter!(ws_protocol, |_d, sh, g| {
    let p = sh.meta.lock().unwrap().protocol.clone();
    js_string(g, &p)
});
ws_getter!(ws_extensions, |_d, sh, g| {
    let e = sh.meta.lock().unwrap().extensions.clone();
    js_string(g, &e)
});

unsafe extern "sysv64" fn ws_url(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_ws(g, cf) {
        Some(d) => {
            let u = d.url.clone();
            js_string(g, &u)
        }
        None => throw_type_error(g, "WebSocket.prototype.url called on a non-WebSocket"),
    }
}

unsafe extern "sysv64" fn ws_binary_type_get(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_ws(g, cf) {
        Some(d) => js_string(g, if d.binary_type == BinaryType::Blob { "blob" } else { "arraybuffer" }),
        None => throw_type_error(g, "WebSocket.prototype.binaryType called on a non-WebSocket"),
    }
}

unsafe extern "sysv64" fn ws_binary_type_set(g: *mut c_void, cf: *mut c_void) -> u64 {
    let want = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let Some(d) = this_ws(g, cf) else {
        return throw_type_error(g, "WebSocket.prototype.binaryType called on a non-WebSocket");
    };
    match want.as_str() {
        "blob" => d.binary_type = BinaryType::Blob,
        "arraybuffer" => d.binary_type = BinaryType::ArrayBuffer,
        // Spec: an unrecognised value is ignored, not an error.
        _ => {}
    }
    UNDEF
}

unsafe extern "sysv64" fn ws_send(g: *mut c_void, cf: *mut c_void) -> u64 {
    let arg = cf_arg(cf, 0);
    // The payload must be read out of JS memory BEFORE anything can allocate.
    let st = state(g);
    let payload: (u8, Vec<u8>) = if glue::rjs_kind(g, arg) == 5 {
        (OP_TEXT, read_string(g, arg).unwrap_or_default().into_bytes())
    } else if host_data::<blob::BlobData>(g, st.blob_class, arg).is_some() {
        // A browser reads a Blob payload asynchronously and fails the
        // connection if the read fails; this runtime reads it now (A-F2), so
        // the failure is thrown from send() instead (DESKTOP.md D-F17).
        match blob::bytes_of(g, arg, "WebSocket.send").unwrap_or(Ok(Vec::new())) {
            Ok(b) => (OP_BINARY, b),
            Err(e) => {
                blob::sync_read_failed("WebSocket.send", e);
                return super::domexception::throw(g, e.name(), e.message());
            }
        }
    } else if let Some(b) = bufferish(g, arg) {
        (OP_BINARY, b.to_vec())
    } else {
        // Everything else is stringified, as WebIDL's `USVString` union does.
        (OP_TEXT, read_string(g, arg).unwrap_or_default().into_bytes())
    };

    let Some(d) = this_ws(g, cf) else {
        return throw_type_error(g, "WebSocket.prototype.send called on a non-WebSocket");
    };
    let Some(sh) = shared_of(d.id) else {
        return UNDEF; // already gone: same as CLOSED
    };
    match sh.ready_state() {
        CONNECTING => {
            return throw_named(g, "InvalidStateError", "WebSocket.send: the socket is still CONNECTING")
        }
        // Spec: while CLOSING or CLOSED the data is not sent, but it IS counted
        // — so an app watching bufferedAmount can see that it was dropped.
        CLOSING | CLOSED => {
            sh.buffered.fetch_add(payload.1.len() as u64, Ordering::SeqCst);
            return UNDEF;
        }
        _ => {}
    }
    let billed = payload.1.len() as u64;
    sh.buffered.fetch_add(billed, Ordering::SeqCst);
    sh.out.lock().unwrap().push_back(Out::Data { opcode: payload.0, payload: payload.1, billed });
    UNDEF
}

unsafe extern "sysv64" fn ws_close(g: *mut c_void, cf: *mut c_void) -> u64 {
    let code_arg = cf_arg(cf, 0);
    let reason_arg = cf_arg(cf, 1);
    let code = if code_arg == UNDEF { 1000u16 } else { clamp_close_code(read_number(g, code_arg)) };
    if code_arg != UNDEF && code != 1000 && !(3000..=4999).contains(&code) {
        return throw_named(
            g,
            "InvalidAccessError",
            &format!("WebSocket.close: {code} is neither 1000 nor in the 3000-4999 range"),
        );
    }
    let reason = if reason_arg == UNDEF { String::new() } else { read_string(g, reason_arg).unwrap_or_default() };
    if reason.len() > 123 {
        return throw_named(
            g,
            "SyntaxError",
            &format!("WebSocket.close: the reason is {} UTF-8 bytes (max 123)", reason.len()),
        );
    }

    let Some(d) = this_ws(g, cf) else {
        return throw_type_error(g, "WebSocket.prototype.close called on a non-WebSocket");
    };
    let Some(sh) = shared_of(d.id) else { return UNDEF };
    match sh.ready_state() {
        CLOSING | CLOSED => {}
        CONNECTING => {
            // Spec: "fail the WebSocket connection" — there is no handshake to
            // close, so the socket is cut and 1006/unclean follows.
            sh.set_state(CLOSING);
            sh.cancel.abort();
        }
        _ => {
            sh.set_state(CLOSING);
            sh.out.lock().unwrap().push_back(Out::Close { code, reason });
        }
    }
    UNDEF
}

// ---------------------------------------------------------------------------
// Pump: the frame-thread half
// ---------------------------------------------------------------------------

/// Dispatches one queued event. Returns `true` when it was the `close` event.
unsafe fn deliver(g: *mut c_void, obj: u64, origin: &str, binary: BinaryType, ev: Incoming) -> bool {
    match ev {
        Incoming::Open => {
            let e = events::new_trusted_event(g, "open");
            events::dispatch(g, obj, e);
            false
        }
        Incoming::Text(s) => {
            let data = js_string(g, &s);
            let e = new_message_event(g, data, origin);
            events::dispatch(g, obj, e);
            false
        }
        Incoming::Binary(bytes) => {
            let data = match binary {
                BinaryType::ArrayBuffer => {
                    let u8v = make_u8(g, &bytes);
                    get(g, u8v, "buffer")
                }
                BinaryType::Blob => blob::create(g, UNDEF, bytes, String::new()),
            };
            let e = new_message_event(g, data, origin);
            events::dispatch(g, obj, e);
            false
        }
        Incoming::Error(_) => {
            // Browsers dispatch a plain Event here and expose no reason; the
            // reason went to stderr from the I/O thread.
            let e = events::new_trusted_event(g, "error");
            events::dispatch(g, obj, e);
            false
        }
        Incoming::Closed { code, reason, clean } => {
            let e = new_close_event(g, code, &reason, clean);
            events::dispatch(g, obj, e);
            true
        }
    }
}

/// Drains every live socket's inbox on the JS thread. Called once per pump.
///
/// Costs one relaxed atomic read when nothing is connected.
pub unsafe fn pump(g: *mut c_void) {
    if LIVE_COUNT.load(Ordering::Relaxed) == 0 {
        return;
    }
    let key = g as usize;
    let entries: Vec<(u64, u64)> = match live().lock().unwrap().get(&key) {
        Some(v) => v.clone(),
        None => return,
    };
    let socket_class = classes(g).socket_class;
    let mut finished: Vec<u64> = Vec::new();
    for (id, obj) in entries {
        let Some(sh) = shared_of(id) else { continue };
        let batch = sh.drain_inbox();
        if batch.is_empty() {
            continue;
        }
        for ev in batch {
            // `binaryType` is read per message, as in a browser: a listener
            // that flips it changes only the messages after its own.
            let (origin, binary) = match host_data::<WsData>(g, socket_class, obj) {
                Some(d) => (d.origin.clone(), d.binary_type),
                None => (String::new(), BinaryType::Blob),
            };
            if deliver(g, obj, &origin, binary, ev) {
                // `close` was the last event this socket can produce.
                finished.push(id);
                break;
            }
        }
    }
    // The connection record stays (see `Drop for WsData`); what is released
    // here is the GC protection that kept an unreferenced open socket alive.
    for id in finished {
        let removed = {
            let mut reg = live().lock().unwrap();
            match reg.get_mut(&key) {
                Some(v) => v.iter().position(|(i, _)| *i == id).map(|p| v.remove(p).1),
                None => None,
            }
        };
        if let Some(obj) = removed {
            crate::ffi::redjs_unprotect(g, obj);
            LIVE_COUNT.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

pub unsafe fn install(g: *mut c_void, event_proto: u64, event_target_proto: u64) {
    let socket_class = make_host_class::<WsData>("WebSocket");

    let message_event = define_class(g, &ClassSpec {
        name: "MessageEvent",
        arity: 1,
        construct: Some(message_event_construct),
        parent_proto: event_proto,
        methods: &[],
        getters: &[
            ("data", me_data),
            ("origin", me_origin),
            ("lastEventId", me_last_event_id),
            ("source", me_source),
            ("ports", me_ports),
        ],
        accessors: &[],
    });

    let close_event = define_class(g, &ClassSpec {
        name: "CloseEvent",
        arity: 1,
        construct: Some(close_event_construct),
        parent_proto: event_proto,
        methods: &[],
        getters: &[("code", ce_code), ("reason", ce_reason), ("wasClean", ce_was_clean)],
        accessors: &[],
    });

    let socket = define_class(g, &ClassSpec {
        name: "WebSocket",
        arity: 1,
        construct: Some(ws_construct),
        parent_proto: event_target_proto,
        methods: &[("send", 1, ws_send), ("close", 0, ws_close)],
        getters: &[
            ("url", ws_url),
            ("readyState", ws_ready_state),
            ("bufferedAmount", ws_buffered),
            ("protocol", ws_protocol),
            ("extensions", ws_extensions),
        ],
        accessors: &[
            ("binaryType", ws_binary_type_get, ws_binary_type_set),
            ("onopen", get_onopen, set_onopen),
            ("onmessage", get_onmessage, set_onmessage),
            ("onerror", get_onerror, set_onerror),
            ("onclose", get_onclose, set_onclose),
        ],
    });

    // The readyState constants live on BOTH the constructor and the prototype
    // (WebIDL const members), RO/DontEnum/DontDelete.
    for (name, v) in [("CONNECTING", 0), ("OPEN", 1), ("CLOSING", 2), ("CLOSED", 3)] {
        define(g, socket.ctor, name, enc_i32(v), 1 | 2 | 4);
        define(g, socket.proto, name, enc_i32(v), 1 | 2 | 4);
    }

    let c: &'static WsClasses = Box::leak(Box::new(WsClasses {
        socket,
        socket_class,
        message_event,
        close_event,
        s_handlers: atom_priv(g, "redjs#wsHandlers"),
    }));
    classes_reg().lock().unwrap().insert(g as usize, c);

    // `WebSocketStream` composes the class above with the Streams cores, both
    // of which exist by now (`streams::install_surface` runs earlier in
    // `webapi::install`). See the script's own header for why it is not
    // optional: the RPC layer this whole module was written for feature-detects
    // `WebSocketStream` and refuses to open a call without it.
    let mut out = 0u64;
    let name = "redjs/websocketstream.js";
    let rc = glue::rjs_eval8(g, WSS_JS.as_ptr(), WSS_JS.len(), name.as_ptr(), name.len(), &mut out);
    if rc == 1 {
        let (mut msg, mut stack) = (0u64, 0u64);
        glue::rjs_error_info(g, out, &mut msg, &mut stack);
        eprintln!("[redjs] websocketstream.js failed: {}", read_string(g, msg).unwrap_or_default());
    }
}

const WSS_JS: &str = include_str!("websocketstream.js");

// ---------------------------------------------------------------------------
// Tests — the protocol, with no JS context and (mostly) no socket
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_accept_key_matches_the_rfc_example() {
        // RFC 6455 §1.3, verbatim.
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn base64_matches_the_reference_vectors() {
        // RFC 4648 §10.
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(b64_encode(input.as_bytes()), want, "b64({input:?})");
        }
    }

    #[test]
    fn a_client_frame_is_always_masked_and_round_trips_after_unmasking() {
        let mask = [0xde, 0xad, 0xbe, 0xef];
        let wire = encode_client_frame(true, OP_TEXT, b"Hello", mask);
        assert_eq!(wire[0], 0x81, "FIN + text opcode");
        assert_eq!(wire[1], 0x80 | 5, "MASK bit + length");
        assert_eq!(&wire[2..6], &mask);
        let unmasked: Vec<u8> = wire[6..].iter().enumerate().map(|(i, b)| b ^ mask[i & 3]).collect();
        assert_eq!(unmasked, b"Hello");
    }

    #[test]
    fn frame_length_uses_the_three_encodings_at_the_right_boundaries() {
        let m = [0u8; 4];
        assert_eq!(encode_client_frame(true, OP_BINARY, &vec![0u8; 125], m)[1], 0x80 | 125);
        let mid = encode_client_frame(true, OP_BINARY, &vec![0u8; 126], m);
        assert_eq!(mid[1], 0x80 | 126);
        assert_eq!(&mid[2..4], &[0, 126]);
        let big = encode_client_frame(true, OP_BINARY, &vec![0u8; 70_000], m);
        assert_eq!(big[1], 0x80 | 127);
        assert_eq!(u64::from_be_bytes(big[2..10].try_into().unwrap()), 70_000);
    }

    /// Server frames are unmasked, so building one is the whole encoder minus
    /// the mask.
    pub(super) fn server_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![if fin { 0x80 | opcode } else { opcode }];
        let n = payload.len();
        if n < 126 {
            out.push(n as u8);
        } else if n <= u16::MAX as usize {
            out.push(126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            out.push(127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn a_partial_frame_is_incomplete_rather_than_an_error() {
        let wire = server_frame(true, OP_TEXT, b"hello world");
        for cut in 0..wire.len() {
            assert_eq!(parse_frame(&wire[..cut], MAX_MESSAGE), Ok(None), "cut at {cut}");
        }
        let (f, used) = parse_frame(&wire, MAX_MESSAGE).unwrap().unwrap();
        assert_eq!(used, wire.len());
        assert_eq!(f.payload, b"hello world");
    }

    #[test]
    fn two_frames_in_one_read_both_come_out() {
        let mut wire = server_frame(true, OP_TEXT, b"a");
        wire.extend(server_frame(true, OP_BINARY, &[1, 2, 3]));
        let (f1, n1) = parse_frame(&wire, MAX_MESSAGE).unwrap().unwrap();
        assert_eq!(f1.opcode, OP_TEXT);
        let (f2, n2) = parse_frame(&wire[n1..], MAX_MESSAGE).unwrap().unwrap();
        assert_eq!(f2.opcode, OP_BINARY);
        assert_eq!(f2.payload, vec![1, 2, 3]);
        assert_eq!(n1 + n2, wire.len());
    }

    #[test]
    fn a_masked_server_frame_is_a_protocol_error() {
        // Accepting one would read the 4-byte mask key as payload.
        let mut wire = server_frame(true, OP_TEXT, b"hi");
        wire[1] |= 0x80;
        assert_eq!(parse_frame(&wire, MAX_MESSAGE).unwrap_err().code, 1002);
    }

    #[test]
    fn reserved_bits_and_opcodes_are_refused() {
        let mut rsv = server_frame(true, OP_TEXT, b"x");
        rsv[0] |= 0x40;
        assert_eq!(parse_frame(&rsv, MAX_MESSAGE).unwrap_err().code, 1002);
        for op in [0x3, 0x7, 0xb, 0xf] {
            let f = server_frame(true, op, b"");
            assert_eq!(parse_frame(&f, MAX_MESSAGE).unwrap_err().code, 1002, "opcode {op:#x}");
        }
    }

    #[test]
    fn control_frames_may_be_neither_fragmented_nor_large() {
        assert_eq!(parse_frame(&server_frame(false, OP_PING, b""), MAX_MESSAGE).unwrap_err().code, 1002);
        assert_eq!(parse_frame(&server_frame(true, OP_PING, &[0u8; 126]), MAX_MESSAGE).unwrap_err().code, 1002);
    }

    #[test]
    fn an_oversized_frame_is_refused_before_it_is_allocated() {
        // The 64-bit length is checked against the limit BEFORE any buffer is
        // sized from it — a hostile 2^60 header must not be an allocation.
        let mut wire = vec![0x82, 127];
        wire.extend_from_slice(&(1u64 << 60).to_be_bytes());
        let e = parse_frame(&wire, MAX_MESSAGE).unwrap_err();
        assert_eq!(e.code, 1009, "{}", e.message);
    }

    #[test]
    fn a_length_with_the_reserved_high_bit_is_refused() {
        let mut wire = vec![0x82, 127];
        wire.extend_from_slice(&u64::MAX.to_be_bytes());
        assert_eq!(parse_frame(&wire, MAX_MESSAGE).unwrap_err().code, 1002);
    }

    #[test]
    fn a_fragmented_message_reassembles_in_order() {
        let mut a = Assembler::default();
        let parts = [
            (false, OP_TEXT, "Hel"),
            (false, OP_CONT, "lo, "),
            (true, OP_CONT, "world"),
        ];
        let mut out = None;
        for (fin, op, s) in parts {
            let f = Frame { fin, opcode: op, payload: s.as_bytes().to_vec() };
            if let Assembled::Text(t) = a.push(&f, MAX_MESSAGE).unwrap() {
                out = Some(t);
            }
        }
        assert_eq!(out.as_deref(), Some("Hello, world"));
    }

    #[test]
    fn interleaving_rules_for_fragments_are_enforced() {
        // A continuation with nothing in progress.
        let mut a = Assembler::default();
        let cont = Frame { fin: true, opcode: OP_CONT, payload: b"x".to_vec() };
        assert_eq!(a.push(&cont, MAX_MESSAGE).unwrap_err().code, 1002);

        // A fresh data frame inside a fragmented message.
        let mut b = Assembler::default();
        let start = Frame { fin: false, opcode: OP_TEXT, payload: b"a".to_vec() };
        b.push(&start, MAX_MESSAGE).unwrap();
        let interrupt = Frame { fin: true, opcode: OP_TEXT, payload: b"b".to_vec() };
        assert_eq!(b.push(&interrupt, MAX_MESSAGE).unwrap_err().code, 1002);
    }

    #[test]
    fn a_text_message_that_is_not_utf8_fails_the_connection() {
        // 1007 exists precisely so this is not delivered as a lossy string.
        let mut a = Assembler::default();
        let f = Frame { fin: true, opcode: OP_TEXT, payload: vec![0xff, 0xfe] };
        assert_eq!(a.push(&f, MAX_MESSAGE).unwrap_err().code, 1007);
    }

    #[test]
    fn invalid_utf8_split_across_fragments_is_still_caught() {
        // Each half is individually meaningless; only the joined message can be
        // validated, which is why validation happens at FIN.
        let mut a = Assembler::default();
        a.push(&Frame { fin: false, opcode: OP_TEXT, payload: vec![0xe2, 0x82] }, MAX_MESSAGE).unwrap();
        let e = a.push(&Frame { fin: true, opcode: OP_CONT, payload: vec![0x28] }, MAX_MESSAGE).unwrap_err();
        assert_eq!(e.code, 1007);
    }

    #[test]
    fn a_message_over_the_limit_is_refused_during_reassembly() {
        let mut a = Assembler::default();
        let f = Frame { fin: false, opcode: OP_BINARY, payload: vec![0u8; 64] };
        a.push(&f, 32).unwrap_err();
    }

    #[test]
    fn close_payloads_round_trip_and_reject_the_reserved_codes() {
        assert_eq!(parse_close_payload(&[]).unwrap(), (1005, String::new()));
        assert_eq!(parse_close_payload(&close_payload(1000, "bye")).unwrap(), (1000, "bye".into()));
        assert_eq!(parse_close_payload(&close_payload(4001, "app")).unwrap(), (4001, "app".into()));
        // A one-byte body cannot carry a code.
        assert_eq!(parse_close_payload(&[3]).unwrap_err().code, 1002);
        // 1005/1006/1015 are local sentinels and must never be on the wire.
        for bad in [1004u16, 1005, 1006, 1015, 999, 5000] {
            assert_eq!(
                parse_close_payload(&close_payload(bad, "")).unwrap_err().code,
                1002,
                "wire code {bad} was accepted"
            );
        }
        assert_eq!(parse_close_payload(&[0x03, 0xe8, 0xff]).unwrap_err().code, 1007);
    }

    #[test]
    fn the_upgrade_request_carries_every_required_header() {
        let u = url::Url::parse("http://example.com:8080/socket?room=1").unwrap();
        let req = handshake_request(&u, "dGhlIHNhbXBsZSBub25jZQ==", &["ion".into(), "cbor".into()]);
        assert!(req.starts_with("GET /socket?room=1 HTTP/1.1\r\n"), "{req}");
        assert!(req.contains("Host: example.com:8080\r\n"), "{req}");
        assert!(req.contains("Upgrade: websocket\r\n"), "{req}");
        assert!(req.contains("Connection: Upgrade\r\n"), "{req}");
        assert!(req.contains("Sec-WebSocket-Version: 13\r\n"), "{req}");
        assert!(req.contains("Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"), "{req}");
        assert!(req.contains("Sec-WebSocket-Protocol: ion, cbor\r\n"), "{req}");
        assert!(req.ends_with("\r\n\r\n"), "{req}");
        // No extensions are offered, so none may be accepted.
        assert!(!req.contains("Sec-WebSocket-Extensions"), "{req}");
    }

    #[test]
    fn a_default_port_is_not_repeated_in_the_host_header() {
        let u = url::Url::parse("https://example.com/s").unwrap();
        assert!(handshake_request(&u, "k", &[]).contains("Host: example.com\r\n"));
    }

    fn head(status: u16, headers: &[(&str, &str)]) -> http::Head {
        http::Head {
            status,
            reason: "Switching Protocols".into(),
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
    const ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

    #[test]
    fn a_correct_101_is_accepted_and_reports_the_subprotocol() {
        let h = head(101, &[
            ("upgrade", "WebSocket"),
            ("connection", "keep-alive, Upgrade"),
            ("sec-websocket-accept", ACCEPT),
            ("sec-websocket-protocol", "ion"),
        ]);
        assert_eq!(validate_handshake(&h, KEY, &["ion".into()]).unwrap(), ("ion".into(), String::new()));
    }

    #[test]
    fn a_wrong_accept_key_fails_the_handshake() {
        // The accept key is the only proof the peer understood the upgrade and
        // is not a cache or a proxy replaying a 101.
        let h = head(101, &[("upgrade", "websocket"), ("connection", "Upgrade"), ("sec-websocket-accept", "AAAA")]);
        assert!(validate_handshake(&h, KEY, &[]).unwrap_err().contains("Sec-WebSocket-Accept"));
    }

    #[test]
    fn a_non_101_answer_names_the_status_it_got() {
        let h = http::Head { status: 404, reason: "Not Found".into(), headers: vec![] };
        let e = validate_handshake(&h, KEY, &[]).unwrap_err();
        assert!(e.contains("404") && e.contains("Not Found"), "{e}");
    }

    #[test]
    fn a_missing_or_wrong_upgrade_pair_fails() {
        let no_upgrade = head(101, &[("connection", "Upgrade"), ("sec-websocket-accept", ACCEPT)]);
        assert!(validate_handshake(&no_upgrade, KEY, &[]).is_err());
        let no_conn = head(101, &[("upgrade", "websocket"), ("sec-websocket-accept", ACCEPT)]);
        assert!(validate_handshake(&no_conn, KEY, &[]).is_err());
    }

    #[test]
    fn an_unoffered_extension_or_subprotocol_fails_the_connection() {
        // Both are silent-wrong-answer shapes: an unnegotiated extension means
        // every following frame decodes wrong, and an invented subprotocol
        // means the peer is speaking something we never agreed to.
        let ext = head(101, &[
            ("upgrade", "websocket"),
            ("connection", "Upgrade"),
            ("sec-websocket-accept", ACCEPT),
            ("sec-websocket-extensions", "permessage-deflate"),
        ]);
        assert!(validate_handshake(&ext, KEY, &[]).unwrap_err().contains("permessage-deflate"));

        let proto = head(101, &[
            ("upgrade", "websocket"),
            ("connection", "Upgrade"),
            ("sec-websocket-accept", ACCEPT),
            ("sec-websocket-protocol", "surprise"),
        ]);
        assert!(validate_handshake(&proto, KEY, &["ion".into()]).unwrap_err().contains("surprise"));
    }

    #[test]
    fn urls_are_normalized_and_refused_the_way_the_html_spec_says() {
        assert_eq!(parse_ws_url("ws://a.example/x").map(|u| u.to_string()).ok(), Some("ws://a.example/x".into()));
        // http(s) is mapped, not refused.
        assert_eq!(parse_ws_url("https://a.example/x").map(|u| u.scheme().to_string()).ok(), Some("wss".into()));
        assert_eq!(parse_ws_url("http://a.example/x").map(|u| u.scheme().to_string()).ok(), Some("ws".into()));
        for bad in ["ftp://a.example/", "/relative", "ws://a.example/x#frag", "not a url"] {
            assert!(parse_ws_url(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn the_ws_to_http_mapping_preserves_authority_and_target() {
        let u = parse_ws_url("wss://a.example:9443/rpc?x=1").unwrap();
        let h = to_http_url(&u).unwrap();
        assert_eq!(h.scheme(), "https");
        assert_eq!(h.host_str(), Some("a.example"));
        assert_eq!(h.port(), Some(9443));
        assert_eq!(h.path(), "/rpc");
        assert_eq!(h.query(), Some("x=1"));
        // Default ports stay default on both sides.
        let d = to_http_url(&parse_ws_url("wss://a.example/").unwrap()).unwrap();
        assert_eq!(d.port(), None);
        assert_eq!(d.port_or_known_default(), Some(443));
    }

    #[test]
    fn a_close_code_is_clamped_rather_than_truncated() {
        // The one that matters: `70000 as u16` truncates to 4464, which is
        // INSIDE the application range and would be accepted and sent.
        assert_eq!(clamp_close_code(70_000.0), 65535);
        assert_eq!(clamp_close_code(-1.0), 0);
        assert_eq!(clamp_close_code(f64::NAN), 0);
        assert_eq!(clamp_close_code(f64::INFINITY), 65535);
        assert_eq!(clamp_close_code(1000.0), 1000);
        assert_eq!(clamp_close_code(4999.4), 4999);
    }

    #[test]
    fn subprotocol_names_must_be_tokens_and_unique() {
        assert!(validate_protocols(&["ion".into(), "cbor.v1".into()]).is_ok());
        assert!(validate_protocols(&["with space".into()]).is_err());
        assert!(validate_protocols(&["".into()]).is_err());
        assert!(validate_protocols(&["a".into(), "A".into()]).is_err());
    }
}

// ---------------------------------------------------------------------------
// Tests — through real JS, against a real RFC 6455 server on loopback
// ---------------------------------------------------------------------------

#[cfg(test)]
mod jsc_tests {
    use super::tests::server_frame;
    use super::{OP_BINARY, OP_CLOSE, OP_CONT, OP_PING, OP_PONG, OP_TEXT};
    use crate::api::Context;
    use crate::webapi::testing::*;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener};
    use std::time::{Duration, Instant};

    /// `testing::ctx` installs the web classes but not the scheduler, and every
    /// WebSocket event arrives through the pump — so the scheduler is what
    /// makes this API observable at all.
    fn ws_ctx() -> Context {
        let c = ctx();
        unsafe {
            crate::web::install(c.raw());
        }
        s(&c, PRELUDE);
        c
    }

    /// Promise adapters over the event surface, so each test body reads as the
    /// sequence it is testing.
    const PRELUDE: &str = "
        globalThis.opened = (ws) => new Promise((res, rej) => {
            ws.addEventListener('open', () => res(ws), { once: true });
            ws.addEventListener('error', () => rej(new Error('error event')), { once: true });
        });
        globalThis.nextMessage = (ws) => new Promise((res) =>
            ws.addEventListener('message', (e) => res(e), { once: true }));
        globalThis.closed = (ws) => new Promise((res) =>
            ws.addEventListener('close', (e) => res(e), { once: true }));
        globalThis.bytesOf = async (d) =>
            Array.from(new Uint8Array(d instanceof Blob ? await d.arrayBuffer() : d)).join(',');
        'ok'";

    fn run(c: &Context, body: &str) -> String {
        s(
            c,
            &format!(
                "globalThis.__out = '<pending>'; (async () => {{ {body} }})().then(\
                   v => {{ globalThis.__out = String(v); }}, \
                   e => {{ globalThis.__out = 'ERROR: ' + ((e && e.name ? e.name + ': ' : '') + ((e && e.message) || e)); }}); 'started'"
            ),
        );
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            unsafe {
                crate::web::pump(c.raw(), crate::web::PUMP_TIMERS | crate::web::PUMP_MICROTASKS, core::ptr::null_mut());
            }
            c.drain_microtasks();
            let out = s(c, "globalThis.__out");
            if out != "<pending>" {
                return out;
            }
            if Instant::now() > deadline {
                return "<timed out>".into();
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    // -- the test server ------------------------------------------------------

    #[derive(Clone, Default)]
    struct Cfg {
        /// `Sec-WebSocket-Protocol` for the 101 (None omits the header).
        protocol: Option<String>,
        /// Answer the upgrade with these raw bytes instead of a 101.
        reject_with: Option<Vec<u8>>,
        /// Send a wrong `Sec-WebSocket-Accept`.
        bad_accept: bool,
        /// Claim an extension that was never offered.
        rogue_extension: bool,
        /// Raw frames pushed immediately after the handshake.
        push: Vec<Vec<u8>>,
        /// Accept the TCP connection and then never answer.
        silent: bool,
    }

    /// Decodes a *client* frame: the mask is mandatory in that direction, which
    /// is the opposite of what `parse_frame` accepts.
    fn parse_client_frame(buf: &[u8]) -> Option<(super::Frame, usize)> {
        if buf.len() < 2 {
            return None;
        }
        let fin = buf[0] & 0x80 != 0;
        let opcode = buf[0] & 0x0f;
        let masked = buf[1] & 0x80 != 0;
        let (len, mut off) = match buf[1] & 0x7f {
            126 => {
                if buf.len() < 4 {
                    return None;
                }
                (u16::from_be_bytes([buf[2], buf[3]]) as usize, 4usize)
            }
            127 => {
                if buf.len() < 10 {
                    return None;
                }
                (u64::from_be_bytes(buf[2..10].try_into().unwrap()) as usize, 10usize)
            }
            n => (n as usize, 2usize),
        };
        let mut mask = [0u8; 4];
        if masked {
            if buf.len() < off + 4 {
                return None;
            }
            mask.copy_from_slice(&buf[off..off + 4]);
            off += 4;
        }
        if buf.len() < off + len {
            return None;
        }
        let mut payload = buf[off..off + len].to_vec();
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i & 3];
            }
        }
        Some((super::Frame { fin, opcode, payload }, off + len))
    }

    /// A one-connection RFC 6455 echo server on loopback. Returns its port.
    fn serve(cfg: Cfg) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else { return };
            let _ = sock.set_read_timeout(Some(Duration::from_secs(20)));
            let _ = sock.set_nodelay(true);

            let mut got: Vec<u8> = Vec::new();
            let mut buf = [0u8; 16 * 1024];
            let head_end = loop {
                match sock.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => {
                        got.extend_from_slice(&buf[..n]);
                        if let Some(i) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                    }
                    Err(_) => return,
                }
            };
            if cfg.silent {
                std::thread::sleep(Duration::from_secs(20));
                return;
            }
            if let Some(raw) = cfg.reject_with {
                let _ = sock.write_all(&raw);
                let _ = sock.flush();
                let _ = sock.shutdown(Shutdown::Both);
                return;
            }

            let head = String::from_utf8_lossy(&got[..head_end]).into_owned();
            let key = head
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("sec-websocket-key:"))
                .map(|l| l[l.find(':').unwrap() + 1..].trim().to_string())
                .unwrap_or_default();
            let accept =
                if cfg.bad_accept { "AAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string() } else { super::accept_key(&key) };

            let mut resp = String::from("HTTP/1.1 101 Switching Protocols\r\n");
            resp.push_str("Upgrade: websocket\r\nConnection: Upgrade\r\n");
            resp.push_str(&format!("Sec-WebSocket-Accept: {accept}\r\n"));
            if let Some(p) = &cfg.protocol {
                resp.push_str(&format!("Sec-WebSocket-Protocol: {p}\r\n"));
            }
            if cfg.rogue_extension {
                resp.push_str("Sec-WebSocket-Extensions: permessage-deflate\r\n");
            }
            resp.push_str("\r\n");
            // ONE write for the 101 and the pushed frames, on purpose: that
            // puts whole frames into the client's handshake over-read, which is
            // the path where a read-before-parse loop deadlocks until the
            // socket times out. Separate writes would make it a coin flip.
            let mut first = resp.into_bytes();
            for f in &cfg.push {
                first.extend_from_slice(f);
            }
            if sock.write_all(&first).is_err() {
                return;
            }
            let _ = sock.flush();

            let mut acc: Vec<u8> = got[head_end..].to_vec();
            let mut kind = OP_TEXT;
            let mut msg: Vec<u8> = Vec::new();
            loop {
                while let Some((f, used)) = parse_client_frame(&acc) {
                    acc.drain(..used);
                    match f.opcode {
                        OP_CLOSE => {
                            // Echo the body verbatim — that is what makes the
                            // code/reason round trip observable from JS.
                            let _ = sock.write_all(&server_frame(true, OP_CLOSE, &f.payload));
                            let _ = sock.flush();
                            let _ = sock.shutdown(Shutdown::Both);
                            return;
                        }
                        OP_PING => {
                            let _ = sock.write_all(&server_frame(true, OP_PONG, &f.payload));
                        }
                        OP_PONG => {
                            // Report the client's pong as data, so a JS test can
                            // see that the automatic reply happened.
                            let note = format!("pong:{}", String::from_utf8_lossy(&f.payload));
                            let _ = sock.write_all(&server_frame(true, OP_TEXT, note.as_bytes()));
                        }
                        OP_TEXT | OP_BINARY => {
                            kind = f.opcode;
                            msg.clear();
                            msg.extend_from_slice(&f.payload);
                            if f.fin {
                                let _ = sock.write_all(&server_frame(true, kind, &msg));
                            }
                        }
                        OP_CONT => {
                            msg.extend_from_slice(&f.payload);
                            if f.fin {
                                let _ = sock.write_all(&server_frame(true, kind, &msg));
                            }
                        }
                        _ => {}
                    }
                    let _ = sock.flush();
                }
                match sock.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => acc.extend_from_slice(&buf[..n]),
                    Err(_) => return,
                }
            }
        });
        port
    }

    // -- the surface, with no socket -----------------------------------------

    #[test]
    fn the_class_is_shaped_like_the_browsers() {
        let c = ws_ctx();
        ok(&c, "typeof WebSocket === 'function'");
        ok(&c, "WebSocket.length === 1");
        // The four constants exist on the constructor AND the prototype.
        ok(&c, "WebSocket.CONNECTING === 0 && WebSocket.OPEN === 1 && WebSocket.CLOSING === 2 && WebSocket.CLOSED === 3");
        ok(&c, "WebSocket.prototype.CONNECTING === 0 && WebSocket.prototype.CLOSED === 3");
        // A real EventTarget, not an object with a bag of callbacks.
        ok(&c, "Object.getPrototypeOf(WebSocket.prototype) === EventTarget.prototype");
        ok(&c, "typeof WebSocket.prototype.addEventListener === 'function'");
        // Real Event subclasses.
        ok(&c, "Object.getPrototypeOf(MessageEvent.prototype) === Event.prototype");
        ok(&c, "Object.getPrototypeOf(CloseEvent.prototype) === Event.prototype");
        ok(&c, "new MessageEvent('message', { data: 42 }).data === 42");
        ok(&c, "new MessageEvent('message') instanceof Event");
        let e = "new CloseEvent('close', { code: 1000, reason: 'bye', wasClean: true })";
        ok(&c, &format!("{e}.code === 1000 && {e}.reason === 'bye' && {e}.wasClean === true"));
        ok(&c, "Object.getOwnPropertyDescriptor(WebSocket.prototype, 'onmessage').set !== undefined");
    }

    #[test]
    fn a_malformed_url_or_protocol_list_throws_a_named_syntax_error() {
        let c = ws_ctx();
        for src in [
            "new WebSocket('ftp://example.com/')",
            "new WebSocket('/relative')",
            "new WebSocket('ws://example.com/x#frag')",
            "new WebSocket('ws://example.com/', ['a', 'a'])",
            "new WebSocket('ws://example.com/', ['not a token'])",
        ] {
            let m = throws(&c, src);
            assert!(!m.is_empty(), "{src} threw an empty message");
        }
        ok(&c, "(() => { try { new WebSocket('ftp://x/'); } catch (e) { return e.name === 'SyntaxError'; } })()");
        ok(&c, "(() => { try { new WebSocket(); } catch (e) { return e instanceof TypeError; } })()");
    }

    // -- the connection ------------------------------------------------------

    #[test]
    fn a_socket_opens_and_reports_its_url_state_and_origin() {
        let port = serve(Cfg::default());
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const ws = new WebSocket('ws://127.0.0.1:{port}/rpc'); \
                 const before = ws.readyState; \
                 await opened(ws); \
                 ws.send('x'); \
                 const e = await nextMessage(ws); \
                 return [before, ws.readyState, ws.url, ws.protocol, ws.extensions, e.origin].join('|');"
            ),
        );
        assert_eq!(out, format!("0|1|ws://127.0.0.1:{port}/rpc|||ws://127.0.0.1:{port}"));
    }

    #[test]
    fn a_text_message_round_trips_including_non_ascii() {
        let port = serve(Cfg::default());
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const ws = new WebSocket('ws://127.0.0.1:{port}/'); await opened(ws); \
                 ws.send('privet \u{00b7} shi jie \u{00b7} ok'); \
                 const e = await nextMessage(ws); \
                 return typeof e.data + ':' + e.data;"
            ),
        );
        assert_eq!(out, "string:privet \u{00b7} shi jie \u{00b7} ok");
    }

    #[test]
    fn binary_arrives_as_a_blob_by_default_and_as_an_arraybuffer_on_request() {
        // The default is the browser's default. Choosing "arraybuffer" here
        // because it is cheaper would break `e.data instanceof Blob` code.
        let port = serve(Cfg::default());
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const ws = new WebSocket('ws://127.0.0.1:{port}/'); await opened(ws); \
                 if (ws.binaryType !== 'blob') return 'DEFAULT ' + ws.binaryType; \
                 ws.send(new Uint8Array([0, 1, 127, 128, 255]).buffer); \
                 const a = await nextMessage(ws); \
                 const asBlob = (a.data instanceof Blob) + ':' + await bytesOf(a.data); \
                 ws.binaryType = 'arraybuffer'; \
                 ws.send(new Uint8Array([9, 8, 7]).buffer); \
                 const b = await nextMessage(ws); \
                 const asAb = (b.data instanceof ArrayBuffer) + ':' + await bytesOf(b.data); \
                 return asBlob + ' / ' + asAb;"
            ),
        );
        assert_eq!(out, "true:0,1,127,128,255 / true:9,8,7");
    }

    #[test]
    fn a_typed_array_view_sends_only_its_own_bytes() {
        // `send(view)` must send the VIEW, not the whole backing buffer — the
        // classic way a framing bug ships extra bytes nobody asked for.
        let port = serve(Cfg::default());
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const ws = new WebSocket('ws://127.0.0.1:{port}/'); await opened(ws); \
                 ws.binaryType = 'arraybuffer'; \
                 const backing = new Uint8Array([1, 2, 3, 4, 5, 6]); \
                 ws.send(backing.subarray(2, 5)); \
                 const e = await nextMessage(ws); \
                 return await bytesOf(e.data);"
            ),
        );
        assert_eq!(out, "3,4,5");
    }

    #[test]
    fn a_blob_can_be_sent_and_comes_back_byte_for_byte() {
        let port = serve(Cfg::default());
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const ws = new WebSocket('ws://127.0.0.1:{port}/'); await opened(ws); \
                 ws.binaryType = 'arraybuffer'; \
                 ws.send(new Blob([new Uint8Array([10, 20, 30])])); \
                 const e = await nextMessage(ws); \
                 return await bytesOf(e.data);"
            ),
        );
        assert_eq!(out, "10,20,30");
    }

    #[test]
    fn a_large_binary_message_survives_the_64_bit_length_path() {
        // 300 000 bytes forces the 127/8-byte length encoding in both
        // directions, and forces reassembly across many socket reads.
        let port = serve(Cfg::default());
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const ws = new WebSocket('ws://127.0.0.1:{port}/'); await opened(ws); \
                 ws.binaryType = 'arraybuffer'; \
                 const n = 300000; const a = new Uint8Array(n); \
                 for (let i = 0; i < n; i++) a[i] = i & 255; \
                 ws.send(a.buffer); \
                 const e = await nextMessage(ws); \
                 const b = new Uint8Array(e.data); \
                 if (b.length !== n) return 'LENGTH ' + b.length; \
                 for (let i = 0; i < n; i++) if (b[i] !== (i & 255)) return 'MISMATCH at ' + i; \
                 return 'exact:' + n + ':' + ws.bufferedAmount;"
            ),
        );
        assert_eq!(out, "exact:300000:0");
    }

    #[test]
    fn buffered_amount_is_a_real_number_and_not_a_constant_zero() {
        // Two halves, both deterministic: bytes that WERE sent have drained by
        // the time their echo arrives, and bytes handed to a closing socket are
        // charged and never sent (HTML, "The WebSocket interface", send()).
        let port = serve(Cfg::default());
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const ws = new WebSocket('ws://127.0.0.1:{port}/'); await opened(ws); \
                 ws.send('abcdefghij'); await nextMessage(ws); \
                 const drained = ws.bufferedAmount; \
                 ws.close(); \
                 ws.send('0123456789'); \
                 return drained + '/' + ws.bufferedAmount + '/' + ws.readyState;"
            ),
        );
        assert_eq!(out, "0/10/2");
    }

    #[test]
    fn the_close_code_and_reason_round_trip_and_the_close_is_clean() {
        let port = serve(Cfg::default());
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const ws = new WebSocket('ws://127.0.0.1:{port}/'); await opened(ws); \
                 ws.close(4321, 'done here'); \
                 const e = await closed(ws); \
                 return [e.code, e.reason, e.wasClean, ws.readyState, e instanceof CloseEvent].join('|');"
            ),
        );
        assert_eq!(out, "4321|done here|true|3|true");
    }

    #[test]
    fn close_arguments_are_validated_the_way_the_spec_says() {
        let port = serve(Cfg::default());
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const ws = new WebSocket('ws://127.0.0.1:{port}/'); await opened(ws); \
                 const names = []; \
                 try {{ ws.close(1001); }} catch (e) {{ names.push(e.name); }} \
                 try {{ ws.close(2999); }} catch (e) {{ names.push(e.name); }} \
                 try {{ ws.close(70000); }} catch (e) {{ names.push(e.name); }} \
                 try {{ ws.close(1000, 'x'.repeat(124)); }} catch (e) {{ names.push(e.name); }} \
                 ws.close(1000, 'ok'); \
                 const e = await closed(ws); \
                 return names.join(',') + '|' + e.code;"
            ),
        );
        // 70000 clamps to 65535 and is refused; truncating it to 4464 would
        // have put a plausible wrong code on the wire.
        assert_eq!(out, "InvalidAccessError,InvalidAccessError,InvalidAccessError,SyntaxError|1000");
    }

    #[test]
    fn a_server_initiated_close_is_reported_with_its_own_code() {
        let port = serve(Cfg {
            push: vec![server_frame(true, OP_CLOSE, &[0x03, 0xf3, b'b', b'y', b'e'])],
            ..Cfg::default()
        });
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const ws = new WebSocket('ws://127.0.0.1:{port}/'); \
                 const e = await closed(ws); \
                 return [e.code, e.reason, e.wasClean].join('|');"
            ),
        );
        // 0x03f3 = 1011 (internal error).
        assert_eq!(out, "1011|bye|true");
    }

    #[test]
    fn a_server_ping_is_answered_with_a_pong_carrying_the_same_payload() {
        // RFC 6455 makes this mandatory; a server using pings as a liveness
        // check drops a client that does not answer.
        let port = serve(Cfg { push: vec![server_frame(true, OP_PING, b"beat")], ..Cfg::default() });
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const ws = new WebSocket('ws://127.0.0.1:{port}/'); await opened(ws); \
                 const e = await nextMessage(ws); return e.data;"
            ),
        );
        assert_eq!(out, "pong:beat");
    }

    #[test]
    fn a_fragmented_server_message_arrives_as_one_message() {
        let port = serve(Cfg {
            push: vec![
                server_frame(false, OP_TEXT, b"one "),
                server_frame(false, OP_CONT, b"two "),
                // A control frame interleaved inside a fragmented message is legal.
                server_frame(true, OP_PONG, b""),
                server_frame(true, OP_CONT, b"three"),
            ],
            ..Cfg::default()
        });
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const ws = new WebSocket('ws://127.0.0.1:{port}/'); await opened(ws); \
                 const e = await nextMessage(ws); return e.data;"
            ),
        );
        assert_eq!(out, "one two three");
    }

    #[test]
    fn a_subprotocol_is_offered_and_reported_back() {
        let port = serve(Cfg { protocol: Some("ion".into()), ..Cfg::default() });
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const ws = new WebSocket('ws://127.0.0.1:{port}/', ['ion', 'cbor']); \
                 await opened(ws); return ws.protocol;"
            ),
        );
        assert_eq!(out, "ion");
    }

    #[test]
    fn a_subprotocol_that_was_never_offered_fails_the_connection() {
        let port = serve(Cfg { protocol: Some("surprise".into()), ..Cfg::default() });
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const ws = new WebSocket('ws://127.0.0.1:{port}/', ['ion']); \
                 const seen = []; \
                 ws.addEventListener('error', () => seen.push('error')); \
                 const e = await closed(ws); \
                 return seen.join(',') + '|' + e.code + '|' + e.wasClean + '|' + ws.readyState;"
            ),
        );
        assert_eq!(out, "error|1006|false|3");
    }

    #[test]
    fn an_unoffered_extension_fails_the_connection_instead_of_being_ignored() {
        // Ignoring it would mean decoding every following frame wrong.
        let port = serve(Cfg { rogue_extension: true, ..Cfg::default() });
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const ws = new WebSocket('ws://127.0.0.1:{port}/'); \
                 const e = await closed(ws); return e.code + '|' + ws.extensions;"
            ),
        );
        assert_eq!(out, "1006|");
    }

    #[test]
    fn a_wrong_accept_key_never_reaches_open() {
        let port = serve(Cfg { bad_accept: true, ..Cfg::default() });
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const ws = new WebSocket('ws://127.0.0.1:{port}/'); \
                 let sawOpen = false; ws.onopen = () => {{ sawOpen = true; }}; \
                 const e = await closed(ws); return sawOpen + '|' + e.code;"
            ),
        );
        assert_eq!(out, "false|1006");
    }

    #[test]
    fn a_plain_http_answer_to_the_upgrade_fires_error_then_close() {
        let port = serve(Cfg {
            reject_with: Some(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()),
            ..Cfg::default()
        });
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                // The `close` promise is resolved FROM the onclose handler on
                // purpose: JSC drains the microtask queue when each native→JS
                // call unwinds, so a promise settled by an `addEventListener`
                // listener can run its continuation before `events.rs` gets to
                // the `on<type>` handler property.
                "const ws = new WebSocket('ws://127.0.0.1:{port}/'); \
                 const order = []; \
                 ws.onerror = () => order.push('error'); \
                 await new Promise((res) => {{ ws.onclose = (e) => {{ \
                     order.push('close:' + e.code + ':' + e.wasClean); res(); \
                 }}; }}); \
                 return order.join(' -> ');"
            ),
        );
        assert_eq!(out, "error -> close:1006:false");
    }

    #[test]
    fn a_connection_that_is_never_answered_can_be_closed_from_js() {
        // close() while CONNECTING must fail the connection promptly rather
        // than sitting on the handshake timeout.
        let port = serve(Cfg { silent: true, ..Cfg::default() });
        let c = ws_ctx();
        let started = Instant::now();
        let out = run(
            &c,
            &format!(
                "const ws = new WebSocket('ws://127.0.0.1:{port}/'); \
                 setTimeout(() => ws.close(), 100); \
                 const e = await closed(ws); \
                 return e.code + '|' + e.wasClean + '|' + ws.readyState;"
            ),
        );
        assert_eq!(out, "1006|false|3");
        assert!(started.elapsed() < Duration::from_secs(10), "close() during CONNECTING did not cut the socket");
    }

    #[test]
    fn a_refused_connection_reports_a_close_rather_than_hanging() {
        let c = ws_ctx();
        // Port 1 on loopback: nothing listens, so connect fails immediately.
        let out = run(
            &c,
            "const ws = new WebSocket('ws://127.0.0.1:1/'); \
             const e = await closed(ws); return e.code + '|' + e.wasClean;",
        );
        assert_eq!(out, "1006|false");
    }

    #[test]
    fn send_before_open_throws_invalidstateerror() {
        let port = serve(Cfg { silent: true, ..Cfg::default() });
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const ws = new WebSocket('ws://127.0.0.1:{port}/'); \
                 let name = 'NONE'; try {{ ws.send('x'); }} catch (e) {{ \
                   name = (e instanceof DOMException ? '' : 'plain ') + e.name + '/' + e.code; }} \
                 ws.close(); await closed(ws); return name;"
            ),
        );
        assert_eq!(out, "InvalidStateError/11");
    }

    #[test]
    fn both_the_handler_property_and_addeventlistener_fire() {
        let port = serve(Cfg::default());
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const ws = new WebSocket('ws://127.0.0.1:{port}/'); \
                 const seen = []; \
                 if (ws.onmessage !== null) return 'UNSET HANDLER IS ' + ws.onmessage; \
                 ws.onmessage = (e) => seen.push('handler:' + e.data); \
                 ws.addEventListener('message', (e) => seen.push('listener:' + e.data)); \
                 await opened(ws); ws.send('hi'); \
                 await new Promise((r) => setTimeout(r, 300)); \
                 return seen.join(',');"
            ),
        );
        // Listeners run before the handler property, as in a browser.
        assert_eq!(out, "listener:hi,handler:hi");
    }

    #[test]
    fn two_sockets_on_one_context_do_not_cross_their_events() {
        let a = serve(Cfg::default());
        let b = serve(Cfg::default());
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const x = new WebSocket('ws://127.0.0.1:{a}/'); \
                 const y = new WebSocket('ws://127.0.0.1:{b}/'); \
                 await Promise.all([opened(x), opened(y)]); \
                 x.send('from-x'); y.send('from-y'); \
                 const [ex, ey] = await Promise.all([nextMessage(x), nextMessage(y)]); \
                 return ex.data + '|' + ey.data;"
            ),
        );
        assert_eq!(out, "from-x|from-y");
    }

    // -- live network, off by default ----------------------------------------

    /// A real `wss://` round trip. Off unless `REDJS_WS_LIVE=1`, because a
    /// suite that depends on the internet is a suite that fails for reasons
    /// that are not the code's.
    #[test]
    fn a_wss_url_completes_a_real_tls_handshake_and_echoes() {
        if std::env::var("REDJS_WS_LIVE").as_deref() != Ok("1") {
            eprintln!("[skip] set REDJS_WS_LIVE=1 (REDJS_WS_LIVE_URL overrides the endpoint) to run this");
            return;
        }
        // Verified against wss://echo.websocket.org/, wss://ws.ifelse.io/ and
        // wss://ws.postman-echo.com/raw — the loop below tolerates a server
        // that sends a greeting frame before it starts echoing.
        let url = std::env::var("REDJS_WS_LIVE_URL").unwrap_or_else(|_| "wss://echo.websocket.org/".into());
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const ws = new WebSocket({url:?}); await opened(ws); \
                 const sent = 'redjs-' + Date.now(); \
                 for (let i = 0; i < 5; i++) {{ \
                     ws.send(sent); \
                     const e = await nextMessage(ws); \
                     if (String(e.data).includes(sent)) return 'echoed'; \
                 }} \
                 return 'NO ECHO';"
            ),
        );
        assert_eq!(out, "echoed", "live wss round trip failed against {url}");
    }

    // -- WebSocketStream ------------------------------------------------------

    #[test]
    fn websocketstream_exists_and_hands_back_a_stream_pair() {
        let port = serve(Cfg { protocol: Some("ion".into()), ..Cfg::default() });
        let c = ws_ctx();
        ok(&c, "typeof WebSocketStream === 'function'");
        let out = run(
            &c,
            &format!(
                "const s = new WebSocketStream('ws://127.0.0.1:{port}/rpc', {{ protocols: ['ion'] }}); \
                 const o = await s.opened; \
                 return [s.url, o.protocol, o.extensions, \
                         o.readable instanceof ReadableStream, o.writable instanceof WritableStream].join('|');"
            ),
        );
        assert_eq!(out, format!("ws://127.0.0.1:{port}/rpc|ion||true|true"));
    }

    #[test]
    fn websocketstream_round_trips_binary_as_arraybuffer() {
        // Exactly the shape `@argon-chat/ion.webcore` uses: write a Uint8Array
        // through the writer, read an ArrayBuffer back off the reader. The
        // library brand-checks `value instanceof ArrayBuffer` and throws
        // otherwise, so the type here is load-bearing, not cosmetic.
        let port = serve(Cfg::default());
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const s = new WebSocketStream('ws://127.0.0.1:{port}/'); \
                 const {{ readable, writable }} = await s.opened; \
                 await writable.getWriter().write(new Uint8Array([0, 1, 2, 250, 255])); \
                 const {{ value, done }} = await readable.getReader().read(); \
                 if (done) return 'DONE'; \
                 if (!(value instanceof ArrayBuffer)) return 'TYPE ' + Object.prototype.toString.call(value); \
                 return Array.from(new Uint8Array(value)).join(',');"
            ),
        );
        assert_eq!(out, "0,1,2,250,255");
    }

    #[test]
    fn websocketstream_closed_carries_the_code_and_reason() {
        let port = serve(Cfg::default());
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const s = new WebSocketStream('ws://127.0.0.1:{port}/'); \
                 await s.opened; \
                 s.close({{ closeCode: 4010, reason: 'bye' }}); \
                 const info = await s.closed; \
                 return info.closeCode + '|' + info.reason;"
            ),
        );
        assert_eq!(out, "4010|bye");
    }

    #[test]
    fn websocketstream_opened_rejects_when_the_connection_fails() {
        // The RPC layer's retry loop depends on this: a failure has to arrive
        // as a rejection of `opened`, not as a stream pair that never delivers.
        let c = ws_ctx();
        let out = run(
            &c,
            "const s = new WebSocketStream('ws://127.0.0.1:1/'); \
             try { await s.opened; return 'OPENED'; } catch (e) { \
               return (e instanceof DOMException ? '' : 'plain ') + e.name + '/' + e.code; }",
        );
        assert_eq!(out, "NetworkError/19");
    }

    #[test]
    fn an_abort_signal_tears_a_websocketstream_down() {
        let port = serve(Cfg { silent: true, ..Cfg::default() });
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const ac = new AbortController(); \
                 const s = new WebSocketStream('ws://127.0.0.1:{port}/', {{ signal: ac.signal }}); \
                 setTimeout(() => ac.abort(), 100); \
                 try {{ await s.opened; return 'OPENED'; }} catch (e) {{ return e.name; }}"
            ),
        );
        assert_eq!(out, "NetworkError");
    }

    #[test]
    fn a_websocketstream_readable_ends_when_the_peer_closes_cleanly() {
        let port = serve(Cfg {
            push: vec![
                server_frame(true, OP_TEXT, b"last"),
                server_frame(true, OP_CLOSE, &[0x03, 0xe8]),
            ],
            ..Cfg::default()
        });
        let c = ws_ctx();
        let out = run(
            &c,
            &format!(
                "const s = new WebSocketStream('ws://127.0.0.1:{port}/'); \
                 const {{ readable }} = await s.opened; \
                 const r = readable.getReader(); \
                 const first = await r.read(); \
                 const second = await r.read(); \
                 const info = await s.closed; \
                 return first.value + '|' + second.done + '|' + info.closeCode;"
            ),
        );
        assert_eq!(out, "last|true|1000");
    }
}
