use super::{Parser, SseEvent};
use crate::webapi::file::testing::{file_ctx, run};
use crate::webapi::testing::*;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn parse(pieces: &[&[u8]]) -> (Vec<SseEvent>, Parser) {
    let mut p = Parser::new("");
    let mut out = Vec::new();
    for piece in pieces {
        p.feed(piece, &mut out);
    }
    (out, p)
}

fn ev(ty: &str, data: &str, id: &str) -> SseEvent {
    SseEvent { ty: ty.into(), data: data.into(), id: id.into() }
}

#[test]
fn the_parser_follows_the_event_stream_rules() {
    let (out, _) = parse(&[b"data: a\ndata:b\n\nevent: tick\ndata\nid: 7\n\n: comment\n\n"]);
    assert_eq!(out, vec![ev("message", "a\nb", ""), ev("tick", "", "7")]);

    let (out, _) = parse(&[b"\xEF\xBB", b"\xBFdata: x\r", b"\ndata: y\r\r", b"data: z\n\ndata: lost"]);
    assert_eq!(out, vec![ev("message", "x\ny", ""), ev("message", "z", "")]);

    let (out, p) = parse(&[b"id: 3\n\nid: bad\0id\nretry: 1500\nretry: 2s\ndata:  two spaces\n\n"]);
    assert_eq!(out, vec![ev("message", " two spaces", "3")]);
    assert_eq!((p.last_id.as_str(), p.retry), ("3", Some(1500)));

    let (out, _) = parse(&[b"data: \xFF\n\n"]);
    assert_eq!(out, vec![ev("message", "\u{FFFD}", "")]);
}

fn serve(handler: impl Fn(usize, &str, &mut TcpStream) + Send + Sync + 'static) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handler = Arc::new(handler);
    let n = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { return };
            let (h, n) = (Arc::clone(&handler), Arc::clone(&n));
            std::thread::spawn(move || {
                let mut head = Vec::new();
                let mut b = [0u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut b) {
                        Ok(0) | Err(_) => return,
                        Ok(k) => head.extend_from_slice(&b[..k]),
                    }
                }
                h(n.fetch_add(1, Ordering::SeqCst), &String::from_utf8_lossy(&head), &mut sock);
            });
        }
    });
    port
}

fn send(sock: &mut TcpStream, bytes: &str) {
    let _ = sock.write_all(bytes.as_bytes()).and_then(|_| sock.flush());
}

const STREAM_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nConnection: close\r\n\r\n";

#[test]
fn events_arrive_with_their_type_id_and_origin() {
    let port = serve(|_, _, sock| {
        send(sock, STREAM_HEAD);
        send(sock, "data: hello\nid: 1\n\n");
        std::thread::sleep(Duration::from_millis(30));
        send(sock, "event: custom\ndata: {\"n\":2}\n\n");
        std::thread::sleep(Duration::from_millis(2000));
    });
    let c = file_ctx();
    let out = run(
        &c,
        &format!(
            "const log = []; \
             const es = new EventSource('http://127.0.0.1:{port}/s'); \
             log.push(es.readyState); \
             return await new Promise((resolve) => {{ \
               es.onopen = () => log.push('open:' + es.readyState); \
               es.onmessage = (e) => log.push([e.type, e.data, e.lastEventId, e.origin === 'http://127.0.0.1:{port}'].join(',')); \
               es.addEventListener('custom', (e) => {{ \
                 log.push([e.type, JSON.parse(e.data).n, e.lastEventId, e instanceof MessageEvent].join(',')); \
                 es.close(); log.push(es.readyState); resolve(log.join(' | ')); \
               }}); \
             }});"
        ),
    );
    assert_eq!(out, "0 | open:1 | message,hello,1,true | custom,2,1,true | 2");
}

#[test]
fn a_dropped_stream_reconnects_with_the_last_event_id_after_retry() {
    let port = serve(|n, head, sock| {
        if n == 0 {
            send(sock, STREAM_HEAD);
            send(sock, "retry: 50\nid: 41\ndata: first\n\n");
        } else {
            send(sock, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n");
            let last = head
                .lines()
                .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("last-event-id")).map(|(_, v)| v.trim()))
                .unwrap_or("-")
                .to_string();
            let chunk = format!("data: again {last}\n\n");
            send(sock, &format!("{:x}\r\n{chunk}\r\n", chunk.len()));
            std::thread::sleep(Duration::from_millis(2000));
        }
    });
    let c = file_ctx();
    let out = run(
        &c,
        &format!(
            "const log = []; \
             const es = new EventSource('http://127.0.0.1:{port}/s'); \
             return await new Promise((resolve) => {{ \
               es.onerror = () => log.push('error:' + es.readyState); \
               es.onopen = () => log.push('open'); \
               es.onmessage = (e) => {{ \
                 log.push(e.data); \
                 if (e.data.startsWith('again')) {{ es.close(); resolve(log.join(' | ')); }} \
               }}; \
             }});"
        ),
    );
    assert_eq!(out, "open | first | error:0 | open | again 41");
}

#[test]
fn a_wrong_answer_fails_the_connection_for_good() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&hits);
    let port = serve(move |_, _, sock| {
        seen.fetch_add(1, Ordering::SeqCst);
        send(sock, "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nhi");
    });
    let c = file_ctx();
    let out = run(
        &c,
        &format!(
            "const es = new EventSource('http://127.0.0.1:{port}/s'); \
             await new Promise((resolve) => {{ es.onerror = resolve; }}); \
             await new Promise((r) => setTimeout(r, 200)); \
             return es.readyState;"
        ),
    );
    assert_eq!(out, "2");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[test]
fn the_interface_has_the_spec_shape() {
    let c = file_ctx();
    ok(&c, "EventSource.CONNECTING === 0 && EventSource.OPEN === 1 && EventSource.CLOSED === 2 && EventSource.prototype.CLOSED === 2");
    ok(&c, "Object.getPrototypeOf(EventSource.prototype) === EventTarget.prototype");
    ok(&c, "(() => { try { new EventSource('http://[::1'); return false; } catch (e) { return e instanceof DOMException && e.name === 'SyntaxError'; } })()");
    assert_eq!(
        run(
            &c,
            "const es = new EventSource('https://example.invalid/x', { withCredentials: true }); \
             const r = [es.url, es.withCredentials, es.onmessage]; es.close(); return r.join(',') + ',' + es.readyState;"
        ),
        "https://example.invalid/x,true,,2"
    );
}
