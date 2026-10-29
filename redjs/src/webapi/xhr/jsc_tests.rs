//! `XMLHttpRequest` through real JS against a real loopback server, on
//! whichever transport this build has (`net-h2` or the blocking one). The
//! assertions hold on both. The two transports differ only in how finely they
//! report progress, and these tests pin only what both guarantee.

use crate::webapi::file::testing::{file_ctx as xhr_ctx, run};
use crate::webapi::testing::*;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

/// One step of an answer: bytes to write, or a pause.
enum Step {
    Send(Vec<u8>),
    Wait(u64),
}

type Handler = Arc<dyn Fn(&str, &[u8]) -> Vec<Step> + Send + Sync>;

/// A keep-alive HTTP/1.1 loopback server. Every request (its head up to the
/// blank line, and its `Content-Length` body) is answered with
/// `handler(head, body)`. Each connection gets its own thread, so a stalled
/// answer holds up only its own request.
fn serve(handler: impl Fn(&str, &[u8]) -> Vec<Step> + Send + Sync + 'static) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handler: Handler = Arc::new(handler);
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(sock) = sock else { return };
            let h = Arc::clone(&handler);
            std::thread::spawn(move || connection(sock, h));
        }
    });
    port
}

fn connection(mut sock: TcpStream, handler: Handler) {
    let _ = sock.set_read_timeout(Some(Duration::from_secs(10)));
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 16384];
    loop {
        // The head.
        let end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i;
            }
            match sock.read(&mut tmp) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
        let len = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim().eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().unwrap_or(0))
            })
            .unwrap_or(0);
        buf.drain(..end + 4);
        // The body.
        while buf.len() < len {
            match sock.read(&mut tmp) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        let body: Vec<u8> = buf.drain(..len).collect();
        for step in handler(&head, &body) {
            match step {
                Step::Send(bytes) => {
                    if sock.write_all(&bytes).and_then(|_| sock.flush()).is_err() {
                        return;
                    }
                }
                Step::Wait(ms) => std::thread::sleep(Duration::from_millis(ms)),
            }
        }
    }
}

/// A complete response with a `Content-Length`.
fn reply(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<Step> {
    vec![Step::Send(response(status, headers, body))]
}

fn response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// A request header's value, by name.
fn header<'a>(head: &'a str, name: &str) -> &'a str {
    head.lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
        })
        .unwrap_or("-")
}

/// `run` over a promise-shaped XHR: `body` defines `xhr` (opened) and may
/// set it up; this sends `send_arg` and resolves with `done()` at `loadend`.
fn xhr(c: &crate::api::Context, setup: &str, send_arg: &str, done: &str) -> String {
    run(
        c,
        &format!(
            "const log = []; \
             const xhr = new XMLHttpRequest(); \
             {setup} \
             return await new Promise((resolve) => {{ \
               xhr.addEventListener('loadend', () => resolve({done})); \
               xhr.send({send_arg}); \
             }});"
        ),
    )
}

// ---------------------------------------------------------------------------
// The Argon client's upload (apps/client lib/uploadFile.ts, multipartUpload.ts)
// ---------------------------------------------------------------------------

/// The client's `xhrPut`, line for line: a PUT of a Blob with upload progress,
/// a timeout slid forward on every progress event, and the `ETag` read at
/// `load`. Until m26 there was no `XMLHttpRequest` at all, and the client fell
/// back to a `fetch` PUT with no progress.
#[test]
fn the_clients_put_reports_upload_progress_and_reads_the_etag() {
    let c = xhr_ctx();
    let port = serve(|head, body| {
        let echo = format!(
            "{} {} {} part={}",
            head.lines().next().unwrap_or(""),
            body.len(),
            header(head, "content-type"),
            header(head, "x-part")
        );
        reply("200 OK", &[("ETag", "\"etag-7\""), ("Content-Type", "text/plain")], echo.as_bytes())
    });
    let n = 300 * 1024;
    let out = xhr(
        &c,
        &format!(
            "const blob = new Blob([new Uint8Array({n}).fill(7)], {{ type: 'video/mp4' }}); \
             const startedAt = performance.now(); \
             let ups = 0, upLast = null; \
             xhr.onreadystatechange = () => log.push('rs' + xhr.readyState); \
             xhr.open('PUT', 'http://127.0.0.1:{port}/part/7'); \
             xhr.setRequestHeader('X-Part', '7'); \
             xhr.timeout = 5000; \
             xhr.upload.onloadstart = (e) => log.push('up:loadstart:' + e.loaded + '/' + e.total); \
             xhr.upload.onprogress = (e) => {{ \
               ups++; upLast = e.loaded + '/' + e.total + ':' + e.lengthComputable; \
               xhr.timeout = Math.ceil(performance.now() - startedAt) + 5000; \
             }}; \
             xhr.upload.onload = (e) => log.push('up:load:' + e.loaded + '/' + e.total); \
             xhr.upload.onloadend = () => log.push('up:loadend'); \
             xhr.onloadstart = () => log.push('loadstart'); \
             xhr.onload = () => log.push('load:' + xhr.status + ':' + xhr.getResponseHeader('ETag') + ':' + xhr.responseText); \
             xhr.onerror = () => log.push('error'); \
             xhr.ontimeout = () => log.push('timeout'); \
             xhr.onabort = () => log.push('abort'); \
             globalThis.__blob = blob; globalThis.__ups = () => ups + '|' + upLast;"
        ),
        "__blob",
        "log.filter((l, i) => l !== log[i - 1]).join(',') + '|' + (__ups().split('|')[0] > 0) + '|' + __ups().split('|')[1]",
    );
    assert_eq!(
        out,
        format!(
            "rs1,loadstart,up:loadstart:0/{n},up:load:{n}/{n},up:loadend,rs2,rs3,rs4,\
             load:200:\"etag-7\":PUT /part/7 HTTP/1.1 {n} video/mp4 part=7|true|{n}/{n}:true"
        )
    );
}

/// `uploadFile.ts` again, for the paths that end badly: a 4xx is a `load`
/// with the status (the client reads `responseText` for its message), and a
/// refused connection is an `error` with status 0.
#[test]
fn an_http_error_is_a_load_and_a_dead_server_is_an_error() {
    let c = xhr_ctx();
    let port = serve(|_, _| reply("413 Payload Too Large", &[], b"too big"));
    assert_eq!(
        xhr(
            &c,
            &format!(
                "xhr.open('PUT', 'http://127.0.0.1:{port}/x'); \
                 for (const t of ['load', 'error', 'abort', 'timeout']) xhr.addEventListener(t, () => log.push(t));"
            ),
            "'abc'",
            "log.join() + ':' + xhr.status + ':' + xhr.statusText + ':' + xhr.responseText",
        ),
        "load:413:Payload Too Large:too big"
    );
    assert_eq!(
        xhr(
            &c,
            "xhr.open('PUT', 'http://127.0.0.1:1/x'); \
             xhr.upload.onprogress = () => {}; \
             for (const t of ['load', 'error', 'abort', 'timeout']) { \
               xhr.addEventListener(t, (e) => log.push(t + ':' + e.loaded + '/' + e.total)); \
               xhr.upload.addEventListener(t, (e) => log.push('up:' + t)); \
             } \
             xhr.onreadystatechange = () => log.push('rs' + xhr.readyState);",
            "new Blob(['abc'])",
            "log.join() + ':' + xhr.status + ':' + JSON.stringify(xhr.statusText) + ':' + xhr.readyState + ':' + \
             xhr.getResponseHeader('etag') + ':' + JSON.stringify(xhr.getAllResponseHeaders())",
        ),
        "rs4,up:error,error:0/0:0:\"\":4:null:\"\""
    );
}

// ---------------------------------------------------------------------------
// The response side
// ---------------------------------------------------------------------------

#[test]
fn a_get_walks_the_ready_states_and_exposes_the_head() {
    let c = xhr_ctx();
    let port = serve(|_, _| {
        reply(
            "200 OK",
            &[
                ("Content-Type", "text/plain; charset=utf-8"),
                ("X-B", "2"),
                ("X-A", "1"),
                ("Set-Cookie", "sid=1; Path=/"),
                ("x-a", "again"),
            ],
            "héllo".as_bytes(),
        )
    });
    let out = xhr(
        &c,
        &format!(
            "xhr.onreadystatechange = () => log.push(xhr.readyState + ':' + xhr.status); \
             xhr.open('get', 'http://127.0.0.1:{port}/a?q=1#frag'); \
             log.push('opened:' + xhr.readyState);"
        ),
        "",
        "[log.filter((l, i) => l !== log[i - 1]).join(), xhr.responseText, xhr.response === xhr.responseText, \
          xhr.statusText, xhr.responseURL, xhr.getResponseHeader('X-A'), String(xhr.getResponseHeader('set-cookie')), \
          JSON.stringify(xhr.getAllResponseHeaders())].join('|')",
    );
    assert_eq!(
        out,
        format!(
            "1:0,opened:1,2:200,3:200,4:200|héllo|true|OK|http://127.0.0.1:{port}/a?q=1|1, again|null|\
             \"content-length: 6\\r\\ncontent-type: text/plain; charset=utf-8\\r\\nx-a: 1, again\\r\\nx-b: 2\\r\\n\""
        )
    );
}

#[test]
fn every_response_type_reads_the_body_its_own_way() {
    let c = xhr_ctx();
    let port = serve(|head, _| {
        if head.starts_with("GET /json") {
            reply("200 OK", &[("Content-Type", "application/json")], br#"{"a":[1,2]}"#)
        } else if head.starts_with("GET /bad-json") {
            reply("200 OK", &[], b"{nope")
        } else {
            reply("200 OK", &[("Content-Type", "image/png")], &[1, 2, 3, 4])
        }
    });
    let get = |rtype: &str, path: &str, done: &str| {
        xhr(
            &c,
            &format!("xhr.open('GET', 'http://127.0.0.1:{port}{path}'); xhr.responseType = '{rtype}';"),
            "",
            done,
        )
    };
    assert_eq!(
        get("arraybuffer", "/bin", "[xhr.response instanceof ArrayBuffer, xhr.response.byteLength, new Uint8Array(xhr.response).join('-'), xhr.response === xhr.response].join()"),
        "true,4,1-2-3-4,true"
    );
    assert_eq!(
        get("blob", "/bin", "[xhr.response instanceof Blob, xhr.response.size, xhr.response.type].join()"),
        "true,4,image/png"
    );
    assert_eq!(get("json", "/json", "JSON.stringify(xhr.response) + (xhr.response === xhr.response)"), "{\"a\":[1,2]}true");
    assert_eq!(get("json", "/bad-json", "String(xhr.response)"), "null");
    assert_eq!(get("text", "/json", "xhr.response"), "{\"a\":[1,2]}");
    // `responseText` is only for '' and 'text'; the type is fixed once loading.
    assert_eq!(
        get(
            "json",
            "/json",
            "(() => { const r = []; \
               try { xhr.responseText; r.push('read'); } catch (e) { r.push(e.name); } \
               try { xhr.responseType = 'text'; r.push('set'); } catch (e) { r.push(e.name); } \
               xhr.responseType = 'bogus'; r.push(xhr.responseType); return r.join(); })()"
        ),
        "InvalidStateError,InvalidStateError,json"
    );
    // overrideMimeType decides the charset `responseText` decodes with.
    let port2 = serve(|_, _| reply("200 OK", &[("Content-Type", "text/plain")], &[0xE9]));
    assert_eq!(
        xhr(
            &c,
            &format!("xhr.open('GET', 'http://127.0.0.1:{port2}/'); xhr.overrideMimeType('text/plain; charset=windows-1252');"),
            "",
            "xhr.responseText",
        ),
        "é"
    );
}

/// A body that arrives in pieces: `progress` events with the declared total,
/// the last one at the full length, and `load` with it again.
#[test]
fn download_progress_counts_to_the_declared_length() {
    let c = xhr_ctx();
    let piece = vec![b'x'; 64 * 1024];
    let port = serve(move |_, _| {
        let total = piece.len() * 3;
        let mut head = format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\n\r\n").into_bytes();
        head.extend_from_slice(&piece);
        vec![
            Step::Send(head),
            Step::Wait(80),
            Step::Send(piece.clone()),
            Step::Wait(80),
            Step::Send(piece.clone()),
        ]
    });
    let out = xhr(
        &c,
        &format!(
            "const ps = []; xhr.open('GET', 'http://127.0.0.1:{port}/big'); \
             xhr.onprogress = (e) => ps.push(e.loaded + '/' + e.total + ':' + e.lengthComputable); \
             xhr.onload = (e) => log.push('load:' + e.loaded + '/' + e.total); globalThis.__ps = ps;"
        ),
        "",
        "[__ps.length > 0, __ps[__ps.length - 1], log.join(), xhr.responseText.length].join('|')",
    );
    assert_eq!(out, "true|196608/196608:true|load:196608/196608|196608");
}

// ---------------------------------------------------------------------------
// abort() and timeout
// ---------------------------------------------------------------------------

#[test]
fn abort_ends_the_request_with_abort_then_loadend_and_resets_to_unsent() {
    let c = xhr_ctx();
    let port = serve(|_, _| {
        let mut r = vec![Step::Wait(3000)];
        r.extend(reply("200 OK", &[], b"late"));
        r
    });
    let out = xhr(
        &c,
        &format!(
            "xhr.open('PUT', 'http://127.0.0.1:{port}/slow'); \
             xhr.onreadystatechange = () => log.push('rs' + xhr.readyState); \
             xhr.upload.onprogress = () => {{}}; \
             xhr.upload.onabort = () => log.push('up:abort'); \
             xhr.upload.onloadend = () => log.push('up:loadend'); \
             xhr.onabort = (e) => log.push('abort:' + e.loaded + ':' + xhr.readyState + ':' + xhr.status); \
             xhr.onload = () => log.push('LOAD'); \
             setTimeout(() => {{ xhr.abort(); log.push('after:' + xhr.readyState); }}, 100);"
        ),
        "'tiny'",
        "new Promise((r) => setTimeout(() => r(log.join()), 50))",
    );
    // The upload (four bytes) ended long before the abort, so only the request
    // itself hears it. readyState is UNSENT again after abort() returns,
    // without another readystatechange.
    assert_eq!(out, "up:loadend,rs4,abort:0:4:0,after:0");
    // abort() on an idle object, and open() on a running one, are silent.
    assert_eq!(
        run(
            &c,
            &format!(
                "const x = new XMLHttpRequest(); const log = []; \
                 for (const t of ['abort', 'loadend', 'readystatechange']) x.addEventListener(t, () => log.push(t)); \
                 x.abort(); x.open('GET', 'http://127.0.0.1:{port}/slow'); x.send(); \
                 x.open('GET', 'http://127.0.0.1:{port}/slow'); \
                 await new Promise((r) => setTimeout(r, 100)); return log.join() + ':' + x.readyState;"
            )
        ),
        "readystatechange:1"
    );
}

#[test]
fn a_timeout_fires_timeout_and_one_set_mid_request_counts_from_send() {
    let c = xhr_ctx();
    let port = serve(|_, _| {
        let mut r = vec![Step::Wait(700)];
        r.extend(reply("200 OK", &[], b"slow"));
        r
    });
    let out = xhr(
        &c,
        &format!(
            "const t0 = performance.now(); \
             xhr.open('GET', 'http://127.0.0.1:{port}/'); xhr.timeout = 150; \
             for (const t of ['load', 'error', 'abort', 'timeout']) xhr.addEventListener(t, () => log.push(t)); \
             xhr.addEventListener('loadend', () => log.push(performance.now() - t0 < 600));"
        ),
        "",
        "log.join() + ':' + xhr.readyState + ':' + xhr.status",
    );
    assert_eq!(out, "timeout,true:4:0");
    // Raised mid-request (the client slides it on every upload progress
    // event), the request lives; lowered below the time already spent, it
    // times out at once.
    assert_eq!(
        xhr(
            &c,
            &format!(
                "xhr.open('GET', 'http://127.0.0.1:{port}/'); xhr.timeout = 200; \
                 setTimeout(() => {{ xhr.timeout = 5000; }}, 100); \
                 for (const t of ['load', 'timeout']) xhr.addEventListener(t, () => log.push(t));"
            ),
            "",
            "log.join() + ':' + xhr.responseText",
        ),
        "load:slow"
    );
    assert_eq!(
        xhr(
            &c,
            &format!(
                "const t0 = performance.now(); xhr.open('GET', 'http://127.0.0.1:{port}/'); \
                 setTimeout(() => {{ xhr.timeout = 50; }}, 150); \
                 for (const t of ['load', 'timeout']) xhr.addEventListener(t, () => log.push(t)); \
                 xhr.addEventListener('loadend', () => log.push(performance.now() - t0 < 600));"
            ),
            "",
            "log.join()",
        ),
        "timeout,true"
    );
}

// ---------------------------------------------------------------------------
// Request bodies and headers
// ---------------------------------------------------------------------------

#[test]
fn each_body_type_brings_its_content_type_unless_one_was_set() {
    let c = xhr_ctx();
    let port = serve(|head, body| {
        let echo = format!(
            "{}|{}|{}|{}",
            header(head, "content-type"),
            header(head, "x-multi"),
            header(head, "referer"),
            String::from_utf8_lossy(body)
        );
        reply("200 OK", &[], echo.as_bytes())
    });
    let post = |setup: &str, arg: &str| {
        xhr(&c, &format!("xhr.open('POST', 'http://127.0.0.1:{port}/'); {setup}"), arg, "xhr.responseText")
    };
    assert_eq!(post("", "'plain'"), "text/plain;charset=UTF-8|-|-|plain");
    assert_eq!(post("", "new URLSearchParams({ a: '1 2', b: 'é' })"), "application/x-www-form-urlencoded;charset=UTF-8|-|-|a=1+2&b=%C3%A9");
    assert_eq!(post("", "new Blob(['b'], { type: 'image/x' })"), "image/x|-|-|b");
    assert_eq!(post("", "new Uint8Array([104, 105])"), "-|-|-|hi");
    assert_eq!(
        post(
            "xhr.setRequestHeader('Content-Type', 'application/json'); \
             xhr.setRequestHeader('X-Multi', 'a'); xhr.setRequestHeader('x-multi', 'b'); \
             xhr.setRequestHeader('Referer', 'http://evil/');",
            "'{}'"
        ),
        "application/json|a, b|-|{}"
    );
    assert!(post("", "(() => { const f = new FormData(); f.append('k', 'v'); return f; })()")
        .starts_with("multipart/form-data; boundary="));
    // GET and HEAD send no body, whatever send() is given.
    assert_eq!(
        xhr(&c, &format!("xhr.open('GET', 'http://127.0.0.1:{port}/');"), "'ignored'", "xhr.responseText"),
        "-|-|-|"
    );
}

// ---------------------------------------------------------------------------
// The interface, and what it refuses
// ---------------------------------------------------------------------------

#[test]
fn the_interface_has_the_spec_shape() {
    let c = xhr_ctx();
    ok(&c, "typeof XMLHttpRequest === 'function' && typeof XMLHttpRequestUpload === 'function' \
            && typeof XMLHttpRequestEventTarget === 'function' && typeof ProgressEvent === 'function'");
    ok(&c, "(() => { const x = new XMLHttpRequest(); \
             return x instanceof XMLHttpRequestEventTarget && x instanceof EventTarget \
               && x.upload instanceof XMLHttpRequestUpload && x.upload instanceof XMLHttpRequestEventTarget \
               && x.upload === x.upload && !(x.upload instanceof XMLHttpRequest) \
               && x.readyState === 0 && x.status === 0 && x.statusText === '' && x.response === '' \
               && x.responseText === '' && x.responseXML === null && x.responseURL === '' \
               && x.timeout === 0 && x.withCredentials === false && x.responseType === '' \
               && x.onload === null && x.upload.onprogress === null && x.onreadystatechange === null; })()");
    ok(&c, "XMLHttpRequest.UNSENT === 0 && XMLHttpRequest.OPENED === 1 && XMLHttpRequest.HEADERS_RECEIVED === 2 \
            && XMLHttpRequest.LOADING === 3 && XMLHttpRequest.DONE === 4 && new XMLHttpRequest().DONE === 4");
    ok(&c, "(() => { const e = new ProgressEvent('progress', { lengthComputable: true, loaded: 5, total: 10, bubbles: true }); \
             return e instanceof Event && e.type === 'progress' && e.lengthComputable && e.loaded === 5 \
               && e.total === 10 && e.bubbles && !e.isTrusted; })()");
    ok(&c, "(() => { const e = new ProgressEvent('x'); return !e.lengthComputable && e.loaded === 0 && e.total === 0; })()");
    ok(&c, "(() => { const x = new XMLHttpRequest(); const f = () => 1; x.onload = f; x.onerror = 'nope'; \
             return x.onload === f && x.onerror === null; })()");
    ok(&c, "(() => { class Mine extends XMLHttpRequest {} const m = new Mine(); \
             return m instanceof Mine && m instanceof XMLHttpRequest && m.readyState === 0; })()");
}

#[test]
fn what_the_spec_forbids_throws_its_exception() {
    let c = xhr_ctx();
    // Port 9, not 1: the pool serializes connects to one origin, and the dead
    // server test above waits on port 1.
    let name = |src: &str| s(&c, &format!("(() => {{ try {{ {src}; return 'no throw'; }} catch (e) {{ return e.name; }} }})()"));
    assert_eq!(name("new XMLHttpRequest().send()"), "InvalidStateError");
    assert_eq!(name("new XMLHttpRequest().setRequestHeader('a', 'b')"), "InvalidStateError");
    assert_eq!(name("new XMLHttpRequest().open('GET', 'http://127.0.0.1:9/', false)"), "InvalidAccessError");
    assert_eq!(name("new XMLHttpRequest().open('bad method', 'http://127.0.0.1:9/')"), "SyntaxError");
    assert_eq!(name("new XMLHttpRequest().open('TRACE', 'http://127.0.0.1:9/')"), "SecurityError");
    assert_eq!(name("new XMLHttpRequest().open('GET', 'http://[bad')"), "SyntaxError");
    assert_eq!(
        name("const x = new XMLHttpRequest(); x.open('GET', 'http://127.0.0.1:9/'); x.setRequestHeader('a b', 'c')"),
        "SyntaxError"
    );
    assert_eq!(
        name("const x = new XMLHttpRequest(); x.open('GET', 'http://127.0.0.1:9/'); x.send(); x.send()"),
        "InvalidStateError"
    );
    assert_eq!(
        name("const x = new XMLHttpRequest(); x.open('GET', 'http://127.0.0.1:9/'); x.send(); x.withCredentials = true"),
        "InvalidStateError"
    );
    assert_eq!(name("XMLHttpRequest.prototype.open.call({}, 'GET', '/')"), "TypeError");
    assert_eq!(name("XMLHttpRequest()"), "TypeError");
    // An unsupported scheme is a network error, reported asynchronously.
    assert_eq!(
        xhr(&c, "xhr.open('GET', 'ftp://example.com/f'); xhr.onerror = () => log.push('error');", "", "log.join() + ':' + xhr.status"),
        "error:0"
    );
}

/// An in-flight request is kept alive by the request itself, as in a browser:
/// nothing references the object, a collection runs, and `load` still comes.
#[test]
fn an_unreferenced_request_still_completes() {
    let c = xhr_ctx();
    let port = serve(|_, _| {
        let mut r = vec![Step::Wait(200)];
        r.extend(reply("200 OK", &[], b"kept"));
        r
    });
    let out = run(
        &c,
        &format!(
            "const got = new Promise((resolve) => {{ (() => {{ \
               const x = new XMLHttpRequest(); x.open('GET', 'http://127.0.0.1:{port}/'); \
               x.onload = function () {{ resolve(this.responseText); }}; x.send(); }})(); }}); \
             for (let i = 0; i < 5; i++) {{ new Array(100000).fill({{}}); }} \
             if (typeof gc === 'function') gc(); \
             return await got;"
        ),
    );
    assert_eq!(out, "kept");
}
