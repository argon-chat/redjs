//! The pooled transport against loopback servers: HTTP/1.1 keep-alive and its
//! framing edges, the per-host limit, the reused-dead-connection retry, abort
//! on both protocols, HTTP/2 multiplexing / GOAWAY / REFUSED_STREAM over h2c
//! (prior knowledge, test-only) and over TLS with ALPN (schannel on both ends).

use super::*;
use crate::webapi::cookies::Credentials;
use crate::webapi::http::stress_tests::{read_request, Duplex, ServerStats, TEST_P12, TEST_P12_PASSWORD};
use crate::webapi::http::{send, Redirect};
use std::io::{Read, Write};
use std::sync::atomic::AtomicBool;

fn get(url: &str) -> Request {
    Request {
        method: "GET".into(),
        url: url.into(),
        headers: vec![],
        body: None,
        redirect: Redirect::Follow,
        credentials: Credentials::Omit,
        connect_timeout: Duration::from_secs(5),
        io_timeout: Duration::from_secs(10),
        observer: None,
    }
}

fn post(url: &str, body: &[u8]) -> Request {
    let mut r = get(url);
    r.method = "POST".into();
    r.body = Some(body.to_vec());
    r
}

// ---------------------------------------------------------------------------
// A scripted HTTP/1.1 server
// ---------------------------------------------------------------------------

/// What the server does with one request.
enum Act {
    /// Write these bytes and keep the connection.
    Reply(Vec<u8>),
    /// Write these bytes, then close.
    ReplyClose(Vec<u8>),
    /// Close without a byte.
    Close,
    /// Write these bytes, then wait (reading) until the client goes away;
    /// records "client closed" when it does.
    Stall(Vec<u8>),
}

struct H1 {
    port: u16,
    stats: Arc<ServerStats>,
    events: Arc<Mutex<Vec<String>>>,
}

/// `act(connection index, request index on that connection, request head)`.
fn h1_server(act: impl Fn(usize, usize, &str) -> Act + Send + Sync + 'static) -> H1 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let stats = Arc::new(ServerStats::default());
    let events = Arc::new(Mutex::new(Vec::new()));
    let act = Arc::new(act);
    let (st, ev) = (Arc::clone(&stats), Arc::clone(&events));
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { return };
            let conn = st.connections.fetch_add(1, Ordering::SeqCst);
            let (st, ev, act) = (Arc::clone(&st), Arc::clone(&ev), Arc::clone(&act));
            std::thread::spawn(move || {
                let mut n = 0;
                while let Some(head) = read_request(&mut sock as &mut dyn Duplex) {
                    st.requests.fetch_add(1, Ordering::SeqCst);
                    ev.lock().unwrap().push(format!("c{conn} r{n} {}", head.lines().next().unwrap_or("")));
                    match act(conn, n, &head) {
                        Act::Reply(b) => {
                            if sock.write_all(&b).is_err() {
                                return;
                            }
                        }
                        Act::ReplyClose(b) => {
                            let _ = sock.write_all(&b);
                            return;
                        }
                        Act::Close => return,
                        Act::Stall(b) => {
                            let _ = sock.write_all(&b);
                            let mut buf = [0u8; 64];
                            let _ = sock.set_read_timeout(Some(Duration::from_secs(10)));
                            loop {
                                match sock.read(&mut buf) {
                                    Ok(0) | Err(_) => break,
                                    Ok(_) => {}
                                }
                            }
                            ev.lock().unwrap().push(format!("c{conn} client closed"));
                            return;
                        }
                    }
                    n += 1;
                }
            });
        }
    });
    H1 { port, stats, events }
}

fn ok_len(body: &str) -> Vec<u8> {
    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}

fn conns(s: &H1) -> usize {
    s.stats.connections.load(Ordering::SeqCst)
}

#[test]
fn sequential_requests_share_one_keep_alive_connection() {
    let s = h1_server(|_, n, _| Act::Reply(ok_len(&format!("answer {n}"))));
    let url = format!("http://127.0.0.1:{}/x", s.port);
    let mut ids = Vec::new();
    for i in 0..5 {
        let r = send(&get(&url), &Cancel::default()).unwrap();
        assert_eq!(String::from_utf8(r.body).unwrap(), format!("answer {i}"));
        assert_eq!(r.protocol, PROTO_H1);
        assert_eq!(r.reused, i > 0, "request {i}");
        ids.push(r.connection_id);
    }
    assert_eq!(conns(&s), 1, "{:?}", s.events.lock().unwrap());
    assert!(ids.windows(2).all(|w| w[0] == w[1]), "{ids:?}");
}

#[test]
fn the_request_says_keep_alive_and_carries_host_and_a_browser_user_agent() {
    let seen = Arc::new(Mutex::new(String::new()));
    let s2 = Arc::clone(&seen);
    let s = h1_server(move |_, _, head| {
        *s2.lock().unwrap() = head.to_string();
        Act::Reply(ok_len("ok"))
    });
    send(&get(&format!("http://127.0.0.1:{}/a?b=1", s.port)), &Cancel::default()).unwrap();
    let head = seen.lock().unwrap().clone();
    assert!(head.starts_with("GET /a?b=1 HTTP/1.1\r\n"), "{head}");
    assert!(head.contains(&format!("Host: 127.0.0.1:{}\r\n", s.port)), "{head}");
    assert!(head.contains("Connection: keep-alive\r\n"), "{head}");
    assert!(head.contains(&format!("User-Agent: {}\r\n", crate::webapi::navigator::user_agent())), "{head}");
    assert!(!head.to_ascii_lowercase().contains("connection: close"), "{head}");
}

#[test]
fn chunked_responses_keep_the_connection() {
    let s = h1_server(|_, n, _| {
        Act::Reply(format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nab{n}\r\n4\r\n-end\r\n0\r\n\r\n").into_bytes())
    });
    let url = format!("http://127.0.0.1:{}/c", s.port);
    for i in 0..3 {
        let r = send(&get(&url), &Cancel::default()).unwrap();
        assert_eq!(String::from_utf8(r.body).unwrap(), format!("ab{i}-end"));
    }
    assert_eq!(conns(&s), 1);
}

#[test]
fn a_close_delimited_body_arrives_whole_and_its_connection_is_not_reused() {
    let s = h1_server(|_, _, _| Act::ReplyClose(b"HTTP/1.1 200 OK\r\n\r\nuntil the end".to_vec()));
    let url = format!("http://127.0.0.1:{}/u", s.port);
    for _ in 0..2 {
        let r = send(&get(&url), &Cancel::default()).unwrap();
        assert_eq!(String::from_utf8(r.body).unwrap(), "until the end");
        assert!(!r.reused);
    }
    assert_eq!(conns(&s), 2);
}

#[test]
fn connection_close_is_honoured() {
    // The server says close but keeps the socket open: reusing it anyway
    // would hang on a connection the server considers finished.
    let s = h1_server(|_, _, _| Act::Reply(b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nok".to_vec()));
    let url = format!("http://127.0.0.1:{}/k", s.port);
    for _ in 0..3 {
        let r = send(&get(&url), &Cancel::default()).unwrap();
        assert_eq!(r.body, b"ok");
        assert!(!r.reused);
    }
    assert_eq!(conns(&s), 3);
}

#[test]
fn a_204_and_a_head_carry_no_body_and_keep_the_connection() {
    let s = h1_server(|_, n, _| {
        if n == 0 {
            Act::Reply(b"HTTP/1.1 204 No Content\r\n\r\n".to_vec())
        } else {
            // HEAD: Content-Length describes the GET body that is NOT sent.
            Act::Reply(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n".to_vec())
        }
    });
    let url = format!("http://127.0.0.1:{}/h", s.port);
    let r = send(&get(&url), &Cancel::default()).unwrap();
    assert_eq!(r.status, 204);
    let mut head = get(&url);
    head.method = "HEAD".into();
    let r = send(&head, &Cancel::default()).unwrap();
    assert_eq!(r.status, 200);
    assert!(r.body.is_empty());
    assert!(r.reused);
    assert_eq!(conns(&s), 1);
}

#[test]
fn a_short_content_length_body_is_an_error_and_the_connection_is_dropped() {
    let s = h1_server(|_, _, _| Act::ReplyClose(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nhelo".to_vec()));
    let e = send(&get(&format!("http://127.0.0.1:{}/s", s.port)), &Cancel::default()).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Protocol, "{}", e.message);
    assert!(e.message.contains("4 of 100"), "{}", e.message);
}

#[test]
fn at_most_six_connections_per_host() {
    let s = h1_server(|_, _, _| {
        std::thread::sleep(Duration::from_millis(300));
        Act::Reply(ok_len("slow"))
    });
    let url = format!("http://127.0.0.1:{}/six", s.port);
    let joins: Vec<_> = (0..14)
        .map(|_| {
            let url = url.clone();
            std::thread::spawn(move || send(&get(&url), &Cancel::default()).map(|r| r.body))
        })
        .collect();
    for j in joins {
        assert_eq!(j.join().unwrap().unwrap(), b"slow");
    }
    assert_eq!(conns(&s), MAX_PER_HOST, "{:?}", s.events.lock().unwrap());
}

/// conn 0 answers its first request, then swallows the second and hangs up —
/// a keep-alive connection the server dropped while it sat in our pool.
fn dies_after_first() -> H1 {
    h1_server(|c, n, _| if c == 0 && n == 1 { Act::Close } else { Act::Reply(ok_len("fine")) })
}

#[test]
fn a_reused_connection_that_died_is_retried_once_for_an_idempotent_request() {
    let s = dies_after_first();
    let url = format!("http://127.0.0.1:{}/r", s.port);
    send(&get(&url), &Cancel::default()).unwrap();
    let r = send(&get(&url), &Cancel::default()).unwrap();
    assert_eq!(r.body, b"fine");
    assert!(!r.reused, "the retry ran on a fresh connection");
    assert_eq!(conns(&s), 2);
    assert_eq!(s.stats.requests.load(Ordering::SeqCst), 3, "{:?}", s.events.lock().unwrap());
}

#[test]
fn a_post_that_reached_a_dying_reused_connection_is_not_retried() {
    let s = dies_after_first();
    let url = format!("http://127.0.0.1:{}/p", s.port);
    send(&get(&url), &Cancel::default()).unwrap();
    let e = send(&post(&url, b"{\"pay\":1}"), &Cancel::default()).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Network, "{}", e.message);
    // The server saw it once; it must never see it twice.
    assert_eq!(s.stats.requests.load(Ordering::SeqCst), 2, "{:?}", s.events.lock().unwrap());
}

#[test]
fn a_fresh_connection_that_dies_is_not_retried() {
    // Chromium's rule is about REUSED connections: a brand-new one that fails
    // tells us about the server, and retrying it would double the damage.
    let s = h1_server(|_, _, _| Act::Close);
    let e = send(&get(&format!("http://127.0.0.1:{}/f", s.port)), &Cancel::default()).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Network, "{}", e.message);
    assert_eq!(conns(&s), 1);
}

#[test]
fn abort_mid_response_closes_the_http11_connection_and_never_pools_it() {
    let s = h1_server(|c, _, _| {
        if c == 0 {
            Act::Stall(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\npartial".to_vec())
        } else {
            Act::Reply(ok_len("never"))
        }
    });
    let url = format!("http://127.0.0.1:{}/abort", s.port);
    let cancel = Cancel::default();
    let c2 = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        c2.abort();
    });
    let t0 = Instant::now();
    let e = send(&get(&url), &cancel).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Aborted, "{}", e.message);
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    // The server sees the client go away...
    let deadline = Instant::now() + Duration::from_secs(3);
    while !s.events.lock().unwrap().iter().any(|e| e.ends_with("client closed")) {
        assert!(Instant::now() < deadline, "the aborted connection stayed open: {:?}", s.events.lock().unwrap());
        std::thread::sleep(Duration::from_millis(20));
    }
    // ...and the next request does not get that connection back.
    let r = send(&get(&url), &Cancel::default()).unwrap();
    assert_eq!(r.body, b"never");
    assert!(!r.reused);
    assert_eq!(conns(&s), 2);
}

/// Records what an [`Observe`](crate::webapi::http::Observe)r is told.
#[derive(Default)]
struct Log(Mutex<Vec<String>>);

impl crate::webapi::http::Observe for Log {
    fn upload(&self, sent: u64, total: u64) {
        self.0.lock().unwrap().push(format!("up {sent}/{total}"));
    }
    fn head(&self, h: &ProgressHead<'_>) {
        self.0.lock().unwrap().push(format!("head {} {} {} {}", h.status, h.protocol, h.redirected, h.url));
    }
    fn body(&self, chunk: &[u8], received: u64, total: Option<u64>) {
        self.0.lock().unwrap().push(format!("body {} {received} {total:?}", chunk.len()));
    }
}

#[test]
fn an_observer_sees_upload_then_the_final_head_then_body() {
    let s = h1_server(|_, _, head| {
        if head.starts_with("POST /start") {
            Act::Reply(b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /final\r\nContent-Length: 4\r\n\r\nskip".to_vec())
        } else {
            Act::Reply(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhelloworld".to_vec())
        }
    });
    let log = Arc::new(Log::default());
    let mut r = post(&format!("http://127.0.0.1:{}/start", s.port), &[7u8; 150_000]);
    r.observer = Some(Observer(log.clone()));
    let resp = send(&r, &Cancel::default()).unwrap();
    assert_eq!(resp.body, b"helloworld");
    let events = log.0.lock().unwrap().clone();
    // Each hop sends the body (307 keeps it), in 64 KiB frames, from zero.
    let ups: Vec<&String> = events.iter().filter(|e| e.starts_with("up ")).collect();
    assert_eq!(ups.iter().filter(|e| e.as_str() == "up 150000/150000").count(), 2, "{events:?}");
    assert_eq!(ups[0], "up 65536/150000", "{events:?}");
    // One head — the 307 is not the answer — after every upload event.
    let heads: Vec<usize> = events.iter().enumerate().filter(|(_, e)| e.starts_with("head")).map(|(i, _)| i).collect();
    assert_eq!(heads.len(), 1, "{events:?}");
    assert_eq!(events[heads[0]], format!("head 200 http/1.1 true http://127.0.0.1:{}/final", s.port));
    let last_up = events.iter().rposition(|e| e.starts_with("up ")).unwrap();
    assert!(last_up < heads[0], "{events:?}");
    // Then the body, counted.
    let body: Vec<&String> = events[heads[0] + 1..].iter().collect();
    assert!(!body.is_empty() && body.iter().all(|e| e.starts_with("body ")), "{events:?}");
    assert!(body.last().unwrap().ends_with(" 10 Some(10)"), "{events:?}");
}

#[test]
fn a_pre_aborted_request_never_dials() {
    let cancel = Cancel::default();
    cancel.abort();
    let e = send(&get("http://192.0.2.1:81/never"), &cancel).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Aborted);
}

#[test]
fn a_stalled_response_times_out_with_the_old_words() {
    let s = h1_server(|_, _, _| Act::Stall(Vec::new()));
    let mut r = get(&format!("http://127.0.0.1:{}/quiet", s.port));
    r.io_timeout = Duration::from_millis(400);
    let e = send(&r, &Cancel::default()).unwrap_err();
    assert_eq!(e.message, "response read timed out");
    let s = h1_server(|_, _, _| Act::Stall(b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\nhalf".to_vec()));
    let mut r = get(&format!("http://127.0.0.1:{}/quiet", s.port));
    r.io_timeout = Duration::from_millis(400);
    let e = send(&r, &Cancel::default()).unwrap_err();
    assert_eq!(e.message, "body read failed: the connection stalled");
}

#[test]
fn slow_responses_over_keep_alive_arrive_byte_exact() {
    // A short version of stress_tests' 500-request run, always on.
    let servers: Vec<_> = (0..2).map(|_| crate::webapi::http::stress_tests::spawn_server(false)).collect();
    let ports: Vec<u16> = servers.iter().map(|(p, _)| *p).collect();
    let out = crate::webapi::http::stress_tests::drive("http", &ports, 48, 12, true, Duration::from_secs(8));
    assert_eq!(out.ok, 48, "{:?}", out.errors);
    let c: usize = servers.iter().map(|(_, s)| s.connections.load(Ordering::SeqCst)).sum();
    assert!(c <= 2 * MAX_PER_HOST, "{c} connections for 2 origins");
}

// ---------------------------------------------------------------------------
// HTTP/2
// ---------------------------------------------------------------------------

#[derive(Default)]
struct H2Stats {
    connections: AtomicUsize,
    streams: AtomicUsize,
    resets: Mutex<Vec<String>>,
}

#[derive(Clone, Copy, PartialEq)]
enum H2Mode {
    Normal,
    /// The first stream on connection 0 is refused (`REFUSED_STREAM`).
    RefuseFirst,
    /// Connection 0 sends GOAWAY as soon as it accepts its first stream, then
    /// answers that stream after a delay.
    GoAwayAfterFirst,
    /// Connection 0 drops the TCP connection after accepting a stream.
    DropMidStream,
}

/// An h2 server on its own runtime thread. Paths: `/n/<k>` answers "n<k>";
/// `/slow` answers after 300 ms; `/hang` sends headers + 5 bytes and waits for
/// the stream to be reset, recording the reason.
fn h2_server(tls: Option<&[&str]>, mode: H2Mode) -> (u16, Arc<H2Stats>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let stats = Arc::new(H2Stats::default());
    let st = Arc::clone(&stats);
    let acceptor = tls.map(|alpn| {
        let id = native_tls::Identity::from_pkcs12(TEST_P12, TEST_P12_PASSWORD).unwrap();
        let mut b = native_tls::TlsAcceptor::builder(id);
        b.accept_alpn(alpn);
        tokio_native_tls::TlsAcceptor::from(b.build().unwrap())
    });
    let h1_fallback = tls.is_some_and(|a| !a.contains(&"h2"));
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            loop {
                let Ok((tcp, _)) = listener.accept().await else { return };
                let conn_index = st.connections.fetch_add(1, Ordering::SeqCst);
                let st = Arc::clone(&st);
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let io: Box<dyn Io> = match acceptor {
                        Some(a) => match a.accept(tcp).await {
                            Ok(t) => Box::new(t),
                            Err(_) => return,
                        },
                        None => Box::new(tcp),
                    };
                    if h1_fallback {
                        serve_h1_over(io, st).await;
                    } else {
                        serve_h2(io, st, conn_index, mode).await;
                    }
                });
            }
        });
    });
    (port, stats)
}

async fn serve_h2(io: Box<dyn Io>, st: Arc<H2Stats>, conn_index: usize, mode: H2Mode) {
    let Ok(mut conn) = h2::server::handshake(io).await else { return };
    let mut first = true;
    while let Some(next) = conn.accept().await {
        let Ok((req, mut respond)) = next else { return };
        st.streams.fetch_add(1, Ordering::SeqCst);
        let on_first_of_0 = first && conn_index == 0;
        first = false;
        if on_first_of_0 && mode == H2Mode::RefuseFirst {
            respond.send_reset(h2::Reason::REFUSED_STREAM);
            continue;
        }
        if on_first_of_0 && mode == H2Mode::DropMidStream {
            return; // the whole connection goes, mid-stream
        }
        if on_first_of_0 && mode == H2Mode::GoAwayAfterFirst {
            // GOAWAY(2^31-1) + PING, then GOAWAY(last processed): streams it
            // already has finish, anything newer is the client's to move.
            conn.graceful_shutdown();
        }
        let st = Arc::clone(&st);
        let path = req.uri().path().to_string();
        tokio::spawn(async move {
            if path == "/hang" {
                let resp = http::Response::builder().status(200).body(()).unwrap();
                let Ok(mut out) = respond.send_response(resp, false) else { return };
                let _ = out.send_data(Bytes::from_static(b"start"), false);
                let reason = std::future::poll_fn(|cx| out.poll_reset(cx)).await;
                st.resets.lock().unwrap().push(match reason { Ok(r) => format!("{r:?}"), Err(e) => format!("error: {e}") });
                return;
            }
            if path == "/slow" || on_first_of_0 {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            let body = format!("n{}", path.rsplit('/').next().unwrap_or(""));
            let resp = http::Response::builder().status(200).header("content-length", body.len()).body(()).unwrap();
            if let Ok(mut out) = respond.send_response(resp, false) {
                let _ = out.send_data(Bytes::from(body), true);
            }
        });
    }
}

/// HTTP/1.1 over an async stream, for the TLS no-h2 fallback test.
async fn serve_h1_over(mut io: Box<dyn Io>, st: Arc<H2Stats>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut acc = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        while let Some(i) = acc.windows(4).position(|w| w == b"\r\n\r\n") {
            st.streams.fetch_add(1, Ordering::SeqCst);
            acc.drain(..i + 4);
            if io.write_all(&ok_len("over tls")).await.is_err() {
                return;
            }
        }
        match io.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => acc.extend_from_slice(&buf[..n]),
        }
    }
}

fn h2c(mode: H2Mode) -> (u16, Arc<H2Stats>) {
    let (port, st) = h2_server(None, mode);
    test_h2c_ports().lock().unwrap().push(port);
    (port, st)
}

fn parallel(urls: Vec<String>) -> Vec<Result<Response, HttpError>> {
    let joins: Vec<_> =
        urls.into_iter().map(|u| std::thread::spawn(move || send(&get(&u), &Cancel::default()))).collect();
    joins.into_iter().map(|j| j.join().unwrap()).collect()
}

#[test]
fn h2_multiplexes_parallel_requests_on_one_connection() {
    let (port, st) = h2c(H2Mode::Normal);
    let urls: Vec<String> = (0..24).map(|k| format!("http://127.0.0.1:{port}/n/{k}")).collect();
    let results = parallel(urls);
    let mut ids = Vec::new();
    for (k, r) in results.into_iter().enumerate() {
        let r = r.unwrap();
        assert_eq!(r.protocol, PROTO_H2);
        assert_eq!(r.status_text, "", "HTTP/2 has no reason phrase, and a browser reports \"\"");
        assert_eq!(String::from_utf8(r.body).unwrap(), format!("n{k}"));
        ids.push(r.connection_id);
    }
    assert_eq!(st.connections.load(Ordering::SeqCst), 1, "first contact must not open one connection per request");
    ids.dedup();
    assert_eq!(ids.len(), 1);
}

#[test]
fn h2_abort_resets_the_stream_and_the_connection_lives_on() {
    let (port, st) = h2c(H2Mode::Normal);
    // Warm the connection, so the abort happens on an established one.
    send(&get(&format!("http://127.0.0.1:{port}/n/0")), &Cancel::default()).unwrap();
    let cancel = Cancel::default();
    let c2 = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        c2.abort();
    });
    let t0 = Instant::now();
    let e = send(&get(&format!("http://127.0.0.1:{port}/hang")), &cancel).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Aborted, "{}", e.message);
    assert!(t0.elapsed() < Duration::from_secs(2));
    let deadline = Instant::now() + Duration::from_secs(3);
    while st.resets.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "the server never saw RST_STREAM");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(st.resets.lock().unwrap()[0], "CANCEL");
    let r = send(&get(&format!("http://127.0.0.1:{port}/n/9")), &Cancel::default()).unwrap();
    assert_eq!(r.body, b"n9");
    assert!(r.reused);
    assert_eq!(st.connections.load(Ordering::SeqCst), 1);
}

#[test]
fn h2_refused_stream_is_retried_on_a_new_connection() {
    let (port, st) = h2c(H2Mode::RefuseFirst);
    let r = send(&post(&format!("http://127.0.0.1:{port}/n/1"), b"x"), &Cancel::default()).unwrap();
    // A POST too: REFUSED_STREAM guarantees the server did nothing with it.
    assert_eq!(r.body, b"n1");
    assert_eq!(st.connections.load(Ordering::SeqCst), 2);
}

#[test]
fn h2_goaway_lets_started_streams_finish_and_moves_new_ones() {
    let (port, st) = h2c(H2Mode::GoAwayAfterFirst);
    let first = {
        let url = format!("http://127.0.0.1:{port}/n/first");
        std::thread::spawn(move || send(&get(&url), &Cancel::default()))
    };
    std::thread::sleep(Duration::from_millis(100));
    let later = parallel((0..6).map(|k| format!("http://127.0.0.1:{port}/n/{k}")).collect());
    let r = first.join().unwrap().unwrap();
    assert_eq!(r.body, b"nfirst", "the stream the server had started completes on the old connection");
    for (k, r) in later.into_iter().enumerate() {
        assert_eq!(r.unwrap().body, format!("n{k}").into_bytes());
    }
    assert_eq!(st.connections.load(Ordering::SeqCst), 2);
}

#[test]
fn h2_a_started_request_on_a_dying_connection_is_a_network_error() {
    let (port, st) = h2c(H2Mode::DropMidStream);
    let e = send(&post(&format!("http://127.0.0.1:{port}/n/1"), b"x"), &Cancel::default()).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Network, "{}", e.message);
    assert_eq!(st.connections.load(Ordering::SeqCst), 1, "not retried");
}

#[test]
fn h2_slow_responses_arrive_byte_exact() {
    let (port, _st) = h2c(H2Mode::Normal);
    let urls: Vec<String> = (0..40).map(|k| format!("http://127.0.0.1:{port}/slow/{k}")).collect();
    for (k, r) in parallel(urls).into_iter().enumerate() {
        assert_eq!(r.unwrap().body, format!("n{k}").into_bytes());
    }
}

#[test]
fn https_negotiates_h2_through_alpn() {
    let (port, st) = h2_server(Some(&["h2", "http/1.1"][..]), H2Mode::Normal);
    let urls: Vec<String> = (0..8).map(|k| format!("https://localhost:{port}/n/{k}")).collect();
    for (k, r) in parallel(urls).into_iter().enumerate() {
        let r = r.unwrap();
        assert_eq!(r.protocol, PROTO_H2);
        assert_eq!(r.body, format!("n{k}").into_bytes());
    }
    assert_eq!(st.connections.load(Ordering::SeqCst), 1);
}

#[test]
fn https_without_h2_falls_back_to_keep_alive_http11() {
    let (port, st) = h2_server(Some(&["http/1.1"][..]), H2Mode::Normal);
    let url = format!("https://localhost:{port}/one");
    for i in 0..3 {
        let r = send(&get(&url), &Cancel::default()).unwrap();
        assert_eq!(r.protocol, PROTO_H1);
        assert_eq!(r.reused, i > 0);
        assert_eq!(r.body, b"over tls");
    }
    assert_eq!(st.connections.load(Ordering::SeqCst), 1);
}

#[test]
fn ipv6_literal_hosts_dial_without_brackets() {
    let Ok(l) = std::net::TcpListener::bind("[::1]:0") else { return };
    let port = l.local_addr().unwrap().port();
    // Some machines filter IPv6 loopback (this one's VPN stack answers
    // WSAEACCES); then there is nothing to test.
    let Ok(probe) = std::net::TcpStream::connect(("::1", port)) else {
        eprintln!("[skip] IPv6 loopback is not reachable here");
        return;
    };
    drop(probe);
    let _ = l.accept();
    let served = Arc::new(AtomicBool::new(false));
    let s2 = Arc::clone(&served);
    std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        read_request(&mut s as &mut dyn Duplex);
        s.write_all(&ok_len("v6")).unwrap();
        s2.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(200));
    });
    let r = send(&get(&format!("http://[::1]:{port}/")), &Cancel::default()).unwrap();
    assert_eq!(r.body, b"v6");
    assert!(served.load(Ordering::SeqCst));
}

#[test]
fn happy_eyeballs_orders_families_alternately() {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let v6 = |n| SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, n)), 1);
    let v4 = |n| SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, n as u8)), 1);
    assert_eq!(interleave(vec![v6(1), v6(2), v4(1), v4(2), v4(3)]), vec![v6(1), v4(1), v6(2), v4(2), v4(3)]);
    assert_eq!(interleave(vec![v4(1), v6(1)]), vec![v4(1), v6(1)]);
}
