//! Blocking HTTP/1.1 client — the transport behind `fetch` for `http:`/`https:`.
//!
//! ## Why this exists at all
//!
//! Until now `fetch` served `file:`-shaped local paths and `data:` URIs, and
//! answered every `http(s)` URL by printing a line to stderr and *resolving*
//! with a status-0 `Response`. A caller asking for a URL got a success-shaped
//! object describing a request that never happened. Everything in this module
//! exists so that a request either really happens or **fails loudly**; see
//! [`fetch`](super::fetch) for the promise side of that contract.
//!
//! ## Shape
//!
//! Synchronous and blocking on purpose. The runtime is a single-threaded pump
//! that holds the VM lock across a frame, so the *only* two designs available
//! are "block the pump" (stalls the application for the whole round trip) and
//! "do the I/O on a worker and hand the result back through the pump". This is
//! the worker half: it never touches JSC, never sees an `EncodedJSValue`, and
//! is callable from any thread. `fetch.rs` owns the handoff (a boxed task on
//! `web::schedule_native`, exactly like image decoding does).
//!
//! Being blocking is what makes that split cheap: no async runtime, no
//! executor, no `tokio` in a shipped player's link.
//!
//! ## Owning the socket
//!
//! HTTP/1.1 framing is implemented here rather than taken from a client crate
//! for one concrete reason: **`AbortSignal` has to be real**. A client crate
//! that owns its own connection pool hands out no socket, and the honest
//! implementation there degrades to "reject the promise and let the socket run
//! to timeout" — a leaked connection per aborted request.
//!
//! Cancellation is done by *polling*, not purely by `shutdown`: the socket
//! carries a short [`POLL`] read/write timeout and every blocking call retries
//! in a loop that checks the abort flag between attempts. `shutdown(Both)` is
//! still issued (it ends the connection promptly and is what wakes a blocked
//! `recv` on Unix), but it is not load-bearing — **on Windows `shutdown` from
//! another thread does not reliably interrupt a `recv` already in progress**,
//! which a test in this module measured as a 20-second wait when the poll loop
//! was missing. Abort latency is therefore bounded by [`POLL`], on every
//! platform, including mid-TLS-handshake.
//!
//! The same loop supplies the stall budget: `io_timeout` is spent in [`POLL`]
//! slices and reset by any byte that arrives, so a slow-but-progressing
//! download is never cut off while a dead connection still gives up on time.
//!
//! ## What is deliberately not here
//!
//! - **Keep-alive / connection pooling.** Every request sends `Connection:
//!   close` and uses a fresh connection. Costs one handshake per request; buys
//!   a body-framing story with no reuse hazards.
//! - **HTTP/2.** Not offered in ALPN, so servers answer 1.1.
//! - **Proxies**: refused by name at the `fetch` layer rather than silently
//!   ignored.
//!
//! ## Cookies
//!
//! [`Request::credentials`] decides, PER HOP, whether the jar
//! ([`super::cookies`]) contributes a `Cookie` header to the request going
//! out and stores the `Set-Cookie` headers of the head that comes back. Per
//! hop is the point: a cookie set by a redirect is on the request it points
//! at, and a hop to another host carries only that host's cookies.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Response heads larger than this are a malformed or hostile peer.
const MAX_HEAD: usize = 64 * 1024;
/// Body ceiling. A cap that rejects loudly beats an allocation that takes the
/// process down; the player streams big assets off disk, not off the network.
pub const MAX_BODY: usize = 256 * 1024 * 1024;
const MAX_REDIRECTS: u32 = 20;
const MAX_CHUNK: u64 = MAX_BODY as u64;
/// Socket timeout slice. Bounds how long an abort can take to be noticed, and
/// is the unit `io_timeout` is spent in. Short enough to feel instant, long
/// enough not to spin.
pub(crate) const POLL: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The request was never attempted — a shape this client refuses.
    Refused,
    /// DNS, connect, TLS, read/write, timeout.
    Network,
    /// The peer answered, but not with something that parses as HTTP/1.1.
    Protocol,
    /// An `AbortSignal` fired.
    Aborted,
}

#[derive(Debug, Clone)]
pub struct HttpError {
    pub kind: ErrorKind,
    pub message: String,
}

impl HttpError {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        HttpError { kind, message: message.into() }
    }
    fn refused(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Refused, m)
    }
    fn network(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Network, m)
    }
    fn protocol(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Protocol, m)
    }
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

/// The `AbortSignal` bridge. Shared between the JS thread (which calls
/// [`abort`](Cancel::abort) from the signal's `abort` listener) and the worker
/// thread (which is parked in a blocking socket call).
///
/// `shutdown` on a duplicated handle is the mechanism: it makes the in-flight
/// `read`/`write` return immediately instead of running to the read timeout.
#[derive(Default)]
pub struct Cancel {
    aborted: AtomicBool,
    /// A `try_clone` of the live socket, present only while one is open.
    socket: Mutex<Option<TcpStream>>,
}

impl Cancel {
    pub fn new() -> Arc<Cancel> {
        Arc::new(Cancel::default())
    }

    /// Idempotent. Safe to call before the socket exists (the flag is checked
    /// at every step) and after it is gone.
    pub fn abort(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        if let Some(s) = self.socket.lock().unwrap().as_ref() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }

    fn check(&self) -> Result<(), HttpError> {
        if self.is_aborted() {
            return Err(HttpError::new(ErrorKind::Aborted, "the request was aborted"));
        }
        Ok(())
    }

    /// Publishes the socket so `abort` can reach it. Races correctly: an abort
    /// that landed between the connect and here is caught by the re-check.
    fn arm(&self, s: &TcpStream) -> Result<(), HttpError> {
        match s.try_clone() {
            Ok(dup) => *self.socket.lock().unwrap() = Some(dup),
            // Without a handle we cannot cancel; the read timeout is the
            // backstop. Not fatal to the request.
            Err(_) => {}
        }
        self.check()
    }

    fn disarm(&self) {
        *self.socket.lock().unwrap() = None;
    }

    /// Maps a socket error, preferring "aborted" when we caused it.
    fn map_io(&self, e: std::io::Error, what: &str) -> HttpError {
        if self.is_aborted() {
            return HttpError::new(ErrorKind::Aborted, "the request was aborted");
        }
        match e.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                HttpError::network(format!("{what} timed out"))
            }
            _ => HttpError::network(format!("{what} failed: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Request / response values
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redirect {
    Follow,
    /// Spec `redirect: "error"` — a redirect response is a network error.
    Error,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub redirect: Redirect,
    /// Fetch's credentials mode: whether each hop sends the jar's cookies and
    /// stores the `Set-Cookie` it gets back (see the module header).
    pub credentials: super::cookies::Credentials,
    /// Ceiling on the TCP connect only. Kept separate from `io_timeout`: a dead
    /// host must fail fast, while a live-but-slow transfer deserves patience.
    pub connect_timeout: Duration,
    /// Per-read/per-write stall ceiling, not a total-request budget — a long
    /// download that keeps delivering bytes is never cut off.
    pub io_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    /// The FINAL URL — after redirects, which is what `Response.url` means.
    pub url: String,
    pub redirected: bool,
    pub body: Vec<u8>,
}

pub fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

// ---------------------------------------------------------------------------
// Head parsing (pure)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
}

/// Parses a complete response head (everything before the blank line).
///
/// Strict on the status line on purpose: a peer that is not speaking HTTP —
/// a captive portal, a TLS-on-a-plaintext-port mixup, a proxy error page — must
/// surface as a protocol failure, not as some default status.
pub fn parse_head(buf: &[u8]) -> Result<Head, HttpError> {
    let text = String::from_utf8_lossy(buf);
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let (status, reason) = parse_status_line(status_line)?;

    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        // Obsolete line folding (leading space) — append to the previous value.
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = headers.last_mut() {
                last.1.push(' ');
                last.1.push_str(line.trim());
                continue;
            }
            return Err(HttpError::protocol("malformed response: header continuation with no header"));
        }
        let Some(colon) = line.find(':') else {
            return Err(HttpError::protocol(format!("malformed response header line: {line:?}")));
        };
        let name = line[..colon].trim_end();
        if name.is_empty() || !name.bytes().all(is_token_byte) {
            return Err(HttpError::protocol(format!("malformed response header name: {name:?}")));
        }
        headers.push((name.to_ascii_lowercase(), line[colon + 1..].trim().to_string()));
    }
    Ok(Head { status, reason, headers })
}

fn parse_status_line(line: &str) -> Result<(u16, String), HttpError> {
    let rest = line
        .strip_prefix("HTTP/1.1 ")
        .or_else(|| line.strip_prefix("HTTP/1.0 "))
        .ok_or_else(|| HttpError::protocol(format!("not an HTTP/1.x response: {:?}", trunc(line, 80))))?;
    let code = rest.split(' ').next().unwrap_or("");
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(HttpError::protocol(format!("malformed status line: {:?}", trunc(line, 80))));
    }
    let status: u16 = code.parse().map_err(|_| HttpError::protocol("malformed status code"))?;
    let reason = rest[code.len()..].trim().to_string();
    Ok((status, reason))
}

fn trunc(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Rejects anything that would let a header value or name inject a new line
/// into the request — request smuggling via a JS-supplied header.
fn validate_header(name: &str, value: &str) -> Result<(), HttpError> {
    if name.is_empty() || !name.bytes().all(is_token_byte) {
        return Err(HttpError::refused(format!("invalid header name: {name:?}")));
    }
    if value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
        return Err(HttpError::refused(format!("invalid header value for {name:?}: contains CR, LF or NUL")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Body framing (pure decisions, reader-driven reads)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    None,
    Length(usize),
    Chunked,
    UntilClose,
}

/// RFC 9112 §6.3, in the order the spec applies it.
pub fn framing_of(method: &str, status: u16, headers: &[(String, String)]) -> Result<Framing, HttpError> {
    if method.eq_ignore_ascii_case("HEAD") || status == 204 || status == 304 || (100..200).contains(&status) {
        return Ok(Framing::None);
    }
    if let Some(te) = header(headers, "transfer-encoding") {
        if te.split(',').any(|t| t.trim().eq_ignore_ascii_case("chunked")) {
            return Ok(Framing::Chunked);
        }
        return Err(HttpError::protocol(format!("unsupported Transfer-Encoding: {te:?}")));
    }
    if let Some(cl) = header(headers, "content-length") {
        // A split Content-Length is the classic smuggling vector; refuse unless
        // every value agrees.
        let mut seen: Option<usize> = None;
        for part in cl.split(',') {
            let n: usize = part
                .trim()
                .parse()
                .map_err(|_| HttpError::protocol(format!("malformed Content-Length: {cl:?}")))?;
            if seen.is_some_and(|s| s != n) {
                return Err(HttpError::protocol(format!("conflicting Content-Length values: {cl:?}")));
            }
            seen = Some(n);
        }
        let n = seen.ok_or_else(|| HttpError::protocol("empty Content-Length"))?;
        if n > MAX_BODY {
            return Err(HttpError::refused(format!("response body of {n} bytes exceeds the {MAX_BODY}-byte limit")));
        }
        return Ok(Framing::Length(n));
    }
    Ok(Framing::UntilClose)
}

/// Exactly `n` more bytes. A short read is an ERROR, never a truncated body:
/// a caller handed half a JSON document with a 200 status is the same class of
/// defect this whole module was written to remove.
pub fn read_exact_body<R: Read>(r: &mut R, n: usize, prefix: &[u8]) -> Result<Vec<u8>, HttpError> {
    let mut out = Vec::with_capacity(n.min(1 << 20));
    out.extend_from_slice(&prefix[..prefix.len().min(n)]);
    let mut buf = [0u8; 16 * 1024];
    while out.len() < n {
        let want = (n - out.len()).min(buf.len());
        let got = r.read(&mut buf[..want]).map_err(|e| HttpError::network(format!("body read failed: {e}")))?;
        if got == 0 {
            return Err(HttpError::protocol(format!(
                "connection closed after {} of {} declared body bytes",
                out.len(),
                n
            )));
        }
        out.extend_from_slice(&buf[..got]);
    }
    Ok(out)
}

/// Until EOF (`Connection: close` framing).
pub fn read_until_close<R: Read>(r: &mut R, prefix: &[u8]) -> Result<Vec<u8>, HttpError> {
    let mut out = prefix.to_vec();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let got = r.read(&mut buf).map_err(|e| HttpError::network(format!("body read failed: {e}")))?;
        if got == 0 {
            return Ok(out);
        }
        if out.len() + got > MAX_BODY {
            return Err(HttpError::refused(format!("response body exceeds the {MAX_BODY}-byte limit")));
        }
        out.extend_from_slice(&buf[..got]);
    }
}

/// `Transfer-Encoding: chunked`. Terminates only on a real zero-length chunk;
/// a stream that dies mid-body is an error, not a short body.
pub fn read_chunked<R: Read>(r: &mut R, prefix: &[u8]) -> Result<Vec<u8>, HttpError> {
    let mut src = Prefixed::new(r, prefix);
    let mut out: Vec<u8> = Vec::new();
    loop {
        let line = src.read_line()?;
        // chunk-size [ ";" chunk-ext ]
        let size_text = line.split(';').next().unwrap_or("").trim();
        if size_text.is_empty() {
            return Err(HttpError::protocol("malformed chunked body: empty chunk size"));
        }
        let size = u64::from_str_radix(size_text, 16)
            .map_err(|_| HttpError::protocol(format!("malformed chunk size: {size_text:?}")))?;
        if size > MAX_CHUNK {
            return Err(HttpError::refused(format!("chunk of {size} bytes exceeds the {MAX_CHUNK}-byte limit")));
        }
        if size == 0 {
            // Trailer section, terminated by a blank line.
            loop {
                let t = src.read_line()?;
                if t.is_empty() {
                    break;
                }
            }
            return Ok(out);
        }
        if out.len() as u64 + size > MAX_BODY as u64 {
            return Err(HttpError::refused(format!("response body exceeds the {MAX_BODY}-byte limit")));
        }
        let start = out.len();
        out.resize(start + size as usize, 0);
        src.read_exact_into(&mut out[start..])?;
        let crlf = src.read_line()?;
        if !crlf.is_empty() {
            return Err(HttpError::protocol("malformed chunked body: missing CRLF after chunk data"));
        }
    }
}

/// A reader with bytes already pulled off the socket in front of it (the head
/// read almost always over-reads into the body).
struct Prefixed<'a, R: Read> {
    inner: &'a mut R,
    buf: Vec<u8>,
    pos: usize,
}

impl<'a, R: Read> Prefixed<'a, R> {
    fn new(inner: &'a mut R, prefix: &[u8]) -> Self {
        Prefixed { inner, buf: prefix.to_vec(), pos: 0 }
    }

    fn fill(&mut self) -> Result<bool, HttpError> {
        let mut tmp = [0u8; 16 * 1024];
        let got = self.inner.read(&mut tmp).map_err(|e| HttpError::network(format!("body read failed: {e}")))?;
        if got == 0 {
            return Ok(false);
        }
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(&tmp[..got]);
        Ok(true)
    }

    /// One CRLF-terminated line, without the CRLF.
    fn read_line(&mut self) -> Result<String, HttpError> {
        loop {
            if let Some(i) = self.buf[self.pos..].windows(2).position(|w| w == b"\r\n") {
                let line = String::from_utf8_lossy(&self.buf[self.pos..self.pos + i]).into_owned();
                self.pos += i + 2;
                return Ok(line);
            }
            if self.buf.len() - self.pos > MAX_HEAD {
                return Err(HttpError::protocol("malformed chunked body: oversized line"));
            }
            if !self.fill()? {
                return Err(HttpError::protocol("connection closed inside a chunked body"));
            }
        }
    }

    fn read_exact_into(&mut self, dst: &mut [u8]) -> Result<(), HttpError> {
        let mut at = 0;
        while at < dst.len() {
            let avail = self.buf.len() - self.pos;
            if avail == 0 {
                if !self.fill()? {
                    return Err(HttpError::protocol("connection closed inside a chunked body"));
                }
                continue;
            }
            let take = avail.min(dst.len() - at);
            dst[at..at + take].copy_from_slice(&self.buf[self.pos..self.pos + take]);
            self.pos += take;
            at += take;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Content-Encoding
// ---------------------------------------------------------------------------

/// Decodes what we advertised in `Accept-Encoding`. `flate2` is already in this
/// crate's graph for `CompressionStream`, so gzip/deflate support is free; `br`
/// is NOT advertised, and a server that sends it anyway is refused by name
/// rather than handed to JS as bytes that look like garbage.
pub fn decode_content_encoding(enc: Option<&str>, bytes: Vec<u8>) -> Result<Vec<u8>, HttpError> {
    let Some(enc) = enc else { return Ok(bytes) };
    let enc = enc.trim();
    if enc.is_empty() || enc.eq_ignore_ascii_case("identity") {
        return Ok(bytes);
    }
    // Multiple codings are legal but vanishingly rare; refuse rather than guess.
    if enc.contains(',') {
        return Err(HttpError::refused(format!("stacked Content-Encoding is not supported: {enc:?}")));
    }
    if bytes.is_empty() {
        return Ok(bytes);
    }
    let mut out = Vec::new();
    let r = if enc.eq_ignore_ascii_case("gzip") || enc.eq_ignore_ascii_case("x-gzip") {
        flate2::read::GzDecoder::new(&bytes[..]).read_to_end(&mut out)
    } else if enc.eq_ignore_ascii_case("deflate") {
        // "deflate" on the wire is zlib in practice, raw deflate in the wild.
        match flate2::read::ZlibDecoder::new(&bytes[..]).read_to_end(&mut out) {
            Ok(n) => Ok(n),
            Err(_) => {
                out.clear();
                flate2::read::DeflateDecoder::new(&bytes[..]).read_to_end(&mut out)
            }
        }
    } else {
        return Err(HttpError::refused(format!("unsupported Content-Encoding: {enc:?}")));
    };
    r.map_err(|e| HttpError::protocol(format!("Content-Encoding {enc} failed to decode: {e}")))?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Redirects
// ---------------------------------------------------------------------------

pub fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Fetch §"HTTP-redirect fetch": 303 (and the historical 301/302-on-POST) turn
/// into GET and drop the body; 307/308 preserve both.
pub fn redirect_method(status: u16, method: &str) -> (String, bool) {
    let upper = method.to_ascii_uppercase();
    match status {
        303 if upper != "HEAD" => ("GET".into(), false),
        301 | 302 if upper == "POST" => ("GET".into(), false),
        _ => (upper, true),
    }
}

/// Resolves `Location` against the URL it came from.
pub fn resolve_location(base: &str, location: &str) -> Result<String, HttpError> {
    let base = url::Url::parse(base).map_err(|e| HttpError::protocol(format!("bad base URL: {e}")))?;
    base.join(location.trim())
        .map(|u| u.to_string())
        .map_err(|e| HttpError::protocol(format!("bad Location header {location:?}: {e}")))
}

/// Same origin = same scheme, host and effective port.
pub fn same_origin(a: &str, b: &str) -> bool {
    let (Ok(a), Ok(b)) = (url::Url::parse(a), url::Url::parse(b)) else { return false };
    a.scheme() == b.scheme() && a.host_str() == b.host_str() && a.port_or_known_default() == b.port_or_known_default()
}

/// Headers that must not survive a cross-origin redirect.
fn strip_sensitive(headers: &mut Vec<(String, String)>) {
    headers.retain(|(k, _)| {
        !matches!(k.to_ascii_lowercase().as_str(), "authorization" | "cookie" | "proxy-authorization" | "www-authenticate")
    });
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// The connected socket, plaintext or TLS.
///
/// `pub(crate)` because [`super::websocket`] borrows this whole transport: a
/// WebSocket *is* an HTTP/1.1 request whose 101 response hands the socket over
/// to a different framing. Reusing the connect + TLS + cancellation path here
/// is what keeps `wss:` on exactly the trust story `https:` already has
/// (platform TLS, no bundled roots, no second crypto stack — see the module
/// header) instead of introducing a second one.
pub(crate) enum Stream {
    Plain(TcpStream),
    Tls(Box<native_tls::TlsStream<TcpStream>>),
}

impl Stream {
    /// The TCP socket underneath, for timeout changes and `shutdown`. Reading
    /// or writing it directly on a TLS connection would corrupt the session;
    /// callers use it for socket *options* only.
    pub(crate) fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            Stream::Tls(s) => s.get_ref(),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

/// A socket that checks the abort flag between blocking attempts.
///
/// Everything downstream (`read_head`, the body readers) is written against
/// `impl Read`, so wrapping here makes every read in this module cancellable
/// without any of them knowing about it.
pub(crate) struct Cancellable<'a> {
    inner: Stream,
    cancel: &'a Cancel,
    /// Total time spent with no bytes moving. Reset by progress.
    stalled: Duration,
    budget: Duration,
}

impl<'a> Cancellable<'a> {
    pub(crate) fn new(inner: Stream, cancel: &'a Cancel, budget: Duration) -> Self {
        Cancellable { inner, cancel, stalled: Duration::ZERO, budget }
    }

    /// Hands the socket back after the head has been read — the WebSocket
    /// upgrade continues on it with a different framing.
    pub(crate) fn into_inner(self) -> Stream {
        self.inner
    }

    fn aborted() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::Interrupted, "the request was aborted")
    }

    fn stalled_out() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::TimedOut, "the connection stalled")
    }

    /// Retries `op` across `POLL` slices, giving up on abort or on the stall
    /// budget. `Ok(0)` from a read means EOF and is progress, not a stall.
    fn poll_io(
        &mut self,
        mut op: impl FnMut(&mut Stream) -> std::io::Result<usize>,
    ) -> std::io::Result<usize> {
        loop {
            if self.cancel.is_aborted() {
                return Err(Self::aborted());
            }
            match op(&mut self.inner) {
                Ok(n) => {
                    self.stalled = Duration::ZERO;
                    return Ok(n);
                }
                Err(e)
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                {
                    if self.cancel.is_aborted() {
                        return Err(Self::aborted());
                    }
                    self.stalled += POLL;
                    if self.stalled >= self.budget {
                        return Err(Self::stalled_out());
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl Read for Cancellable<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // The closure cannot borrow `buf` and `self` at once through `poll_io`,
        // so the raw parts are rebuilt inside. Safe: `buf` outlives the call and
        // nothing else aliases it.
        let ptr = buf.as_mut_ptr();
        let len = buf.len();
        self.poll_io(move |s| s.read(unsafe { core::slice::from_raw_parts_mut(ptr, len) }))
    }
}

impl Write for Cancellable<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let ptr = buf.as_ptr();
        let len = buf.len();
        self.poll_io(move |s| s.write(unsafe { core::slice::from_raw_parts(ptr, len) }))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

pub(crate) fn connect(u: &url::Url, connect_timeout: Duration, cancel: &Cancel) -> Result<Stream, HttpError> {
    let host = u.host_str().ok_or_else(|| HttpError::refused(format!("URL has no host: {u}")))?;
    let port = u
        .port_or_known_default()
        .ok_or_else(|| HttpError::refused(format!("URL has no port and no default: {u}")))?;
    let secure = u.scheme() == "https";

    cancel.check()?;
    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|e| HttpError::network(format!("could not resolve {host}: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(HttpError::network(format!("could not resolve {host}")));
    }

    let mut last: Option<std::io::Error> = None;
    let mut tcp: Option<TcpStream> = None;
    for a in addrs {
        cancel.check()?;
        match TcpStream::connect_timeout(&a, connect_timeout) {
            Ok(s) => {
                tcp = Some(s);
                break;
            }
            Err(e) => last = Some(e),
        }
    }
    let tcp = tcp.ok_or_else(|| {
        HttpError::network(match last {
            Some(e) => format!("could not connect to {host}:{port}: {e}"),
            None => format!("could not connect to {host}:{port}"),
        })
    })?;
    let _ = tcp.set_nodelay(true);
    // POLL, not io_timeout: the retry loop in `Cancellable` owns the real
    // budget, and a short socket timeout is what makes abort prompt.
    let _ = tcp.set_read_timeout(Some(POLL));
    let _ = tcp.set_write_timeout(Some(POLL));

    // Armed BEFORE the TLS handshake so an abort during it lands too.
    cancel.arm(&tcp)?;

    if !secure {
        return Ok(Stream::Plain(tcp));
    }
    let connector = native_tls::TlsConnector::new()
        .map_err(|e| HttpError::network(format!("TLS setup failed: {e}")))?;

    // The handshake reads and writes on a socket with a POLL timeout, so it
    // comes back as WouldBlock rather than completing in one call. Looping here
    // is what makes a handshake against a slow or unreachable peer abortable.
    let mut stalled = Duration::ZERO;
    let mut attempt = connector.connect(host, tcp);
    loop {
        match attempt {
            Ok(s) => return Ok(Stream::Tls(Box::new(s))),
            Err(native_tls::HandshakeError::WouldBlock(mid)) => {
                if cancel.is_aborted() {
                    return Err(HttpError::new(ErrorKind::Aborted, "the request was aborted"));
                }
                stalled += POLL;
                if stalled >= connect_timeout {
                    return Err(HttpError::network(format!("TLS handshake with {host} timed out")));
                }
                attempt = mid.handshake();
            }
            Err(e) => {
                if cancel.is_aborted() {
                    return Err(HttpError::new(ErrorKind::Aborted, "the request was aborted"));
                }
                return Err(HttpError::network(format!("TLS handshake with {host} failed: {e}")));
            }
        }
    }
}

/// The request line + headers exactly as they go on the wire. Split out so the
/// framing is testable without a socket.
pub fn serialize_request(
    u: &url::Url,
    method: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
) -> Result<Vec<u8>, HttpError> {
    let mut target = u.path().to_string();
    if target.is_empty() {
        target.push('/');
    }
    if let Some(q) = u.query() {
        target.push('?');
        target.push_str(q);
    }

    let mut out = Vec::with_capacity(256 + body.map_or(0, |b| b.len()));
    out.extend_from_slice(format!("{method} {target} HTTP/1.1\r\n").as_bytes());

    let host = u.host_str().unwrap_or("");
    let host_header = match (u.port(), u.scheme()) {
        (Some(p), _) => format!("{host}:{p}"),
        (None, _) => host.to_string(),
    };

    let mut seen: Vec<String> = Vec::new();
    for (k, v) in headers {
        validate_header(k, v)?;
        let lower = k.to_ascii_lowercase();
        // Framing is ours; a JS-supplied one would desynchronize the parser.
        if matches!(lower.as_str(), "host" | "content-length" | "transfer-encoding" | "connection") {
            continue;
        }
        seen.push(lower);
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    let has = |n: &str| seen.iter().any(|s| s == n);

    out.extend_from_slice(format!("Host: {host_header}\r\n").as_bytes());
    if !has("user-agent") {
        out.extend_from_slice(b"User-Agent: RedPew\r\n");
    }
    if !has("accept") {
        out.extend_from_slice(b"Accept: */*\r\n");
    }
    if !has("accept-encoding") {
        out.extend_from_slice(b"Accept-Encoding: gzip, deflate\r\n");
    }
    // No pooling — see the module header.
    out.extend_from_slice(b"Connection: close\r\n");
    // Spec: a body-bearing method without a body still declares zero length.
    let needs_length = body.is_some() || matches!(method, "POST" | "PUT" | "PATCH");
    if needs_length {
        out.extend_from_slice(format!("Content-Length: {}\r\n", body.map_or(0, |b| b.len())).as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    if let Some(b) = body {
        out.extend_from_slice(b);
    }
    Ok(out)
}

pub(crate) fn read_head<R: Read>(r: &mut R, cancel: &Cancel) -> Result<(Head, Vec<u8>), HttpError> {
    let mut acc: Vec<u8> = Vec::with_capacity(2048);
    let mut buf = [0u8; 8 * 1024];
    loop {
        // Scan only the tail that could contain a newly completed terminator.
        let scan_from = acc.len().saturating_sub(3);
        let got = r.read(&mut buf).map_err(|e| cancel.map_io(e, "response read"))?;
        if got == 0 {
            if acc.is_empty() {
                return Err(HttpError::network("the server closed the connection without a response"));
            }
            return Err(HttpError::protocol("the server closed the connection inside the response head"));
        }
        acc.extend_from_slice(&buf[..got]);
        if let Some(i) = acc[scan_from..].windows(4).position(|w| w == b"\r\n\r\n") {
            let end = scan_from + i;
            let head = parse_head(&acc[..end])?;
            let rest = acc[end + 4..].to_vec();
            return Ok((head, rest));
        }
        if acc.len() > MAX_HEAD {
            return Err(HttpError::protocol(format!("response head exceeds {MAX_HEAD} bytes")));
        }
    }
}

/// One hop: connect, write, read. No redirect handling.
fn round_trip(
    u: &url::Url,
    method: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    req: &Request,
    cancel: &Cancel,
) -> Result<(Head, Vec<u8>), HttpError> {
    let inner = connect(u, req.connect_timeout, cancel)?;
    let mut stream = Cancellable { inner, cancel, stalled: Duration::ZERO, budget: req.io_timeout };

    let wire = serialize_request(u, method, headers, body)?;
    stream.write_all(&wire).map_err(|e| cancel.map_io(e, "request write"))?;
    stream.flush().map_err(|e| cancel.map_io(e, "request write"))?;

    let (head, rest) = read_head(&mut stream, cancel)?;

    // 1xx informational responses are not the answer — read past them.
    let (head, rest) = if (100..200).contains(&head.status) {
        read_head_after_informational(&mut stream, rest, cancel)?
    } else {
        (head, rest)
    };

    let framing = framing_of(method, head.status, &head.headers)?;
    let raw = match framing {
        Framing::None => Vec::new(),
        Framing::Length(n) => read_exact_body(&mut stream, n, &rest).map_err(|e| reclassify(e, cancel))?,
        Framing::Chunked => read_chunked(&mut stream, &rest).map_err(|e| reclassify(e, cancel))?,
        Framing::UntilClose => read_until_close(&mut stream, &rest).map_err(|e| reclassify(e, cancel))?,
    };
    let decoded = decode_content_encoding(header(&head.headers, "content-encoding"), raw)?;
    Ok((head, decoded))
}

/// After a 1xx, the real head follows immediately.
fn read_head_after_informational<R: Read>(
    r: &mut R,
    rest: Vec<u8>,
    cancel: &Cancel,
) -> Result<(Head, Vec<u8>), HttpError> {
    let mut acc = rest;
    let mut buf = [0u8; 8 * 1024];
    loop {
        if let Some(i) = acc.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = parse_head(&acc[..i])?;
            let tail = acc[i + 4..].to_vec();
            if (100..200).contains(&head.status) {
                acc = tail;
                continue;
            }
            return Ok((head, tail));
        }
        let got = r.read(&mut buf).map_err(|e| cancel.map_io(e, "response read"))?;
        if got == 0 {
            return Err(HttpError::protocol("the server closed the connection after an informational response"));
        }
        acc.extend_from_slice(&buf[..got]);
        if acc.len() > MAX_HEAD {
            return Err(HttpError::protocol(format!("response head exceeds {MAX_HEAD} bytes")));
        }
    }
}

/// A body read that failed because *we* aborted is an abort, not a protocol bug.
fn reclassify(e: HttpError, cancel: &Cancel) -> HttpError {
    if cancel.is_aborted() {
        return HttpError::new(ErrorKind::Aborted, "the request was aborted");
    }
    e
}

/// Performs `req`, following redirects per its policy.
///
/// Blocking. Call from a worker thread; the result goes back to JS through the
/// pump (see `fetch.rs`).
pub fn send(req: &Request, cancel: &Cancel) -> Result<Response, HttpError> {
    let mut current = req.url.clone();
    let mut method = req.method.to_ascii_uppercase();
    let mut body = req.body.clone();
    let mut headers = req.headers.clone();
    let mut redirected = false;

    for _ in 0..=MAX_REDIRECTS {
        cancel.check()?;
        let u = url::Url::parse(&current).map_err(|e| HttpError::refused(format!("invalid URL {current:?}: {e}")))?;
        if u.scheme() != "http" && u.scheme() != "https" {
            return Err(HttpError::refused(format!("scheme {:?} is not supported by fetch", u.scheme())));
        }

        // The jar, per hop: this hop's cookies on the way out, this hop's
        // Set-Cookie on the way back — before the redirect is followed, so the
        // next hop already sees what this one set.
        let cookies = req.credentials.applies(&u);
        let hop = if cookies { super::cookies::with_cookies(&u, &headers) } else { None };
        let (head, bytes) =
            round_trip(&u, &method, hop.as_deref().unwrap_or(&headers), body.as_deref(), req, cancel)?;
        cancel.disarm();
        if cookies {
            super::cookies::store_response(&u, &head.headers);
        }

        if is_redirect(head.status) {
            if let Some(loc) = header(&head.headers, "location") {
                if req.redirect == Redirect::Error {
                    return Err(HttpError::network(format!(
                        "redirect to {loc:?} refused: the request set redirect: \"error\""
                    )));
                }
                let next = resolve_location(&current, loc)?;
                let (next_method, keep_body) = redirect_method(head.status, &method);
                if !same_origin(&current, &next) {
                    strip_sensitive(&mut headers);
                }
                if !keep_body {
                    body = None;
                    headers.retain(|(k, _)| !k.eq_ignore_ascii_case("content-type"));
                }
                method = next_method;
                current = next;
                redirected = true;
                continue;
            }
        }

        return Ok(Response {
            status: head.status,
            status_text: head.reason,
            headers: head.headers,
            url: current,
            redirected,
            body: bytes,
        });
    }
    Err(HttpError::network(format!("too many redirects (more than {MAX_REDIRECTS})")))
}

// ---------------------------------------------------------------------------
// Tests — the wire protocol, no JS context and no network
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_normal_head_parses() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2").unwrap();
        assert_eq!(head.status, 200);
        assert_eq!(head.reason, "OK");
        assert_eq!(header(&head.headers, "CONTENT-TYPE"), Some("application/json"));
    }

    #[test]
    fn a_reasonless_status_line_still_parses() {
        // Nginx and friends omit the reason phrase; a strict split on three
        // parts would turn a perfectly good 204 into a protocol error.
        let head = parse_head(b"HTTP/1.1 204 \r\n").unwrap();
        assert_eq!(head.status, 204);
        assert_eq!(head.reason, "");
    }

    #[test]
    fn a_non_http_peer_is_a_protocol_error_not_a_default_status() {
        // The whole point: no synthetic status may be invented for a peer we
        // did not understand.
        for junk in [
            &b"<html>hello</html>"[..],
            &b"\x16\x03\x01\x02\x00\x01"[..], // a TLS ClientHello on a plaintext port
            &b"HTTP/1.1 2O0 OK\r\n"[..],      // letter O
            &b"HTTP/9.9 200 OK\r\n"[..],
        ] {
            assert!(parse_head(junk).is_err(), "accepted junk as a response head: {junk:?}");
        }
    }

    #[test]
    fn a_malformed_header_line_is_refused() {
        assert!(parse_head(b"HTTP/1.1 200 OK\r\nthis-is-not-a-header\r\n").is_err());
        assert!(parse_head(b"HTTP/1.1 200 OK\r\nbad header: x\r\n").is_err());
    }

    #[test]
    fn folded_headers_join_onto_the_previous_value() {
        let head = parse_head(b"HTTP/1.1 200 OK\r\nX-Long: a\r\n  b\r\n").unwrap();
        assert_eq!(header(&head.headers, "x-long"), Some("a b"));
    }

    #[test]
    fn framing_follows_the_spec_order() {
        let chunked = vec![("transfer-encoding".into(), "chunked".into()), ("content-length".into(), "5".into())];
        // Transfer-Encoding wins over Content-Length (RFC 9112 §6.3).
        assert_eq!(framing_of("GET", 200, &chunked).unwrap(), Framing::Chunked);

        let len = vec![("content-length".into(), "5".into())];
        assert_eq!(framing_of("GET", 200, &len).unwrap(), Framing::Length(5));
        // A body-less status has no body even when it declares a length.
        assert_eq!(framing_of("GET", 204, &len).unwrap(), Framing::None);
        assert_eq!(framing_of("GET", 304, &len).unwrap(), Framing::None);
        assert_eq!(framing_of("HEAD", 200, &len).unwrap(), Framing::None);
        assert_eq!(framing_of("GET", 200, &[]).unwrap(), Framing::UntilClose);
    }

    #[test]
    fn conflicting_content_lengths_are_refused() {
        let bad = vec![("content-length".into(), "5, 9".into())];
        assert!(framing_of("GET", 200, &bad).is_err());
        let same = vec![("content-length".into(), "5, 5".into())];
        assert_eq!(framing_of("GET", 200, &same).unwrap(), Framing::Length(5));
    }

    #[test]
    fn an_unknown_transfer_encoding_is_refused() {
        let bad = vec![("transfer-encoding".into(), "gzip".into())];
        assert!(framing_of("GET", 200, &bad).is_err());
    }

    #[test]
    fn chunked_decodes_across_reads() {
        // The RFC's own example: the payload contains CRLFs of its own, so a
        // decoder that scans for line breaks instead of honouring chunk sizes
        // gets this wrong.
        let wire = b"4\r\nWiki\r\n5\r\npedia\r\ne\r\n in\r\n\r\nchunks.\r\n0\r\n\r\n";
        let mut r = &wire[..];
        assert_eq!(read_chunked(&mut r, &[]).unwrap(), b"Wikipedia in\r\n\r\nchunks.");
    }

    #[test]
    fn chunked_honours_the_prefix_already_read_off_the_socket() {
        // The head read almost always over-reads into the body; losing that
        // prefix silently truncates every chunked response.
        let all = b"3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n";
        let (prefix, rest) = all.split_at(7);
        let mut r = rest;
        assert_eq!(read_chunked(&mut r, prefix).unwrap(), b"abcdef");
    }

    #[test]
    fn a_truncated_chunked_body_is_an_error_not_a_short_body() {
        // No zero-chunk: the peer died mid-transfer. Returning "abc" here would
        // hand JS a silently incomplete document with a 200 status.
        let mut r = &b"3\r\nabc\r\n3\r\nde"[..];
        let e = read_chunked(&mut r, &[]).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Protocol, "{}", e.message);
    }

    #[test]
    fn chunk_extensions_are_tolerated() {
        let mut r = &b"3;name=value\r\nabc\r\n0\r\n\r\n"[..];
        assert_eq!(read_chunked(&mut r, &[]).unwrap(), b"abc");
    }

    #[test]
    fn a_chunked_trailer_is_consumed() {
        let mut r = &b"3\r\nabc\r\n0\r\nX-Checksum: 1\r\n\r\n"[..];
        assert_eq!(read_chunked(&mut r, &[]).unwrap(), b"abc");
    }

    #[test]
    fn a_short_content_length_body_is_an_error() {
        let mut r = &b"abc"[..];
        let e = read_exact_body(&mut r, 10, &[]).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Protocol, "{}", e.message);
    }

    #[test]
    fn a_content_length_body_can_come_entirely_from_the_prefix() {
        let mut r = &b""[..];
        assert_eq!(read_exact_body(&mut r, 3, b"abcdef").unwrap(), b"abc");
    }

    #[test]
    fn gzip_round_trips_through_the_decoder() {
        use flate2::write::GzEncoder;
        let mut e = GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(b"hello over the wire").unwrap();
        let gz = e.finish().unwrap();
        assert_eq!(decode_content_encoding(Some("gzip"), gz).unwrap(), b"hello over the wire");
    }

    #[test]
    fn an_unadvertised_content_encoding_is_refused_by_name() {
        let e = decode_content_encoding(Some("br"), vec![1, 2, 3]).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Refused);
        assert!(e.message.contains("br"), "{}", e.message);
    }

    #[test]
    fn identity_and_absent_encodings_pass_through() {
        assert_eq!(decode_content_encoding(None, vec![1, 2]).unwrap(), vec![1, 2]);
        assert_eq!(decode_content_encoding(Some("identity"), vec![1, 2]).unwrap(), vec![1, 2]);
    }

    #[test]
    fn redirect_method_rewrites_match_the_fetch_spec() {
        assert_eq!(redirect_method(303, "POST"), ("GET".to_string(), false));
        assert_eq!(redirect_method(303, "HEAD"), ("HEAD".to_string(), true));
        assert_eq!(redirect_method(302, "POST"), ("GET".to_string(), false));
        assert_eq!(redirect_method(301, "POST"), ("GET".to_string(), false));
        assert_eq!(redirect_method(307, "POST"), ("POST".to_string(), true));
        assert_eq!(redirect_method(308, "PUT"), ("PUT".to_string(), true));
        assert_eq!(redirect_method(302, "GET"), ("GET".to_string(), true));
    }

    #[test]
    fn location_resolves_relative_absolute_and_scheme_relative() {
        let base = "https://api.example.com/v1/thing?x=1";
        assert_eq!(resolve_location(base, "/v2/other").unwrap(), "https://api.example.com/v2/other");
        assert_eq!(resolve_location(base, "other").unwrap(), "https://api.example.com/v1/other");
        assert_eq!(resolve_location(base, "https://cdn.example.net/a").unwrap(), "https://cdn.example.net/a");
        assert_eq!(resolve_location(base, "//cdn.example.net/a").unwrap(), "https://cdn.example.net/a");
    }

    #[test]
    fn origin_comparison_accounts_for_default_ports() {
        assert!(same_origin("https://a.example/x", "https://a.example:443/y"));
        assert!(!same_origin("https://a.example/x", "http://a.example/x"));
        assert!(!same_origin("https://a.example/x", "https://b.example/x"));
    }

    #[test]
    fn credentials_are_dropped_on_a_cross_origin_redirect() {
        let mut h = vec![
            ("authorization".to_string(), "Bearer secret".to_string()),
            ("cookie".to_string(), "sid=1".to_string()),
            ("accept".to_string(), "*/*".to_string()),
        ];
        strip_sensitive(&mut h);
        assert_eq!(h, vec![("accept".to_string(), "*/*".to_string())]);
    }

    #[test]
    fn a_header_cannot_inject_a_request_line() {
        let inject = vec![("X-Evil".to_string(), "a\r\nX-Injected: 1".to_string())];
        let u = url::Url::parse("http://example.com/").unwrap();
        let e = serialize_request(&u, "GET", &inject, None).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Refused, "{}", e.message);

        let bad_name = vec![("X Evil".to_string(), "1".to_string())];
        assert!(serialize_request(&u, "GET", &bad_name, None).is_err());
    }

    #[test]
    fn the_request_line_carries_path_and_query_and_a_host_header() {
        let u = url::Url::parse("http://example.com:8080/a/b?q=1&r=2").unwrap();
        let wire = serialize_request(&u, "GET", &[], None).unwrap();
        let text = String::from_utf8(wire).unwrap();
        assert!(text.starts_with("GET /a/b?q=1&r=2 HTTP/1.1\r\n"), "{text}");
        assert!(text.contains("Host: example.com:8080\r\n"), "{text}");
        assert!(text.contains("Connection: close\r\n"), "{text}");
        assert!(text.ends_with("\r\n\r\n"), "{text}");
        // A GET carries no Content-Length.
        assert!(!text.contains("Content-Length"), "{text}");
    }

    #[test]
    fn a_body_bearing_method_always_declares_its_length() {
        let u = url::Url::parse("https://example.com/submit").unwrap();
        let wire = serialize_request(&u, "POST", &[], Some(b"{\"a\":1}")).unwrap();
        let text = String::from_utf8(wire).unwrap();
        assert!(text.contains("Content-Length: 7\r\n"), "{text}");
        assert!(text.ends_with("\r\n\r\n{\"a\":1}"), "{text}");

        // ...including an empty one, or the server waits for a body forever.
        let empty = String::from_utf8(serialize_request(&u, "POST", &[], None).unwrap()).unwrap();
        assert!(empty.contains("Content-Length: 0\r\n"), "{empty}");
    }

    #[test]
    fn caller_framing_headers_are_ignored_rather_than_duplicated() {
        let u = url::Url::parse("http://example.com/").unwrap();
        let h = vec![
            ("Content-Length".to_string(), "999".to_string()),
            ("Transfer-Encoding".to_string(), "chunked".to_string()),
            ("Host".to_string(), "evil.example".to_string()),
        ];
        let text = String::from_utf8(serialize_request(&u, "POST", &h, Some(b"ab")).unwrap()).unwrap();
        assert_eq!(text.matches("Content-Length").count(), 1, "{text}");
        assert!(text.contains("Content-Length: 2\r\n"), "{text}");
        assert!(!text.contains("chunked"), "{text}");
        assert!(text.contains("Host: example.com\r\n"), "{text}");
        assert!(!text.contains("evil.example"), "{text}");
    }

    fn req(url: &str) -> Request {
        Request {
            method: "GET".into(),
            url: url.into(),
            headers: vec![],
            body: None,
            redirect: Redirect::Follow,
            credentials: crate::webapi::cookies::Credentials::Omit,
            connect_timeout: Duration::from_secs(5),
            io_timeout: Duration::from_secs(10),
        }
    }

    // -- cookies, per hop (webapi::cookies holds the rules; this is the wiring) --

    fn cookie_line(request: &[u8]) -> Option<String> {
        String::from_utf8_lossy(request)
            .lines()
            .find_map(|l| l.strip_prefix("cookie: ").or_else(|| l.strip_prefix("Cookie: ")).map(str::to_string))
    }

    #[test]
    fn include_stores_a_redirects_set_cookie_and_sends_it_on_the_next_hop() {
        use crate::webapi::cookies;
        let _g = cookies::test_lock();
        cookies::reset_for_tests();
        let (port, seen) = serve(vec![
            b"HTTP/1.1 302 Found\r\nSet-Cookie: hop=1; Path=/\r\nLocation: /final\r\nContent-Length: 0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nSet-Cookie: done=2; Path=/\r\nContent-Length: 2\r\n\r\nok".to_vec(),
        ]);
        let mut r = req(&format!("http://127.0.0.1:{port}/start"));
        r.credentials = cookies::Credentials::Include;
        let resp = send(&r, &Cancel::default()).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(cookie_line(&seen.recv().unwrap()), None, "nothing in the jar for the first hop");
        assert_eq!(cookie_line(&seen.recv().unwrap()).as_deref(), Some("hop=1"), "the 302's cookie rode the next hop");
        let jar = cookies::header_for(&url::Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap(), cookies::Api::Http);
        assert_eq!(jar.as_deref(), Some("hop=1; done=2"));
        cookies::reset_for_tests();
    }

    #[test]
    fn a_hop_to_another_host_carries_only_that_hosts_cookies() {
        use crate::webapi::cookies;
        let _g = cookies::test_lock();
        cookies::reset_for_tests();
        let (b, seen_b) = serve(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec()]);
        let (a, seen_a) = serve(vec![format!(
            "HTTP/1.1 302 Found\r\nLocation: http://localhost:{b}/there\r\nContent-Length: 0\r\n\r\n"
        )
        .into_bytes()]);
        cookies::set_from_header(&url::Url::parse("http://127.0.0.1/").unwrap(), "only-ip=1", cookies::Api::Http).unwrap();
        cookies::set_from_header(&url::Url::parse("http://localhost/").unwrap(), "only-lh=1", cookies::Api::Http).unwrap();
        let mut r = req(&format!("http://127.0.0.1:{a}/start"));
        r.credentials = cookies::Credentials::Include;
        r.headers.push(("cookie".into(), "script=1".into()));
        send(&r, &Cancel::default()).unwrap();
        assert_eq!(cookie_line(&seen_a.recv().unwrap()).as_deref(), Some("script=1; only-ip=1"));
        // Cross-origin: the script's header is stripped, the jar re-decides.
        assert_eq!(cookie_line(&seen_b.recv().unwrap()).as_deref(), Some("only-lh=1"));
        cookies::reset_for_tests();
    }

    #[test]
    fn omit_and_same_origin_neither_send_nor_store() {
        use crate::webapi::cookies;
        let _g = cookies::test_lock();
        cookies::reset_for_tests();
        cookies::set_from_header(&url::Url::parse("http://127.0.0.1/").unwrap(), "pre=1", cookies::Api::Http).unwrap();
        for mode in [cookies::Credentials::Omit, cookies::Credentials::SameOrigin] {
            let (port, seen) = serve(vec![b"HTTP/1.1 200 OK\r\nSet-Cookie: nope=1\r\nContent-Length: 0\r\n\r\n".to_vec()]);
            let mut r = req(&format!("http://127.0.0.1:{port}/"));
            r.credentials = mode;
            send(&r, &Cancel::default()).unwrap();
            assert_eq!(cookie_line(&seen.recv().unwrap()), None, "{mode:?} sent the jar");
        }
        let jar = cookies::header_for(&url::Url::parse("http://127.0.0.1/").unwrap(), cookies::Api::Http);
        assert_eq!(jar.as_deref(), Some("pre=1"), "neither mode stored the response's cookie");
        cookies::reset_for_tests();
    }

    #[test]
    fn a_non_http_scheme_is_refused_without_touching_the_network() {
        let e = send(&req("ftp://example.com/file"), &Cancel::default()).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Refused, "{}", e.message);
        assert!(e.message.contains("ftp"), "{}", e.message);
    }

    #[test]
    fn a_pre_aborted_request_never_opens_a_socket() {
        let cancel = Cancel::default();
        cancel.abort();
        // 192.0.2.0/24 is TEST-NET-1: unroutable by RFC. If we ever dialed it,
        // this test would sit on the connect timeout instead of returning.
        let mut r = req("http://192.0.2.1:81/never");
        r.connect_timeout = Duration::from_secs(30);
        let started = std::time::Instant::now();
        let e = send(&r, &cancel).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Aborted, "{}", e.message);
        assert!(started.elapsed() < Duration::from_secs(2), "an aborted request still dialed out");
    }

    #[test]
    fn a_refused_connection_is_a_network_error_not_an_empty_response() {
        // Port 1 on loopback: nothing listens, connect fails fast.
        let e = send(&req("http://127.0.0.1:1/nope"), &Cancel::default()).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Network, "{}", e.message);
    }

    // -- against a real socket -------------------------------------------------
    //
    // A loopback listener speaking canned HTTP/1.1. Real connect, real write,
    // real read, real framing — everything except TLS, which needs a
    // certificate authority and is verified against live servers instead (see
    // the report accompanying this change).

    /// Serves `replies` in order, one per accepted connection. Returns the port
    /// and a channel yielding each request as the server saw it.
    fn serve(replies: Vec<Vec<u8>>) -> (u16, std::sync::mpsc::Receiver<Vec<u8>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for reply in replies {
                let Ok((mut sock, _)) = listener.accept() else { return };
                let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
                // Read the request head (the client always sends Content-Length,
                // so the body length is knowable, but the tests only assert on
                // what arrives before the body ends).
                let mut got = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    match sock.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            got.extend_from_slice(&buf[..n]);
                            if let Some(i) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                                let len = String::from_utf8_lossy(&got[..i])
                                    .lines()
                                    .find_map(|l| {
                                        l.to_ascii_lowercase()
                                            .strip_prefix("content-length:")
                                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                    })
                                    .unwrap_or(0);
                                if got.len() >= i + 4 + len {
                                    break;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                let _ = tx.send(got);
                let _ = sock.write_all(&reply);
                let _ = sock.flush();
                let _ = sock.shutdown(Shutdown::Write);
            }
        });
        (port, rx)
    }

    #[test]
    fn a_real_request_over_a_real_socket_succeeds() {
        let (port, seen) = serve(vec![
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 17\r\n\r\n{\"instance\":\"ok\"}".to_vec(),
        ]);
        let r = send(&req(&format!("http://127.0.0.1:{port}/resolve")), &Cancel::default()).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.status_text, "OK");
        assert_eq!(header(&r.headers, "content-type"), Some("application/json"));
        assert_eq!(String::from_utf8(r.body).unwrap(), "{\"instance\":\"ok\"}");
        assert!(!r.redirected);

        let request = String::from_utf8(seen.recv().unwrap()).unwrap();
        assert!(request.starts_with("GET /resolve HTTP/1.1\r\n"), "{request}");
        assert!(request.contains(&format!("Host: 127.0.0.1:{port}\r\n")), "{request}");
    }

    #[test]
    fn a_post_body_and_headers_actually_reach_the_server() {
        // The regression that matters most: a request that "succeeds" but whose
        // body or auth header never left the process.
        let (port, seen) = serve(vec![b"HTTP/1.1 204 No Content\r\n\r\n".to_vec()]);
        let mut r = req(&format!("http://127.0.0.1:{port}/api/2/envelope/"));
        r.method = "POST".into();
        r.headers = vec![
            ("Content-Type".into(), "application/x-sentry-envelope".into()),
            ("Authorization".into(), "Bearer s3cret".into()),
        ];
        r.body = Some(b"{\"event_id\":\"abc\"}".to_vec());

        let resp = send(&r, &Cancel::default()).unwrap();
        assert_eq!(resp.status, 204);
        assert!(resp.body.is_empty(), "204 must have no body");

        let request = String::from_utf8(seen.recv().unwrap()).unwrap();
        assert!(request.starts_with("POST /api/2/envelope/ HTTP/1.1\r\n"), "{request}");
        assert!(request.contains("Authorization: Bearer s3cret\r\n"), "{request}");
        assert!(request.contains("Content-Type: application/x-sentry-envelope\r\n"), "{request}");
        assert!(request.contains("Content-Length: 18\r\n"), "{request}");
        assert!(request.ends_with("{\"event_id\":\"abc\"}"), "{request}");
    }

    #[test]
    fn a_chunked_response_arrives_whole() {
        let (port, _seen) = serve(vec![
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n".to_vec(),
        ]);
        let r = send(&req(&format!("http://127.0.0.1:{port}/stream")), &Cancel::default()).unwrap();
        assert_eq!(String::from_utf8(r.body).unwrap(), "hello world");
    }

    #[test]
    fn an_error_status_is_a_response_not_a_rejection() {
        // 404 is a successful round trip: the transport must report it as a
        // response and let the caller read `ok`/`status`.
        let (port, _seen) = serve(vec![
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot here!".to_vec(),
        ]);
        let r = send(&req(&format!("http://127.0.0.1:{port}/missing")), &Cancel::default()).unwrap();
        assert_eq!(r.status, 404);
        assert_eq!(String::from_utf8(r.body).unwrap(), "not here!");
    }

    #[test]
    fn redirects_are_followed_and_reported() {
        let (port, _seen) = serve(vec![
            b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nthere".to_vec(),
        ]);
        let r = send(&req(&format!("http://127.0.0.1:{port}/start")), &Cancel::default()).unwrap();
        assert_eq!(r.status, 200);
        assert!(r.redirected);
        assert!(r.url.ends_with("/final"), "final URL not reported: {}", r.url);
        assert_eq!(String::from_utf8(r.body).unwrap(), "there");
    }

    #[test]
    fn redirect_error_mode_fails_instead_of_following() {
        let (port, _seen) = serve(vec![
            b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\n\r\n".to_vec(),
        ]);
        let mut r = req(&format!("http://127.0.0.1:{port}/start"));
        r.redirect = Redirect::Error;
        let e = send(&r, &Cancel::default()).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Network, "{}", e.message);
    }

    #[test]
    fn a_peer_that_hangs_up_mid_body_fails_rather_than_truncating() {
        // Declares 100 bytes, sends 4, closes. The old status-0 behaviour is the
        // shape being guarded against: a caller must not receive "helo" as a
        // complete 200 response.
        let (port, _seen) = serve(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nhelo".to_vec()]);
        let e = send(&req(&format!("http://127.0.0.1:{port}/short")), &Cancel::default()).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Protocol, "{}", e.message);
    }

    #[test]
    fn a_peer_that_is_not_speaking_http_fails_loudly() {
        let (port, _seen) = serve(vec![b"I am not an HTTP server\r\n\r\n".to_vec()]);
        let e = send(&req(&format!("http://127.0.0.1:{port}/junk")), &Cancel::default()).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Protocol, "{}", e.message);
    }

    // -- against the real internet ---------------------------------------------
    //
    // Ignored by default: the rest of this suite is hermetic and must stay
    // runnable offline. These are the TLS half, which a loopback listener
    // cannot cover without a certificate authority. Run with
    //   cargo test --lib -- --ignored webapi::http
    // and see the report accompanying this change for the results.

    #[test]
    #[ignore = "requires internet access"]
    fn https_against_a_real_server_works() {
        let r = send(&req("https://example.com/"), &Cancel::default()).unwrap();
        assert_eq!(r.status, 200, "{}", r.status_text);
        let text = String::from_utf8_lossy(&r.body).to_ascii_lowercase();
        assert!(text.contains("example domain"), "unexpected body: {}", &text[..text.len().min(200)]);
    }

    #[test]
    #[ignore = "requires internet access"]
    fn https_json_and_gzip_come_back_decoded() {
        // A JSON API that gzips when asked — exercises Accept-Encoding, the
        // gzip decoder, and chunked framing in one trip.
        let r = send(&req("https://api.github.com/meta"), &Cancel::default()).unwrap();
        assert_eq!(r.status, 200, "{}", r.status_text);
        let text = String::from_utf8(r.body).expect("gzip left non-UTF-8 bytes behind");
        assert!(text.starts_with('{'), "body was not decoded JSON: {}", &text[..text.len().min(120)]);
    }

    #[test]
    #[ignore = "requires internet access"]
    fn a_real_redirect_chain_is_followed() {
        let r = send(&req("http://github.com/"), &Cancel::default()).unwrap();
        assert_eq!(r.status, 200);
        assert!(r.redirected);
        assert!(r.url.starts_with("https://"), "did not upgrade to https: {}", r.url);
    }

    #[test]
    #[ignore = "requires internet access"]
    fn an_invalid_certificate_is_rejected() {
        // The single most important property of the TLS choice. A client that
        // accepts these is worse than no client: it turns every request into a
        // silent wrong answer that an attacker chooses.
        for host in ["expired.badssl.com", "self-signed.badssl.com", "wrong.host.badssl.com", "untrusted-root.badssl.com"] {
            let e = send(&req(&format!("https://{host}/")), &Cancel::default())
                .err()
                .unwrap_or_else(|| panic!("{host} was ACCEPTED — certificate verification is not happening"));
            assert_eq!(e.kind, ErrorKind::Network, "{host}: {}", e.message);
            // Not just "it failed": the failure has to be about the
            // CERTIFICATE. An unreachable host would otherwise let this test
            // pass while verification was switched off.
            assert!(
                e.message.to_ascii_lowercase().contains("certificate"),
                "{host} failed, but not on certificate validation: {}",
                e.message
            );
        }
    }

    #[test]
    #[ignore = "requires internet access"]
    fn a_valid_certificate_chain_is_accepted() {
        // The other half: verification that rejects everything is equally broken.
        let r = send(&req("https://sha256.badssl.com/"), &Cancel::default()).unwrap();
        assert_eq!(r.status, 200);
    }


    #[test]
    fn abort_mid_flight_shuts_the_socket_down_now() {
        // A listener that accepts and then says nothing. Without a real socket
        // shutdown this sits until `io_timeout`; the assertion is that abort
        // returns in well under that.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let held = listener.accept();
            std::thread::sleep(Duration::from_secs(20));
            drop(held);
        });

        let cancel = Cancel::new();
        let waker = std::sync::Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            waker.abort();
        });

        let mut r = req(&format!("http://127.0.0.1:{port}/silent"));
        r.io_timeout = Duration::from_secs(30);
        let started = std::time::Instant::now();
        let e = send(&r, &cancel).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Aborted, "{}", e.message);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "abort waited for the read timeout instead of shutting the socket down ({:?})",
            started.elapsed()
        );
    }
}
