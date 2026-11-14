//! The HTTP cache: a private (one user, one app) disk cache under both
//! transports, consulted per redirect hop, with RFC 9111's rules and
//! Chromium's choices where the RFC leaves one.
//!
//! ## Where it sits
//!
//! Between the redirect loop ([`super::Hops`]) and the wire. Each hop asks
//! [`plan`] what to do: answer from disk, ask the server conditionally, or go
//! to the network. The network's answer goes to [`settle`], which stores it,
//! refreshes the entry a 304 confirmed, or drops what an unsafe method made
//! stale. A redirect is a hop like any other. Argon's `GET /files/<id>`
//! answers `302, Cache-Control: private, max-age=300`, so for five minutes that
//! hop is free, and the CDN's `200, max-age=2592000` behind it is free for
//! thirty days.
//!
//! The cache is OFF until an embedder calls [`install`] with a folder, and
//! [`uninstall`] turns it off again. With none installed, every request behaves
//! exactly as before this module existed: `force-cache` goes to the network,
//! and `only-if-cached` is refused at the `fetch` layer.
//!
//! ## Freshness (RFC 9111 §4.2)
//!
//! - Age is RFC 9111's: `Age`, `Date`, and the request and response times.
//! - The lifetime is `max-age` if present. Otherwise it is `Expires − Date`,
//!   where an unparseable `Expires` means already stale. `s-maxage` is
//!   ignored, because this is a private cache.
//! - Without either, Chromium's heuristics apply:
//!   - a 200/203 without `must-revalidate` gets 10% of
//!     `Date − Last-Modified`;
//!   - a 300/301/308/410 is fresh forever;
//!   - anything else has a lifetime of 0.
//! - `no-cache`, in `Cache-Control` or `Pragma`, means a lifetime of 0. The
//!   entry is always revalidated before use.
//! - `stale-while-revalidate=N` serves a stale entry for N seconds past its
//!   lifetime and refreshes it in the background. `must-revalidate` and
//!   `no-cache` turn that off.
//! - `immutable` changes nothing. It exists to stop a browser's *reload* from
//!   revalidating fresh subresources, and nothing here reloads a page.
//!
//! ## What is stored
//!
//! Only the answer to a `GET`, and only a full one:
//! - never a 206, a 304 or a 1xx;
//! - never `no-store` or `Vary: *`;
//! - never an entry bigger than one eighth of the cap.
//!
//! Beyond that, the status must be cacheable by default (200, 203, 204, 300,
//! 301, 308, 404, 405, 410, 414, 501), or the answer must say so explicitly
//! (`max-age`, `Expires`, `public`, `private`). This is a private cache, so
//! `private` and requests with `Authorization` are fine.
//!
//! Before an answer is stored, `Set-Cookie` and the hop-by-hop headers are
//! removed. The jar already has the cookies, and a cached answer must never
//! set them again.
//!
//! **One divergence:** an answer that could only ever be served by
//! `force-cache` or `only-if-cached` is not stored. That is an answer with a
//! lifetime of 0, no validator and no `stale-while-revalidate`, which is every
//! API response without caching headers. Chromium stores those and rewrites
//! them on every request. Here they cost no disk write, and a `force-cache`
//! that wanted one goes to the network instead.
//!
//! Range requests bypass the cache entirely. Chromium keeps sparse entries
//! for them; here they are not looked up, not stored and do not invalidate
//! anything. HEAD, OPTIONS and TRACE bypass it too. The other methods
//! (unsafe) invalidate the URL on a 2xx/3xx answer, and so do the
//! same-origin `Location`/`Content-Location` of that answer.
//!
//! `Vary` is honoured. One entry per URL (as in Chromium), and the request
//! values it varied on are kept as SHA-256 digests, never raw. A `Cookie` or
//! `Authorization` value never reaches the disk this way.
//!
//! ## Modes (Fetch `cache`)
//!
//! | mode | looks up | uses | stores |
//! |---|---|---|---|
//! | `default` | yes | fresh; stale when SWR allows; stale with validators after a 304 | yes |
//! | `no-store` | no | — | no |
//! | `reload` | no | — | yes |
//! | `no-cache` | yes | only after a 304 | yes |
//! | `force-cache` | yes | any match, stale or not | yes |
//! | `only-if-cached` | yes | any match; no match is a network error | no request is made |
//!
//! In `default` mode, a request that carries its own conditional header
//! (`If-None-Match`, `If-Modified-Since`, `If-Match`, `If-Unmodified-Since`,
//! `If-Range`) is treated as `no-store`, as Fetch says. Its 304 is the
//! script's to read.
//!
//! The request's own `Cache-Control`/`Pragma` count too, as Chromium counts
//! them:
//! - `no-cache` behaves as `reload`;
//! - `max-age=0` behaves as `no-cache`;
//! - `no-store` behaves as `no-store`.
//!
//! The other request directives (`max-stale`, `min-fresh`, other `max-age`
//! values) are ignored, as they are in Chromium.
//!
//! [`add_mode_headers`] puts Fetch's request headers on an explicit mode:
//! - `no-cache` sends `Cache-Control: max-age=0`;
//! - `no-store` and `reload` send `Pragma: no-cache` and
//!   `Cache-Control: no-cache`.
//!
//! **Divergence from the spec:** these headers are not added when the mode
//! was only *derived* from a conditional header. The spec's letter would add
//! `Cache-Control: no-cache` to a script's own revalidation.
//!
//! ## On disk
//!
//! One folder, given by the embedder (skathi uses
//! `%LOCALAPPDATA%\<app>\cache\http`):
//!
//! ```text
//!   <32 hex>.e   one entry: the first 16 bytes of SHA-256(URL without fragment)
//!   index        LRU order: (hash, size, last used) per entry
//!   mode         "plain" or "sealed <tag>"; a change purges every entry
//!   lock         held by the instance that writes
//! ```
//!
//! An entry is written to `<hash>.<pid>.tmp` and renamed over the old one.
//! A crash leaves either the old entry or the new one, never half of one.
//! The payload ends in a CRC-32, so an entry torn by a power cut reads as
//! corrupt and is deleted, not served. URLs never appear in file names.
//!
//! Writes, deletions and eviction run on one writer thread, `redjs-http-cache`.
//! A lookup is one file read on the requesting thread. The pooled transport
//! moves that read to its blocking pool, so it never holds up the network
//! thread.
//!
//! The owning instance reads only what its in-memory entry list holds. It
//! wrote every entry there is, so a URL the list lacks is a miss without a
//! file open. On the pooled transport it also skips the trip through the
//! blocking pool: a first load's redirect continues at once, on the
//! connection its first hop just freed, as it does with no cache. A
//! read-only instance always looks, since the owner keeps writing.
//!
//! - **Index.** Rewritten every 30 s when something changed, and at
//!   [`flush`]/[`uninstall`]. It holds only the LRU order: [`install`]
//!   rebuilds the entry list from the folder itself, so a lost or stale index
//!   costs recency and nothing else.
//! - **Eviction.** Least recently used first. When a write takes the folder
//!   over its cap, entries are evicted until it is back under 90%.
//! - **Two instances.** The first takes `lock` (exclusively, on Windows) and
//!   writes. A second one reads entries and serves hits but never writes,
//!   evicts or purges, and [`Stats::writable`] says so. Elsewhere, every
//!   instance writes, and only the atomic renames protect them.
//!
//! ## Encryption
//!
//! The embedder decides, with a [`Sealer`] in [`Config`], whose
//! `seal`/`open` wrap every entry's payload. redjs carries no cipher.
//! skathi's sealer follows the app's storage encryption setting
//! (sk-storage, docs/STORAGE.md). A different sealer tag than the one the
//! folder was written with purges the folder at install: a cache is cheaper
//! to refill than to migrate. An entry that fails to open is deleted and
//! counted in [`Stats::errors`].

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::{header, HttpError, Request};

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/// Fetch's `RequestCache`. Plain transports (XHR, an embedder's loaders) use
/// [`CacheMode::Default`], as a browser's do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheMode {
    #[default]
    Default,
    NoStore,
    Reload,
    NoCache,
    ForceCache,
    OnlyIfCached,
}

impl CacheMode {
    pub fn parse(s: &str) -> Option<CacheMode> {
        Some(match s {
            "default" => CacheMode::Default,
            "no-store" => CacheMode::NoStore,
            "reload" => CacheMode::Reload,
            "no-cache" => CacheMode::NoCache,
            "force-cache" => CacheMode::ForceCache,
            "only-if-cached" => CacheMode::OnlyIfCached,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CacheMode::Default => "default",
            CacheMode::NoStore => "no-store",
            CacheMode::Reload => "reload",
            CacheMode::NoCache => "no-cache",
            CacheMode::ForceCache => "force-cache",
            CacheMode::OnlyIfCached => "only-if-cached",
        }
    }
}

/// What the cache did for a response's final hop, for an embedder's devtools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Outcome {
    /// Not consulted: no cache is installed, or the request cannot use one
    /// (its method, a `Range`, `no-store`).
    #[default]
    Off,
    /// Consulted; the answer came from the network and was not stored.
    Miss,
    /// From the network, and stored.
    Stored,
    /// From disk; no request was made (stale-while-revalidate included).
    Hit,
    /// From disk, after the server answered 304 to a conditional request.
    Revalidated,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Off => "off",
            Outcome::Miss => "miss",
            Outcome::Stored => "stored",
            Outcome::Hit => "hit",
            Outcome::Revalidated => "revalidated",
        }
    }
}

/// The embedder's cipher for entries at rest. Whole-payload: `seal` gets an
/// entry's serialized headers and body, and `open` must return exactly that,
/// or an error for anything it did not seal (wrong key, tampered bytes).
pub trait Sealer: Send + Sync {
    /// Names the cipher AND the key. The folder records it, and a folder
    /// written under another tag is purged at [`install`].
    fn tag(&self) -> String;
    fn seal(&self, plain: &[u8]) -> Result<Vec<u8>, String>;
    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, String>;
}

pub struct Config {
    pub dir: PathBuf,
    /// The cap on the entries' total size on disk. Chromium's default is
    /// computed from the free space; skathi's is a fixed 256 MB.
    pub max_bytes: u64,
    /// `None` stores entries as plaintext, as every browser does.
    pub sealer: Option<Arc<dyn Sealer>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub dir: PathBuf,
    pub max_bytes: u64,
    pub entries: u64,
    pub bytes: u64,
    /// This instance owns the folder: it stores, evicts and purges. `false`:
    /// another instance does, and this one only serves what is there.
    pub writable: bool,
    /// Entries are sealed by the embedder's [`Sealer`].
    pub sealed: bool,
    pub hits: u64,
    pub revalidated: u64,
    pub stored: u64,
    pub misses: u64,
    /// Hops that could not use the cache (method, `Range`, `no-store`).
    pub bypassed: u64,
    pub evicted: u64,
    /// Entries that could not be read, opened or written.
    pub errors: u64,
}

/// Turns the cache on over `cfg.dir`, replacing any cache installed before
/// (which is flushed first). Creates the folder, takes the writer's lock if
/// no other instance holds it, purges the folder when the sealer's tag (or
/// its absence) differs from what wrote it, and reconciles the index with
/// the files.
pub fn install(cfg: Config) -> Result<Stats, String> {
    uninstall();
    let store = Store::open(cfg)?;
    let stats = store.stats();
    *STORE.write().unwrap() = Some(store);
    Ok(stats)
}

/// Turns the cache off: the index is written, the writer thread ends, the
/// lock is released. Requests in flight finish without it.
pub fn uninstall() {
    let old = STORE.write().unwrap().take();
    if let Some(s) = old {
        s.stop();
    }
}

pub fn installed() -> bool {
    STORE.read().unwrap().is_some()
}

pub fn stats() -> Option<Stats> {
    current().map(|s| s.stats())
}

/// Deletes every entry. Returns the bytes freed. An instance that does not
/// own the folder cannot, and says why.
pub fn clear() -> Result<u64, String> {
    let s = current().ok_or_else(|| String::from("no HTTP cache is installed"))?;
    if !s.writable {
        return Err(String::from("another instance owns the HTTP cache folder; only it can clear it"));
    }
    let (tx, rx) = mpsc::channel();
    s.send(Job::Clear(tx));
    rx.recv_timeout(Duration::from_secs(10)).map_err(|_| String::from("the cache writer did not answer"))
}

/// Writes the index now, and waits (≤ 2 s) for writes already queued. For an
/// embedder's quit path.
pub fn flush() {
    if let Some(s) = current() {
        s.flush();
    }
}

// ---------------------------------------------------------------------------
// Per hop: the transports' side
// ---------------------------------------------------------------------------

/// What one hop does.
pub(crate) enum Action {
    /// The network, as if there were no cache. [`settle`] may still store
    /// the answer, or invalidate.
    Network,
    /// Answer from disk. `revalidate`: stale-while-revalidate, refresh the
    /// entry in the background ([`revalidate_in_background`]).
    Serve { entry: Entry, revalidate: bool },
    /// Ask the network with these conditional headers added. A 304 serves
    /// `entry`, refreshed.
    Validate { entry: Entry, conditional: Vec<(String, String)> },
    /// `only-if-cached`, and nothing usable.
    Fail(HttpError),
}

/// [`plan`]'s answer: the action, and what [`settle`] needs afterwards.
pub(crate) struct Planned {
    pub(crate) action: Action,
    ctx: Option<Ctx>,
}

impl Planned {
    fn off() -> Planned {
        Planned { action: Action::Network, ctx: None }
    }

    pub(crate) fn into_parts(self) -> (Action, Option<Ctx>) {
        (self.action, self.ctx)
    }
}

/// One hop's cache context, carried from [`plan`] to [`settle`].
pub(crate) struct Ctx {
    store: Arc<Store>,
    key: String,
    hash: Hash,
    url: String,
    /// GET in a mode that stores. `false` with `safe: true` cannot happen
    /// (such a hop gets no context at all).
    store_ok: bool,
    /// The method may change state on the server (POST, PUT, DELETE, …).
    unsafe_method: bool,
    /// Digests of the request headers, by lower-case name, for `Vary`.
    request_headers: Vec<(String, String)>,
    request_time: i64,
}

/// The entry an answer came from, or the one a 304 refreshed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) key: String,
    pub(crate) status: u16,
    pub(crate) reason: String,
    pub(crate) request_time: i64,
    pub(crate) response_time: i64,
    /// Lower-case names; the SHA-256 of each request value (absent → `None`).
    pub(crate) vary: Vec<(String, Option<[u8; 32]>)>,
    pub(crate) headers: Vec<(String, String)>,
    /// The body is as the server sent it (still content-coded; the pooled
    /// transport's form). `false`: already decoded (the blocking transport's).
    pub(crate) coded: bool,
    pub(crate) body: Vec<u8>,
}

/// What a hop's network answer became.
pub(crate) enum Settled {
    /// Serve the network's answer as received; the cache did this with it.
    Network(Outcome),
    /// A 304 confirmed this entry (headers merged, times renewed): serve it.
    Refreshed(Entry),
}

/// Will [`plan`] read the disk for this hop? The pooled transport moves
/// those calls to its blocking pool; every other plan is instant. (The
/// blocking transport is on a worker thread already and does not ask.)
#[cfg_attr(not(feature = "net-h2"), allow(dead_code))]
pub(crate) fn reads_disk(method: &str, url: &str, headers: &[(String, String)], mode: CacheMode) -> bool {
    method.eq_ignore_ascii_case("GET")
        && !has(headers, "range")
        && !matches!(effective_mode(mode, headers), CacheMode::NoStore | CacheMode::Reload)
        && current().is_some_and(|s| key_of(url).is_some_and(|k| s.may_have(hash_of(&k))))
}

/// Decides one hop. `headers` are the hop's request headers as they will be
/// sent (the jar's `Cookie` included), `url` its URL. Reads at most one file.
pub(crate) fn plan(method: &str, url: &str, headers: &[(String, String)], mode: CacheMode) -> Planned {
    let Some(store) = current() else { return Planned::off() };
    let method = method.to_ascii_uppercase();
    let Some(key) = key_of(url) else { return Planned::off() };
    if has(headers, "range") {
        store.c.bypassed.fetch_add(1, Ordering::Relaxed);
        return Planned::off();
    }
    let unsafe_method = !matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS" | "TRACE");
    let hash = hash_of(&key);
    let ctx = |store_ok: bool| Ctx {
        store: Arc::clone(&store),
        key: key.clone(),
        hash,
        url: url.to_string(),
        store_ok,
        unsafe_method,
        request_headers: headers.to_vec(),
        request_time: now_secs(),
    };
    if unsafe_method {
        // Not cacheable here, but its answer may invalidate (settle).
        return Planned { action: Action::Network, ctx: Some(ctx(false)) };
    }
    let mode = effective_mode(mode, headers);
    if method != "GET" || mode == CacheMode::NoStore {
        store.c.bypassed.fetch_add(1, Ordering::Relaxed);
        return Planned::off();
    }
    if mode == CacheMode::Reload {
        return Planned { action: Action::Network, ctx: Some(ctx(true)) };
    }
    let only = mode == CacheMode::OnlyIfCached;
    let found = store.read(hash, &key).filter(|e| vary_matches(e, headers));
    let Some(entry) = found else {
        if only {
            store.c.misses.fetch_add(1, Ordering::Relaxed);
            return Planned {
                action: Action::Fail(HttpError::network(format!(
                    "{url} is not in the HTTP cache, and the request said cache: \"only-if-cached\""
                ))),
                ctx: None,
            };
        }
        return Planned { action: Action::Network, ctx: Some(ctx(true)) };
    };
    let action = match mode {
        CacheMode::ForceCache | CacheMode::OnlyIfCached => Action::Serve { entry, revalidate: false },
        CacheMode::NoCache => match conditional_headers(&entry) {
            Some(conditional) => Action::Validate { entry, conditional },
            None => Action::Network,
        },
        _ => {
            let now = now_secs();
            let cc = Cc::of(&entry.headers);
            let lifetime = freshness_lifetime(&entry, &cc);
            let age = current_age(&entry, now);
            if age < lifetime {
                Action::Serve { entry, revalidate: false }
            } else if !cc.must_revalidate
                && !cc.no_cache
                && cc.swr.is_some_and(|swr| age < lifetime.saturating_add(swr))
            {
                Action::Serve { entry, revalidate: true }
            } else {
                match conditional_headers(&entry) {
                    Some(conditional) => Action::Validate { entry, conditional },
                    None => Action::Network,
                }
            }
        }
    };
    if matches!(action, Action::Serve { .. }) {
        store.c.hits.fetch_add(1, Ordering::Relaxed);
    }
    Planned { action, ctx: Some(ctx(!only)) }
}

/// A hop's network answer. `validating` is the entry a [`Action::Validate`]
/// asked about. `coded`: `body` is as received (see [`Entry::coded`]).
pub(crate) fn settle(
    ctx: Option<Ctx>,
    validating: Option<Entry>,
    status: u16,
    reason: &str,
    headers: &[(String, String)],
    body: &[u8],
    coded: bool,
) -> Settled {
    let Some(ctx) = ctx else { return Settled::Network(Outcome::Off) };
    let store = &ctx.store;
    if ctx.unsafe_method {
        if (200..400).contains(&status) {
            store.invalidate(&ctx.key);
            for name in ["location", "content-location"] {
                let Some(v) = header(headers, name) else { continue };
                let Ok(other) = super::resolve_location(&ctx.url, v) else { continue };
                if super::same_origin(&ctx.url, &other) {
                    if let Some(k) = key_of(&other) {
                        store.invalidate(&k);
                    }
                }
            }
        }
        return Settled::Network(Outcome::Off);
    }
    if let Some(mut entry) = validating {
        if status == 304 {
            merge_304(&mut entry, headers);
            entry.request_time = ctx.request_time;
            entry.response_time = now_secs();
            store.c.revalidated.fetch_add(1, Ordering::Relaxed);
            if ctx.store_ok {
                store.put(ctx.hash, &entry);
            }
            return Settled::Refreshed(entry);
        }
    }
    // A 304 here answered the script's own conditional request, not ours.
    if !ctx.store_ok || !store.writable || status == 304 {
        store.c.misses.fetch_add(1, Ordering::Relaxed);
        return Settled::Network(Outcome::Miss);
    }
    let entry = Entry {
        key: ctx.key.clone(),
        status,
        reason: reason.to_string(),
        request_time: ctx.request_time,
        response_time: now_secs(),
        vary: Vec::new(),
        headers: storable_headers(headers),
        coded,
        body: Vec::new(),
    };
    match worth_storing(&entry, headers, body.len() as u64, store.max_bytes) {
        Some(vary) => {
            let entry = Entry { vary: vary_digests(&vary, &ctx.request_headers), body: body.to_vec(), ..entry };
            store.put(ctx.hash, &entry);
            store.c.stored.fetch_add(1, Ordering::Relaxed);
            Settled::Network(Outcome::Stored)
        }
        None => {
            // An answer that replaces the entry but cannot be stored makes
            // the old one wrong (Chromium dooms it too).
            store.remove(ctx.hash);
            store.c.misses.fetch_add(1, Ordering::Relaxed);
            Settled::Network(Outcome::Miss)
        }
    }
}

/// stale-while-revalidate: refreshes `url` off the request's path, once per
/// URL at a time. The refresh is its own request in mode `no-cache`, with the
/// original's headers and credentials; nobody waits for it.
pub(crate) fn revalidate_in_background(req: &Request, url: &str) {
    static INFLIGHT: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let Some(key) = key_of(url) else { return };
    if !INFLIGHT.lock().unwrap().get_or_insert_with(HashSet::new).insert(key.clone()) {
        return;
    }
    let r = Request {
        method: "GET".into(),
        url: url.to_string(),
        headers: req.headers.clone(),
        body: None,
        redirect: super::Redirect::Follow,
        credentials: req.credentials,
        connect_timeout: req.connect_timeout,
        io_timeout: req.io_timeout,
        observer: None,
        cache: CacheMode::NoCache,
    };
    let done_key = key.clone();
    let started = super::start(
        r,
        (*super::Cancel::new()).clone(),
        Box::new(move |_| {
            if let Some(set) = INFLIGHT.lock().unwrap().as_mut() {
                set.remove(&done_key);
            }
        }),
    );
    if started.is_err() {
        if let Some(set) = INFLIGHT.lock().unwrap().as_mut() {
            set.remove(&key);
        }
    }
}

/// An [`super::Observe`]r sees an answer it was not shown on the wire (one
/// from disk, or the full answer to a conditional request, which goes out
/// unobserved because its 304 would not be the answer) as it would have seen
/// the network's: the head, then the body as one piece, as received. Only the
/// request's final answer is observed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn observe(
    req: &Request,
    url: &str,
    redirected: bool,
    protocol: &'static str,
    status: u16,
    reason: &str,
    headers: &[(String, String)],
    body: &[u8],
) {
    let Some(o) = &req.observer else { return };
    if !super::is_final(status, headers) {
        return;
    }
    o.0.head(&super::ProgressHead { status, status_text: reason, headers, url, redirected, protocol });
    if !body.is_empty() {
        let n = body.len() as u64;
        o.0.body(body, n, Some(n));
    }
}

/// Fetch's request headers for an explicitly chosen mode (see the module
/// header; not for a mode derived from a conditional header).
pub(crate) fn add_mode_headers(mode: CacheMode, headers: &mut Vec<(String, String)>) {
    match mode {
        CacheMode::NoCache => {
            if !has(headers, "cache-control") {
                headers.push(("cache-control".into(), "max-age=0".into()));
            }
        }
        CacheMode::NoStore | CacheMode::Reload => {
            if !has(headers, "pragma") {
                headers.push(("pragma".into(), "no-cache".into()));
            }
            if !has(headers, "cache-control") {
                headers.push(("cache-control".into(), "no-cache".into()));
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// The rules (pure)
// ---------------------------------------------------------------------------

/// The mode after the request's own headers had their say (module header).
fn effective_mode(mode: CacheMode, headers: &[(String, String)]) -> CacheMode {
    let conditional = ["if-none-match", "if-modified-since", "if-match", "if-unmodified-since", "if-range"]
        .iter()
        .any(|n| has(headers, n));
    if conditional && mode != CacheMode::OnlyIfCached {
        return CacheMode::NoStore;
    }
    if mode != CacheMode::Default {
        return mode;
    }
    let cc = Cc::of_named(headers, "cache-control");
    if cc.no_store {
        CacheMode::NoStore
    } else if cc.no_cache || pragma_no_cache(headers) {
        CacheMode::Reload
    } else if cc.max_age == Some(0) {
        CacheMode::NoCache
    } else {
        CacheMode::Default
    }
}

/// `Cache-Control` as one record. Repeated directives: the first wins.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Cc {
    max_age: Option<i64>,
    no_cache: bool,
    no_store: bool,
    must_revalidate: bool,
    swr: Option<i64>,
    public: bool,
    private: bool,
}

impl Cc {
    fn of(headers: &[(String, String)]) -> Cc {
        Cc::of_named(headers, "cache-control")
    }

    fn of_named(headers: &[(String, String)], name: &str) -> Cc {
        let mut cc = Cc::default();
        for (k, v) in headers {
            if !k.eq_ignore_ascii_case(name) {
                continue;
            }
            for d in split_list(v) {
                let (n, val) = match d.split_once('=') {
                    Some((n, val)) => (n.trim().to_ascii_lowercase(), Some(val.trim().trim_matches('"'))),
                    None => (d.trim().to_ascii_lowercase(), None),
                };
                // A malformed number is a value of 0: stale, never "forever".
                let secs = |v: Option<&str>| v.map(|s| s.parse::<i64>().unwrap_or(0).max(0));
                match n.as_str() {
                    "max-age" if cc.max_age.is_none() => cc.max_age = Some(secs(val).unwrap_or(0)),
                    "stale-while-revalidate" if cc.swr.is_none() => cc.swr = secs(val),
                    "no-cache" => cc.no_cache = true,
                    "no-store" => cc.no_store = true,
                    "must-revalidate" => cc.must_revalidate = true,
                    "public" => cc.public = true,
                    "private" => cc.private = true,
                    _ => {}
                }
            }
        }
        cc
    }
}

/// A comma-separated header list, commas inside quotes kept.
fn split_list(v: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut quoted) = (0, false);
    for (i, c) in v.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                out.push(&v[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&v[start..]);
    out.into_iter().map(str::trim).filter(|s| !s.is_empty()).collect()
}

fn pragma_no_cache(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("pragma"))
        .any(|(_, v)| split_list(v).iter().any(|d| d.eq_ignore_ascii_case("no-cache")))
}

fn has(headers: &[(String, String)], name: &str) -> bool {
    headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name))
}

fn date(headers: &[(String, String)], name: &str) -> Option<i64> {
    header(headers, name).and_then(crate::webapi::cookies::parse_cookie_date)
}

/// RFC 9111 §4.2.1, with Chromium's heuristics (module header). Seconds;
/// `i64::MAX` is forever.
fn freshness_lifetime(e: &Entry, cc: &Cc) -> i64 {
    if cc.no_cache || cc.no_store || (!has(&e.headers, "cache-control") && pragma_no_cache(&e.headers)) {
        return 0;
    }
    if let Some(m) = cc.max_age {
        return m;
    }
    let served = date(&e.headers, "date").unwrap_or(e.response_time);
    if has(&e.headers, "expires") {
        return date(&e.headers, "expires").map_or(0, |x| (x - served).max(0));
    }
    if matches!(e.status, 200 | 203) && !cc.must_revalidate {
        if let Some(lm) = date(&e.headers, "last-modified") {
            if lm <= served {
                return (served - lm) / 10;
            }
        }
    }
    if matches!(e.status, 300 | 301 | 308 | 410) {
        return i64::MAX;
    }
    0
}

/// RFC 9111 §4.2.3. Seconds.
fn current_age(e: &Entry, now: i64) -> i64 {
    let served = date(&e.headers, "date").unwrap_or(e.response_time);
    let age_value = header(&e.headers, "age").and_then(|v| v.trim().parse::<i64>().ok()).unwrap_or(0).max(0);
    let apparent = (e.response_time - served).max(0);
    let delay = (e.response_time - e.request_time).max(0);
    let initial = apparent.max(age_value.saturating_add(delay));
    initial.saturating_add((now - e.response_time).max(0))
}

fn conditional_headers(e: &Entry) -> Option<Vec<(String, String)>> {
    let mut out = Vec::new();
    if let Some(etag) = header(&e.headers, "etag") {
        out.push(("if-none-match".to_string(), etag.to_string()));
    }
    if let Some(lm) = header(&e.headers, "last-modified") {
        out.push(("if-modified-since".to_string(), lm.to_string()));
    }
    (!out.is_empty()).then_some(out)
}

const HEURISTIC: &[u16] = &[200, 203, 204, 300, 301, 308, 404, 405, 410, 414, 501];

/// `Some(vary names)` when the answer is stored (module header, "What is
/// stored"), `None` otherwise.
fn worth_storing(e: &Entry, raw_headers: &[(String, String)], body_len: u64, max_bytes: u64) -> Option<Vec<String>> {
    let cc = Cc::of(raw_headers);
    if cc.no_store || e.status < 200 || e.status == 206 || e.status == 304 {
        return None;
    }
    if body_len > max_bytes / 8 {
        return None;
    }
    let vary: Vec<String> = raw_headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("vary"))
        .flat_map(|(_, v)| split_list(v).into_iter().map(|s| s.to_ascii_lowercase()).collect::<Vec<_>>())
        .collect();
    if vary.iter().any(|v| v == "*") {
        return None;
    }
    let explicit = cc.max_age.is_some() || has(raw_headers, "expires") || cc.public || cc.private;
    if !explicit && !HEURISTIC.contains(&e.status) {
        return None;
    }
    // Could anything but force-cache ever serve it? (the one divergence)
    let lifetime = freshness_lifetime(e, &cc);
    let validators = has(raw_headers, "etag") || has(raw_headers, "last-modified");
    if lifetime == 0 && !validators && cc.swr.is_none() {
        return None;
    }
    Some(vary)
}

/// The headers an entry keeps: no `Set-Cookie`, nothing hop-by-hop.
fn storable_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let named: Vec<String> = headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("connection"))
        .flat_map(|(_, v)| split_list(v).into_iter().map(|s| s.to_ascii_lowercase()).collect::<Vec<_>>())
        .collect();
    headers
        .iter()
        .filter(|(k, _)| {
            let k = k.to_ascii_lowercase();
            !matches!(
                k.as_str(),
                "set-cookie"
                    | "set-cookie2"
                    | "connection"
                    | "keep-alive"
                    | "proxy-connection"
                    | "proxy-authenticate"
                    | "proxy-authorization"
                    | "te"
                    | "trailer"
                    | "transfer-encoding"
                    | "upgrade"
            ) && !named.contains(&k)
        })
        .cloned()
        .collect()
}

/// Headers a 304 never overwrites: Chromium's list
/// (`HttpResponseHeaders::Update`). Above all, a 304 does not describe the
/// stored BODY (`content-*`, `etag`).
fn non_updatable(lower: &str) -> bool {
    matches!(
        lower,
        "connection"
            | "proxy-connection"
            | "keep-alive"
            | "www-authenticate"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-location"
            | "content-md5"
            | "etag"
            | "content-encoding"
            | "content-range"
            | "content-type"
            | "content-length"
            | "x-frame-options"
            | "x-xss-protection"
            | "set-cookie"
            | "set-cookie2"
    ) || lower.starts_with("x-content-")
        || lower.starts_with("x-webkit-")
}

fn merge_304(e: &mut Entry, headers: &[(String, String)]) {
    let fresh: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| !non_updatable(&k.to_ascii_lowercase()))
        .cloned()
        .collect();
    let names: HashSet<String> = fresh.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();
    e.headers.retain(|(k, _)| !names.contains(&k.to_ascii_lowercase()));
    e.headers.extend(fresh);
}

/// A request header's value as `Vary` compares it: every line of that name,
/// trimmed and joined.
fn request_value(headers: &[(String, String)], name: &str) -> Option<String> {
    let vals: Vec<&str> = headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim()).collect();
    (!vals.is_empty()).then(|| vals.join(", "))
}

fn digest(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

fn vary_digests(names: &[String], request: &[(String, String)]) -> Vec<(String, Option<[u8; 32]>)> {
    names.iter().map(|n| (n.clone(), request_value(request, n).map(|v| digest(&v)))).collect()
}

fn vary_matches(e: &Entry, request: &[(String, String)]) -> bool {
    e.vary.iter().all(|(n, d)| request_value(request, n).map(|v| digest(&v)) == *d)
}

/// The cache key: the URL without its fragment. `None` for a URL that does
/// not parse (the transport refuses those anyway).
fn key_of(url: &str) -> Option<String> {
    let mut u = url::Url::parse(url).ok()?;
    u.set_fragment(None);
    Some(u.to_string())
}

type Hash = [u8; 16];

fn hash_of(key: &str) -> Hash {
    let d = Sha256::digest(key.as_bytes());
    let mut h = [0u8; 16];
    h.copy_from_slice(&d[..16]);
    h
}

fn hex(h: &Hash) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Hash> {
    if s.len() != 32 {
        return None;
    }
    let mut h = [0u8; 16];
    for (i, b) in h.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(h)
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

// ---------------------------------------------------------------------------
// The entry format
// ---------------------------------------------------------------------------

const ENTRY_MAGIC: &[u8; 4] = b"VNHC";
const INDEX_MAGIC: &[u8; 4] = b"VNHI";
const VERSION: u8 = 1;
const FLAG_SEALED: u8 = 1;
const HEAD_LEN: usize = 8;

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u64(b.len() as u64);
        self.0.extend_from_slice(b);
    }
    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
}

struct R<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(n).filter(|e| *e <= self.b.len()).ok_or("truncated entry")?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self) -> Result<&'a [u8], String> {
        let n = self.u64()?;
        self.take(usize::try_from(n).map_err(|_| "entry length overflows")?)
    }
    fn str(&mut self) -> Result<String, String> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| String::from("entry text is not UTF-8"))
    }
}

fn crc32(b: &[u8]) -> u32 {
    let mut c = flate2::Crc::new();
    c.update(b);
    c.sum()
}

/// The payload: everything about the entry, then its CRC-32.
fn encode_payload(e: &Entry) -> Vec<u8> {
    let mut w = W(Vec::with_capacity(e.body.len() + 512));
    w.str(&e.key);
    w.u16(e.status);
    w.str(&e.reason);
    w.u64(e.request_time as u64);
    w.u64(e.response_time as u64);
    w.u32(e.vary.len() as u32);
    for (n, d) in &e.vary {
        w.str(n);
        match d {
            Some(d) => {
                w.u8(1);
                w.0.extend_from_slice(d);
            }
            None => w.u8(0),
        }
    }
    w.u32(e.headers.len() as u32);
    for (k, v) in &e.headers {
        w.str(k);
        w.str(v);
    }
    w.u8(e.coded as u8);
    w.bytes(&e.body);
    let crc = crc32(&w.0);
    w.u32(crc);
    w.0
}

fn decode_payload(p: &[u8]) -> Result<Entry, String> {
    if p.len() < 4 {
        return Err("truncated entry".into());
    }
    let (fields, tail) = p.split_at(p.len() - 4);
    if crc32(fields) != u32::from_le_bytes(tail.try_into().unwrap()) {
        return Err("CRC mismatch (a torn or damaged entry)".into());
    }
    let mut r = R { b: fields, at: 0 };
    let key = r.str()?;
    let status = r.u16()?;
    let reason = r.str()?;
    let request_time = r.u64()? as i64;
    let response_time = r.u64()? as i64;
    let mut vary = Vec::new();
    for _ in 0..r.u32()? {
        let n = r.str()?;
        let d = match r.u8()? {
            1 => Some(<[u8; 32]>::try_from(r.take(32)?).unwrap()),
            _ => None,
        };
        vary.push((n, d));
    }
    let mut headers = Vec::new();
    for _ in 0..r.u32()? {
        let k = r.str()?;
        let v = r.str()?;
        headers.push((k, v));
    }
    let coded = r.u8()? != 0;
    let body = r.bytes()?.to_vec();
    Ok(Entry { key, status, reason, request_time, response_time, vary, headers, coded, body })
}

fn encode_file(payload: Vec<u8>, sealer: Option<&dyn Sealer>) -> Result<Vec<u8>, String> {
    let (flags, payload) = match sealer {
        Some(s) => (FLAG_SEALED, s.seal(&payload)?),
        None => (0, payload),
    };
    let mut out = Vec::with_capacity(HEAD_LEN + payload.len());
    out.extend_from_slice(ENTRY_MAGIC);
    out.push(VERSION);
    out.push(flags);
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&payload);
    Ok(out)
}

fn decode_file(bytes: &[u8], sealer: Option<&dyn Sealer>) -> Result<Entry, String> {
    if bytes.len() < HEAD_LEN || &bytes[..4] != ENTRY_MAGIC {
        return Err("not a cache entry".into());
    }
    if bytes[4] != VERSION {
        return Err(format!("entry format {} (this build reads {VERSION})", bytes[4]));
    }
    let sealed = bytes[5] & FLAG_SEALED != 0;
    let body = &bytes[HEAD_LEN..];
    match (sealed, sealer) {
        (true, Some(s)) => decode_payload(&s.open(body)?),
        (false, None) => decode_payload(body),
        (true, None) => Err("a sealed entry in a plaintext cache".into()),
        (false, Some(_)) => Err("a plaintext entry in a sealed cache".into()),
    }
}

fn encode_index(map: &HashMap<Hash, Meta>) -> Vec<u8> {
    let mut w = W(Vec::with_capacity(16 + map.len() * 32));
    w.0.extend_from_slice(INDEX_MAGIC);
    w.u8(VERSION);
    w.u32(map.len() as u32);
    for (h, m) in map {
        w.0.extend_from_slice(h);
        w.u64(m.size);
        w.u64(m.used);
    }
    let crc = crc32(&w.0);
    w.u32(crc);
    w.0
}

/// Last-used times by hash; `None` for a missing or damaged index.
fn decode_index(b: &[u8]) -> Option<HashMap<Hash, u64>> {
    if b.len() < 13 || &b[..4] != INDEX_MAGIC || b[4] != VERSION {
        return None;
    }
    let (fields, tail) = b.split_at(b.len() - 4);
    if crc32(fields) != u32::from_le_bytes(tail.try_into().ok()?) {
        return None;
    }
    let mut r = R { b: fields, at: 5 };
    let n = r.u32().ok()?;
    let mut out = HashMap::with_capacity(n as usize);
    for _ in 0..n {
        let h: Hash = r.take(16).ok()?.try_into().ok()?;
        let _size = r.u64().ok()?;
        out.insert(h, r.u64().ok()?);
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// How often a changed index is rewritten.
const INDEX_EVERY: Duration = Duration::from_secs(30);

static STORE: RwLock<Option<Arc<Store>>> = RwLock::new(None);

fn current() -> Option<Arc<Store>> {
    STORE.read().unwrap().clone()
}

#[derive(Clone, Copy, Debug)]
struct Meta {
    size: u64,
    /// Unix ms.
    used: u64,
}

#[derive(Default)]
struct Index {
    map: HashMap<Hash, Meta>,
    bytes: u64,
    dirty: bool,
}

impl Index {
    fn set(&mut self, h: Hash, m: Meta) {
        if let Some(old) = self.map.insert(h, m) {
            self.bytes -= old.size;
        }
        self.bytes += m.size;
        self.dirty = true;
    }

    fn drop_entry(&mut self, h: &Hash) -> Option<Meta> {
        let m = self.map.remove(h)?;
        self.bytes -= m.size;
        self.dirty = true;
        Some(m)
    }
}

#[derive(Default)]
struct Counters {
    hits: AtomicU64,
    revalidated: AtomicU64,
    stored: AtomicU64,
    misses: AtomicU64,
    bypassed: AtomicU64,
    evicted: AtomicU64,
    errors: AtomicU64,
}

enum Job {
    Put(Hash, Vec<u8>),
    Remove(Hash),
    Clear(mpsc::Sender<u64>),
    Flush(mpsc::Sender<()>),
    Stop,
}

pub(crate) struct Store {
    dir: PathBuf,
    max_bytes: u64,
    sealer: Option<Arc<dyn Sealer>>,
    writable: bool,
    index: Mutex<Index>,
    jobs: Mutex<Option<mpsc::Sender<Job>>>,
    writer: Mutex<Option<std::thread::JoinHandle<()>>>,
    lock: Mutex<Option<File>>,
    c: Counters,
}

/// Takes `<dir>\lock` for this process; `None` when another instance holds it.
fn take_lock(dir: &Path) -> Option<File> {
    let mut o = OpenOptions::new();
    o.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        o.share_mode(1 /* FILE_SHARE_READ: others may read who holds it, nobody may take it */);
    }
    let mut f = o.open(dir.join("lock")).ok()?;
    let _ = f.set_len(0);
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    let _ = write!(f, "pid {}\nexe {exe}\n", std::process::id());
    Some(f)
}

fn mode_line(sealer: Option<&dyn Sealer>) -> String {
    match sealer {
        Some(s) => format!("sealed {}", s.tag()),
        None => String::from("plain"),
    }
}

impl Store {
    fn open(cfg: Config) -> Result<Arc<Store>, String> {
        let dir = cfg.dir;
        fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
        let lock = take_lock(&dir);
        let writable = lock.is_some();
        let mode = mode_line(cfg.sealer.as_deref());
        let on_disk = fs::read_to_string(dir.join("mode")).ok().map(|s| s.trim().to_string());
        if on_disk.as_deref() != Some(mode.as_str()) {
            if writable {
                purge(&dir);
                write_atomic(&dir.join("mode"), format!("{mode}\n").as_bytes())
                    .map_err(|e| format!("could not write {}: {e}", dir.join("mode").display()))?;
            } else if on_disk.is_some() {
                return Err(format!(
                    "the instance that owns {} keeps it in another mode ({}); this one runs without a cache",
                    dir.display(),
                    on_disk.unwrap_or_default()
                ));
            }
        }
        // The files are the truth; the index only remembers when each was used.
        let saved = fs::read(dir.join("index")).ok().and_then(|b| decode_index(&b)).unwrap_or_default();
        let mut index = Index::default();
        if let Ok(rd) = fs::read_dir(&dir) {
            for ent in rd.flatten() {
                let name = ent.file_name().to_string_lossy().into_owned();
                if name.ends_with(".tmp") {
                    if writable {
                        let _ = fs::remove_file(ent.path());
                    }
                    continue;
                }
                let Some(h) = name.strip_suffix(".e").and_then(unhex) else { continue };
                let Ok(md) = ent.metadata() else { continue };
                let mtime = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_millis() as u64);
                index.set(h, Meta { size: md.len(), used: saved.get(&h).copied().unwrap_or(mtime) });
            }
        }
        index.dirty = false;
        let store = Arc::new(Store {
            dir,
            max_bytes: cfg.max_bytes.max(1),
            sealer: cfg.sealer,
            writable,
            index: Mutex::new(index),
            jobs: Mutex::new(None),
            writer: Mutex::new(None),
            lock: Mutex::new(lock),
            c: Counters::default(),
        });
        if writable {
            store.evict();
            let (tx, rx) = mpsc::channel();
            let w = Arc::clone(&store);
            let handle = std::thread::Builder::new()
                .name("redjs-http-cache".into())
                .spawn(move || w.run_writer(rx))
                .map_err(|e| format!("the cache writer could not be started: {e}"))?;
            *store.jobs.lock().unwrap() = Some(tx);
            *store.writer.lock().unwrap() = Some(handle);
        }
        Ok(store)
    }

    fn path(&self, h: Hash) -> PathBuf {
        self.dir.join(format!("{}.e", hex(&h)))
    }

    fn stats(&self) -> Stats {
        let ix = self.index.lock().unwrap();
        let n = |a: &AtomicU64| a.load(Ordering::Relaxed);
        Stats {
            dir: self.dir.clone(),
            max_bytes: self.max_bytes,
            entries: ix.map.len() as u64,
            bytes: ix.bytes,
            writable: self.writable,
            sealed: self.sealer.is_some(),
            hits: n(&self.c.hits),
            revalidated: n(&self.c.revalidated),
            stored: n(&self.c.stored),
            misses: n(&self.c.misses),
            bypassed: n(&self.c.bypassed),
            evicted: n(&self.c.evicted),
            errors: n(&self.c.errors),
        }
    }

    fn send(&self, job: Job) {
        if let Some(tx) = self.jobs.lock().unwrap().as_ref() {
            let _ = tx.send(job);
        }
    }

    /// Can there be an entry file for `h`? The owner's list is complete: it
    /// was read from the folder at install, and every write, removal and
    /// eviction since is this instance's own and updates it (a write after
    /// its rename, a removal before its delete). A read-only instance cannot
    /// know what the owner wrote since, so it always looks.
    fn may_have(&self, h: Hash) -> bool {
        !self.writable || self.index.lock().unwrap().map.contains_key(&h)
    }

    /// One entry, if it is there, readable and for this key.
    fn read(&self, h: Hash, key: &str) -> Option<Entry> {
        if !self.may_have(h) {
            return None;
        }
        let bytes = match fs::read(self.path(h)) {
            Ok(b) => b,
            Err(e) => {
                // Deleted behind the cache's back: unlisted, so the next lookup
                // does not open it again.
                if self.writable && e.kind() == std::io::ErrorKind::NotFound {
                    self.index.lock().unwrap().drop_entry(&h);
                }
                return None;
            }
        };
        match decode_file(&bytes, self.sealer.as_deref()) {
            Ok(e) if e.key == key => {
                let mut ix = self.index.lock().unwrap();
                let used = now_ms();
                match ix.map.get_mut(&h) {
                    Some(m) => {
                        m.used = used;
                        ix.dirty = true;
                    }
                    // How a read-only instance learns of the owner's entries.
                    None if !self.writable => ix.set(h, Meta { size: bytes.len() as u64, used }),
                    // The owner evicted or removed it during the read: it stays
                    // dropped, since its file is gone or going.
                    None => {}
                }
                Some(e)
            }
            // Another URL with the same 128-bit prefix: never in practice.
            Ok(_) => None,
            Err(why) => {
                self.c.errors.fetch_add(1, Ordering::Relaxed);
                if self.c.errors.load(Ordering::Relaxed) <= 5 {
                    eprintln!("[redjs:http-cache] dropping entry {}: {why}", hex(&h));
                }
                self.remove(h);
                None
            }
        }
    }

    fn put(&self, h: Hash, e: &Entry) {
        if self.writable {
            self.send(Job::Put(h, encode_payload(e)));
        }
    }

    fn remove(&self, h: Hash) {
        if self.writable {
            self.send(Job::Remove(h));
        }
    }

    fn invalidate(&self, key: &str) {
        self.remove(hash_of(key));
    }

    fn flush(&self) {
        let (tx, rx) = mpsc::channel();
        self.send(Job::Flush(tx));
        let _ = rx.recv_timeout(Duration::from_secs(2));
    }

    fn stop(&self) {
        self.send(Job::Stop);
        let handle = self.writer.lock().unwrap().take();
        if let Some(h) = handle {
            let _ = h.join();
        }
        *self.jobs.lock().unwrap() = None;
        *self.lock.lock().unwrap() = None;
    }

    fn run_writer(self: Arc<Self>, rx: mpsc::Receiver<Job>) {
        let mut last = Instant::now();
        loop {
            match rx.recv_timeout(INDEX_EVERY) {
                Ok(Job::Put(h, payload)) => self.write_entry(h, payload),
                Ok(Job::Remove(h)) => {
                    let had = self.index.lock().unwrap().drop_entry(&h).is_some();
                    if had || self.path(h).exists() {
                        let _ = fs::remove_file(self.path(h));
                    }
                }
                Ok(Job::Clear(ack)) => {
                    let freed = {
                        let mut ix = self.index.lock().unwrap();
                        let b = ix.bytes;
                        *ix = Index { dirty: true, ..Index::default() };
                        b
                    };
                    purge(&self.dir);
                    self.write_index();
                    let _ = ack.send(freed);
                }
                Ok(Job::Flush(ack)) => {
                    self.write_index();
                    let _ = ack.send(());
                }
                Ok(Job::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.write_index();
                    return;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if last.elapsed() >= INDEX_EVERY {
                if self.index.lock().unwrap().dirty {
                    self.write_index();
                }
                last = Instant::now();
            }
        }
    }

    fn write_entry(&self, h: Hash, payload: Vec<u8>) {
        let bytes = match encode_file(payload, self.sealer.as_deref()) {
            Ok(b) => b,
            Err(e) => {
                self.c.errors.fetch_add(1, Ordering::Relaxed);
                eprintln!("[redjs:http-cache] could not seal entry {}: {e}", hex(&h));
                return;
            }
        };
        let path = self.path(h);
        if let Err(e) = write_atomic(&path, &bytes) {
            self.c.errors.fetch_add(1, Ordering::Relaxed);
            if self.c.errors.load(Ordering::Relaxed) <= 5 {
                eprintln!("[redjs:http-cache] could not write {}: {e}", path.display());
            }
            return;
        }
        self.index.lock().unwrap().set(h, Meta { size: bytes.len() as u64, used: now_ms() });
        self.evict();
    }

    /// Over the cap: least recently used first, down to 90% of it.
    fn evict(&self) {
        let victims: Vec<Hash> = {
            let mut ix = self.index.lock().unwrap();
            if ix.bytes <= self.max_bytes {
                return;
            }
            let target = self.max_bytes / 10 * 9;
            let mut order: Vec<(Hash, Meta)> = ix.map.iter().map(|(h, m)| (*h, *m)).collect();
            order.sort_by_key(|(_, m)| m.used);
            let mut out = Vec::new();
            for (h, _) in order {
                if ix.bytes <= target {
                    break;
                }
                ix.drop_entry(&h);
                out.push(h);
            }
            out
        };
        self.c.evicted.fetch_add(victims.len() as u64, Ordering::Relaxed);
        for h in victims {
            let _ = fs::remove_file(self.path(h));
        }
    }

    fn write_index(&self) {
        let bytes = {
            let mut ix = self.index.lock().unwrap();
            ix.dirty = false;
            encode_index(&ix.map)
        };
        if let Err(e) = write_atomic(&self.dir.join("index"), &bytes) {
            self.c.errors.fetch_add(1, Ordering::Relaxed);
            eprintln!("[redjs:http-cache] could not write the index: {e}");
        }
    }
}

/// Every entry, the index and any temporary file; `mode` and `lock` stay.
fn purge(dir: &Path) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.ends_with(".e") || name.ends_with(".tmp") || name == "index" {
            let _ = fs::remove_file(ent.path());
        }
    }
}

/// To `<name>.<pid>.tmp`, then renamed over `path`. No fsync: a torn entry
/// fails its CRC and is dropped, which is all a cache needs.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!("{name}.{}.tmp", std::process::id()));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

// ---------------------------------------------------------------------------
// Tests: the rules and the format, no network and no global install
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn entry(status: u16, headers: &[(&str, &str)], at: i64) -> Entry {
        Entry {
            key: "https://x.test/a".into(),
            status,
            reason: "OK".into(),
            request_time: at,
            response_time: at,
            vary: vec![],
            headers: h(headers),
            coded: true,
            body: b"body".to_vec(),
        }
    }

    const T0: i64 = 1_791_000_000; // 2026-10-03
    const T0_DATE: &str = "Sat, 03 Oct 2026 04:00:00 GMT";

    #[test]
    fn the_date_constant_is_t0() {
        assert_eq!(crate::webapi::cookies::parse_cookie_date(T0_DATE), Some(T0));
    }

    #[test]
    fn lifetime_follows_max_age_then_expires_then_heuristics() {
        let life = |e: &Entry| freshness_lifetime(e, &Cc::of(&e.headers));
        assert_eq!(life(&entry(200, &[("cache-control", "max-age=60")], T0)), 60);
        // max-age wins over Expires.
        assert_eq!(life(&entry(200, &[("cache-control", "max-age=60"), ("expires", "Sat, 03 Oct 2026 05:00:00 GMT")], T0)), 60);
        assert_eq!(life(&entry(200, &[("date", T0_DATE), ("expires", "Sat, 03 Oct 2026 05:00:00 GMT")], T0)), 3600);
        // An unparseable Expires means already stale.
        assert_eq!(life(&entry(200, &[("expires", "0")], T0)), 0);
        // 10% of Date − Last-Modified, for a 200 only.
        let lm = [("date", T0_DATE), ("last-modified", "Thu, 03 Sep 2026 04:00:00 GMT")];
        assert_eq!(life(&entry(200, &lm, T0)), 30 * 86_400 / 10);
        assert_eq!(life(&entry(404, &lm, T0)), 0);
        assert_eq!(life(&entry(200, &[lm[0], lm[1], ("cache-control", "must-revalidate")], T0)), 0);
        // Permanent redirects and 410 are fresh forever; a 302 is not.
        assert_eq!(life(&entry(301, &[], T0)), i64::MAX);
        assert_eq!(life(&entry(308, &[], T0)), i64::MAX);
        assert_eq!(life(&entry(302, &[], T0)), 0);
        // no-cache, either way.
        assert_eq!(life(&entry(200, &[("cache-control", "max-age=60, no-cache")], T0)), 0);
        assert_eq!(life(&entry(200, &[("pragma", "no-cache"), ("expires", "Sat, 03 Oct 2026 05:00:00 GMT")], T0)), 0);
        // A malformed max-age is 0, never forever.
        assert_eq!(life(&entry(200, &[("cache-control", "max-age=soon")], T0)), 0);
    }

    #[test]
    fn age_counts_the_age_header_the_delay_and_the_time_since() {
        let mut e = entry(200, &[("date", T0_DATE), ("age", "100")], T0);
        e.request_time = T0 - 2;
        assert_eq!(current_age(&e, T0), 102);
        assert_eq!(current_age(&e, T0 + 50), 152);
        // A server clock behind ours: the apparent age is counted.
        let late = entry(200, &[("date", "Sat, 03 Oct 2026 03:59:00 GMT")], T0);
        assert_eq!(current_age(&late, T0), 60);
    }

    #[test]
    fn what_is_stored() {
        let cap = 1 << 20;
        let ok = |e: &Entry| worth_storing(e, &e.headers, e.body.len() as u64, cap).is_some();
        assert!(ok(&entry(200, &[("cache-control", "max-age=60")], T0)));
        assert!(ok(&entry(200, &[("etag", "\"x\"")], T0)), "validators make a stale entry useful");
        assert!(!ok(&entry(200, &[], T0)), "nothing could serve it but force-cache");
        assert!(!ok(&entry(200, &[("cache-control", "no-store, max-age=60")], T0)));
        assert!(!ok(&entry(206, &[("cache-control", "max-age=60")], T0)));
        assert!(!ok(&entry(200, &[("cache-control", "max-age=60"), ("vary", "*")], T0)));
        // A 302 only with explicit freshness: Argon's /files hop.
        assert!(ok(&entry(302, &[("cache-control", "private, max-age=300")], T0)));
        assert!(!ok(&entry(302, &[("etag", "\"x\"")], T0)));
        assert!(ok(&entry(301, &[], T0)));
        // An eighth of the cap at most.
        let mut big = entry(200, &[("cache-control", "max-age=60")], T0);
        big.body = vec![0; (cap / 8 + 1) as usize];
        assert!(!ok(&big));
    }

    #[test]
    fn stored_headers_lose_cookies_and_hop_by_hop() {
        let kept = storable_headers(&h(&[
            ("content-type", "image/png"),
            ("set-cookie", "a=1"),
            ("connection", "keep-alive, x-private"),
            ("keep-alive", "timeout=5"),
            ("x-private", "1"),
            ("transfer-encoding", "chunked"),
            ("etag", "\"v\""),
        ]));
        assert_eq!(kept, h(&[("content-type", "image/png"), ("etag", "\"v\"")]));
    }

    #[test]
    fn a_304_updates_what_it_may_and_nothing_about_the_body() {
        let mut e = entry(
            200,
            &[("content-type", "image/png"), ("etag", "\"v1\""), ("cache-control", "max-age=1"), ("date", "old")],
            T0,
        );
        merge_304(
            &mut e,
            &h(&[
                ("cache-control", "max-age=600"),
                ("date", T0_DATE),
                ("etag", "\"v2\""),
                ("content-type", "text/html"),
                ("content-length", "0"),
                ("set-cookie", "a=1"),
                ("x-content-type-options", "nosniff"),
            ]),
        );
        assert_eq!(
            e.headers,
            h(&[("content-type", "image/png"), ("etag", "\"v1\""), ("cache-control", "max-age=600"), ("date", T0_DATE)])
        );
    }

    #[test]
    fn modes_from_request_headers() {
        use CacheMode::*;
        assert_eq!(effective_mode(Default, &h(&[])), Default);
        assert_eq!(effective_mode(Default, &h(&[("If-None-Match", "\"x\"")])), NoStore);
        assert_eq!(effective_mode(ForceCache, &h(&[("if-modified-since", "x")])), NoStore);
        assert_eq!(effective_mode(OnlyIfCached, &h(&[("if-modified-since", "x")])), OnlyIfCached);
        assert_eq!(effective_mode(Default, &h(&[("cache-control", "no-cache")])), Reload);
        assert_eq!(effective_mode(Default, &h(&[("pragma", "no-cache")])), Reload);
        assert_eq!(effective_mode(Default, &h(&[("cache-control", "max-age=0")])), NoCache);
        assert_eq!(effective_mode(Default, &h(&[("cache-control", "no-store")])), NoStore);
        assert_eq!(effective_mode(Default, &h(&[("cache-control", "max-age=10, max-stale")])), Default);
        // An explicit mode is not overridden by the request's directives.
        assert_eq!(effective_mode(ForceCache, &h(&[("cache-control", "no-cache")])), ForceCache);
    }

    #[test]
    fn fetch_adds_its_headers_for_explicit_modes_only_where_absent() {
        let mut a = vec![];
        add_mode_headers(CacheMode::NoCache, &mut a);
        assert_eq!(a, h(&[("cache-control", "max-age=0")]));
        let mut b = h(&[("Pragma", "x")]);
        add_mode_headers(CacheMode::Reload, &mut b);
        assert_eq!(b, h(&[("Pragma", "x"), ("cache-control", "no-cache")]));
        let mut c = vec![];
        add_mode_headers(CacheMode::ForceCache, &mut c);
        assert!(c.is_empty());
    }

    #[test]
    fn vary_compares_digests_of_the_request_values() {
        let req = h(&[("accept", "image/*"), ("cookie", "sid=secret")]);
        let mut e = entry(200, &[], T0);
        e.vary = vary_digests(&["accept".into(), "cookie".into(), "x-absent".into()], &req);
        assert!(vary_matches(&e, &req));
        assert!(!vary_matches(&e, &h(&[("accept", "image/*"), ("cookie", "sid=other")])));
        assert!(!vary_matches(&e, &h(&[("accept", "image/*"), ("cookie", "sid=secret"), ("x-absent", "1")])));
        // The raw value never lands in the entry.
        let bytes = encode_payload(&e);
        assert!(!bytes.windows(10).any(|w| w == b"sid=secret"));
    }

    #[test]
    fn split_list_keeps_quoted_commas() {
        assert_eq!(split_list(r#"no-cache="a, b", max-age=5 ,, private"#), vec![r#"no-cache="a, b""#, "max-age=5", "private"]);
        let cc = Cc::of(&h(&[("Cache-Control", "max-age=5"), ("cache-control", "max-age=9, stale-while-revalidate=30")]));
        assert_eq!((cc.max_age, cc.swr), (Some(5), Some(30)));
    }

    struct Xor(u8);
    impl Sealer for Xor {
        fn tag(&self) -> String {
            format!("xor{}", self.0)
        }
        fn seal(&self, p: &[u8]) -> Result<Vec<u8>, String> {
            Ok(p.iter().map(|b| b ^ self.0).collect())
        }
        fn open(&self, s: &[u8]) -> Result<Vec<u8>, String> {
            Ok(s.iter().map(|b| b ^ self.0).collect())
        }
    }

    #[test]
    fn entries_round_trip_plain_and_sealed_and_damage_is_caught() {
        let mut e = entry(200, &[("content-type", "image/png"), ("etag", "\"v\"")], T0);
        e.vary = vec![("accept".into(), Some([7; 32])), ("x".into(), None)];
        let plain = encode_file(encode_payload(&e), None).unwrap();
        assert_eq!(decode_file(&plain, None).unwrap(), e);
        let sealed = encode_file(encode_payload(&e), Some(&Xor(0x5a))).unwrap();
        assert_eq!(decode_file(&sealed, Some(&Xor(0x5a))).unwrap(), e);
        assert!(decode_file(&sealed, None).is_err());
        assert!(decode_file(&plain, Some(&Xor(0x5a))).is_err());
        // The wrong key opens to garbage, which the CRC refuses.
        assert!(decode_file(&sealed, Some(&Xor(0x11))).is_err());
        // A torn write.
        let mut torn = plain.clone();
        torn.truncate(plain.len() - 3);
        assert!(decode_file(&torn, None).is_err());
        let mut flipped = plain;
        let mid = flipped.len() / 2;
        flipped[mid] ^= 1;
        assert!(decode_file(&flipped, None).is_err());
    }

    #[test]
    fn the_index_round_trips_and_refuses_damage() {
        let mut m = HashMap::new();
        m.insert([1; 16], Meta { size: 10, used: 99 });
        m.insert([2; 16], Meta { size: 20, used: 7 });
        let b = encode_index(&m);
        let back = decode_index(&b).unwrap();
        assert_eq!(back.get(&[1; 16]), Some(&99));
        assert_eq!(back.get(&[2; 16]), Some(&7));
        let mut bad = b.clone();
        bad[9] ^= 0xff;
        assert!(decode_index(&bad).is_none());
    }

    #[test]
    fn the_key_drops_the_fragment_and_names_no_url_on_disk() {
        assert_eq!(key_of("https://X.test/a?b=1#frag").as_deref(), Some("https://x.test/a?b=1"));
        let name = hex(&hash_of("https://x.test/a"));
        assert_eq!(name.len(), 32);
        assert_eq!(unhex(&name), Some(hash_of("https://x.test/a")));
    }
}
