//! The transport behind `fetch` for `http:`/`https:`.
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
//! ## Two transports, one contract
//!
//! The JS side never touches a socket: `fetch.rs` calls [`start`] and gets the
//! outcome back through a callback on a network thread, which it hands to the
//! pump (`web::schedule_native`, exactly like image decoding). What runs in
//! between depends on the `net-h2` cargo feature.
//!
//! **With `net-h2` (vue-native turns it on): [`pool`], a browser-shaped network
//! stack.** One dedicated thread runs a single-threaded tokio runtime; every
//! request is a task on it. Connections are pooled per origin
//! (scheme, host, port):
//!
//! - **HTTP/2 over TLS through ALPN** (`h2`, then `http/1.1`). One connection per
//!   origin, every request a stream on it; SETTINGS and flow control are the
//!   `h2` crate's, PING keepalive detects a connection that stopped answering,
//!   a GOAWAY moves requests the server never started onto a new connection.
//!   Aborting a request resets its stream (`RST_STREAM(CANCEL)`); the
//!   connection lives on.
//! - **HTTP/1.1 keep-alive** when the server does not offer h2: at most six
//!   connections per origin (Chromium's and Firefox's number), reuse only after
//!   a fully consumed, correctly framed response and never after
//!   `Connection: close`, an idle timeout, and Chromium's retry rule — a
//!   reused connection that turns out dead before any response byte is retried
//!   once on a fresh one, when that is safe (idempotent method, or the request
//!   was never written). An aborted HTTP/1.1 request closes its connection;
//!   it is never returned to the pool.
//!
//! Why it matters here: the Argon client on this project's machines talks to
//! its backend through a tun2socks VPN, and every one of its requests used to
//! open a fresh connection AND ask the server to close it
//! (`Connection: close`). On that path 10–40 % of such exchanges lost the
//! tail of the response and stalled for the whole read budget — curl.exe with
//! that one header too, and never without it. A browser keeps one connection
//! open and multiplexes everything over it; so does this.
//!
//! **Without `net-h2`: the blocking transport in this file.** One worker thread
//! per request, a fresh connection each time, HTTP/1.1 only — what redjs always
//! had, minus the timed reads (next section) and minus `Connection: close`: the
//! client reads the framed response and closes the socket itself, because
//! asking the SERVER to close is what stalled on this project's VPN path (the
//! report has the curl A/B).
//!
//! ## No timed blocking I/O, on either path
//!
//! Until m26 the blocking transport waited by giving every socket a 200 ms
//! `SO_RCVTIMEO`/`SO_SNDTIMEO` and retrying. On Windows a `recv` that hits
//! `SO_RCVTIMEO` is cancelled mid-flight and MSDN declares the socket's state
//! *indeterminate*; and schannel treats a timed-out read inside a TLS handshake
//! (`TimedOut`) as a failure, where only `WouldBlock` means "try again". Both
//! paths now wait on readiness instead: the pooled one through tokio (IOCP),
//! the blocking one with a non-blocking socket and `WSAPoll`/`poll`
//! ([`sock`]). A byte is either in the kernel's buffer or in ours; no wait can
//! drop one. (Measured in research/reports/m26/net-h2-pool.md: a loopback
//! run of ~650 000 timed-out `recv`s lost NO bytes, so this is hygiene plus
//! the schannel fix, not the cause of the production stalls — those followed
//! `Connection: close`, see below.)
//!
//! ## The design call: crates for the protocols, our own pool
//!
//! The pooled transport is `hyper` 1.x's per-connection client API
//! (`client::conn::http1` / `http2`, which bring `h2`) over
//! `tokio-native-tls`, behind the `net-h2` feature. Not a hand-rolled HTTP/2,
//! and not hyper-util's pooled `Client`:
//!
//! - **Protocols are where hand-rolling goes wrong quietly.** HTTP/2 is HPACK
//!   (with Huffman and a dynamic table), per-stream and per-connection flow
//!   control, SETTINGS acknowledgement, stream-state rules and GOAWAY
//!   semantics; `h2` has years of production behind it and a test suite we
//!   would otherwise have to write. HTTP/1.1 keep-alive needs exact framing on
//!   every path to reuse a connection safely; hyper's is the reference.
//! - **The old objection was abort, and an async stack answers it.** The
//!   header of this file used to say a client crate "hands out no socket", so
//!   abort degrades to abandoning the promise. With per-connection hyper the
//!   pool is OURS: an aborted HTTP/2 request drops its stream future, which
//!   sends `RST_STREAM`; an aborted HTTP/1.1 request drops its connection task,
//!   which closes the socket, and its pool slot is released.
//! - **The pool is ours on purpose.** hyper-util's `Client` has no per-host
//!   connection limit, opens one connection per concurrent request on first
//!   contact before ALPN is known, and retries on its own terms. Browsers'
//!   rules (six per host, one h2 connection that the other requests wait for,
//!   the reuse/retry rules above) are a few hundred lines on top of
//!   per-connection hyper, and they are the part an app can feel.
//! - **"No tokio in a shipped player's link"** still holds for builds without
//!   the feature. vue-native already links tokio, hyper and h2 (its LiveKit
//!   stack) at the same versions, so the feature adds little there; the
//!   runtime is our own (one thread, current-thread scheduler), never
//!   borrowed from an embedder.
//! - **TLS stays the platform's** (schannel on Windows): `native-tls` with ALPN,
//!   no bundled roots, no second crypto stack.
//!
//! ## Cancellation and the stall budget
//!
//! [`Cancel`] is the `AbortSignal` bridge. On the pooled path an abort wakes
//! the request's task at once; on the blocking path the socket is shut down
//! (which ends the readiness wait) and every wait is cut into [`POLL`] slices
//! as a backstop. `io_timeout` is a *stall* budget, not a total: it is reset by
//! every byte that moves, so a slow-but-progressing transfer is never cut off,
//! while a dead connection still gives up on time.
//!
//! ## Proxies
//!
//! Not used. Neither transport reads the system proxy settings or
//! `HTTP(S)_PROXY`/`ALL_PROXY`; every connection goes direct. (Earlier
//! versions of this header said proxies were "refused by name at the `fetch`
//! layer". No such check existed.) A page behind a mandatory proxy sees a
//! connection error. vue-native's docs/DESKTOP.md §4.2 lists this as F-3.
//!
//! ## Cookies
//!
//! [`Request::credentials`] decides, PER HOP, whether the jar
//! ([`super::cookies`]) contributes a `Cookie` header to the request going
//! out and stores the `Set-Cookie` headers of the head that comes back. Per
//! hop is the point: a cookie set by a redirect is on the request it points
//! at, and a hop to another host carries only that host's cookies. Redirects
//! are followed here (not by hyper), on both transports, by one shared state
//! machine ([`Hops`]).
//!
//! ## The HTTP cache
//!
//! Also per hop, on both transports. Before each hop, [`cache`] decides
//! whether the disk answers it, whether the server is asked conditionally, or
//! whether the request goes out as is. After the hop it decides what to store.
//! A redirect with a `max-age` therefore costs no round trip while it is fresh.
//! The cache is off until an embedder installs a folder
//! ([`cache::install`]); [`Request::cache`] is Fetch's `cache` mode, and
//! [`Response::cache`] says what happened.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod sock;
pub(crate) use sock::Wait;
use sock::Polled;

#[cfg(feature = "net-h2")]
pub mod pool;

pub mod cache;

/// Response heads larger than this are a malformed or hostile peer.
pub(crate) const MAX_HEAD: usize = 64 * 1024;
/// Body ceiling. A cap that rejects loudly beats an allocation that takes the
/// process down; big local assets are read off disk, not off the network.
pub const MAX_BODY: usize = 256 * 1024 * 1024;
const MAX_REDIRECTS: u32 = 20;
const MAX_CHUNK: u64 = MAX_BODY as u64;
/// The blocking transport's wait slice: how long a readiness wait runs before
/// the abort flag is looked at again. Not a socket timeout — nothing is
/// cancelled when it elapses (see [`sock`]).
pub(crate) const POLL: Duration = Duration::from_millis(200);

/// TCP connections the BLOCKING transport has opened, process-wide. The pooled
/// transport counts its own ([`pool::stats`]).
pub static CONNECTS: AtomicU64 = AtomicU64::new(0);
/// Test-only override of [`POLL`] (the lost-bytes experiment amplifies waits).
#[cfg(test)]
pub(crate) static TEST_POLL_MS: AtomicU64 = AtomicU64::new(0);
/// Test builds trust the loopback test identity in `http/testdata` (a
/// self-signed `localhost` certificate; its key lives only in the .p12 the
/// test servers load).
#[cfg(test)]
pub(crate) const TEST_ROOT_PEM: &[u8] = include_bytes!("http/testdata/cert.pem");
#[cfg(test)]
mod stress_tests;

/// The protocol names a response reports — ALPN's spelling, which is also what
/// a browser's `PerformanceResourceTiming.nextHopProtocol` says.
pub const PROTO_H1: &str = "http/1.1";
pub const PROTO_H2: &str = "h2";


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

#[derive(Default)]
struct CancelState {
    aborted: AtomicBool,
    /// A `try_clone` of the blocking transport's live socket, present only
    /// while one is open.
    socket: Mutex<Option<TcpStream>>,
    /// Wakes the pooled transport's task, which is parked in a `select!`.
    #[cfg(feature = "net-h2")]
    wake: tokio::sync::Notify,
}

/// The `AbortSignal` bridge. Shared between the JS thread (which calls
/// [`abort`](Cancel::abort) from the signal's `abort` listener) and whatever
/// is carrying the request: a blocking worker parked in a readiness wait, or
/// a task on the network runtime.
///
/// A cheap handle (`Clone` shares the one state): the transports keep their
/// own copy, so a request can be cancelled from anywhere that holds one.
#[derive(Default, Clone)]
pub struct Cancel(Arc<CancelState>);

impl Cancel {
    pub fn new() -> Arc<Cancel> {
        Arc::new(Cancel::default())
    }

    /// Idempotent. Safe to call before the socket exists (the flag is checked
    /// at every step) and after it is gone.
    pub fn abort(&self) {
        self.0.aborted.store(true, Ordering::SeqCst);
        // Ends the blocking path's readiness wait at once (POLLHUP); without
        // it, the next `POLL` slice notices the flag.
        if let Some(s) = self.0.socket.lock().unwrap().as_ref() {
            let _ = s.shutdown(Shutdown::Both);
        }
        #[cfg(feature = "net-h2")]
        self.0.wake.notify_waiters();
    }

    pub fn is_aborted(&self) -> bool {
        self.0.aborted.load(Ordering::SeqCst)
    }

    fn check(&self) -> Result<(), HttpError> {
        if self.is_aborted() {
            return Err(HttpError::new(ErrorKind::Aborted, "the request was aborted"));
        }
        Ok(())
    }

    /// Resolves once the request is aborted. Registers before it looks, so an
    /// abort that lands in between is not missed.
    #[cfg(feature = "net-h2")]
    pub(crate) async fn aborted(&self) {
        loop {
            let notified = self.0.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_aborted() {
                return;
            }
            notified.await;
        }
    }

    /// Publishes the socket so `abort` can reach it. Races correctly: an abort
    /// that landed between the connect and here is caught by the re-check.
    fn arm(&self, s: &TcpStream) -> Result<(), HttpError> {
        match s.try_clone() {
            Ok(dup) => *self.0.socket.lock().unwrap() = Some(dup),
            // Without a handle the wait slice is the backstop. Not fatal.
            Err(_) => {}
        }
        self.check()
    }

    #[cfg_attr(feature = "net-h2", allow(dead_code))]
    fn disarm(&self) {
        *self.0.socket.lock().unwrap() = None;
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
    /// Progress as it happens (XMLHttpRequest's events); `None` for `fetch`,
    /// whose body is delivered whole.
    pub observer: Option<Observer>,
    /// Fetch's `cache` mode: how the HTTP cache ([`cache`]) treats every hop.
    /// Inert while no cache is installed.
    pub cache: cache::CacheMode,
}

/// Progress of one request while it is in flight — what XMLHttpRequest needs
/// for `upload.onprogress`, `readyState` 2 → 3 → 4 and `onprogress`.
///
/// Called on a NETWORK thread, never the JS thread: an implementation records
/// or queues what it needs and lets the pump deliver it (as `fetch.rs` does
/// with its outcome). Calls arrive in order: `upload`* → `head` → `body`*,
/// then [`Done`] with the whole `Response` (or the error). Only the FINAL hop
/// is reported: a redirect's head and body are never seen here, its upload is
/// (the body is sent again on a 307/308, and counted again from zero).
///
/// The blocking transport (no `net-h2`) reports upload per 64 KiB written,
/// and the body as ONE piece once it is complete; the pooled one reports each
/// piece as it arrives.
pub trait Observe: Send + Sync {
    /// Request-body bytes handed to the connection so far, of `total`.
    fn upload(&self, sent: u64, total: u64) {
        let _ = (sent, total);
    }
    /// The final response's head, before any of its body.
    fn head(&self, head: &ProgressHead<'_>) {
        let _ = head;
    }
    /// A piece of the final response's body, as received: still
    /// content-coded when the head names a `Content-Encoding` (the `Response`
    /// handed to [`Done`] carries the decoded whole). `received` counts every
    /// piece so far; `total` is the declared `Content-Length`, if any.
    fn body(&self, chunk: &[u8], received: u64, total: Option<u64>) {
        let _ = (chunk, received, total);
    }
}

/// What [`Observe::head`] sees.
pub struct ProgressHead<'a> {
    pub status: u16,
    pub status_text: &'a str,
    /// Lower-case names, `set-cookie` included (the jar has stored them; a
    /// script-facing API filters them, as `fetch.rs` does).
    pub headers: &'a [(String, String)],
    /// The final URL, after redirects.
    pub url: &'a str,
    pub redirected: bool,
    pub protocol: &'static str,
}

/// A shareable [`Observe`] (a `Request` is `Clone`).
#[derive(Clone)]
pub struct Observer(pub Arc<dyn Observe>);

impl std::fmt::Debug for Observer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Observer")
    }
}

/// Whether a response with this head is the request's answer: not a redirect
/// (which is followed, or under `redirect: "error"` fails the request).
pub(crate) fn is_final(status: u16, headers: &[(String, String)]) -> bool {
    !(is_redirect(status) && header(headers, "location").is_some())
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
    /// Diagnostics for the final hop (devtools' Network panel; not exposed to
    /// script): which protocol carried it ...
    pub protocol: &'static str,
    /// ... whether its connection had already carried an earlier request ...
    pub reused: bool,
    /// ... and which connection that was (process-unique, both transports
    /// number from [`next_connection_id`]) ...
    pub connection_id: u64,
    /// ... and the address it was connected to.
    pub remote_addr: Option<std::net::SocketAddr>,
    /// What the HTTP cache did for the final hop. An answer from disk has no
    /// connection: `protocol` "", `connection_id` 0, no address.
    pub cache: cache::Outcome,
}

static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);

/// A process-unique connection number, for diagnostics.
pub(crate) fn next_connection_id() -> u64 {
    NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed)
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

/// Request headers the transport owns: framing, connection management, and
/// the names HTTP/2 forbids outright. A script that sets one is ignored rather
/// than obeyed — Fetch lists every one of these as a forbidden request-header
/// name, which a browser drops silently too.
pub(crate) fn is_transport_header(lower: &str) -> bool {
    matches!(
        lower,
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "upgrade"
            | "http2-settings"
    )
}

/// The redirect loop, shared by both transports: which URL, method, body and
/// headers the next hop carries, the jar applied per hop.
pub(crate) struct Hops<'a> {
    req: &'a Request,
    pub(crate) current: String,
    pub(crate) method: String,
    pub(crate) body: Option<Vec<u8>>,
    headers: Vec<(String, String)>,
    pub(crate) redirected: bool,
    left: u32,
}

/// One hop to make.
pub(crate) struct Hop {
    pub(crate) url: url::Url,
    /// The request's headers with this hop's `Cookie` applied (when the
    /// credentials mode says so).
    pub(crate) headers: Vec<(String, String)>,
    cookies: bool,
}

impl<'a> Hops<'a> {
    pub(crate) fn new(req: &'a Request) -> Self {
        let mut headers = req.headers.clone();
        // Fetch's headers for an explicit cache mode (`Pragma: no-cache` & co).
        cache::add_mode_headers(req.cache, &mut headers);
        Hops {
            req,
            current: req.url.clone(),
            method: req.method.to_ascii_uppercase(),
            body: req.body.clone(),
            headers,
            redirected: false,
            left: MAX_REDIRECTS + 1,
        }
    }

    pub(crate) fn next(&mut self) -> Result<Hop, HttpError> {
        if self.left == 0 {
            return Err(HttpError::network(format!("too many redirects (more than {MAX_REDIRECTS})")));
        }
        self.left -= 1;
        let u = url::Url::parse(&self.current)
            .map_err(|e| HttpError::refused(format!("invalid URL {:?}: {e}", self.current)))?;
        if u.scheme() != "http" && u.scheme() != "https" {
            return Err(HttpError::refused(format!("scheme {:?} is not supported by fetch", u.scheme())));
        }
        // The jar, per hop: this hop's cookies on the way out, this hop's
        // Set-Cookie on the way back — before the redirect is followed, so the
        // next hop already sees what this one set.
        let cookies = self.req.credentials.applies(&u);
        let headers = if cookies { super::cookies::with_cookies(&u, &self.headers) } else { None }
            .unwrap_or_else(|| self.headers.clone());
        Ok(Hop { url: u, headers, cookies })
    }

    /// After a hop's head: stores its cookies, then either sets up the next
    /// hop (`Ok(true)`) or says this response is the answer (`Ok(false)`).
    pub(crate) fn after(&mut self, hop: &Hop, status: u16, headers: &[(String, String)]) -> Result<bool, HttpError> {
        self.store_cookies(hop, headers);
        self.follow(status, headers)
    }

    /// The jar's half of [`Hops::after`]: this hop's `Set-Cookie`, when the
    /// credentials mode says so.
    pub(crate) fn store_cookies(&self, hop: &Hop, headers: &[(String, String)]) {
        if hop.cookies {
            super::cookies::store_response(&hop.url, headers);
        }
    }

    /// The redirect half of [`Hops::after`]. On its own for an answer the HTTP
    /// cache served or refreshed: its `Location` is followed, and the cookies
    /// came (if at all) with the 304 that refreshed it.
    pub(crate) fn follow(&mut self, status: u16, headers: &[(String, String)]) -> Result<bool, HttpError> {
        if !is_redirect(status) {
            return Ok(false);
        }
        let Some(loc) = header(headers, "location") else { return Ok(false) };
        if self.req.redirect == Redirect::Error {
            return Err(HttpError::network(format!(
                "redirect to {loc:?} refused: the request set redirect: \"error\""
            )));
        }
        let next = resolve_location(&self.current, loc)?;
        let (next_method, keep_body) = redirect_method(status, &self.method);
        if !same_origin(&self.current, &next) {
            strip_sensitive(&mut self.headers);
        }
        if !keep_body {
            self.body = None;
            self.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("content-type"));
        }
        self.method = next_method;
        self.current = next;
        self.redirected = true;
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// What [`start`] calls with the outcome, exactly once, on a network thread.
pub type Done = Box<dyn FnOnce(Result<Response, HttpError>) + Send + 'static>;

/// Starts `req` off the calling thread; `done` receives the outcome. This is
/// `fetch`'s entry point: with `net-h2` the request is a task on the network
/// runtime (no thread of its own), otherwise a worker thread runs [`send`].
///
/// `Err` only when the request could not be started at all (no thread); then
/// `done` has been dropped uncalled.
pub fn start(req: Request, cancel: Cancel, done: Done) -> Result<(), HttpError> {
    #[cfg(feature = "net-h2")]
    {
        pool::start(req, cancel, done)
    }
    #[cfg(not(feature = "net-h2"))]
    {
        std::thread::Builder::new()
            .name("redjs-fetch".into())
            .spawn(move || done(send_blocking(&req, &cancel)))
            .map(|_| ())
            .map_err(|e| HttpError::network(format!("could not start a request thread: {e}")))
    }
}

/// Performs `req`, following redirects per its policy. Blocking: call from a
/// worker thread (tests and tools; `fetch` uses [`start`]).
pub fn send(req: &Request, cancel: &Cancel) -> Result<Response, HttpError> {
    #[cfg(feature = "net-h2")]
    {
        pool::send(req, cancel)
    }
    #[cfg(not(feature = "net-h2"))]
    {
        send_blocking(req, cancel)
    }
}

// ---------------------------------------------------------------------------
// The blocking transport (HTTP/1.1, one fresh connection per request)
// ---------------------------------------------------------------------------

/// The connected socket, plaintext or TLS, over a non-blocking [`Polled`].
///
/// `pub(crate)` because [`super::websocket`] borrows this whole transport: a
/// WebSocket *is* an HTTP/1.1 request whose 101 response hands the socket over
/// to a different framing. Reusing the connect + TLS + cancellation path here
/// is what keeps `wss:` on exactly the trust story `https:` already has
/// (platform TLS, no bundled roots, no second crypto stack — see the module
/// header) instead of introducing a second one.
pub(crate) enum Stream {
    Plain(Polled),
    Tls(Box<native_tls::TlsStream<Polled>>),
}

impl Stream {
    /// The TCP socket underneath, for `shutdown`. Reading or writing it
    /// directly on a TLS connection would corrupt the session.
    pub(crate) fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s.tcp(),
            Stream::Tls(s) => s.get_ref().tcp(),
        }
    }

    /// How a read or write that cannot complete at once waits (see [`Wait`]).
    pub(crate) fn set_wait(&mut self, wait: Wait) {
        match self {
            Stream::Plain(s) => s.set_wait(wait),
            Stream::Tls(s) => s.get_mut().set_wait(wait),
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

/// The host to dial and to name in SNI: `host_str` keeps an IPv6 literal's
/// brackets, which neither the resolver nor TLS wants.
pub(crate) fn dial_host(u: &url::Url) -> Result<String, HttpError> {
    match u.host() {
        Some(url::Host::Domain(d)) => Ok(d.to_string()),
        Some(url::Host::Ipv4(a)) => Ok(a.to_string()),
        Some(url::Host::Ipv6(a)) => Ok(a.to_string()),
        None => Err(HttpError::refused(format!("URL has no host: {u}"))),
    }
}

/// Resolved addresses, ordered the way RFC 8305 (Happy Eyeballs v2) asks:
/// families interleaved, starting with whichever the resolver put first. A
/// family that fails (this machine's IPv6 does, fast) costs one attempt, not
/// every address of that family.
pub(crate) fn interleave(addrs: Vec<std::net::SocketAddr>) -> Vec<std::net::SocketAddr> {
    let Some(first) = addrs.first().copied() else { return addrs };
    let (mut a, mut b): (Vec<_>, Vec<_>) = addrs.into_iter().partition(|x| x.is_ipv6() == first.is_ipv6());
    a.reverse();
    b.reverse();
    let mut out = Vec::with_capacity(a.len() + b.len());
    loop {
        match (a.pop(), b.pop()) {
            (None, None) => return out,
            (x, y) => {
                out.extend(x);
                out.extend(y);
            }
        }
    }
}

/// A TLS connector with the platform's roots (plus, in test builds, the
/// loopback test identity's certificate).
pub(crate) fn tls_builder() -> native_tls::TlsConnectorBuilder {
    #[allow(unused_mut)]
    let mut b = native_tls::TlsConnector::builder();
    #[cfg(test)]
    b.add_root_certificate(native_tls::Certificate::from_pem(TEST_ROOT_PEM).expect("test certificate"));
    b
}

/// Connect + (for `https`) TLS handshake, every wait a readiness wait. The
/// returned stream waits in [`Wait::Block`] mode with the connect budget;
/// callers set their own budget for what follows.
pub(crate) fn connect(u: &url::Url, connect_timeout: Duration, cancel: &Cancel) -> Result<Stream, HttpError> {
    let host = dial_host(u)?;
    let port = u
        .port_or_known_default()
        .ok_or_else(|| HttpError::refused(format!("URL has no port and no default: {u}")))?;
    let secure = u.scheme() == "https" || u.scheme() == "wss";

    cancel.check()?;
    let addrs: Vec<_> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| HttpError::network(format!("could not resolve {host}: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(HttpError::network(format!("could not resolve {host}")));
    }

    let mut last: Option<std::io::Error> = None;
    let mut tcp: Option<TcpStream> = None;
    for a in interleave(addrs) {
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
    CONNECTS.fetch_add(1, Ordering::SeqCst);

    // Armed BEFORE the TLS handshake so an abort during it lands too.
    cancel.arm(&tcp)?;
    let polled = Polled::new(tcp, cancel.clone(), Wait::Block { budget: connect_timeout })
        .map_err(|e| HttpError::network(format!("could not configure the socket: {e}")))?;

    if !secure {
        return Ok(Stream::Plain(polled));
    }
    let connector = tls_builder().build().map_err(|e| HttpError::network(format!("TLS setup failed: {e}")))?;
    // The socket blocks (in readiness waits) underneath, so the handshake runs
    // in one call: schannel never sees `WouldBlock` or a timed-out read.
    let started = Instant::now();
    match connector.connect(&host, polled) {
        Ok(s) => Ok(Stream::Tls(Box::new(s))),
        Err(_) if cancel.is_aborted() => Err(HttpError::new(ErrorKind::Aborted, "the request was aborted")),
        Err(native_tls::HandshakeError::WouldBlock(_)) => {
            Err(HttpError::network(format!("TLS handshake with {host} stalled")))
        }
        Err(native_tls::HandshakeError::Failure(e)) => {
            if started.elapsed() >= connect_timeout {
                Err(HttpError::network(format!("TLS handshake with {host} timed out")))
            } else {
                Err(HttpError::network(format!("TLS handshake with {host} failed: {e}")))
            }
        }
    }
}

/// The request line + headers exactly as they go on the wire (blocking
/// transport). Split out so the framing is testable without a socket.
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
    let host_header = match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    };

    let mut seen: Vec<String> = Vec::new();
    for (k, v) in headers {
        validate_header(k, v)?;
        let lower = k.to_ascii_lowercase();
        // Framing is ours; a JS-supplied one would desynchronize the parser.
        if is_transport_header(&lower) {
            continue;
        }
        seen.push(lower);
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    let has = |n: &str| seen.iter().any(|s| s == n);

    out.extend_from_slice(format!("Host: {host_header}\r\n").as_bytes());
    if !has("user-agent") {
        out.extend_from_slice(format!("User-Agent: {}\r\n", super::navigator::user_agent()).as_bytes());
    }
    if !has("accept") {
        out.extend_from_slice(b"Accept: */*\r\n");
    }
    if !has("accept-encoding") {
        out.extend_from_slice(b"Accept-Encoding: gzip, deflate\r\n");
    }
    // No pooling on this transport, but no `Connection: close` either: asking
    // the SERVER to close is what stalled (measured, research/reports/m26/
    // net-h2-pool.md §2 — on this project's VPN path the tail of a response
    // the server closes behind is lost now and then, with curl too). The
    // response is framed, so the client reads it whole and closes the socket
    // itself.
    if needs_length(method, body) {
        out.extend_from_slice(format!("Content-Length: {}\r\n", body.map_or(0, |b| b.len())).as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    if let Some(b) = body {
        out.extend_from_slice(b);
    }
    Ok(out)
}

/// Spec: a body-bearing method without a body still declares zero length, or
/// the server waits for a body forever.
pub(crate) fn needs_length(method: &str, body: Option<&[u8]>) -> bool {
    body.is_some() || matches!(method, "POST" | "PUT" | "PATCH")
}

/// Maps a socket error, preferring "aborted" when we caused it.
fn map_io(cancel: &Cancel, e: std::io::Error, what: &str) -> HttpError {
    if cancel.is_aborted() {
        return HttpError::new(ErrorKind::Aborted, "the request was aborted");
    }
    match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
            HttpError::network(format!("{what} timed out"))
        }
        _ => HttpError::network(format!("{what} failed: {e}")),
    }
}

pub(crate) fn read_head<R: Read>(r: &mut R, cancel: &Cancel) -> Result<(Head, Vec<u8>), HttpError> {
    let mut acc: Vec<u8> = Vec::with_capacity(2048);
    let mut buf = [0u8; 8 * 1024];
    loop {
        // Scan only the tail that could contain a newly completed terminator.
        let scan_from = acc.len().saturating_sub(3);
        let got = r.read(&mut buf).map_err(|e| map_io(cancel, e, "response read"))?;
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

/// One hop on the blocking transport: connect, write, read. No redirects.
/// `url_now`/`redirected` describe this hop for an [`Observe`]r.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(feature = "net-h2", allow(dead_code))]
fn round_trip(
    u: &url::Url,
    method: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    req: &Request,
    cancel: &Cancel,
    url_now: &str,
    redirected: bool,
) -> Result<(Head, Vec<u8>, Option<std::net::SocketAddr>), HttpError> {
    let mut stream = connect(u, req.connect_timeout, cancel)?;
    stream.set_wait(Wait::Block { budget: req.io_timeout });
    let remote = stream.tcp().peer_addr().ok();

    let wire = serialize_request(u, method, headers, body)?;
    let body_len = body.map_or(0, |b| b.len());
    let (head_part, body_part) = wire.split_at(wire.len() - body_len);
    stream.write_all(head_part).map_err(|e| map_io(cancel, e, "request write"))?;
    let mut sent = 0usize;
    for piece in body_part.chunks(64 * 1024) {
        stream.write_all(piece).map_err(|e| map_io(cancel, e, "request write"))?;
        sent += piece.len();
        if let Some(o) = &req.observer {
            o.0.upload(sent as u64, body_len as u64);
        }
    }
    stream.flush().map_err(|e| map_io(cancel, e, "request write"))?;

    let (head, rest) = read_head(&mut stream, cancel)?;

    // 1xx informational responses are not the answer — read past them.
    let (head, rest) = if (100..200).contains(&head.status) {
        read_head_after_informational(&mut stream, rest, cancel)?
    } else {
        (head, rest)
    };

    let observed = req.observer.as_ref().filter(|_| is_final(head.status, &head.headers));
    if let Some(o) = observed {
        o.0.head(&ProgressHead {
            status: head.status,
            status_text: &head.reason,
            headers: &head.headers,
            url: url_now,
            redirected,
            protocol: PROTO_H1,
        });
    }

    let framing = framing_of(method, head.status, &head.headers)?;
    let raw = match framing {
        Framing::None => Vec::new(),
        Framing::Length(n) => read_exact_body(&mut stream, n, &rest).map_err(|e| reclassify(e, cancel))?,
        Framing::Chunked => read_chunked(&mut stream, &rest).map_err(|e| reclassify(e, cancel))?,
        Framing::UntilClose => read_until_close(&mut stream, &rest).map_err(|e| reclassify(e, cancel))?,
    };
    if let Some(o) = observed {
        if !raw.is_empty() {
            let declared = match framing {
                Framing::Length(n) => Some(n as u64),
                _ => None,
            };
            o.0.body(&raw, raw.len() as u64, declared);
        }
    }
    let decoded = decode_content_encoding(header(&head.headers, "content-encoding"), raw)?;
    Ok((head, decoded, remote))
}

/// After a 1xx, the real head follows immediately.
#[cfg_attr(feature = "net-h2", allow(dead_code))]
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
        let got = r.read(&mut buf).map_err(|e| map_io(cancel, e, "response read"))?;
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
#[cfg_attr(feature = "net-h2", allow(dead_code))]
fn reclassify(e: HttpError, cancel: &Cancel) -> HttpError {
    if cancel.is_aborted() {
        return HttpError::new(ErrorKind::Aborted, "the request was aborted");
    }
    e
}

/// The blocking transport's request: redirects, cookies per hop, a fresh
/// connection per hop, the HTTP cache per hop ([`cache`]).
#[cfg_attr(feature = "net-h2", allow(dead_code))]
pub(crate) fn send_blocking(req: &Request, cancel: &Cancel) -> Result<Response, HttpError> {
    use cache::{Action, Outcome, Settled};
    let mut hops = Hops::new(req);
    loop {
        cancel.check()?;
        let hop = hops.next()?;
        let (action, ctx) = cache::plan(&hops.method, hop.url.as_str(), &hop.headers, req.cache).into_parts();
        let (validating, conditional) = match action {
            Action::Fail(e) => return Err(e),
            Action::Serve { entry, revalidate } => {
                if revalidate {
                    cache::revalidate_in_background(req, hop.url.as_str());
                }
                if hops.follow(entry.status, &entry.headers)? {
                    continue;
                }
                return blocking_from_cache(req, &hops, entry, Outcome::Hit, ("", 0, None));
            }
            Action::Validate { entry, conditional } => (Some(entry), conditional),
            Action::Network => (None, Vec::new()),
        };
        // A conditional exchange is not observed: its 304 is not the answer.
        let asked = validating.is_some();
        let quiet;
        let (sent, headers) = if asked {
            quiet = Request { observer: None, ..req.clone() };
            let mut h = hop.headers.clone();
            h.extend(conditional);
            (&quiet, std::borrow::Cow::Owned(h))
        } else {
            (req, std::borrow::Cow::Borrowed(&hop.headers[..]))
        };
        let result = round_trip(
            &hop.url,
            &hops.method,
            &headers,
            hops.body.as_deref(),
            sent,
            cancel,
            &hops.current,
            hops.redirected,
        );
        cancel.disarm();
        let (head, bytes, remote) = result?;
        let id = next_connection_id();
        match cache::settle(ctx, validating, head.status, &head.reason, &head.headers, &bytes, false) {
            Settled::Refreshed(entry) => {
                hops.store_cookies(&hop, &head.headers);
                if hops.follow(entry.status, &entry.headers)? {
                    continue;
                }
                return blocking_from_cache(req, &hops, entry, Outcome::Revalidated, (PROTO_H1, id, remote));
            }
            Settled::Network(outcome) => {
                if hops.after(&hop, head.status, &head.headers)? {
                    continue;
                }
                if asked {
                    let (url, r) = (&hops.current, hops.redirected);
                    cache::observe(req, url, r, PROTO_H1, head.status, &head.reason, &head.headers, &bytes);
                }
                return Ok(Response {
                    status: head.status,
                    status_text: head.reason,
                    headers: head.headers,
                    url: hops.current,
                    redirected: hops.redirected,
                    body: bytes,
                    protocol: PROTO_H1,
                    reused: false,
                    connection_id: id,
                    remote_addr: remote,
                    cache: outcome,
                });
            }
        }
    }
}

/// An answer the cache served (or a 304 refreshed) on the blocking transport.
/// `net`: the connection that carried the 304, if one did.
#[cfg_attr(feature = "net-h2", allow(dead_code))]
fn blocking_from_cache(
    req: &Request,
    hops: &Hops<'_>,
    entry: cache::Entry,
    outcome: cache::Outcome,
    net: (&'static str, u64, Option<std::net::SocketAddr>),
) -> Result<Response, HttpError> {
    let (url, redirected) = (hops.current.clone(), hops.redirected);
    cache::observe(req, &url, redirected, net.0, entry.status, &entry.reason, &entry.headers, &entry.body);
    let body = if entry.coded {
        decode_content_encoding(header(&entry.headers, "content-encoding"), entry.body)?
    } else {
        entry.body
    };
    Ok(Response {
        status: entry.status,
        status_text: entry.reason,
        headers: entry.headers,
        url,
        redirected,
        body,
        protocol: net.0,
        reused: false,
        connection_id: net.1,
        remote_addr: net.2,
        cache: outcome,
    })
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
        // The client closes; it never asks the server to (see serialize_request).
        assert!(!text.to_ascii_lowercase().contains("connection:"), "{text}");
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
            observer: None,
            cache: cache::CacheMode::Default,
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

    #[derive(Default)]
    struct Seen(std::sync::Mutex<Vec<String>>);

    impl Observe for Seen {
        fn upload(&self, sent: u64, total: u64) {
            self.0.lock().unwrap().push(format!("up {sent}/{total}"));
        }
        fn head(&self, h: &ProgressHead<'_>) {
            self.0.lock().unwrap().push(format!("head {}", h.status));
        }
        fn body(&self, chunk: &[u8], received: u64, _total: Option<u64>) {
            self.0.lock().unwrap().push(format!("body {} {received}", chunk.len()));
        }
    }

    #[test]
    fn an_observer_hears_upload_head_and_body_in_order() {
        // Both transports: the blocking one reports the body as one piece,
        // the pooled one per frame; the order and the totals are the contract.
        let (port, _seen) = serve(vec![b"HTTP/1.1 201 Created\r\nContent-Length: 7\r\n\r\ncreated".to_vec()]);
        let seen = Arc::new(Seen::default());
        let mut r = req(&format!("http://127.0.0.1:{port}/up"));
        r.method = "PUT".into();
        r.body = Some(vec![1u8; 70_000]);
        r.observer = Some(Observer(seen.clone()));
        let resp = send(&r, &Cancel::default()).unwrap();
        assert_eq!(resp.status, 201);
        let ev = seen.0.lock().unwrap().clone();
        let head = ev.iter().position(|e| e == "head 201").expect("no head event");
        assert_eq!(ev[head - 1], "up 70000/70000", "{ev:?}");
        assert!(ev[..head].iter().all(|e| e.starts_with("up ")), "{ev:?}");
        assert!(ev[head + 1..].iter().all(|e| e.starts_with("body ")), "{ev:?}");
        assert!(ev.last().unwrap().ends_with(" 7"), "{ev:?}");
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
