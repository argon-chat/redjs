//! The HTTP cache (src/webapi/http/cache.rs) end to end: real requests through
//! whichever transport this build has (the pooled one with `net-h2`, the
//! blocking one without) against a loopback server that counts what reaches
//! it. Its own process because the cache is process-wide: an install here must
//! not leak into the library's other network tests. One test at a time, for
//! the same reason (`SERIAL`).
//!
//!   cargo test --release --no-default-features --features net-h2 --test http_cache
//!   cargo test --release --no-default-features --test http_cache

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use redjs::webapi::cookies::Credentials;
use redjs::webapi::http::cache::{self, CacheMode, Config, Outcome, Sealer};
use redjs::webapi::http::{send, Cancel, Observe, Observer, ProgressHead, Redirect, Request, Response};

static SERIAL: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// A loopback HTTP/1.1 server with keep-alive, scripted per request
// ---------------------------------------------------------------------------

/// One request as the server saw it.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    headers: HashMap<String, String>,
}

struct Server {
    port: u16,
    log: Arc<Mutex<Vec<Seen>>>,
}

impl Server {
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
    fn count(&self) -> usize {
        self.log.lock().unwrap().len()
    }
    fn last(&self) -> Seen {
        self.log.lock().unwrap().last().cloned().expect("the server saw a request")
    }
    fn hits(&self, path: &str) -> usize {
        self.log.lock().unwrap().iter().filter(|s| s.path == path).count()
    }
}

fn reply(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\ncontent-length: {}\r\n", body.len()).into_bytes();
    for (k, v) in headers {
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

fn serve(handler: impl Fn(&Seen) -> Vec<u8> + Send + Sync + 'static) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let log = Arc::new(Mutex::new(Vec::new()));
    let (l, handler) = (Arc::clone(&log), Arc::new(handler));
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(sock) = sock else { return };
            let (l, handler) = (Arc::clone(&l), Arc::clone(&handler));
            std::thread::spawn(move || connection(sock, &l, &*handler));
        }
    });
    Server { port, log }
}

fn connection(mut sock: TcpStream, log: &Mutex<Vec<Seen>>, handler: &dyn Fn(&Seen) -> Vec<u8>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i;
            }
            match sock.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
        buf.drain(..end + 4);
        let mut lines = head.split("\r\n");
        let mut first = lines.next().unwrap_or("").split(' ');
        let method = first.next().unwrap_or("").to_string();
        let path = first.next().unwrap_or("").to_string();
        let headers: HashMap<String, String> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        while buf.len() < len {
            match sock.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        buf.drain(..len);
        let seen = Seen { method, path, headers };
        log.lock().unwrap().push(seen.clone());
        if sock.write_all(&handler(&seen)).is_err() {
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static DIRS: AtomicUsize = AtomicUsize::new(0);

fn fresh_dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "redjs-http-cache-{}-{}",
        std::process::id(),
        DIRS.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Installs a cache over a new folder for the length of one test.
struct Installed(PathBuf);

impl Installed {
    fn new(max_bytes: u64, sealer: Option<Arc<dyn Sealer>>) -> Installed {
        let dir = fresh_dir();
        Installed::at(dir, max_bytes, sealer)
    }
    fn at(dir: PathBuf, max_bytes: u64, sealer: Option<Arc<dyn Sealer>>) -> Installed {
        cache::install(Config { dir: dir.clone(), max_bytes, sealer }).expect("install");
        Installed(dir)
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        cache::uninstall();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn request(method: &str, url: &str, headers: &[(&str, &str)], mode: CacheMode) -> Request {
    Request {
        method: method.into(),
        url: url.into(),
        headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        body: (method == "POST").then(|| b"x".to_vec()),
        redirect: Redirect::Follow,
        credentials: Credentials::Omit,
        connect_timeout: Duration::from_secs(5),
        io_timeout: Duration::from_secs(10),
        observer: None,
        cache: mode,
    }
}

fn get(url: &str) -> Response {
    go(request("GET", url, &[], CacheMode::Default))
}

fn go(r: Request) -> Response {
    let resp = send(&r, &Cancel::default()).unwrap_or_else(|e| panic!("{} {}: {}", r.method, r.url, e.message));
    // Writes are the writer thread's; the next lookup must see this one.
    cache::flush();
    resp
}

fn hdr<'a>(r: &'a Response, name: &str) -> Option<&'a str> {
    r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

const MB: u64 = 1 << 20;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_answer_is_served_from_disk_without_its_cookie() {
    let _s = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _c = Installed::new(64 * MB, None);
    let srv = serve(|_| {
        reply("200 OK", &[("cache-control", "max-age=60"), ("content-type", "text/plain"), ("set-cookie", "a=1")], b"hello")
    });
    let a = get(&srv.url("/a"));
    assert_eq!((a.status, a.cache, a.body.as_slice()), (200, Outcome::Stored, &b"hello"[..]));
    let b = get(&srv.url("/a"));
    assert_eq!((b.status, b.cache, b.body.as_slice()), (200, Outcome::Hit, &b"hello"[..]));
    assert_eq!(srv.count(), 1, "the second request never reached the server");
    assert_eq!(hdr(&b, "content-type"), Some("text/plain"));
    assert_eq!(hdr(&b, "set-cookie"), None, "a cached answer never sets a cookie again");
    assert_eq!((b.protocol, b.connection_id, b.remote_addr), ("", 0, None));
    // A fragment is not part of the key.
    assert_eq!(get(&srv.url("/a#x")).cache, Outcome::Hit);
    let st = cache::stats().unwrap();
    assert_eq!((st.entries, st.hits, st.stored), (1, 2, 1));
}

#[test]
fn each_redirect_hop_is_cached_on_its_own_terms() {
    let _s = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _c = Installed::new(64 * MB, None);
    // Argon's shape: a private 302 for five minutes, a CDN answer for thirty days.
    let srv = serve(|s| {
        if s.path.starts_with("/files/") {
            let loc = s.path.replace("/files/", "/cdn/");
            reply("302 Found", &[("location", &loc), ("cache-control", "private, max-age=300")], b"")
        } else {
            reply("200 OK", &[("cache-control", "max-age=2592000"), ("etag", "\"v1\""), ("content-type", "image/png")], b"PNG")
        }
    });
    let a = get(&srv.url("/files/1"));
    assert_eq!((a.status, a.redirected, a.cache), (200, true, Outcome::Stored));
    assert_eq!(srv.count(), 2);
    let b = get(&srv.url("/files/1"));
    assert_eq!((b.status, b.redirected, b.cache, b.body.as_slice()), (200, true, Outcome::Hit, &b"PNG"[..]));
    assert_eq!(b.url, srv.url("/cdn/1"));
    assert_eq!(srv.count(), 2, "neither hop went out again");
    // The CDN URL on its own is a hit too: it is the same entry.
    assert_eq!(get(&srv.url("/cdn/1")).cache, Outcome::Hit);
}

#[test]
fn a_stale_entry_is_revalidated_and_a_304_merges_its_headers() {
    let _s = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _c = Installed::new(64 * MB, None);
    let srv = serve(|s| {
        if s.headers.get("if-none-match").map(String::as_str) == Some("\"v1\"") {
            reply("304 Not Modified", &[("cache-control", "no-cache"), ("x-round", "2"), ("content-type", "text/html")], b"")
        } else {
            reply(
                "200 OK",
                &[("cache-control", "no-cache"), ("etag", "\"v1\""), ("x-round", "1"), ("content-type", "text/plain")],
                b"body-v1",
            )
        }
    });
    assert_eq!(get(&srv.url("/v")).cache, Outcome::Stored);
    let b = get(&srv.url("/v"));
    assert_eq!((b.status, b.cache, b.body.as_slice()), (200, Outcome::Revalidated, &b"body-v1"[..]));
    assert_eq!(srv.last().headers.get("if-none-match").map(String::as_str), Some("\"v1\""));
    assert_eq!(hdr(&b, "x-round"), Some("2"), "the 304's headers replace the stored ones");
    assert_eq!(hdr(&b, "content-type"), Some("text/plain"), "but never the ones that describe the body");
    assert!(b.connection_id > 0, "a revalidation used a connection");
    assert_eq!(cache::stats().unwrap().revalidated, 1);
}

#[test]
fn the_fetch_cache_modes() {
    let _s = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _c = Installed::new(64 * MB, None);
    let srv = serve(|s| {
        if s.headers.contains_key("if-none-match") {
            reply("304 Not Modified", &[], b"")
        } else {
            reply("200 OK", &[("cache-control", "max-age=0"), ("etag", "\"e\"")], b"m")
        }
    });
    let u = srv.url("/m");
    let mode = |m: CacheMode| go(request("GET", &u, &[], m));

    // only-if-cached on a miss: an error, and nothing was sent.
    let miss = send(&request("GET", &u, &[], CacheMode::OnlyIfCached), &Cancel::default());
    assert!(miss.is_err());
    assert_eq!(srv.count(), 0);

    // no-store: neither looked up nor stored, and says so on the wire.
    assert_eq!(mode(CacheMode::NoStore).cache, Outcome::Off);
    assert_eq!(srv.last().headers.get("cache-control").map(String::as_str), Some("no-cache"));
    assert_eq!(srv.last().headers.get("pragma").map(String::as_str), Some("no-cache"));
    assert_eq!(cache::stats().unwrap().entries, 0);

    // reload: not looked up, but stored.
    assert_eq!(mode(CacheMode::Reload).cache, Outcome::Stored);
    assert_eq!(srv.count(), 2);

    // force-cache and only-if-cached take the stale entry without asking.
    assert_eq!(mode(CacheMode::ForceCache).cache, Outcome::Hit);
    assert_eq!(mode(CacheMode::OnlyIfCached).cache, Outcome::Hit);
    assert_eq!(srv.count(), 2);

    // default: stale (max-age=0) with a validator, so a conditional request.
    assert_eq!(mode(CacheMode::Default).cache, Outcome::Revalidated);
    // no-cache: always conditional, with Fetch's max-age=0.
    assert_eq!(mode(CacheMode::NoCache).cache, Outcome::Revalidated);
    let last = srv.last();
    assert_eq!(last.headers.get("cache-control").map(String::as_str), Some("max-age=0"));
    assert_eq!(last.headers.get("if-none-match").map(String::as_str), Some("\"e\""));
    assert_eq!(srv.count(), 4);

    // The script's own conditional request is passed through untouched: its 304 is the answer.
    let own = go(request("GET", &u, &[("if-none-match", "\"e\"")], CacheMode::Default));
    assert_eq!((own.status, own.cache), (304, Outcome::Off));
}

#[test]
fn vary_selects_and_a_mismatch_goes_to_the_network() {
    let _s = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _c = Installed::new(64 * MB, None);
    let srv = serve(|s| {
        let lang = s.headers.get("accept-language").cloned().unwrap_or_default();
        reply("200 OK", &[("cache-control", "max-age=60"), ("vary", "Accept-Language")], lang.as_bytes())
    });
    let u = srv.url("/vary");
    let with = |l: &str| go(request("GET", &u, &[("accept-language", l)], CacheMode::Default));
    assert_eq!(with("ru").cache, Outcome::Stored);
    assert_eq!(with("ru").cache, Outcome::Hit);
    let en = with("en");
    assert_eq!((en.cache, en.body.as_slice()), (Outcome::Stored, &b"en"[..]));
    assert_eq!(srv.count(), 2);
}

#[test]
fn unsafe_methods_invalidate_and_ranges_bypass() {
    let _s = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _c = Installed::new(64 * MB, None);
    let srv = serve(|s| match s.method.as_str() {
        "POST" => reply("200 OK", &[], b"posted"),
        _ if s.headers.contains_key("range") => reply("206 Partial Content", &[("content-range", "bytes 0-0/5")], b"h"),
        _ => reply("200 OK", &[("cache-control", "max-age=60")], b"hello"),
    });
    let u = srv.url("/p");
    assert_eq!(get(&u).cache, Outcome::Stored);
    assert_eq!(get(&u).cache, Outcome::Hit);
    let post = go(request("POST", &u, &[], CacheMode::Default));
    assert_eq!((post.status, post.cache), (200, Outcome::Off));
    assert_eq!(get(&u).cache, Outcome::Stored, "the POST made the entry stale");
    let ranged = go(request("GET", &u, &[("range", "bytes=0-0")], CacheMode::Default));
    assert_eq!((ranged.status, ranged.cache), (206, Outcome::Off));
    assert_eq!(get(&u).cache, Outcome::Hit, "a Range request neither stored nor dropped anything");
    assert_eq!(srv.hits("/p"), 4);
}

#[test]
fn stale_while_revalidate_answers_at_once_and_refreshes_behind() {
    let _s = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _c = Installed::new(64 * MB, None);
    let srv = serve(|_| reply("200 OK", &[("cache-control", "max-age=0, stale-while-revalidate=60")], b"swr"));
    let u = srv.url("/swr");
    assert_eq!(get(&u).cache, Outcome::Stored);
    let b = get(&u);
    assert_eq!((b.cache, b.body.as_slice()), (Outcome::Hit, &b"swr"[..]));
    let until = Instant::now() + Duration::from_secs(5);
    while srv.count() < 2 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(srv.count(), 2, "the background refresh reached the server");
}

#[test]
fn entries_survive_a_restart_and_the_cap_evicts_the_least_recently_used() {
    let _s = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let dir = fresh_dir();
    let srv = serve(|_| reply("200 OK", &[("cache-control", "max-age=600")], &[7u8; 10_000]));
    {
        let _c = Installed::at(dir.clone(), 64 * MB, None);
        assert_eq!(get(&srv.url("/keep")).cache, Outcome::Stored);
        cache::uninstall();
        // Installed's drop would delete the folder; this test keeps it.
        std::mem::forget(_c);
    }
    let c = Installed::at(dir.clone(), 64 * MB, None);
    assert_eq!(get(&srv.url("/keep")).cache, Outcome::Hit, "the entry outlived its process' cache");
    assert_eq!(srv.count(), 1);
    drop(c);

    // Room for about twelve 10 KB entries (and an eighth of it, 16 KB, admits one).
    let _c = Installed::new(130_000, None);
    for i in 0..20 {
        get(&srv.url(&format!("/e{i}")));
        // Distinct last-used times, so the order is not a tie.
        std::thread::sleep(Duration::from_millis(3));
    }
    let st = cache::stats().unwrap();
    assert!(st.bytes <= 130_000, "{st:?}");
    assert!(st.evicted > 0, "{st:?}");
    // The first ones went first.
    let before = srv.count();
    assert_eq!(get(&srv.url("/e19")).cache, Outcome::Hit);
    assert_eq!(get(&srv.url("/e1")).cache, Outcome::Stored);
    assert_eq!(srv.count(), before + 1);
}

struct Xor(u8);

impl Sealer for Xor {
    fn tag(&self) -> String {
        format!("xor-{}", self.0)
    }
    fn seal(&self, p: &[u8]) -> Result<Vec<u8>, String> {
        Ok(p.iter().map(|b| b ^ self.0).collect())
    }
    fn open(&self, s: &[u8]) -> Result<Vec<u8>, String> {
        Ok(s.iter().map(|b| b ^ self.0).collect())
    }
}

#[test]
fn a_sealed_cache_writes_no_plaintext_and_a_new_mode_purges() {
    let _s = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let dir = fresh_dir();
    let srv = serve(|_| reply("200 OK", &[("cache-control", "max-age=600")], b"PLAINTEXT-MARKER"));
    let c = Installed::at(dir.clone(), 64 * MB, Some(Arc::new(Xor(0x5a))));
    assert_eq!(get(&srv.url("/secret")).cache, Outcome::Stored);
    assert_eq!(get(&srv.url("/secret")).cache, Outcome::Hit);
    for ent in std::fs::read_dir(&dir).unwrap().flatten() {
        let bytes = std::fs::read(ent.path()).unwrap();
        assert!(!bytes.windows(16).any(|w| w == b"PLAINTEXT-MARKER"), "{:?}", ent.path());
        assert!(!bytes.windows(7).any(|w| w == b"/secret"), "{:?}", ent.path());
    }
    cache::uninstall();
    std::mem::forget(c);
    // Another key: the folder is purged, not misread.
    let _c = Installed::at(dir.clone(), 64 * MB, Some(Arc::new(Xor(0x11))));
    assert_eq!(cache::stats().unwrap().entries, 0);
    assert_eq!(get(&srv.url("/secret")).cache, Outcome::Stored);
}

#[test]
fn a_damaged_entry_is_dropped_not_served() {
    let _s = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let c = Installed::new(64 * MB, None);
    let srv = serve(|_| reply("200 OK", &[("cache-control", "max-age=600")], b"good"));
    assert_eq!(get(&srv.url("/d")).cache, Outcome::Stored);
    for ent in std::fs::read_dir(&c.0).unwrap().flatten() {
        if ent.path().extension().is_some_and(|e| e == "e") {
            let mut b = std::fs::read(ent.path()).unwrap();
            let n = b.len();
            b[n - 10] ^= 0xff;
            std::fs::write(ent.path(), b).unwrap();
        }
    }
    let r = get(&srv.url("/d"));
    assert_eq!((r.cache, r.body.as_slice()), (Outcome::Stored, &b"good"[..]));
    assert_eq!(cache::stats().unwrap().errors, 1);
    assert_eq!(srv.count(), 2);
}

#[cfg(windows)]
#[test]
fn a_second_instance_reads_but_never_writes() {
    use std::os::windows::fs::OpenOptionsExt;
    let _s = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let dir = fresh_dir();
    let srv = serve(|_| reply("200 OK", &[("cache-control", "max-age=600")], b"shared"));
    {
        let c = Installed::at(dir.clone(), 64 * MB, None);
        assert_eq!(get(&srv.url("/s")).cache, Outcome::Stored);
        cache::uninstall();
        std::mem::forget(c);
    }
    // "The other instance": the lock, held the way the cache holds it.
    let held = std::fs::OpenOptions::new().read(true).write(true).share_mode(1).open(dir.join("lock")).unwrap();
    let _c = Installed::at(dir.clone(), 64 * MB, None);
    assert!(!cache::stats().unwrap().writable);
    assert_eq!(get(&srv.url("/s")).cache, Outcome::Hit, "it serves what the owner stored");
    assert_eq!(get(&srv.url("/t")).cache, Outcome::Miss);
    assert_eq!(get(&srv.url("/t")).cache, Outcome::Miss, "…but stores nothing itself");
    assert!(cache::clear().is_err());
    drop(held);
}

/// What XMLHttpRequest's observer sees.
#[derive(Default)]
struct Watch {
    heads: Mutex<Vec<u16>>,
    bytes: AtomicUsize,
}

impl Observe for Watch {
    fn head(&self, h: &ProgressHead<'_>) {
        self.heads.lock().unwrap().push(h.status);
    }
    fn body(&self, chunk: &[u8], _received: u64, _total: Option<u64>) {
        self.bytes.fetch_add(chunk.len(), Ordering::SeqCst);
    }
}

#[test]
fn an_observer_sees_a_cached_answer_as_it_would_the_network() {
    let _s = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _c = Installed::new(64 * MB, None);
    let srv = serve(|s| {
        if s.headers.contains_key("if-none-match") {
            reply("304 Not Modified", &[], b"")
        } else if s.path == "/o/fresh" {
            reply("200 OK", &[("cache-control", "max-age=600")], b"12345")
        } else {
            reply("200 OK", &[("cache-control", "no-cache"), ("etag", "\"o\"")], b"123")
        }
    });
    let observed = |path: &str| {
        let w = Arc::new(Watch::default());
        let mut r = request("GET", &srv.url(path), &[], CacheMode::Default);
        r.observer = Some(Observer(w.clone() as Arc<dyn Observe>));
        let resp = go(r);
        let heads = w.heads.lock().unwrap().clone();
        (resp.cache, heads, w.bytes.load(Ordering::SeqCst))
    };
    assert_eq!(observed("/o/fresh"), (Outcome::Stored, vec![200], 5));
    assert_eq!(observed("/o/fresh"), (Outcome::Hit, vec![200], 5));
    assert_eq!(observed("/o/v"), (Outcome::Stored, vec![200], 3));
    // The 304 is not shown; the refreshed answer is.
    assert_eq!(observed("/o/v"), (Outcome::Revalidated, vec![200], 3));
}

#[test]
fn a_content_coded_answer_is_served_decoded() {
    use std::io::Write as _;
    let _s = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _c = Installed::new(64 * MB, None);
    let text = "a body worth compressing, ".repeat(50);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(text.as_bytes()).unwrap();
    let coded = gz.finish().unwrap();
    let srv = serve(move |_| reply("200 OK", &[("cache-control", "max-age=600"), ("content-encoding", "gzip")], &coded));
    let a = get(&srv.url("/gz"));
    let b = get(&srv.url("/gz"));
    assert_eq!((a.cache, b.cache), (Outcome::Stored, Outcome::Hit));
    assert_eq!(a.body, text.as_bytes());
    assert_eq!(b.body, text.as_bytes());
    assert_eq!(hdr(&b, "content-encoding"), Some("gzip"));
}

#[test]
fn clear_empties_the_folder() {
    let _s = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let c = Installed::new(64 * MB, None);
    let srv = serve(|_| reply("200 OK", &[("cache-control", "max-age=600")], b"x"));
    get(&srv.url("/c1"));
    get(&srv.url("/c2"));
    assert_eq!(cache::stats().unwrap().entries, 2);
    assert!(cache::clear().unwrap() > 0);
    assert_eq!(cache::stats().unwrap().entries, 0);
    let left: Vec<_> = std::fs::read_dir(&c.0)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "e"))
        .collect();
    assert!(left.is_empty());
    assert_eq!(get(&srv.url("/c1")).cache, Outcome::Stored);
}

#[test]
fn the_owner_opens_only_the_entries_it_lists() {
    // The owner wrote every entry there is, so a URL its list lacks is a miss
    // without a file open. Proof: an entry file copied in behind its back is
    // not served, and one deleted behind its back is a plain miss.
    let _s = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let srv = serve(|_| reply("200 OK", &[("cache-control", "max-age=600")], b"listed"));
    let donor = fresh_dir();
    {
        let c = Installed::at(donor.clone(), 64 * MB, None);
        assert_eq!(get(&srv.url("/l")).cache, Outcome::Stored);
        cache::uninstall();
        std::mem::forget(c);
    }
    let c = Installed::new(64 * MB, None);
    let entries = |dir: &PathBuf| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "e"))
            .collect()
    };
    for p in entries(&donor) {
        std::fs::copy(&p, c.0.join(p.file_name().unwrap())).unwrap();
    }
    assert_eq!(get(&srv.url("/l")).cache, Outcome::Stored, "the unlisted file was not read");
    assert_eq!(srv.count(), 2);
    assert_eq!(get(&srv.url("/l")).cache, Outcome::Hit);
    for p in entries(&c.0) {
        std::fs::remove_file(p).unwrap();
    }
    assert_eq!(get(&srv.url("/l")).cache, Outcome::Stored);
    assert_eq!(srv.count(), 3);
    assert_eq!(cache::stats().unwrap().errors, 0, "a missing file is a miss, not damage");
    let _ = std::fs::remove_dir_all(&donor);
}
