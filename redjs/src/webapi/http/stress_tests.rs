//! Slow-server stress: the "lost bytes" experiment, kept as a regression test.
//!
//! The hypothesis (research/reports/m26/net-h2-pool.md §2): on Windows a
//! blocking `recv` that hits `SO_RCVTIMEO` cancels an overlapped operation and
//! leaves the socket in an "indeterminate state" (MSDN), so bytes arriving
//! around the cancel can be lost. A server that answers slowly drives many
//! such timeouts per request; if bytes vanish, a Content-Length or chunked
//! body either stalls (the missing bytes never come) or arrives wrong.
//!
//! Each request names a seed; the response's delays, size (1–5 KB), framing
//! and bytes are all derived from it, so the client can check the body it got
//! byte for byte. Ignored by default (each variant takes ~30 s):
//!
//!   cargo test --release … webapi::http::stress_tests -- --ignored --nocapture

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

pub(super) const TEST_P12: &[u8] = include_bytes!("testdata/localhost.p12");
pub(super) const TEST_P12_PASSWORD: &str = "redjs-test";

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Everything about one response, derived from its seed.
pub(super) struct Plan {
    pub head_delay_ms: u64,
    pub body: Vec<u8>,
    /// (piece length, delay before it in ms)
    pub pieces: Vec<(usize, u64)>,
    pub chunked: bool,
}

pub(super) fn plan(seed: u64, slow: bool) -> Plan {
    let mut s = seed.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ 0xA5A5;
    let len = 1024 + (splitmix(&mut s) % 4097) as usize;
    let body: Vec<u8> = (0..len).map(|_| splitmix(&mut s) as u8).collect();
    let head_delay_ms = if slow { 1000 + splitmix(&mut s) % 400 } else { 0 };
    let n = 1 + (splitmix(&mut s) % 4) as usize;
    let mut pieces = Vec::new();
    let mut left = len;
    for i in 0..n {
        let take = if i == n - 1 { left } else { (left / (n - i)).max(1) };
        let gap = if slow && i > 0 { splitmix(&mut s) % 250 } else { 0 };
        pieces.push((take, gap));
        left -= take;
    }
    Plan { head_delay_ms, body, pieces, chunked: seed % 2 == 1 }
}

pub(super) trait Duplex: std::io::Read + std::io::Write + Send {}
impl<T: std::io::Read + std::io::Write + Send> Duplex for T {}

#[derive(Default)]
pub(super) struct ServerStats {
    pub connections: AtomicUsize,
    pub requests: AtomicUsize,
}

/// Reads one request head (and discards its Content-Length body). `None` at EOF.
pub(super) fn read_request(s: &mut dyn Duplex) -> Option<String> {
    let mut acc = Vec::new();
    let mut b = [0u8; 4096];
    loop {
        if let Some(i) = acc.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&acc[..i]).into_owned();
            let len = head
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                .unwrap_or(0);
            let mut have = acc.len() - (i + 4);
            while have < len {
                match s.read(&mut b) {
                    Ok(0) | Err(_) => return None,
                    Ok(n) => have += n,
                }
            }
            return Some(head);
        }
        match s.read(&mut b) {
            Ok(0) | Err(_) => return None,
            Ok(n) => acc.extend_from_slice(&b[..n]),
        }
    }
}

/// A keep-alive capable HTTP/1.1 server answering `/slow/<seed>` (and
/// `/fast/<seed>`) from [`plan`]. Plaintext, or TLS over the test identity.
pub(super) fn spawn_server(tls: bool) -> (u16, Arc<ServerStats>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let stats = Arc::new(ServerStats::default());
    let st = Arc::clone(&stats);
    let acceptor = if tls {
        let id = native_tls::Identity::from_pkcs12(TEST_P12, TEST_P12_PASSWORD).unwrap();
        Some(Arc::new(native_tls::TlsAcceptor::new(id).unwrap()))
    } else {
        None
    };
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(sock) = sock else { return };
            st.connections.fetch_add(1, Ordering::SeqCst);
            let st = Arc::clone(&st);
            let acceptor = acceptor.clone();
            std::thread::spawn(move || {
                let _ = sock.set_nodelay(true);
                let mut s: Box<dyn Duplex> = match acceptor {
                    Some(a) => match a.accept(sock) {
                        Ok(t) => Box::new(t),
                        Err(_) => return,
                    },
                    None => Box::new(sock),
                };
                while let Some(head) = read_request(&mut *s) {
                    st.requests.fetch_add(1, Ordering::SeqCst);
                    let line = head.lines().next().unwrap_or("").to_string();
                    let path = line.split(' ').nth(1).unwrap_or("/");
                    let slow = path.starts_with("/slow/");
                    let seed: u64 = path.rsplit('/').next().and_then(|x| x.parse().ok()).unwrap_or(0);
                    let close = head.to_ascii_lowercase().contains("\r\nconnection: close");
                    let p = plan(seed, slow);
                    std::thread::sleep(Duration::from_millis(p.head_delay_ms));
                    let head = if p.chunked {
                        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_string()
                    } else {
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", p.body.len())
                    };
                    if s.write_all(head.as_bytes()).and_then(|_| s.flush()).is_err() {
                        return;
                    }
                    let mut at = 0;
                    for (len, gap) in &p.pieces {
                        std::thread::sleep(Duration::from_millis(*gap));
                        let piece = &p.body[at..at + len];
                        at += len;
                        let r = if p.chunked {
                            s.write_all(format!("{:x}\r\n", piece.len()).as_bytes())
                                .and_then(|_| s.write_all(piece))
                                .and_then(|_| s.write_all(b"\r\n"))
                        } else {
                            s.write_all(piece)
                        };
                        if r.and_then(|_| s.flush()).is_err() {
                            return;
                        }
                    }
                    if p.chunked && s.write_all(b"0\r\n\r\n").and_then(|_| s.flush()).is_err() {
                        return;
                    }
                    if close {
                        return;
                    }
                }
            });
        }
    });
    (port, stats)
}

pub(super) struct Outcome {
    pub ok: usize,
    pub mismatched: usize,
    pub errors: Vec<String>,
    pub elapsed: Duration,
}

/// `total` requests from `threads` client threads, spread over `ports`.
pub(super) fn drive(scheme: &str, ports: &[u16], total: usize, threads: usize, slow: bool, io_timeout: Duration) -> Outcome {
    let next = Arc::new(AtomicUsize::new(0));
    let ok = Arc::new(AtomicUsize::new(0));
    let bad = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(std::sync::Mutex::new(Vec::new()));
    let started = Instant::now();
    let mut joins = Vec::new();
    for t in 0..threads {
        let (next, ok, bad, errors) = (Arc::clone(&next), Arc::clone(&ok), Arc::clone(&bad), Arc::clone(&errors));
        let port = ports[t % ports.len()];
        let scheme = scheme.to_string();
        joins.push(std::thread::spawn(move || loop {
            let i = next.fetch_add(1, Ordering::SeqCst);
            if i >= total {
                return;
            }
            let seed = i as u64;
            let kind = if slow { "slow" } else { "fast" };
            let req = Request {
                method: "GET".into(),
                url: format!("{scheme}://localhost:{port}/{kind}/{seed}"),
                headers: vec![],
                body: None,
                redirect: Redirect::Follow,
                credentials: crate::webapi::cookies::Credentials::Omit,
                connect_timeout: Duration::from_secs(10),
                io_timeout,
                observer: None,
            };
            let t0 = Instant::now();
            match send(&req, &Cancel::default()) {
                Ok(r) if r.status == 200 && r.body == plan(seed, slow).body => {
                    ok.fetch_add(1, Ordering::SeqCst);
                }
                Ok(r) => {
                    bad.fetch_add(1, Ordering::SeqCst);
                    errors.lock().unwrap().push(format!(
                        "seed {seed}: status {} and {} body bytes, expected {}",
                        r.status,
                        r.body.len(),
                        plan(seed, slow).body.len()
                    ));
                }
                Err(e) => errors.lock().unwrap().push(format!("seed {seed} after {:?}: {}", t0.elapsed(), e.message)),
            }
        }));
    }
    for j in joins {
        j.join().unwrap();
    }
    let errors = std::mem::take(&mut *errors.lock().unwrap());
    Outcome { ok: ok.load(Ordering::SeqCst), mismatched: bad.load(Ordering::SeqCst), errors, elapsed: started.elapsed() }
}

fn run_variant(name: &str, tls: bool, slice_ms: u64) {
    TEST_POLL_MS.store(slice_ms, Ordering::SeqCst);
    let servers: Vec<_> = (0..4).map(|_| spawn_server(tls)).collect();
    let ports: Vec<u16> = servers.iter().map(|(p, _)| *p).collect();
    let connects_before = connections_opened();
    let out = drive(if tls { "https" } else { "http" }, &ports, 500, 24, true, Duration::from_secs(8));
    TEST_POLL_MS.store(0, Ordering::SeqCst);
    let conns: usize = servers.iter().map(|(_, s)| s.connections.load(Ordering::SeqCst)).sum();
    println!(
        "[stress] {name}: ok {} / 500, mismatched {}, errors {}, server connections {}, client connects {}, {:?}",
        out.ok,
        out.mismatched,
        out.errors.len(),
        conns,
        connections_opened() - connects_before,
        out.elapsed
    );
    for e in out.errors.iter().take(12) {
        println!("[stress]   {e}");
    }
    assert_eq!(out.ok, 500, "{name}: {} failed, first: {:?}", 500 - out.ok, out.errors.first());
}

#[test]
#[ignore = "slow (~30 s): the lost-bytes experiment"]
fn slow_responses_arrive_byte_exact_plain() {
    run_variant("plain, 200 ms slices", false, 0);
}

#[test]
#[ignore = "slow (~30 s): the lost-bytes experiment"]
fn slow_responses_arrive_byte_exact_tls() {
    run_variant("tls, 200 ms slices", true, 0);
}

#[test]
#[ignore = "slow (~30 s): the lost-bytes experiment, timeouts amplified 200x"]
fn slow_responses_arrive_byte_exact_plain_1ms_slices() {
    run_variant("plain, 1 ms slices", false, 1);
}

#[test]
#[ignore = "slow (~30 s): the lost-bytes experiment, timeouts amplified 200x"]
fn slow_responses_arrive_byte_exact_tls_1ms_slices() {
    run_variant("tls, 1 ms slices", true, 1);
}

// -- live, harmless: the discovery endpoint the client resolves at boot -------

const LIVE_URL: &str = "https://api.argon.gl/api/discovery/resolve?email=vn-probe%40example.invalid";

fn live_req() -> Request {
    Request {
        method: "GET".into(),
        url: LIVE_URL.into(),
        headers: vec![],
        body: None,
        redirect: Redirect::Follow,
        credentials: crate::webapi::cookies::Credentials::Omit,
        connect_timeout: Duration::from_secs(15),
        io_timeout: Duration::from_secs(20),
        observer: None,
    }
}

fn proto_of(r: &Response) -> String {
    format!("{} {}", r.protocol, if r.reused { "reused" } else { "new" })
}

/// Connections the transport under test has opened so far.
pub(super) fn connections_opened() -> u64 {
    #[cfg(feature = "net-h2")]
    {
        let s = super::pool::stats();
        s.h1_connections + s.h2_connections
    }
    #[cfg(not(feature = "net-h2"))]
    {
        CONNECTS.load(Ordering::SeqCst)
    }
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

fn report(label: &str, mut ms: Vec<f64>, errors: &[String], connects: u64, protos: &[String]) {
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut sorted: Vec<String> = protos.to_vec();
    sorted.sort();
    let mut kinds: Vec<String> = Vec::new();
    for p in &sorted {
        match kinds.last_mut() {
            Some(last) if last.starts_with(&format!("{p} x")) => {
                let n: usize = last.rsplit('x').next().unwrap().parse().unwrap();
                *last = format!("{p} x{}", n + 1);
            }
            _ => kinds.push(format!("{p} x1")),
        }
    }
    println!(
        "[live] {label}: ok {} err {} | p50 {:.0} ms p95 {:.0} ms max {:.0} ms | connections opened {} | protocol {:?}",
        ms.len(),
        errors.len(),
        pct(&ms, 0.5),
        pct(&ms, 0.95),
        ms.last().copied().unwrap_or(f64::NAN),
        connects,
        kinds
    );
    for e in errors.iter().take(8) {
        println!("[live]   {e}");
    }
}

/// The same live run with the wait slice changed: 25 s and 1 ms. Against the
/// pre-m26 transport (SO_RCVTIMEO slices) this was the A/B that cleared the
/// timed `recv`: stalls at 25 s (no `recv` ever cancelled) as at 200 ms.
#[cfg(not(feature = "net-h2"))]
#[test]
#[ignore = "requires internet access: api.argon.gl"]
fn live_argon_discovery_slice_ab() {
    for ms in [25_000u64, 1] {
        TEST_POLL_MS.store(ms, Ordering::SeqCst);
        println!("[live] -- slice {ms} ms --");
        live_argon_discovery_sequential_and_parallel();
    }
    TEST_POLL_MS.store(0, Ordering::SeqCst);
}

#[test]
#[ignore = "requires internet access: api.argon.gl"]
fn live_argon_discovery_sequential_and_parallel() {
    // Sequential: 50 in a row.
    let c0 = connections_opened();
    let (mut ms, mut errors, mut protos) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..50 {
        let t0 = Instant::now();
        match send(&live_req(), &Cancel::default()) {
            Ok(r) => {
                assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
                ms.push(t0.elapsed().as_secs_f64() * 1000.0);
                protos.push(proto_of(&r));
            }
            Err(e) => errors.push(format!("after {:?}: {}", t0.elapsed(), e.message)),
        }
    }
    report("50 sequential", ms, &errors, connections_opened() - c0, &protos);

    // Parallel: 24 at once, released together.
    let c0 = connections_opened();
    let gate = Arc::new(std::sync::Barrier::new(24));
    let joins: Vec<_> = (0..24)
        .map(|_| {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                gate.wait();
                let t0 = Instant::now();
                send(&live_req(), &Cancel::default())
                    .map(|r| (t0.elapsed().as_secs_f64() * 1000.0, proto_of(&r)))
                    .map_err(|e| format!("after {:?}: {}", t0.elapsed(), e.message))
            })
        })
        .collect();
    let (mut ms, mut errors, mut protos) = (Vec::new(), Vec::new(), Vec::new());
    for j in joins {
        match j.join().unwrap() {
            Ok((m, p)) => {
                ms.push(m);
                protos.push(p);
            }
            Err(e) => errors.push(e),
        }
    }
    report("24 parallel", ms, &errors, connections_opened() - c0, &protos);
}
