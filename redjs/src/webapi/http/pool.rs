//! The pooled transport (`net-h2`): HTTP/2 over TLS through ALPN, HTTP/1.1
//! keep-alive otherwise — the rules a browser's network stack follows. The
//! header of `http.rs` says why it is shaped this way; this file is the
//! mechanics.
//!
//! ## Threads
//!
//! One OS thread, `redjs-net`, runs a current-thread tokio runtime for the
//! life of the process (started by the first request). Every request and every
//! connection is a task on it. Name resolution runs on tokio's blocking pool
//! (`getaddrinfo` blocks), at most four threads. Nothing here touches JSC: the
//! outcome goes to a [`Done`] callback, which `fetch.rs` hands to the pump.
//!
//! ## The pool
//!
//! Per origin (scheme, host, port) one [`Origin`] record:
//!
//! - **h2**: at most one live HTTP/2 connection; every request to the origin is
//!   a stream on it (the `h2` crate queues streams past the server's
//!   `SETTINGS_MAX_CONCURRENT_STREAMS`).
//! - **HTTP/1.1**: idle keep-alive connections (most recently used first), and
//!   a count of open ones capped at [`MAX_PER_HOST`].
//! - **First contact**: until a connection has told us (via ALPN) what the
//!   origin speaks, only ONE connection is dialled; concurrent requests wait
//!   for it. If it is h2 they all become streams on it — 24 parallel requests
//!   open one connection, as in Chromium. If it is HTTP/1.1 the waiters dial
//!   their own, up to six.
//!
//! An HTTP/1.1 connection goes back to the pool only after its response was
//! read to the end AND hyper reports it ready for another request (it is not,
//! after `Connection: close`, a close-delimited body, or any framing error).
//! Idle ones are closed after [`H1_IDLE`]; an h2 connection with no stream for
//! [`H2_IDLE`] is dropped. Every h2 connection PINGs every [`PING_INTERVAL`]
//! (also while idle); no answer within [`PING_TIMEOUT`] declares it dead, which
//! fails its in-flight streams and takes it out of the pool — the remedy for a
//! flow that silently stopped (this project's VPN path produces those).
//!
//! ## Retries (Chromium's rules)
//!
//! A request is retried on another connection when the server provably never
//! saw it: hyper handed the request back unsent, the stream was refused
//! (`REFUSED_STREAM`), or a GOAWAY named a last stream below it. A request
//! that failed on a REUSED connection before any response byte arrived is
//! retried once more on a fresh connection, but only for an idempotent method —
//! the server may have acted on a POST. At most [`MAX_RETRIES`] retries.
//!
//! ## Budgets
//!
//! `connect_timeout` covers resolve + TCP + TLS + the HTTP/2 preface, as one
//! budget. `io_timeout` is a stall budget: reset by every request-body frame
//! handed to the connection and every response-body frame received. Waiting for
//! a free connection (six busy HTTP/1.1 connections) has no timeout of its own,
//! as in a browser; abort ends it.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Notify;

use super::{
    decode_content_encoding, dial_host, header, interleave, is_final, is_transport_header, needs_length,
    next_connection_id, tls_builder, validate_header, Cancel, Done, ErrorKind, Hops, HttpError, Observer, ProgressHead,
    Request, Response, MAX_BODY, PROTO_H1, PROTO_H2,
};

/// HTTP/1.1 connections per origin — Chromium's and Firefox's default.
pub const MAX_PER_HOST: usize = 6;
/// An idle HTTP/1.1 connection is closed after this. Servers commonly keep
/// one for 60–120 s; hyper notices a server's earlier close by itself.
pub const H1_IDLE: Duration = Duration::from_secs(90);
/// An HTTP/2 connection with no stream for this long is dropped.
pub const H2_IDLE: Duration = Duration::from_secs(300);
/// HTTP/2 PING cadence, while busy and while idle.
pub const PING_INTERVAL: Duration = Duration::from_secs(15);
/// No PING acknowledgement within this → the connection is dead.
pub const PING_TIMEOUT: Duration = Duration::from_secs(10);
/// Happy Eyeballs (RFC 8305): the next address is tried this long after the
/// previous attempt started, or at once when it fails.
const ATTEMPT_DELAY: Duration = Duration::from_millis(250);
const REAP_EVERY: Duration = Duration::from_secs(5);
pub const MAX_RETRIES: u32 = 2;
/// Request bodies are handed to the connection in frames of this size, so the
/// stall budget sees an upload progress.
const BODY_FRAME: usize = 64 * 1024;
/// Response head ceiling: Chromium's (256 KiB). The blocking transport's
/// stricter `MAX_HEAD` predates it.
const HEAD_LIMIT: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

fn log_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("REDJS_NET_LOG").is_some_and(|v| v != "0" && !v.is_empty()))
}

/// `REDJS_NET_LOG=1`: one stderr line per connection event and retry.
macro_rules! net_log {
    ($($t:tt)*) => {
        if log_on() {
            eprintln!("[redjs:net] {}", format_args!($($t)*));
        }
    };
}

static H1_OPENED: AtomicU64 = AtomicU64::new(0);
static H2_OPENED: AtomicU64 = AtomicU64::new(0);
static REQUESTS: AtomicU64 = AtomicU64::new(0);
static REUSED: AtomicU64 = AtomicU64::new(0);
static RETRIES: AtomicU64 = AtomicU64::new(0);

/// Process-wide counters (tests, diagnostics).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub h1_connections: u64,
    pub h2_connections: u64,
    /// Attempts put on a connection (a retried request counts twice).
    pub requests: u64,
    /// ... of which on a connection that had carried an earlier one.
    pub reused: u64,
    pub retries: u64,
}

pub fn stats() -> Stats {
    Stats {
        h1_connections: H1_OPENED.load(Ordering::SeqCst),
        h2_connections: H2_OPENED.load(Ordering::SeqCst),
        requests: REQUESTS.load(Ordering::SeqCst),
        reused: REUSED.load(Ordering::SeqCst),
        retries: RETRIES.load(Ordering::SeqCst),
    }
}

fn network(m: impl Into<String>) -> HttpError {
    HttpError::new(ErrorKind::Network, m)
}

fn aborted() -> HttpError {
    HttpError::new(ErrorKind::Aborted, "the request was aborted")
}

// ---------------------------------------------------------------------------
// The runtime
// ---------------------------------------------------------------------------

fn runtime() -> Result<&'static tokio::runtime::Handle, HttpError> {
    static RT: OnceLock<Result<tokio::runtime::Handle, String>> = OnceLock::new();
    RT.get_or_init(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .max_blocking_threads(4)
            .thread_name("redjs-net-resolve")
            .build()
            .map_err(|e| format!("the network runtime could not be built: {e}"))?;
        let handle = rt.handle().clone();
        std::thread::Builder::new()
            .name("redjs-net".into())
            .spawn(move || rt.block_on(reaper()))
            .map_err(|e| format!("the network thread could not be started: {e}"))?;
        Ok(handle)
    })
    .as_ref()
    .map_err(|e| network(e.clone()))
}

/// Starts `req` as a task on the network runtime.
pub(super) fn start(req: Request, cancel: Cancel, done: Done) -> Result<(), HttpError> {
    runtime()?.spawn(async move {
        let result = run(&req, &cancel).await;
        done(result);
    });
    Ok(())
}

/// Blocking wrapper over [`start`], for callers on a plain thread.
pub(super) fn send(req: &Request, cancel: &Cancel) -> Result<Response, HttpError> {
    let (tx, rx) = std::sync::mpsc::channel();
    start(
        req.clone(),
        cancel.clone(),
        Box::new(move |r| {
            let _ = tx.send(r);
        }),
    )?;
    rx.recv().unwrap_or_else(|_| Err(network("the network thread went away")))
}

// ---------------------------------------------------------------------------
// The pool
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    https: bool,
    host: String,
    port: u16,
}

impl Key {
    fn of(u: &url::Url) -> Result<Key, HttpError> {
        let host = dial_host(u)?;
        let port = u
            .port_or_known_default()
            .ok_or_else(|| HttpError::refused(format!("URL has no port and no default: {u}")))?;
        Ok(Key { https: u.scheme() == "https", host, port })
    }

    /// Cleartext HTTP/2 with prior knowledge: tests only (a browser never
    /// speaks h2c), so the h2 machinery can be exercised against a loopback
    /// server without a certificate.
    fn h2c(&self) -> bool {
        #[cfg(test)]
        {
            !self.https && test_h2c_ports().lock().unwrap().contains(&self.port)
        }
        #[cfg(not(test))]
        {
            false
        }
    }
}

#[cfg(test)]
pub(crate) fn test_h2c_ports() -> &'static Mutex<Vec<u16>> {
    static P: OnceLock<Mutex<Vec<u16>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(Vec::new()))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Proto {
    H1,
    H2,
}

impl Proto {
    fn name(self) -> &'static str {
        match self {
            Proto::H1 => PROTO_H1,
            Proto::H2 => PROTO_H2,
        }
    }
}

struct H2Entry {
    id: u64,
    addr: SocketAddr,
    tx: http2::SendRequest<ReqBody>,
    /// Streams in flight.
    active: Arc<AtomicUsize>,
    /// Requests ever started on it (`> 0` = the next one is a reuse).
    started: u64,
    last_used: Instant,
}

struct IdleH1 {
    id: u64,
    addr: SocketAddr,
    tx: http1::SendRequest<ReqBody>,
    task: tokio::task::AbortHandle,
    since: Instant,
}

struct Origin {
    h2: Option<H2Entry>,
    idle: Vec<IdleH1>,
    /// HTTP/1.1 connections open or being dialled (idle ones included).
    h1_open: usize,
    /// A dial whose protocol is not known yet is in flight; others wait.
    probing: bool,
    /// What the last connection to this origin negotiated.
    learned: Option<Proto>,
    notify: Arc<Notify>,
}

impl Default for Origin {
    fn default() -> Self {
        Origin { h2: None, idle: Vec::new(), h1_open: 0, probing: false, learned: None, notify: Arc::new(Notify::new()) }
    }
}

fn lock() -> MutexGuard<'static, HashMap<Key, Origin>> {
    static POOL: OnceLock<Mutex<HashMap<Key, Origin>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap_or_else(|p| p.into_inner())
}

fn wake(key: &Key) {
    let n = lock().get(key).map(|o| Arc::clone(&o.notify));
    if let Some(n) = n {
        n.notify_waiters();
    }
}

/// An HTTP/1.1 connection checked out of the pool. Dropping it without
/// [`release_h1`] CLOSES the connection and frees its slot — the path an
/// abort, a stall or any error takes, so a connection in an unknown state is
/// never reused.
struct H1Lease {
    key: Key,
    id: u64,
    addr: SocketAddr,
    tx: Option<http1::SendRequest<ReqBody>>,
    task: tokio::task::AbortHandle,
    armed: bool,
}

impl Drop for H1Lease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.task.abort();
        drop(self.tx.take());
        if let Some(o) = lock().get_mut(&self.key) {
            o.h1_open = o.h1_open.saturating_sub(1);
        }
        wake(&self.key);
    }
}

/// A stream's hold on the origin's HTTP/2 connection.
struct H2Lease {
    key: Key,
    id: u64,
    addr: SocketAddr,
    tx: http2::SendRequest<ReqBody>,
    active: Arc<AtomicUsize>,
}

impl Drop for H2Lease {
    fn drop(&mut self) {
        if self.active.fetch_sub(1, Ordering::SeqCst) == 1 {
            if let Some(h) = lock().get_mut(&self.key).and_then(|o| o.h2.as_mut()) {
                if h.id == self.id {
                    h.last_used = Instant::now();
                }
            }
        }
    }
}

enum Lease {
    H1(H1Lease),
    H2(H2Lease),
}

impl Lease {
    fn id(&self) -> u64 {
        match self {
            Lease::H1(l) => l.id,
            Lease::H2(l) => l.id,
        }
    }
    fn proto(&self) -> Proto {
        match self {
            Lease::H1(_) => Proto::H1,
            Lease::H2(_) => Proto::H2,
        }
    }
    fn addr(&self) -> SocketAddr {
        match self {
            Lease::H1(l) => l.addr,
            Lease::H2(l) => l.addr,
        }
    }
}

/// A dial slot taken under the pool lock. Dropped without being consumed
/// (the dial failed, or the task went away) it gives the slot back.
struct Reservation {
    key: Key,
    probe: bool,
    armed: bool,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(o) = lock().get_mut(&self.key) {
            o.h1_open = o.h1_open.saturating_sub(1);
            if self.probe {
                o.probing = false;
            }
        }
        wake(&self.key);
    }
}

enum Step {
    Got(Lease, bool),
    Dial(Reservation),
    Wait(Pin<Box<tokio::sync::futures::OwnedNotified>>),
}

/// A connection for `key`: the h2 one, an idle HTTP/1.1 one, or a new one.
/// `avoid` (a retry) skips the connection that just failed and every idle
/// HTTP/1.1 connection — they may share its fate (a restarted server).
async fn checkout(key: &Key, cancel: &Cancel, connect_timeout: Duration, avoid: Option<u64>) -> Result<(Lease, bool), HttpError> {
    loop {
        let step = {
            let mut p = lock();
            let o = p.entry(key.clone()).or_default();
            let mut step = None;
            if o.h2.as_ref().is_some_and(|h| h.tx.is_closed()) {
                o.h2 = None;
            }
            if let Some(h) = o.h2.as_mut() {
                if Some(h.id) != avoid {
                    let reused = h.started > 0;
                    h.started += 1;
                    h.active.fetch_add(1, Ordering::SeqCst);
                    let lease = H2Lease { key: key.clone(), id: h.id, addr: h.addr, tx: h.tx.clone(), active: Arc::clone(&h.active) };
                    step = Some(Step::Got(Lease::H2(lease), reused));
                }
            }
            if step.is_none() && avoid.is_none() {
                while let Some(c) = o.idle.pop() {
                    if c.tx.is_closed() || !c.tx.is_ready() || c.since.elapsed() >= H1_IDLE {
                        c.task.abort();
                        o.h1_open = o.h1_open.saturating_sub(1);
                        continue;
                    }
                    let lease = H1Lease { key: key.clone(), id: c.id, addr: c.addr, tx: Some(c.tx), task: c.task, armed: true };
                    step = Some(Step::Got(Lease::H1(lease), true));
                    break;
                }
            }
            match step {
                Some(s) => s,
                None => {
                    let probe = o.learned != Some(Proto::H1);
                    if o.h1_open < MAX_PER_HOST && !(probe && o.probing) {
                        o.h1_open += 1;
                        if probe {
                            o.probing = true;
                        }
                        Step::Dial(Reservation { key: key.clone(), probe, armed: true })
                    } else {
                        // Registered while the lock is held: a release that
                        // happens right after it is not missed.
                        let mut n = Box::pin(Arc::clone(&o.notify).notified_owned());
                        n.as_mut().enable();
                        Step::Wait(n)
                    }
                }
            }
        };
        match step {
            Step::Got(lease, reused) => return Ok((lease, reused)),
            Step::Dial(reservation) => return dial_into_pool(key, reservation, cancel, connect_timeout).await,
            Step::Wait(n) => {
                tokio::select! {
                    _ = n => {}
                    _ = cancel.aborted() => return Err(aborted()),
                    // Belt and braces: re-examine the pool now and then.
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
        }
    }
}

async fn dial_into_pool(key: &Key, mut slot: Reservation, cancel: &Cancel, connect_timeout: Duration) -> Result<(Lease, bool), HttpError> {
    let dialed = dial(key, cancel, connect_timeout).await?;
    slot.armed = false;
    let mut p = lock();
    let o = p.entry(key.clone()).or_default();
    if slot.probe {
        o.probing = false;
    }
    let notify = Arc::clone(&o.notify);
    let lease = match dialed {
        Dialed::H2 { id, tx, addr } => {
            o.h1_open = o.h1_open.saturating_sub(1);
            o.learned = Some(Proto::H2);
            let active = Arc::new(AtomicUsize::new(1));
            // Replaces a connection that is going away, if any; its in-flight
            // streams keep their own handles.
            o.h2 = Some(H2Entry { id, addr, tx: tx.clone(), active: Arc::clone(&active), started: 1, last_used: Instant::now() });
            Lease::H2(H2Lease { key: key.clone(), id, addr, tx, active })
        }
        Dialed::H1 { id, tx, task, addr } => {
            o.learned = Some(Proto::H1);
            Lease::H1(H1Lease { key: key.clone(), id, addr, tx: Some(tx), task, armed: true })
        }
    };
    drop(p);
    notify.notify_waiters();
    Ok((lease, false))
}

/// Back into the pool once hyper says the connection can take another
/// request; closed otherwise. Awaited BEFORE the answer is delivered, so the
/// caller's next request finds the connection idle (hyper's connection task
/// needs one turn after the body's last frame; the wait is that turn).
async fn release_h1(mut lease: H1Lease) {
    let ready = match lease.tx.as_mut() {
        Some(tx) => matches!(tokio::time::timeout(Duration::from_millis(250), tx.ready()).await, Ok(Ok(()))),
        None => false,
    };
    if !ready {
        net_log!("h1 #{} {}:{} not reusable, closed", lease.id, lease.key.host, lease.key.port);
        return; // `lease` drops: closed, slot freed
    }
    let tx = lease.tx.take().expect("checked above");
    lease.armed = false;
    let idle = IdleH1 { id: lease.id, addr: lease.addr, tx, task: lease.task.clone(), since: Instant::now() };
    match lock().get_mut(&lease.key) {
        Some(o) => o.idle.push(idle),
        None => idle.task.abort(),
    }
    wake(&lease.key);
}

/// The h2 connection `id` ended: out of the pool.
fn forget_h2(key: &Key, id: u64) {
    if let Some(o) = lock().get_mut(key) {
        if o.h2.as_ref().is_some_and(|h| h.id == id) {
            o.h2 = None;
        }
    }
    wake(key);
}

async fn reaper() {
    loop {
        tokio::time::sleep(REAP_EVERY).await;
        let mut woken = Vec::new();
        {
            let mut p = lock();
            for (key, o) in p.iter_mut() {
                let before = o.idle.len();
                o.idle.retain(|c| {
                    let keep = !c.tx.is_closed() && c.since.elapsed() < H1_IDLE;
                    if !keep {
                        c.task.abort();
                    }
                    keep
                });
                let closed = before - o.idle.len();
                o.h1_open = o.h1_open.saturating_sub(closed);
                let h2_idle = o.h2.as_ref().is_some_and(|h| {
                    h.tx.is_closed() || (h.active.load(Ordering::SeqCst) == 0 && h.last_used.elapsed() >= H2_IDLE)
                });
                if h2_idle {
                    // Dropping our handle lets hyper close it (GOAWAY).
                    if let Some(h) = o.h2.take() {
                        net_log!("h2 #{} {}:{} idle, dropped", h.id, key.host, key.port);
                    }
                }
                if closed > 0 || h2_idle {
                    woken.push(Arc::clone(&o.notify));
                }
            }
        }
        for n in woken {
            n.notify_waiters();
        }
    }
}

// ---------------------------------------------------------------------------
// Dialling
// ---------------------------------------------------------------------------

trait Io: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Io for T {}

enum Dialed {
    H1 { id: u64, tx: http1::SendRequest<ReqBody>, task: tokio::task::AbortHandle, addr: SocketAddr },
    H2 { id: u64, tx: http2::SendRequest<ReqBody>, addr: SocketAddr },
}

/// Resolve, connect (Happy Eyeballs), TLS with ALPN, protocol handshake — one
/// `connect_timeout` budget for all of it, abortable throughout.
async fn dial(key: &Key, cancel: &Cancel, connect_timeout: Duration) -> Result<Dialed, HttpError> {
    let stage = AtomicU8::new(0);
    tokio::select! {
        r = tokio::time::timeout(connect_timeout, dial_steps(key, &stage)) => match r {
            Ok(r) => r,
            Err(_) => Err(network(match stage.load(Ordering::SeqCst) {
                0 => format!("could not resolve {}: timed out", key.host),
                1 => format!("could not connect to {}:{}: timed out", key.host, key.port),
                2 => format!("TLS handshake with {} timed out", key.host),
                _ => format!("HTTP/2 handshake with {} timed out", key.host),
            })),
        },
        _ = cancel.aborted() => Err(aborted()),
    }
}

async fn dial_steps(key: &Key, stage: &AtomicU8) -> Result<Dialed, HttpError> {
    let t0 = Instant::now();
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((key.host.as_str(), key.port))
        .await
        .map_err(|e| network(format!("could not resolve {}: {e}", key.host)))?
        .collect();
    if addrs.is_empty() {
        return Err(network(format!("could not resolve {}", key.host)));
    }
    stage.store(1, Ordering::SeqCst);
    let (tcp, addr) = race_connect(&key.host, key.port, interleave(addrs)).await?;
    let _ = tcp.set_nodelay(true);

    let (io, proto): (Box<dyn Io>, Proto) = if key.https {
        stage.store(2, Ordering::SeqCst);
        let mut b = tls_builder();
        b.request_alpns(&["h2", "http/1.1"]);
        let connector = b.build().map_err(|e| network(format!("TLS setup failed: {e}")))?;
        let tls = tokio_native_tls::TlsConnector::from(connector)
            .connect(&key.host, tcp)
            .await
            .map_err(|e| network(format!("TLS handshake with {} failed: {e}", key.host)))?;
        let alpn = tls.get_ref().negotiated_alpn().ok().flatten();
        let proto = if alpn.as_deref() == Some(b"h2") { Proto::H2 } else { Proto::H1 };
        (Box::new(tls), proto)
    } else {
        (Box::new(tcp), if key.h2c() { Proto::H2 } else { Proto::H1 })
    };

    stage.store(3, Ordering::SeqCst);
    let id = next_connection_id();
    let dialed = match proto {
        Proto::H2 => {
            let (tx, conn) = http2::Builder::new(TokioExecutor::new())
                .timer(TokioTimer::new())
                .keep_alive_interval(PING_INTERVAL)
                .keep_alive_timeout(PING_TIMEOUT)
                .keep_alive_while_idle(true)
                .max_header_list_size(HEAD_LIMIT as u32)
                .handshake::<_, ReqBody>(TokioIo::new(io))
                .await
                .map_err(|e| network(format!("HTTP/2 handshake with {} failed: {e}", key.host)))?;
            let k = key.clone();
            tokio::spawn(async move {
                let r = conn.await;
                net_log!(
                    "h2 #{id} {}:{} closed{}",
                    k.host,
                    k.port,
                    r.err().map(|e| format!(": {e}")).unwrap_or_default()
                );
                forget_h2(&k, id);
            });
            H2_OPENED.fetch_add(1, Ordering::SeqCst);
            Dialed::H2 { id, tx, addr }
        }
        Proto::H1 => {
            let (tx, conn) = http1::Builder::new()
                .title_case_headers(true)
                .max_buf_size(HEAD_LIMIT)
                .max_headers(1000)
                .handshake::<_, ReqBody>(TokioIo::new(io))
                .await
                .map_err(|e| network(format!("HTTP/1.1 handshake with {} failed: {e}", key.host)))?;
            let task = tokio::spawn(async move {
                let _ = conn.await;
            })
            .abort_handle();
            H1_OPENED.fetch_add(1, Ordering::SeqCst);
            Dialed::H1 { id, tx, task, addr }
        }
    };
    net_log!(
        "dial {}://{}:{} -> {} #{id} via {addr} in {} ms",
        if key.https { "https" } else { "http" },
        key.host,
        key.port,
        proto.name(),
        t0.elapsed().as_millis()
    );
    Ok(dialed)
}

/// RFC 8305 connection racing: attempts start [`ATTEMPT_DELAY`] apart (or as
/// soon as the previous one fails); the first to connect wins and the rest are
/// dropped.
async fn race_connect(host: &str, port: u16, addrs: Vec<SocketAddr>) -> Result<(tokio::net::TcpStream, SocketAddr), HttpError> {
    type Attempt = (SocketAddr, std::io::Result<tokio::net::TcpStream>);
    let mut set: tokio::task::JoinSet<Attempt> = tokio::task::JoinSet::new();
    let mut next = addrs.into_iter();
    let mut last: Option<std::io::Error> = None;
    let launch = |set: &mut tokio::task::JoinSet<Attempt>, a: SocketAddr| {
        set.spawn(async move { (a, tokio::net::TcpStream::connect(a).await) });
    };
    if let Some(a) = next.next() {
        launch(&mut set, a);
    }
    while !set.is_empty() {
        let more = next.len() > 0;
        tokio::select! {
            joined = set.join_next() => match joined {
                Some(Ok((a, Ok(s)))) => return Ok((s, a)),
                Some(Ok((_, Err(e)))) => {
                    last = Some(e);
                    if let Some(a) = next.next() {
                        launch(&mut set, a);
                    }
                }
                Some(Err(_)) | None => {}
            },
            _ = tokio::time::sleep(ATTEMPT_DELAY), if more => {
                if let Some(a) = next.next() {
                    launch(&mut set, a);
                }
            }
        }
    }
    Err(network(match last {
        Some(e) => format!("could not connect to {host}:{port}: {e}"),
        None => format!("could not connect to {host}:{port}"),
    }))
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// "Something moved" — the stall budget's clock.
struct Progress {
    base: Instant,
    last_ms: AtomicU64,
}

impl Progress {
    fn new() -> Arc<Progress> {
        Arc::new(Progress { base: Instant::now(), last_ms: AtomicU64::new(0) })
    }
    fn touch(&self) {
        self.last_ms.store(self.base.elapsed().as_millis() as u64, Ordering::SeqCst);
    }
    fn last(&self) -> Instant {
        self.base + Duration::from_millis(self.last_ms.load(Ordering::SeqCst))
    }
}

/// Resolves once nothing has moved for `budget`.
async fn stalled(p: &Progress, budget: Duration) {
    loop {
        let at = p.last() + budget;
        if Instant::now() >= at {
            return;
        }
        tokio::time::sleep_until(at.into()).await;
    }
}

/// A request body that reports progress as the connection takes it: hyper
/// pulls a frame only when it can write it (HTTP/1.1: the socket accepts more;
/// HTTP/2: the stream and connection windows allow more), so "handed over" is
/// "written", give or take one frame of buffering.
pub(super) struct ReqBody {
    data: Bytes,
    progress: Arc<Progress>,
    observer: Option<Observer>,
    sent: u64,
    total: u64,
}

impl Body for ReqBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        if self.data.is_empty() {
            return Poll::Ready(None);
        }
        let n = self.data.len().min(BODY_FRAME);
        let chunk = self.data.split_to(n);
        self.progress.touch();
        self.sent += n as u64;
        if let Some(o) = &self.observer {
            o.0.upload(self.sent, self.total);
        }
        Poll::Ready(Some(Ok(Frame::data(chunk))))
    }

    fn is_end_stream(&self) -> bool {
        self.data.is_empty()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.data.len() as u64)
    }
}

/// A hop's request, validated once, before any connection is touched (a
/// refused header must not cost a pooled connection).
struct Prepared {
    method: http::Method,
    /// origin-form (`/path?query`): what HTTP/1.1 puts on the request line.
    origin_form: http::Uri,
    /// absolute form: HTTP/2's `:scheme` + `:authority` + `:path`.
    absolute: http::Uri,
    /// `Host` for HTTP/1.1 (HTTP/2 carries it as `:authority`).
    host: http::HeaderValue,
    headers: Vec<(http::HeaderName, http::HeaderValue)>,
    body: Option<Bytes>,
    needs_length: bool,
}

/// `http::Uri` is stricter than the URL Standard's serializer about a few
/// printable bytes (`|`, `^`, a raw `"` in a query...); those are
/// percent-encoded rather than refused, which is what goes on the wire from a
/// browser too.
fn to_uri(s: &str) -> Result<http::Uri, HttpError> {
    if let Ok(u) = s.parse::<http::Uri>() {
        return Ok(u);
    }
    let mut out = String::with_capacity(s.len() + 16);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~:/?#[]@!$&'()*+,;=%".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out.parse::<http::Uri>().map_err(|e| HttpError::refused(format!("invalid request URL {s:?}: {e}")))
}

fn prepare(u: &url::Url, method: &str, headers: &[(String, String)], body: Option<&[u8]>) -> Result<Prepared, HttpError> {
    let m = http::Method::from_bytes(method.as_bytes())
        .map_err(|_| HttpError::refused(format!("invalid method {method:?}")))?;
    let mut target = u.path().to_string();
    if target.is_empty() {
        target.push('/');
    }
    if let Some(q) = u.query() {
        target.push('?');
        target.push_str(q);
    }
    let host = u.host_str().unwrap_or("");
    let authority = match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    };
    let origin_form = to_uri(&target)?;
    let absolute = to_uri(&format!("{}://{authority}{target}", u.scheme()))?;
    let host = http::HeaderValue::from_str(&authority).map_err(|_| HttpError::refused(format!("invalid host {authority:?}")))?;

    let mut out = Vec::with_capacity(headers.len() + 4);
    let mut seen: Vec<String> = Vec::new();
    for (k, v) in headers {
        validate_header(k, v)?;
        let lower = k.to_ascii_lowercase();
        if is_transport_header(&lower) {
            continue;
        }
        let name = http::HeaderName::from_bytes(k.as_bytes())
            .map_err(|_| HttpError::refused(format!("invalid header name: {k:?}")))?;
        let value = http::HeaderValue::from_bytes(v.as_bytes())
            .map_err(|_| HttpError::refused(format!("invalid header value for {k:?}")))?;
        out.push((name, value));
        seen.push(lower);
    }
    let has = |n: &str| seen.iter().any(|s| s == n);
    if !has("user-agent") {
        if let Ok(v) = http::HeaderValue::from_str(&super::super::navigator::user_agent()) {
            out.push((http::header::USER_AGENT, v));
        }
    }
    if !has("accept") {
        out.push((http::header::ACCEPT, http::HeaderValue::from_static("*/*")));
    }
    if !has("accept-encoding") {
        out.push((http::header::ACCEPT_ENCODING, http::HeaderValue::from_static("gzip, deflate")));
    }
    if !has("accept-language") {
        if let Ok(v) = http::HeaderValue::from_str(&super::super::navigator::accept_language()) {
            out.push((http::header::ACCEPT_LANGUAGE, v));
        }
    }
    Ok(Prepared {
        method: m,
        origin_form,
        absolute,
        host,
        headers: out,
        body: body.map(Bytes::copy_from_slice),
        needs_length: needs_length(method, body),
    })
}

impl Prepared {
    fn request(&self, proto: Proto, progress: &Arc<Progress>, observer: Option<&Observer>) -> hyper::Request<ReqBody> {
        let data = self.body.clone().unwrap_or_default();
        let total = data.len() as u64;
        let mut r = hyper::Request::new(ReqBody {
            data,
            progress: Arc::clone(progress),
            observer: observer.cloned(),
            sent: 0,
            total,
        });
        *r.method_mut() = self.method.clone();
        match proto {
            Proto::H1 => {
                *r.uri_mut() = self.origin_form.clone();
                *r.version_mut() = http::Version::HTTP_11;
                let h = r.headers_mut();
                h.insert(http::header::HOST, self.host.clone());
                // HTTP/1.1 is persistent by default; saying so explicitly is
                // what Chromium does, and what an HTTP/1.0 hop needs.
                h.insert(http::header::CONNECTION, http::HeaderValue::from_static("keep-alive"));
            }
            Proto::H2 => {
                *r.uri_mut() = self.absolute.clone();
                *r.version_mut() = http::Version::HTTP_2;
            }
        }
        let h = r.headers_mut();
        for (k, v) in &self.headers {
            h.append(k.clone(), v.clone());
        }
        if self.needs_length {
            let n = self.body.as_ref().map_or(0, |b| b.len());
            h.insert(http::header::CONTENT_LENGTH, http::HeaderValue::from(n));
        }
        r
    }
}

/// The run of one `fetch`: redirects, the jar per hop, the HTTP cache per hop
/// ([`super::cache`]), the pool per hop.
async fn run(req: &Request, cancel: &Cancel) -> Result<Response, HttpError> {
    use super::cache::{self, Action, Outcome, Settled};
    let mut hops = Hops::new(req);
    loop {
        cancel.check()?;
        let hop = hops.next()?;
        // A lookup reads a file: on the blocking pool, never on this thread.
        let planned = if cache::reads_disk(&hops.method, hop.url.as_str(), &hop.headers, req.cache) {
            let (m, u, h, mode) = (hops.method.clone(), hop.url.to_string(), hop.headers.clone(), req.cache);
            tokio::task::spawn_blocking(move || cache::plan(&m, &u, &h, mode))
                .await
                .map_err(|e| network(format!("the HTTP cache lookup failed: {e}")))?
        } else {
            cache::plan(&hops.method, hop.url.as_str(), &hop.headers, req.cache)
        };
        let (action, ctx) = planned.into_parts();
        let (validating, conditional) = match action {
            Action::Fail(e) => return Err(e),
            Action::Serve { entry, revalidate } => {
                if revalidate {
                    cache::revalidate_in_background(req, hop.url.as_str());
                }
                if hops.follow(entry.status, &entry.headers)? {
                    continue;
                }
                return from_cache(req, &hops, entry, Outcome::Hit, None).await;
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
        let key = Key::of(&hop.url)?;
        let prepared = prepare(&hop.url, &hops.method, &headers, hops.body.as_deref())?;
        let seen = Seen { url: &hops.current, redirected: hops.redirected };
        let a = exchange(&key, &hop.url, &hops.method, &prepared, sent, cancel, seen).await?;
        match cache::settle(ctx, validating, a.status, &a.reason, &a.headers, &a.body, true) {
            Settled::Refreshed(entry) => {
                hops.store_cookies(&hop, &a.headers);
                if hops.follow(entry.status, &entry.headers)? {
                    continue;
                }
                let net = (a.proto.name(), a.reused, a.id, a.addr);
                return from_cache(req, &hops, entry, Outcome::Revalidated, Some(net)).await;
            }
            Settled::Network(outcome) => {
                if hops.after(&hop, a.status, &a.headers)? {
                    continue;
                }
                if asked {
                    let (url, r) = (&hops.current, hops.redirected);
                    cache::observe(req, url, r, a.proto.name(), a.status, &a.reason, &a.headers, &a.body);
                }
                let body = decode_off_thread(header(&a.headers, "content-encoding"), a.body).await?;
                return Ok(Response {
                    status: a.status,
                    status_text: a.reason,
                    headers: a.headers,
                    url: hops.current,
                    redirected: hops.redirected,
                    body,
                    protocol: a.proto.name(),
                    reused: a.reused,
                    connection_id: a.id,
                    remote_addr: Some(a.addr),
                    cache: outcome,
                });
            }
        }
    }
}

/// An answer the cache served, or a 304 refreshed (`net`: the connection that
/// carried the 304).
async fn from_cache(
    req: &Request,
    hops: &Hops<'_>,
    entry: super::cache::Entry,
    outcome: super::cache::Outcome,
    net: Option<(&'static str, bool, u64, SocketAddr)>,
) -> Result<Response, HttpError> {
    let (url, redirected) = (hops.current.clone(), hops.redirected);
    let protocol = net.map_or("", |n| n.0);
    super::cache::observe(req, &url, redirected, protocol, entry.status, &entry.reason, &entry.headers, &entry.body);
    let body = if entry.coded {
        decode_off_thread(header(&entry.headers, "content-encoding"), entry.body).await?
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
        protocol,
        reused: net.is_some_and(|n| n.1),
        connection_id: net.map_or(0, |n| n.2),
        remote_addr: net.map(|n| n.3),
        cache: outcome,
    })
}

/// Decoding a big body is CPU work; keep it off the runtime's one thread.
async fn decode_off_thread(enc: Option<&str>, body: Vec<u8>) -> Result<Vec<u8>, HttpError> {
    if enc.is_some() && body.len() > 256 * 1024 {
        let enc = enc.map(str::to_string);
        tokio::task::spawn_blocking(move || decode_content_encoding(enc.as_deref(), body))
            .await
            .map_err(|e| network(format!("decoding the body failed: {e}")))?
    } else {
        decode_content_encoding(enc, body)
    }
}

struct Answer {
    status: u16,
    reason: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    proto: Proto,
    reused: bool,
    id: u64,
    addr: SocketAddr,
}

struct Failure {
    error: HttpError,
    /// The server provably never saw the request.
    unsent: bool,
    /// Some of the response had arrived (never retried).
    answered: bool,
}

impl Failure {
    fn of(error: HttpError) -> Failure {
        Failure { error, unsent: false, answered: false }
    }
}

fn idempotent(method: &str) -> bool {
    matches!(method, "GET" | "HEAD" | "OPTIONS" | "TRACE" | "PUT" | "DELETE")
}

/// One hop, with the pool's retry rules.
/// What an [`Observe`]r is told about the hop being made.
#[derive(Clone, Copy)]
struct Seen<'a> {
    url: &'a str,
    redirected: bool,
}

async fn exchange(
    key: &Key,
    u: &url::Url,
    method: &str,
    prepared: &Prepared,
    req: &Request,
    cancel: &Cancel,
    seen: Seen<'_>,
) -> Result<Answer, HttpError> {
    let mut avoid: Option<u64> = None;
    let mut retries = 0u32;
    let mut reuse_retry_spent = false;
    loop {
        cancel.check()?;
        let (lease, reused) = checkout(key, cancel, req.connect_timeout, avoid).await?;
        REQUESTS.fetch_add(1, Ordering::SeqCst);
        if reused {
            REUSED.fetch_add(1, Ordering::SeqCst);
        }
        let id = lease.id();
        let addr = lease.addr();
        let proto = lease.proto();
        match attempt(lease, prepared, method, req, cancel, seen).await {
            Ok((status, reason, headers, body)) => {
                return Ok(Answer { status, reason, headers, body, proto, reused, id, addr });
            }
            Err(f) => {
                let reuse_rule = !f.unsent && !f.answered && reused && idempotent(method) && !reuse_retry_spent;
                if (f.unsent || reuse_rule) && retries < MAX_RETRIES && !cancel.is_aborted() {
                    if reuse_rule {
                        reuse_retry_spent = true;
                    }
                    retries += 1;
                    RETRIES.fetch_add(1, Ordering::SeqCst);
                    net_log!(
                        "retry {method} {u} after {} #{id}{}: {}",
                        proto.name(),
                        if f.unsent { " (never sent)" } else { " (reused connection died)" },
                        f.error.message
                    );
                    avoid = Some(id);
                    continue;
                }
                return Err(f.error);
            }
        }
    }
}

fn h2_error(e: &hyper::Error) -> Option<&h2::Error> {
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(s) = src {
        if let Some(h) = s.downcast_ref::<h2::Error>() {
            return Some(h);
        }
        src = s.source();
    }
    None
}

/// The server never started this stream: refused, or above a GOAWAY's last
/// stream id (either way RFC 9113 §8.7 says it is safe to retry).
fn h2_never_started(e: &hyper::Error) -> bool {
    match h2_error(e) {
        Some(h) => {
            (h.is_go_away() && h.is_remote()) || (h.is_reset() && h.is_remote() && h.reason() == Some(h2::Reason::REFUSED_STREAM))
        }
        None => false,
    }
}

/// What a failure before the response head says, in the words the blocking
/// transport always used where it has an equivalent.
fn head_error(e: &hyper::Error, proto: Proto) -> HttpError {
    if let Some(h) = h2_error(e) {
        if h.is_go_away() && h.is_remote() {
            return network(format!("the server closed the HTTP/2 connection (GOAWAY {:?})", h.reason()));
        }
        if h.is_reset() {
            return network(format!("the server reset the HTTP/2 stream ({:?})", h.reason()));
        }
        if h.is_io() {
            return network(format!("response read failed: {h}"));
        }
        return network(format!("HTTP/2 error: {h}"));
    }
    if e.is_timeout() {
        // hyper's own timer: the h2 keepalive PING went unanswered.
        return network("response read timed out: the connection stopped answering (keepalive PING)");
    }
    if e.is_incomplete_message() {
        return network("the server closed the connection without a response");
    }
    // (`is_parse_too_large` is a server-feature API; the message is stable.)
    if e.is_parse() && e.to_string().contains("too large") {
        return HttpError::protocol(format!("response head exceeds {HEAD_LIMIT} bytes"));
    }
    if e.is_parse() {
        return HttpError::protocol(format!("malformed {} response: {e}", proto.name()));
    }
    if e.is_canceled() || e.is_closed() {
        return network("the connection closed before the request was sent");
    }
    network(format!("response read failed: {e}"))
}

/// The `io::Error` at the bottom of a hyper error, if there is one.
fn io_kind(e: &hyper::Error) -> Option<std::io::ErrorKind> {
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(s) = src {
        if let Some(io) = s.downcast_ref::<std::io::Error>() {
            return Some(io.kind());
        }
        src = s.source();
    }
    None
}

/// A body that ended early is a PROTOCOL error (a short document with a 200
/// is the defect this module exists to prevent), named as the blocking
/// transport always named it.
fn body_error(e: &hyper::Error, got: usize, declared: Option<u64>) -> HttpError {
    let ended_early = e.is_incomplete_message()
        || io_kind(e) == Some(std::io::ErrorKind::UnexpectedEof)
        || h2_error(e).is_some_and(|h| h.is_io());
    if ended_early {
        return match declared {
            Some(n) => HttpError::protocol(format!("connection closed after {got} of {n} declared body bytes")),
            None => HttpError::protocol("connection closed inside the response body"),
        };
    }
    if e.is_timeout() {
        return network("body read failed: the connection stopped answering (keepalive PING)");
    }
    if let Some(h) = h2_error(e) {
        if h.is_reset() {
            return network(format!("the server reset the HTTP/2 stream mid-body ({:?})", h.reason()));
        }
    }
    network(format!("body read failed: {e}"))
}

type Parts = (u16, String, Vec<(String, String)>, Vec<u8>);

/// Sends on the leased connection and reads the whole answer. Abort or a
/// stall drops the in-flight futures: an HTTP/2 stream is reset, an HTTP/1.1
/// connection is closed (its lease drops without being released).
async fn attempt(
    lease: Lease,
    prepared: &Prepared,
    method: &str,
    req: &Request,
    cancel: &Cancel,
    seen: Seen<'_>,
) -> Result<Parts, Failure> {
    let progress = Progress::new();
    let proto = lease.proto();
    let request = prepared.request(proto, &progress, req.observer.as_ref());

    // The head.
    let (resp, lease) = match lease {
        Lease::H2(mut l) => {
            let sent = l.tx.try_send_request(request);
            let r = tokio::select! {
                r = sent => r,
                _ = stalled(&progress, req.io_timeout) => return Err(Failure::of(network("response read timed out"))),
                _ = cancel.aborted() => return Err(Failure::of(aborted())),
            };
            match r {
                Ok(resp) => (resp, Lease::H2(l)),
                Err(mut e) => {
                    let unsent = e.take_message().is_some() || h2_never_started(e.error());
                    return Err(Failure { error: head_error(e.error(), proto), unsent, answered: false });
                }
            }
        }
        Lease::H1(mut l) => {
            let Some(tx) = l.tx.as_mut() else { return Err(Failure::of(network("internal: a leased connection had no sender"))) };
            let sent = tx.try_send_request(request);
            let r = tokio::select! {
                r = sent => r,
                _ = stalled(&progress, req.io_timeout) => return Err(Failure::of(network("response read timed out"))),
                _ = cancel.aborted() => return Err(Failure::of(aborted())),
            };
            match r {
                Ok(resp) => (resp, Lease::H1(l)),
                Err(mut e) => {
                    let unsent = e.take_message().is_some();
                    return Err(Failure { error: head_error(e.error(), proto), unsent, answered: false });
                }
            }
        }
    };
    progress.touch();

    let status = resp.status().as_u16();
    // HTTP/2 has no reason phrase, and a browser's `statusText` is "" there.
    let reason = match proto {
        Proto::H2 => String::new(),
        Proto::H1 => match resp.extensions().get::<hyper::ext::ReasonPhrase>() {
            Some(r) => String::from_utf8_lossy(r.as_bytes()).into_owned(),
            None => resp.status().canonical_reason().unwrap_or("").to_string(),
        },
    };
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect();
    let declared: Option<u64> = if method.eq_ignore_ascii_case("HEAD") {
        None
    } else {
        header(&headers, "content-length").and_then(|v| v.trim().parse().ok())
    };
    if let Some(n) = declared {
        if n > MAX_BODY as u64 {
            return Err(Failure {
                error: HttpError::refused(format!("response body of {n} bytes exceeds the {MAX_BODY}-byte limit")),
                unsent: false,
                answered: true,
            });
        }
    }
    // Only the request's answer is observed, not a redirect on the way to it.
    let observer = req.observer.as_ref().filter(|_| is_final(status, &headers));
    if let Some(o) = observer {
        o.0.head(&ProgressHead {
            status,
            status_text: &reason,
            headers: &headers,
            url: seen.url,
            redirected: seen.redirected,
            protocol: proto.name(),
        });
    }

    // The body.
    let streams = observer.is_some_and(|o| o.0.streams());
    let mut incoming = resp.into_body();
    let mut received = 0u64;
    let mut body: Vec<u8> = Vec::with_capacity(if streams { 0 } else { declared.unwrap_or(0).min(1 << 20) as usize });
    loop {
        let frame = tokio::select! {
            f = incoming.frame() => f,
            _ = stalled(&progress, req.io_timeout) => {
                return Err(Failure { error: network("body read failed: the connection stalled"), unsent: false, answered: true });
            }
            _ = cancel.aborted() => return Err(Failure { error: aborted(), unsent: false, answered: true }),
        };
        match frame {
            None => break,
            Some(Ok(f)) => {
                progress.touch();
                if let Ok(d) = f.into_data() {
                    received += d.len() as u64;
                    if streams {
                        if let Some(o) = observer {
                            o.0.body(&d, received, declared);
                        }
                        continue;
                    }
                    if body.len() + d.len() > MAX_BODY {
                        return Err(Failure {
                            error: HttpError::refused(format!("response body exceeds the {MAX_BODY}-byte limit")),
                            unsent: false,
                            answered: true,
                        });
                    }
                    body.extend_from_slice(&d);
                    if let Some(o) = observer {
                        o.0.body(&d, body.len() as u64, declared);
                    }
                }
            }
            Some(Err(e)) => {
                return Err(Failure { error: body_error(&e, received as usize, declared), unsent: false, answered: true });
            }
        }
    }
    drop(incoming);
    match lease {
        Lease::H1(l) => release_h1(l).await,
        Lease::H2(l) => drop(l),
    }
    Ok((status, reason, headers, body))
}

#[cfg(test)]
mod tests;
