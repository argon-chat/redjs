//! The cookie store (RFC 6265bis): what `fetch(…, { credentials: "include" })`
//! and every WebSocket handshake read from and write to.
//!
//! # Why this exists now
//!
//! Until this module, `fetch` refused `credentials: "include"` by name, because
//! there was no jar and accepting the option would have sent the request
//! WITHOUT the cookies the caller believed it attached. That refusal was
//! honest, and fatal for an app runtime: an API client that sets
//! `include` on every call (the Argon client's Ion transport does) turns the
//! rejection into a silent "network error" on every request. So the refusal is
//! gone and this is what replaced it: a real store, with the browser's rules.
//!
//! # What is here, in the RFC's own order
//!
//! - **§5.6, parsing `Set-Cookie`:** name/value (a nameless cookie included),
//!   `Expires` (the cookie-date algorithm of §5.1.1, two-digit years and all),
//!   `Max-Age` (wins over `Expires`; `<= 0` deletes), `Domain`, `Path`,
//!   `Secure`, `HttpOnly`, `SameSite`. Attribute values over 1024 bytes are
//!   ignored; a name+value over 4096 bytes, or one with a control character,
//!   ignores the cookie.
//! - **§5.7, storing:** host-only vs domain cookies; a `Domain` that does not
//!   domain-match the host, or that is a public suffix other than the host
//!   itself (a bare TLD, or one of the multi-label suffixes listed in
//!   [`PUBLIC_SUFFIXES`]), is refused; a `Secure` cookie from an insecure
//!   origin is refused, and so is a non-secure cookie that would shadow a
//!   secure one ("leave secure cookies alone"); `SameSite=None` without
//!   `Secure` is refused; the `__Secure-` and `__Host-` prefixes are enforced;
//!   expiry is capped at 400 days; replacing keeps the original creation time;
//!   an already-expired cookie deletes its namesake. A non-HTTP API cannot set,
//!   overwrite or read an `HttpOnly` cookie.
//! - **§5.8.3, the `Cookie` header:** domain-match (or exact host for a
//!   host-only cookie), path-match, `Secure` only over a secure transport,
//!   expired cookies left out, longest path first and then oldest first.
//!
//! "Secure transport" is `https:`/`wss:`, plus loopback (`localhost`,
//! `127.0.0.0/8`, `[::1]`) over plain `http:` — Chromium's "potentially
//! trustworthy" rule, and what makes a local dev server work.
//!
//! # Who reads and writes it
//!
//! - `fetch` ([`super::http::send`]): per redirect HOP, the jar is asked for
//!   that hop's URL right before the request goes out, and that hop's
//!   `Set-Cookie` headers are stored right after its head arrives — so a
//!   cookie a 302 sets is on the request the 302 points at, and a hop to
//!   another host gets only that host's cookies.
//! - `WebSocket` ([`super::websocket`]): the opening handshake carries the
//!   cookies for the URL and stores the 101's `Set-Cookie`, as browsers do.
//! - The embedder: [`set`] / [`remove`] / [`get`] — the shape of Electron's
//!   `session.cookies` — so a host can provision a cookie before any JS runs.
//! - **Nothing in JS can read it.** `Response.headers` never carries
//!   `Set-Cookie` / `Set-Cookie2` (Fetch's forbidden response header names), and
//!   `document.cookie` answers `""` (see below).
//!
//! # `credentials`
//!
//! Fetch's three modes, against this runtime's page origin, `app://redjs`
//! (`bootstrap.js`'s `location`):
//!
//! | mode | cookies sent and stored |
//! |---|---|
//! | `omit` | never |
//! | `same-origin` (the default) | only for a URL same-origin with the page — and `app://redjs` is an opaque origin to the URL Standard, so for no network URL at all |
//! | `include` | always |
//!
//! So an API call that wants cookies says `include`, exactly as a page served
//! from one origin calling an API on another has to in a browser.
//!
//! # Divergences, named
//!
//! - **C-1, `SameSite` does not restrict.** In a browser, a request from a page
//!   at `app://redjs` to `https://api.example.com` is cross-site, so only
//!   `SameSite=None` cookies would ride along. Here the app is the first party
//!   of every request it makes — it is an installed application, there is no
//!   other site in the process whose requests could carry the user's cookies
//!   (which is the attack `SameSite` exists to stop), and that is how the
//!   Electron build of the same app behaves (its page is same-site with its
//!   API). `SameSite` is parsed, stored and persisted, and a `Lax`/`Strict`/
//!   unspecified cookie is still sent; the first time that happens it is said
//!   once on stderr.
//! - **C-2, no third-party-cookie blocking and no partitioning** (CHIPS,
//!   `Partitioned`): there is one first party, so there is nothing to
//!   partition by. `Partitioned` is accepted and ignored.
//! - **C-3, the public suffix list is partial:** bare TLDs and the
//!   [`PUBLIC_SUFFIXES`] table, not Mozilla's ~10 000-entry list. A `Domain` on
//!   an unlisted multi-label suffix is accepted where a browser would refuse it.
//! - **C-4, `document.cookie` is `""` and ignores writes.** The page's URL is
//!   `app://redjs/`, a scheme with no cookies — which is also what a browser
//!   says for a document whose origin is opaque. Said once, on first use.
//! - **C-5, a script-set `Cookie` request header still goes out.** redjs has no
//!   forbidden-request-header filter (a browser silently drops `Cookie` from a
//!   request's `Headers`); when the jar also has cookies for the hop, the two
//!   are joined with `"; "`, the script's first. A cross-origin redirect still
//!   strips the script's value (`http::strip_sensitive`).
//!
//! # Persistence
//!
//! Process-wide and in memory, plus a [`CookieHost`] the embedder installs —
//! the same seam [`super::storage::StorageHost`] is. Only PERSISTENT cookies
//! (an `Expires` or `Max-Age`) reach the host; a session cookie dies with the
//! process, which is what "session" means in a browser. Without a host the jar
//! works and says once that nothing survives a restart.

use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

/// `SameSite`, as stored. `Unspecified` is RFC 6265bis's "Default" — no
/// attribute, or a value that is none of the three.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SameSite {
    None,
    Lax,
    Strict,
    Unspecified,
}

impl SameSite {
    pub fn as_str(self) -> &'static str {
        match self {
            SameSite::None => "None",
            SameSite::Lax => "Lax",
            SameSite::Strict => "Strict",
            SameSite::Unspecified => "",
        }
    }

    /// The inverse of [`as_str`](SameSite::as_str), case-insensitive; anything
    /// else is `Unspecified` (RFC 6265bis §5.6.7).
    pub fn parse(s: &str) -> SameSite {
        if s.eq_ignore_ascii_case("none") {
            SameSite::None
        } else if s.eq_ignore_ascii_case("lax") {
            SameSite::Lax
        } else if s.eq_ignore_ascii_case("strict") {
            SameSite::Strict
        } else {
            SameSite::Unspecified
        }
    }
}

/// One stored cookie (RFC 6265bis §5.7's record). Public so a [`CookieHost`]
/// can serialise it however it likes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    /// Canonical: lower-case, ASCII (IDNA), no leading dot.
    pub domain: String,
    pub path: String,
    /// Sent only to `domain` exactly — no `Domain` attribute was given.
    pub host_only: bool,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: SameSite,
    /// Unix seconds. `None` = a session cookie, which is never persisted.
    pub expires: Option<i64>,
    /// Unix microseconds, unique within the jar (the tie-break §5.8.3 sorts on).
    pub creation: i64,
    /// Unix microseconds.
    pub last_access: i64,
}

impl Cookie {
    fn expired(&self, now_s: i64) -> bool {
        self.expires.is_some_and(|e| e <= now_s)
    }

    fn same_key(&self, other: &Cookie) -> bool {
        self.name == other.name && self.domain == other.domain && self.path == other.path
    }
}

/// Which kind of API is asking. `HttpOnly` cookies exist for [`Api::Http`]
/// only — `fetch`, WebSocket and the embedder — never for script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    Http,
    NonHttp,
}

// ---------------------------------------------------------------------------
// credentials
// ---------------------------------------------------------------------------

/// Fetch's request credentials mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Credentials {
    Omit,
    SameOrigin,
    Include,
}

impl Credentials {
    /// `None` for a string that is not one of the three.
    pub fn parse(s: &str) -> Option<Credentials> {
        match s {
            "omit" => Some(Credentials::Omit),
            "same-origin" => Some(Credentials::SameOrigin),
            "include" => Some(Credentials::Include),
            _ => None,
        }
    }

    /// Fetch's "include credentials" for a request to `u`: are cookies sent
    /// to, and stored from, this hop?
    pub fn applies(self, u: &url::Url) -> bool {
        match self {
            Credentials::Omit => false,
            Credentials::Include => true,
            // The page's origin is `app://redjs`, which the URL Standard makes
            // opaque — no request URL is ever same-origin with it. Written as a
            // comparison rather than `false` so the rule reads as the rule.
            Credentials::SameOrigin => same_origin_as_page(u),
        }
    }
}

/// The page origin this runtime publishes (`bootstrap.js`'s `location`).
pub const PAGE_ORIGIN: &str = "app://redjs";

fn same_origin_as_page(u: &url::Url) -> bool {
    match url::Url::parse(PAGE_ORIGIN) {
        // Opaque origins are never equal to anything, themselves included.
        Ok(page) => page.origin().is_tuple() && page.origin() == u.origin(),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// The host seam
// ---------------------------------------------------------------------------

/// Where persistent cookies live between runs. One per embedder; the shape of
/// [`super::storage::StorageHost`] and for the same reasons — where bytes go
/// and whether they are encrypted is the embedder's question, the cookie rules
/// are this module's.
///
/// Whole-jar on purpose: the only crash-safe file update is a whole-file
/// rename, and a host that wants to coalesce writes does it on its own clock.
pub trait CookieHost: Send + Sync {
    /// Every persisted cookie. Called once, lazily, at the jar's first use
    /// (or at [`set_host`] when the jar was already in use). Expired entries
    /// may be returned; they are dropped here.
    fn load(&self) -> Vec<Cookie>;

    /// Every PERSISTENT cookie after a change. Called with the jar's lock
    /// held, from whichever thread changed it (a `fetch` worker included), so
    /// it must stage, not block, and must not call back into this module.
    fn store(&self, cookies: &[Cookie]);

    /// Where it persists, for advisories.
    fn describe(&self) -> String {
        String::from("the host's cookie backend")
    }
}

static HOST: RwLock<Option<&'static dyn CookieHost>> = RwLock::new(None);

/// Installs the persistence backend. Install it BEFORE anything touches the
/// jar (the embedder's own [`set`] included); a late install merges what is on
/// disk under what is already in memory, so nothing is lost either way.
pub fn set_host(host: &'static dyn CookieHost) {
    *HOST.write().unwrap() = Some(host);
    let mut jar = jar().lock().unwrap();
    if jar.loaded {
        let now = now_s();
        for c in host.load() {
            if !c.expired(now) && !jar.cookies.iter().any(|x| x.same_key(&c)) {
                jar.cookies.push(c);
            }
        }
        jar.persist();
    }
}

fn host() -> Option<&'static dyn CookieHost> {
    *HOST.read().unwrap()
}

// ---------------------------------------------------------------------------
// The jar
// ---------------------------------------------------------------------------

/// Chromium's limits: 180 per domain, 3300 in all. Past them the least
/// recently used cookie goes (expired ones first).
const MAX_PER_DOMAIN: usize = 180;
const MAX_TOTAL: usize = 3300;
/// RFC 6265bis §5.5: a user agent MUST cap `Expires`/`Max-Age` at 400 days.
const MAX_AGE_CAP_S: i64 = 400 * 24 * 60 * 60;
const MAX_NAME_VALUE: usize = 4096;
const MAX_ATTRIBUTE_VALUE: usize = 1024;

#[derive(Default)]
struct Jar {
    cookies: Vec<Cookie>,
    loaded: bool,
    /// The last creation stamp handed out, so two cookies set in the same
    /// microsecond still sort in the order they were set.
    last_stamp: i64,
}

impl Jar {
    fn ensure_loaded(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        if let Some(h) = host() {
            let now = now_s();
            self.cookies = h.load().into_iter().filter(|c| !c.expired(now)).collect();
            self.last_stamp = self.cookies.iter().map(|c| c.creation).max().unwrap_or(0);
        }
    }

    fn stamp(&mut self) -> i64 {
        let now = now_us();
        self.last_stamp = if now > self.last_stamp { now } else { self.last_stamp + 1 };
        self.last_stamp
    }

    /// Hands the persistent cookies to the host.
    fn persist(&self) {
        match host() {
            Some(h) => {
                let keep: Vec<Cookie> = self.cookies.iter().filter(|c| c.expires.is_some()).cloned().collect();
                h.store(&keep);
            }
            None => advise_no_host(),
        }
    }

    fn evict(&mut self, now: i64) {
        self.cookies.retain(|c| !c.expired(now));
        let domain_counts = |cs: &Vec<Cookie>, d: &str| cs.iter().filter(|c| c.domain == d).count();
        let mut domains: Vec<String> = self.cookies.iter().map(|c| c.domain.clone()).collect();
        domains.sort();
        domains.dedup();
        for d in domains {
            while domain_counts(&self.cookies, &d) > MAX_PER_DOMAIN {
                self.drop_lru(|c| c.domain == d);
            }
        }
        while self.cookies.len() > MAX_TOTAL {
            self.drop_lru(|_| true);
        }
    }

    fn drop_lru(&mut self, pred: impl Fn(&Cookie) -> bool) {
        if let Some((i, _)) =
            self.cookies.iter().enumerate().filter(|(_, c)| pred(c)).min_by_key(|(_, c)| c.last_access)
        {
            self.cookies.remove(i);
        }
    }
}

fn jar() -> &'static Mutex<Jar> {
    static J: OnceLock<Mutex<Jar>> = OnceLock::new();
    J.get_or_init(|| Mutex::new(Jar::default()))
}

fn advise_no_host() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        eprintln!(
            "[redjs] cookies: no CookieHost installed — persistent cookies are IN MEMORY ONLY and \
             nothing survives a restart. Call redjs::webapi::cookies::set_host()."
        );
    });
}

fn advise_same_site(c: &Cookie, host: &str) {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let ss = match c.same_site {
            SameSite::Unspecified => "no SameSite (Lax by default in a browser)".to_string(),
            other => format!("SameSite={}", other.as_str()),
        };
        eprintln!(
            "[redjs] cookies (C-1): cookie {:?} ({ss}) was sent to {host}. A browser page at \
             {PAGE_ORIGIN} would count that request as cross-site and withhold it; here the app is \
             the first party of every request it makes, so SameSite does not restrict. Said once.",
            c.name
        );
    });
}

fn now_us() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as i64).unwrap_or(0)
}

fn now_s() -> i64 {
    now_us() / 1_000_000
}

// ---------------------------------------------------------------------------
// Hosts, domains and paths (RFC 6265bis §5.1.3, §5.1.4)
// ---------------------------------------------------------------------------

/// Multi-label public suffixes refused as a `Domain` (C-3: a working subset,
/// not the full list). Bare TLDs are refused by rule, not by table.
pub const PUBLIC_SUFFIXES: &[&str] = &[
    // ccTLD second levels that registrars sell under.
    "co.uk", "org.uk", "me.uk", "ltd.uk", "plc.uk", "net.uk", "ac.uk", "gov.uk", "sch.uk",
    "com.au", "net.au", "org.au", "edu.au", "gov.au", "asn.au", "id.au",
    "co.nz", "net.nz", "org.nz", "govt.nz", "ac.nz",
    "co.jp", "ne.jp", "or.jp", "ac.jp", "go.jp", "gr.jp", "ad.jp", "ed.jp", "lg.jp",
    "co.kr", "or.kr", "ne.kr", "go.kr", "ac.kr",
    "com.br", "net.br", "org.br", "gov.br", "edu.br",
    "com.cn", "net.cn", "org.cn", "gov.cn", "edu.cn",
    "com.tw", "net.tw", "org.tw", "com.hk", "net.hk", "org.hk", "com.sg", "net.sg", "org.sg",
    "co.in", "net.in", "org.in", "firm.in", "gen.in", "ind.in",
    "com.mx", "org.mx", "gob.mx", "com.ar", "com.co", "com.pe", "com.ve",
    "com.tr", "net.tr", "org.tr", "gov.tr", "com.ua", "net.ua", "org.ua", "com.ru", "net.ru",
    "org.ru", "pp.ru", "msk.ru", "spb.ru", "com.pl", "net.pl", "org.pl",
    "co.za", "org.za", "net.za", "gov.za", "co.il", "org.il", "net.il", "ac.il",
    "com.eg", "com.sa", "com.my", "com.ph", "com.sk", "co.th", "in.th", "co.id", "or.id",
    // Private suffixes whose subdomains belong to different people.
    "github.io", "githubusercontent.com", "gitlab.io", "herokuapp.com", "appspot.com",
    "blogspot.com", "cloudfront.net", "azurewebsites.net", "netlify.app", "vercel.app",
    "pages.dev", "workers.dev", "web.app", "firebaseapp.com", "fly.dev", "onrender.com",
    "ngrok.io", "ngrok-free.app", "s3.amazonaws.com", "glitch.me", "repl.co",
];

/// Is `domain` (canonical) a public suffix — somewhere nobody may set a
/// cookie for every site under?
pub fn is_public_suffix(domain: &str) -> bool {
    !domain.contains('.') || PUBLIC_SUFFIXES.contains(&domain)
}

/// A URL's host, canonical: lower-case, IDNA, IPv6 in brackets.
fn canonical_host(u: &url::Url) -> Option<String> {
    u.host_str().map(|h| h.to_ascii_lowercase())
}

fn is_ip(host: &str) -> bool {
    host.starts_with('[') || host.parse::<std::net::Ipv4Addr>().is_ok()
}

/// §5.1.3 domain-match.
pub fn domain_match(host: &str, domain: &str) -> bool {
    if host == domain {
        return true;
    }
    !is_ip(host)
        && host.len() > domain.len()
        && host.ends_with(domain)
        && host.as_bytes()[host.len() - domain.len() - 1] == b'.'
}

/// §5.1.4 default-path, from a request URL's path.
pub fn default_path(uri_path: &str) -> String {
    if !uri_path.starts_with('/') {
        return "/".into();
    }
    match uri_path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => uri_path[..i].to_string(),
    }
}

/// §5.1.4 path-match.
pub fn path_match(request_path: &str, cookie_path: &str) -> bool {
    if request_path == cookie_path {
        return true;
    }
    request_path.starts_with(cookie_path)
        && (cookie_path.ends_with('/') || request_path.as_bytes().get(cookie_path.len()) == Some(&b'/'))
}

/// A transport the `Secure` attribute trusts: `https:`/`wss:`, or loopback
/// over anything (Chromium's "potentially trustworthy origin").
pub fn is_secure_transport(u: &url::Url) -> bool {
    if matches!(u.scheme(), "https" | "wss") {
        return true;
    }
    match u.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost") || d.to_ascii_lowercase().ends_with(".localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

fn request_path(u: &url::Url) -> &str {
    let p = u.path();
    if p.is_empty() {
        "/"
    } else {
        p
    }
}

/// Cookies exist for `http(s):` and `ws(s):` only.
fn cookie_scheme(u: &url::Url) -> bool {
    matches!(u.scheme(), "http" | "https" | "ws" | "wss")
}

// ---------------------------------------------------------------------------
// The cookie-date algorithm (RFC 6265bis §5.1.1)
// ---------------------------------------------------------------------------

fn is_delimiter(b: u8) -> bool {
    b == 0x09 || (0x20..=0x2f).contains(&b) || (0x3b..=0x40).contains(&b) || (0x5b..=0x60).contains(&b) || (0x7b..=0x7e).contains(&b)
}

/// `1*max DIGIT`, then either the end or a non-digit (and anything after).
fn leading_digits(tok: &[u8], min: usize, max: usize) -> Option<u32> {
    let n = tok.iter().take_while(|b| b.is_ascii_digit()).count();
    if n < min || n > max {
        return None;
    }
    std::str::from_utf8(&tok[..n]).ok()?.parse().ok()
}

fn parse_time(tok: &[u8]) -> Option<(u32, u32, u32)> {
    let mut parts = [0u32; 3];
    let mut i = 0usize;
    for (k, part) in parts.iter_mut().enumerate() {
        let n = tok[i..].iter().take_while(|b| b.is_ascii_digit()).count();
        if n == 0 || n > 2 {
            return None;
        }
        *part = std::str::from_utf8(&tok[i..i + n]).ok()?.parse().ok()?;
        i += n;
        if k < 2 {
            if tok.get(i) != Some(&b':') {
                return None;
            }
            i += 1;
        }
    }
    // `time = hms-time [ non-digit *OCTET ]`
    if tok.get(i).is_some_and(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((parts[0], parts[1], parts[2]))
}

fn parse_month(tok: &[u8]) -> Option<u32> {
    if tok.len() < 3 {
        return None;
    }
    let m = std::str::from_utf8(&tok[..3]).ok()?.to_ascii_lowercase();
    ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"]
        .iter()
        .position(|x| *x == m)
        .map(|i| i as u32 + 1)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

/// A cookie-date to Unix seconds; `None` when the algorithm says "fail".
pub fn parse_cookie_date(s: &str) -> Option<i64> {
    let (mut time, mut day, mut month, mut year) = (None, None, None, None);
    for tok in s.as_bytes().split(|b| is_delimiter(*b)).filter(|t| !t.is_empty()) {
        if time.is_none() {
            if let Some(t) = parse_time(tok) {
                time = Some(t);
                continue;
            }
        }
        if day.is_none() {
            if let Some(d) = leading_digits(tok, 1, 2) {
                day = Some(d);
                continue;
            }
        }
        if month.is_none() {
            if let Some(m) = parse_month(tok) {
                month = Some(m);
                continue;
            }
        }
        if year.is_none() {
            if let Some(y) = leading_digits(tok, 2, 4) {
                year = Some(y);
                continue;
            }
        }
    }
    let (h, mi, sec) = time?;
    let (d, m) = (day?, month?);
    let mut y = year? as i64;
    if (70..=99).contains(&y) {
        y += 1900;
    } else if (0..=69).contains(&y) {
        y += 2000;
    }
    if !(1..=31).contains(&d) || y < 1601 || h > 23 || mi > 59 || sec > 59 || d > days_in_month(y, m) {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400 + (h * 3600 + mi * 60 + sec) as i64)
}

// ---------------------------------------------------------------------------
// Parsing Set-Cookie (§5.6) and storing it (§5.7)
// ---------------------------------------------------------------------------

/// Why a `Set-Cookie` was ignored. A sentence, for tests and diagnostics.
pub type Ignored = String;

fn has_ctl(s: &str) -> bool {
    s.bytes().any(|b| b <= 0x08 || (0x0a..=0x1f).contains(&b) || b == 0x7f)
}

/// The parsed attributes of one `Set-Cookie`, before the store's rules.
struct Parsed {
    name: String,
    value: String,
    expires: Option<i64>,
    max_age: Option<i64>,
    domain: Option<String>,
    path: Option<String>,
    secure: bool,
    http_only: bool,
    same_site: SameSite,
}

fn parse_set_cookie(header: &str, now: i64) -> Result<Parsed, Ignored> {
    let (pair, attrs) = match header.find(';') {
        Some(i) => (&header[..i], &header[i + 1..]),
        None => (header, ""),
    };
    if has_ctl(header) {
        return Err("the set-cookie-string contains a control character".into());
    }
    let (name, value) = match pair.find('=') {
        Some(i) => (pair[..i].trim(), pair[i + 1..].trim()),
        None => ("", pair.trim()),
    };
    if name.is_empty() && value.is_empty() {
        return Err("both the name and the value are empty".into());
    }
    if name.len() + value.len() > MAX_NAME_VALUE {
        return Err(format!("the name and value are over {MAX_NAME_VALUE} bytes"));
    }
    let mut p = Parsed {
        name: name.to_string(),
        value: value.to_string(),
        expires: None,
        max_age: None,
        domain: None,
        path: None,
        secure: false,
        http_only: false,
        same_site: SameSite::Unspecified,
    };
    for av in attrs.split(';') {
        let (k, v) = match av.find('=') {
            Some(i) => (av[..i].trim(), av[i + 1..].trim()),
            None => (av.trim(), ""),
        };
        if v.len() > MAX_ATTRIBUTE_VALUE {
            continue;
        }
        match k.to_ascii_lowercase().as_str() {
            "expires" => {
                if let Some(t) = parse_cookie_date(v) {
                    p.expires = Some(t.min(now + MAX_AGE_CAP_S));
                }
            }
            "max-age" => {
                let b = v.as_bytes();
                let ok = !b.is_empty()
                    && (b[0].is_ascii_digit() || b[0] == b'-')
                    && b[1..].iter().all(|c| c.is_ascii_digit());
                if ok {
                    // A number too long for i64 is still a number: a huge
                    // positive one is "far future" (capped), a huge negative
                    // one is "already gone".
                    let delta = v.parse::<i64>().unwrap_or(if b[0] == b'-' { -1 } else { i64::MAX });
                    p.max_age = Some(if delta <= 0 { i64::MIN } else { now.saturating_add(delta.min(MAX_AGE_CAP_S)) });
                }
            }
            "domain" => {
                if !v.is_empty() {
                    p.domain = Some(v.trim_start_matches('.').to_string());
                }
            }
            "path" => {
                p.path = v.starts_with('/').then(|| v.to_string());
            }
            "secure" => p.secure = true,
            "httponly" => p.http_only = true,
            "samesite" => p.same_site = SameSite::parse(v),
            _ => {}
        }
    }
    Ok(p)
}

/// The store algorithm (§5.7) for one parsed cookie arriving from `u`.
fn store_parsed(jar: &mut Jar, u: &url::Url, p: Parsed, api: Api, now: i64) -> Result<(), Ignored> {
    let host = canonical_host(u).ok_or("the URL has no host")?;
    let secure_origin = is_secure_transport(u);

    // Domain (§5.7 steps 5-6).
    let (domain, host_only) = match p.domain {
        None => (host.clone(), true),
        Some(raw) => {
            let d = match url::Host::parse(&raw) {
                Ok(url::Host::Domain(d)) => d.to_ascii_lowercase(),
                Ok(url::Host::Ipv4(ip)) => ip.to_string(),
                Ok(url::Host::Ipv6(ip)) => format!("[{ip}]"),
                Err(_) => return Err(format!("Domain={raw:?} is not a host name")),
            };
            if is_public_suffix(&d) && d != host {
                return Err(format!("Domain={d:?} is a public suffix"));
            }
            if is_public_suffix(&d) {
                (host.clone(), true)
            } else if !domain_match(&host, &d) {
                return Err(format!("Domain={d:?} does not domain-match the host {host:?}"));
            } else {
                (d, false)
            }
        }
    };

    if p.secure && !secure_origin {
        return Err("Secure was set over an insecure transport".into());
    }
    if p.http_only && api == Api::NonHttp {
        return Err("HttpOnly cannot be set by script".into());
    }
    if p.same_site == SameSite::None && !p.secure {
        return Err("SameSite=None requires Secure".into());
    }
    // Prefixes (§4.1.3), matched case-insensitively as 6265bis now says.
    let lower = p.name.to_ascii_lowercase();
    if lower.starts_with("__secure-") && !(p.secure && secure_origin) {
        return Err("a __Secure- cookie must be Secure and set over a secure transport".into());
    }
    if lower.starts_with("__host-")
        && !(p.secure && secure_origin && host_only && p.path.as_deref() == Some("/"))
    {
        return Err("a __Host- cookie must be Secure, host-only and Path=/".into());
    }

    let path = p.path.unwrap_or_else(|| default_path(request_path(u)));
    // Max-Age wins over Expires (§5.6.2).
    let expires = p.max_age.or(p.expires);

    // "Leave secure cookies alone" (§5.7 step 15).
    if !p.secure && !secure_origin {
        let shadowed = jar.cookies.iter().any(|c| {
            c.secure
                && c.name == p.name
                && (domain_match(&domain, &c.domain) || domain_match(&c.domain, &domain))
                && path_match(&path, &c.path)
        });
        if shadowed {
            return Err("a Secure cookie of that name already covers this host and path".into());
        }
    }

    let mut cookie = Cookie {
        name: p.name,
        value: p.value,
        domain,
        path,
        host_only,
        secure: p.secure,
        http_only: p.http_only,
        same_site: p.same_site,
        expires: expires.map(|e| e.max(i64::MIN + 1)),
        creation: 0,
        last_access: 0,
    };

    if let Some(i) = jar.cookies.iter().position(|c| c.same_key(&cookie)) {
        if jar.cookies[i].http_only && api == Api::NonHttp {
            return Err("script cannot overwrite an HttpOnly cookie".into());
        }
        cookie.creation = jar.cookies[i].creation;
        jar.cookies.remove(i);
    } else {
        cookie.creation = jar.stamp();
    }
    cookie.last_access = now_us();
    // An already-expired cookie is a deletion: its namesake is gone (above)
    // and it is not stored.
    if !cookie.expired(now) {
        jar.cookies.push(cookie);
    }
    jar.evict(now);
    Ok(())
}

/// Stores one `Set-Cookie` header value received from `u`.
pub fn set_from_header(u: &url::Url, header: &str, api: Api) -> Result<(), Ignored> {
    if !cookie_scheme(u) {
        return Err(format!("{}: URLs have no cookies", u.scheme()));
    }
    let now = now_s();
    let parsed = parse_set_cookie(header, now)?;
    let mut jar = jar().lock().unwrap();
    jar.ensure_loaded();
    let persistent_before = jar.cookies.iter().filter(|c| c.expires.is_some()).count();
    let r = store_parsed(&mut jar, u, parsed, api, now);
    if r.is_ok() || persistent_before != jar.cookies.iter().filter(|c| c.expires.is_some()).count() {
        jar.persist();
    }
    r
}

/// Every `Set-Cookie` in a response head (names lower-case, as
/// [`super::http::parse_head`] gives them). An ignored cookie is ignored,
/// silently, as in a browser — the rest of the response is unaffected.
pub fn store_response(u: &url::Url, headers: &[(String, String)]) {
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("set-cookie") {
            let _ = set_from_header(u, v, Api::Http);
        }
    }
}

// ---------------------------------------------------------------------------
// The Cookie header (§5.8.3)
// ---------------------------------------------------------------------------

/// The `Cookie` header value for a request to `u`, or `None` when no cookie
/// applies. Updates each sent cookie's last-access time.
pub fn header_for(u: &url::Url, api: Api) -> Option<String> {
    if !cookie_scheme(u) {
        return None;
    }
    let host = canonical_host(u)?;
    let path = request_path(u).to_string();
    let secure = is_secure_transport(u);
    let now = now_s();
    let stamp = now_us();
    let mut jar = jar().lock().unwrap();
    jar.ensure_loaded();
    let mut hits: Vec<&mut Cookie> = jar
        .cookies
        .iter_mut()
        .filter(|c| {
            !c.expired(now)
                && if c.host_only { host == c.domain } else { domain_match(&host, &c.domain) }
                && path_match(&path, &c.path)
                && (!c.secure || secure)
                && (!c.http_only || api == Api::Http)
        })
        .collect();
    if hits.is_empty() {
        return None;
    }
    hits.sort_by(|a, b| b.path.len().cmp(&a.path.len()).then(a.creation.cmp(&b.creation)));
    let mut out = String::new();
    for c in hits {
        c.last_access = stamp;
        if c.same_site != SameSite::None {
            advise_same_site(c, &host);
        }
        if !out.is_empty() {
            out.push_str("; ");
        }
        if !c.name.is_empty() {
            out.push_str(&c.name);
            out.push('=');
        }
        out.push_str(&c.value);
    }
    Some(out)
}

/// `headers` with the jar's cookies for `u` folded in (C-5: after a
/// script-set `Cookie`, never instead of it). `None` = nothing to add.
pub fn with_cookies(u: &url::Url, headers: &[(String, String)]) -> Option<Vec<(String, String)>> {
    let jar_value = header_for(u, Api::Http)?;
    let mut out = headers.to_vec();
    match out.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case("cookie")) {
        Some((_, v)) if !v.trim().is_empty() => {
            v.push_str("; ");
            v.push_str(&jar_value);
        }
        Some((_, v)) => *v = jar_value,
        None => out.push(("cookie".into(), jar_value)),
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// The embedder's API: Electron's `session.cookies`
// ---------------------------------------------------------------------------

/// `session.cookies.set({...})`, field for field. `url` is required: it is the
/// origin the cookie is set "from", so it decides what `Secure` may claim and
/// what a host-only cookie's host is.
#[derive(Clone, Debug)]
pub struct HostCookie {
    pub url: String,
    pub name: String,
    pub value: String,
    /// `Some(".argon.gl")` / `Some("argon.gl")` = a domain cookie; `None` = host-only.
    pub domain: Option<String>,
    /// `None` = the default path of `url`.
    pub path: Option<String>,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: SameSite,
    /// `None` = a session cookie (never persisted).
    pub expires: Option<SystemTime>,
}

/// Sets a cookie with every rule a `Set-Cookie` from `url` would face — a
/// host cannot provision a cookie a server could not have set — except that
/// the embedder is an HTTP API, so `HttpOnly` is allowed.
pub fn set(c: HostCookie) -> Result<(), Ignored> {
    let u = url::Url::parse(&c.url).map_err(|e| format!("cookies::set: invalid url {:?}: {e}", c.url))?;
    if !cookie_scheme(&u) {
        return Err(format!("cookies::set: {}: URLs have no cookies", u.scheme()));
    }
    if c.name.contains(['=', ';']) || has_ctl(&c.name) || c.value.contains(';') || has_ctl(&c.value) {
        return Err("cookies::set: the name or value contains '=', ';' or a control character".into());
    }
    let bad = |s: &Option<String>| s.as_deref().is_some_and(|s| s.contains(';') || has_ctl(s));
    if bad(&c.domain) || bad(&c.path) {
        return Err("cookies::set: the domain or path contains ';' or a control character".into());
    }
    let now = now_s();
    let p = Parsed {
        name: c.name,
        value: c.value,
        expires: c.expires.map(|t| {
            let s = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
            s.min(now + MAX_AGE_CAP_S)
        }),
        max_age: None,
        domain: c.domain.map(|d| d.trim_start_matches('.').to_string()).filter(|d| !d.is_empty()),
        path: c.path.filter(|p| p.starts_with('/')),
        secure: c.secure,
        http_only: c.http_only,
        same_site: c.same_site,
    };
    let mut jar = jar().lock().unwrap();
    jar.ensure_loaded();
    let r = store_parsed(&mut jar, &u, p, Api::Http, now);
    jar.persist();
    r.map_err(|e| format!("cookies::set: {e}"))
}

/// `session.cookies.remove(url, name)`: every cookie named `name` that a
/// request to `url` would carry, whatever its `Secure` flag or expiry.
/// Returns how many went.
pub fn remove(url: &str, name: &str) -> usize {
    let Ok(u) = url::Url::parse(url) else { return 0 };
    let Some(host) = canonical_host(&u) else { return 0 };
    let path = request_path(&u).to_string();
    let mut jar = jar().lock().unwrap();
    jar.ensure_loaded();
    let before = jar.cookies.len();
    jar.cookies.retain(|c| {
        let applies = (if c.host_only { host == c.domain } else { domain_match(&host, &c.domain) })
            && path_match(&path, &c.path);
        !(c.name == name && applies)
    });
    let gone = before - jar.cookies.len();
    if gone > 0 {
        jar.persist();
    }
    gone
}

/// `session.cookies.get(...)`: a snapshot for the EMBEDDER — every live
/// cookie, or the ones a request to `url` would carry (HttpOnly included).
/// Never exposed to script.
pub fn get(url: Option<&str>) -> Vec<Cookie> {
    let now = now_s();
    let mut jar = jar().lock().unwrap();
    jar.ensure_loaded();
    let target = url.and_then(|s| url::Url::parse(s).ok());
    jar.cookies
        .iter()
        .filter(|c| !c.expired(now))
        .filter(|c| match &target {
            None => true,
            Some(u) => {
                let host = canonical_host(u).unwrap_or_default();
                (if c.host_only { host == c.domain } else { domain_match(&host, &c.domain) })
                    && path_match(request_path(u), &c.path)
                    && (!c.secure || is_secure_transport(u))
            }
        })
        .cloned()
        .collect()
}

/// Empties the jar (and the host's copy).
pub fn clear() {
    let mut jar = jar().lock().unwrap();
    jar.ensure_loaded();
    jar.cookies.clear();
    jar.persist();
}

/// Test-only: an unloaded, empty jar. Process-global state, so the tests
/// that use it serialise on [`test_lock`].
#[cfg(test)]
pub(crate) fn reset_for_tests() {
    let mut jar = jar().lock().unwrap();
    *jar = Jar::default();
}

#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: Mutex<()> = Mutex::new(());
    L.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Tests — the RFC's rules, no socket and no JS (the wiring is tested where it
// lives: fetch.rs, websocket.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> url::Url {
        url::Url::parse(s).unwrap()
    }

    fn set_h(url: &str, h: &str) -> Result<(), Ignored> {
        set_from_header(&u(url), h, Api::Http)
    }

    fn sent(url: &str) -> String {
        header_for(&u(url), Api::Http).unwrap_or_default()
    }

    #[test]
    fn a_host_only_cookie_goes_to_its_host_and_not_to_a_subdomain() {
        let _g = test_lock();
        reset_for_tests();
        set_h("https://api.example.com/", "a=1").unwrap();
        assert_eq!(sent("https://api.example.com/x"), "a=1");
        assert_eq!(sent("https://sub.api.example.com/x"), "");
        assert_eq!(sent("https://example.com/x"), "");
    }

    #[test]
    fn a_domain_cookie_covers_the_domain_and_every_subdomain() {
        let _g = test_lock();
        reset_for_tests();
        set_h("https://api.example.com/", "d=1; Domain=.example.com").unwrap();
        assert_eq!(sent("https://example.com/"), "d=1");
        assert_eq!(sent("https://api.example.com/"), "d=1");
        assert_eq!(sent("https://deep.api.example.com/"), "d=1");
        // Suffix of the string but not a label boundary.
        assert_eq!(sent("https://notexample.com/"), "");
    }

    #[test]
    fn a_domain_the_host_does_not_match_is_refused() {
        let _g = test_lock();
        reset_for_tests();
        assert!(set_h("https://api.example.com/", "x=1; Domain=other.com").is_err());
        assert!(set_h("https://example.com/", "x=1; Domain=api.example.com").is_err());
        assert_eq!(sent("https://other.com/"), "");
    }

    #[test]
    fn a_public_suffix_domain_is_refused_unless_it_is_the_host_itself() {
        let _g = test_lock();
        reset_for_tests();
        assert!(set_h("https://example.com/", "x=1; Domain=com").is_err(), "a bare TLD");
        assert!(set_h("https://shop.example.co.uk/", "x=1; Domain=co.uk").is_err());
        assert!(set_h("https://me.github.io/", "x=1; Domain=github.io").is_err());
        // Domain=<the host> on a single-label host: becomes host-only.
        set_h("http://localhost/", "l=1; Domain=localhost").unwrap();
        assert_eq!(sent("http://localhost/"), "l=1");
        let c = get(Some("http://localhost/")).into_iter().find(|c| c.name == "l").unwrap();
        assert!(c.host_only);
    }

    #[test]
    fn an_ip_host_takes_only_an_identical_domain() {
        let _g = test_lock();
        reset_for_tests();
        set_h("http://127.0.0.1/", "ip=1").unwrap();
        assert_eq!(sent("http://127.0.0.1/"), "ip=1");
        assert!(domain_match("127.0.0.1", "127.0.0.1"));
        assert!(!domain_match("10.0.0.1", "0.0.1"), "no suffix matching on an IP");
        assert!(set_h("http://10.0.0.1/", "x=1; Domain=0.0.1").is_err());
        // A control character anywhere in the string drops the whole cookie.
        assert!(set_h("http://127.0.0.1/", "p=1; Path=/a\u{1}b").is_err());
        assert!(set_h("http://127.0.0.1/", "p=1; Domain=127.0.0.1\u{7f}").is_err());
    }

    #[test]
    fn paths_match_on_segment_boundaries_and_default_to_the_directory() {
        assert_eq!(default_path("/a/b/c"), "/a/b");
        assert_eq!(default_path("/a"), "/");
        assert_eq!(default_path(""), "/");
        assert_eq!(default_path("x"), "/");
        assert!(path_match("/a/b", "/a"));
        assert!(path_match("/a/", "/a/"));
        assert!(path_match("/a/b", "/a/"));
        assert!(!path_match("/ab", "/a"));
        assert!(path_match("/a", "/a"));
        assert!(!path_match("/", "/a"));

        let _g = test_lock();
        reset_for_tests();
        set_h("https://example.com/api/v1/login", "p=1").unwrap();
        assert_eq!(sent("https://example.com/api/v1/other"), "p=1");
        assert_eq!(sent("https://example.com/api/v2"), "");
        set_h("https://example.com/", "q=1; Path=/api").unwrap();
        assert_eq!(sent("https://example.com/apix"), "");
        assert!(sent("https://example.com/api/v1/x").contains("q=1"));
    }

    #[test]
    fn longer_paths_come_first_then_older_cookies() {
        let _g = test_lock();
        reset_for_tests();
        set_h("https://example.com/", "a=1; Path=/").unwrap();
        set_h("https://example.com/", "b=2; Path=/deep/er").unwrap();
        set_h("https://example.com/", "c=3; Path=/deep").unwrap();
        set_h("https://example.com/", "d=4; Path=/").unwrap();
        assert_eq!(sent("https://example.com/deep/er/x"), "b=2; c=3; a=1; d=4");
    }

    #[test]
    fn secure_cookies_need_a_secure_transport_both_ways() {
        let _g = test_lock();
        reset_for_tests();
        assert!(set_h("http://example.com/", "s=1; Secure").is_err(), "set over http");
        set_h("https://example.com/", "s=1; Secure").unwrap();
        assert_eq!(sent("https://example.com/"), "s=1");
        assert_eq!(sent("http://example.com/"), "", "never sent over http");
        assert_eq!(sent("wss://example.com/"), "s=1");
        assert_eq!(sent("ws://example.com/"), "");
        // Loopback is potentially trustworthy, as in Chromium.
        set_h("http://localhost/", "ls=1; Secure").unwrap();
        assert_eq!(sent("http://localhost/"), "ls=1");
    }

    #[test]
    fn an_insecure_cookie_cannot_shadow_a_secure_one() {
        let _g = test_lock();
        reset_for_tests();
        set_h("https://example.com/", "sid=secure; Secure").unwrap();
        assert!(set_h("http://example.com/", "sid=evil").is_err());
        assert_eq!(sent("https://example.com/"), "sid=secure");
    }

    #[test]
    fn same_site_none_needs_secure_and_prefixes_are_enforced() {
        let _g = test_lock();
        reset_for_tests();
        assert!(set_h("https://example.com/", "n=1; SameSite=None").is_err());
        set_h("https://example.com/", "n=1; SameSite=None; Secure").unwrap();
        assert!(set_h("https://example.com/", "__Secure-a=1").is_err());
        set_h("https://example.com/", "__Secure-a=1; Secure").unwrap();
        assert!(set_h("https://example.com/", "__Host-b=1; Secure; Path=/; Domain=example.com").is_err());
        assert!(set_h("https://example.com/x/y", "__Host-b=1; Secure").is_err(), "no Path=/");
        set_h("https://example.com/x/y", "__Host-b=1; Secure; Path=/").unwrap();
        let s = sent("https://example.com/");
        assert!(s.contains("n=1") && s.contains("__Secure-a=1") && s.contains("__Host-b=1"), "{s}");
    }

    #[test]
    fn max_age_wins_over_expires_and_zero_deletes() {
        let _g = test_lock();
        reset_for_tests();
        set_h("https://example.com/", "m=1; Expires=Wed, 21 Oct 2015 07:28:00 GMT; Max-Age=3600").unwrap();
        assert_eq!(sent("https://example.com/"), "m=1", "Max-Age kept it alive");
        set_h("https://example.com/", "m=1; Max-Age=0").unwrap();
        assert_eq!(sent("https://example.com/"), "", "Max-Age=0 deleted it");
        set_h("https://example.com/", "e=1").unwrap();
        set_h("https://example.com/", "e=2; Expires=Thu, 01 Jan 1970 00:00:00 GMT").unwrap();
        assert_eq!(sent("https://example.com/"), "", "a past Expires deletes the namesake");
    }

    #[test]
    fn expiry_is_capped_at_400_days() {
        let _g = test_lock();
        reset_for_tests();
        set_h("https://example.com/", "far=1; Max-Age=999999999999").unwrap();
        set_h("https://example.com/", "far2=1; Expires=Fri, 01 Jan 9999 00:00:00 GMT").unwrap();
        let now = now_s();
        for c in get(None) {
            let e = c.expires.unwrap();
            assert!(e <= now + MAX_AGE_CAP_S + 1 && e > now + MAX_AGE_CAP_S - 10, "{} {e}", c.name);
        }
    }

    #[test]
    fn replacing_a_cookie_keeps_its_creation_time() {
        let _g = test_lock();
        reset_for_tests();
        set_h("https://example.com/", "a=1").unwrap();
        set_h("https://example.com/", "b=1").unwrap();
        set_h("https://example.com/", "a=2").unwrap();
        // `a` is still the older one, so it still sorts first.
        assert_eq!(sent("https://example.com/"), "a=2; b=1");
    }

    #[test]
    fn script_cannot_read_set_or_overwrite_http_only() {
        let _g = test_lock();
        reset_for_tests();
        set_h("https://example.com/", "h=1; HttpOnly").unwrap();
        assert_eq!(header_for(&u("https://example.com/"), Api::NonHttp), None);
        assert!(set_from_header(&u("https://example.com/"), "h=evil", Api::NonHttp).is_err());
        assert!(set_from_header(&u("https://example.com/"), "z=1; HttpOnly", Api::NonHttp).is_err());
        assert_eq!(sent("https://example.com/"), "h=1");
    }

    #[test]
    fn nameless_cookies_control_characters_and_oversized_ones() {
        let _g = test_lock();
        reset_for_tests();
        set_h("https://example.com/", "justavalue").unwrap();
        assert_eq!(sent("https://example.com/"), "justavalue");
        assert!(set_h("https://example.com/", "a=b\u{1}c").is_err());
        assert!(set_h("https://example.com/", "=").is_err());
        assert!(set_h("https://example.com/", &format!("big={}", "x".repeat(5000))).is_err());
        // An over-long attribute is ignored, not the cookie.
        set_h("https://example.com/", &format!("ok=1; Path=/{}", "p".repeat(1100))).unwrap();
        assert!(sent("https://example.com/").contains("ok=1"));
    }

    #[test]
    fn the_cookie_date_algorithm() {
        let t = parse_cookie_date("Wed, 21 Oct 2015 07:28:00 GMT").unwrap();
        assert_eq!(t, 1_445_412_480);
        // The RFC's tolerance: other separators, order, two-digit years.
        assert_eq!(parse_cookie_date("Wednesday, 21-Oct-15 07:28:00 GMT"), Some(t));
        assert_eq!(parse_cookie_date("21 Oct 2015 07:28:00"), Some(t));
        assert_eq!(parse_cookie_date("Thu, 01 Jan 70 00:00:00 GMT"), Some(0));
        assert_eq!(parse_cookie_date("Sat, 01 Jan 2000 00:00:00 GMT"), Some(946_684_800));
        // Failures.
        assert_eq!(parse_cookie_date("not a date"), None);
        assert_eq!(parse_cookie_date("31 Feb 2015 00:00:00"), None);
        assert_eq!(parse_cookie_date("21 Oct 2015 25:00:00"), None);
        assert_eq!(parse_cookie_date("21 Oct 1500 00:00:00"), None);
        assert_eq!(parse_cookie_date("21 Oct 2015"), None, "no time");
    }

    #[test]
    fn credentials_modes_decide_per_url() {
        assert!(!Credentials::Omit.applies(&u("https://example.com/")));
        assert!(Credentials::Include.applies(&u("https://example.com/")));
        // app://redjs is opaque: same-origin is never true for a network URL.
        assert!(!Credentials::SameOrigin.applies(&u("https://example.com/")));
        assert!(!Credentials::SameOrigin.applies(&u("http://redjs/")));
        assert_eq!(Credentials::parse("include"), Some(Credentials::Include));
        assert_eq!(Credentials::parse("bogus"), None);
    }

    #[test]
    fn with_cookies_appends_after_a_script_cookie_and_never_replaces_it() {
        let _g = test_lock();
        reset_for_tests();
        set_h("https://example.com/", "jar=1").unwrap();
        let out = with_cookies(&u("https://example.com/"), &[("cookie".into(), "mine=1".into())]).unwrap();
        assert_eq!(out, vec![("cookie".to_string(), "mine=1; jar=1".to_string())]);
        let out = with_cookies(&u("https://example.com/"), &[]).unwrap();
        assert_eq!(out, vec![("cookie".to_string(), "jar=1".to_string())]);
        assert!(with_cookies(&u("https://other.com/"), &[]).is_none());
    }

    #[test]
    fn the_host_api_follows_the_server_rules() {
        let _g = test_lock();
        reset_for_tests();
        let base = HostCookie {
            url: "https://argon.example/".into(),
            name: "Probe".into(),
            value: "a=1&b=2".into(),
            domain: Some(".argon.example".into()),
            path: Some("/".into()),
            secure: true,
            http_only: true,
            same_site: SameSite::None,
            expires: Some(SystemTime::now() + std::time::Duration::from_secs(30 * 86_400)),
        };
        set(base.clone()).unwrap();
        assert_eq!(sent("https://api.argon.example/ion/x"), "Probe=a=1&b=2");
        assert_eq!(sent("http://api.argon.example/"), "", "Secure");
        assert_eq!(header_for(&u("https://api.argon.example/"), Api::NonHttp), None, "HttpOnly");
        // A host cannot set what a server could not.
        assert!(set(HostCookie { domain: Some("other.example".into()), ..base.clone() }).is_err());
        assert!(set(HostCookie { url: "http://argon.example/".into(), ..base.clone() }).is_err());
        assert!(set(HostCookie { value: "a;b".into(), ..base.clone() }).is_err());
        assert_eq!(get(Some("https://api.argon.example/")).len(), 1);
        assert_eq!(remove("https://api.argon.example/", "Probe"), 1);
        assert_eq!(sent("https://api.argon.example/"), "");
    }

    #[test]
    fn a_session_cookie_is_never_handed_to_the_host() {
        struct Rec(Mutex<Vec<Vec<Cookie>>>);
        impl CookieHost for Rec {
            fn load(&self) -> Vec<Cookie> {
                vec![Cookie {
                    name: "old".into(),
                    value: "disk".into(),
                    domain: "example.com".into(),
                    path: "/".into(),
                    host_only: true,
                    secure: false,
                    http_only: false,
                    same_site: SameSite::Lax,
                    expires: Some(now_s() + 3600),
                    creation: 1,
                    last_access: 1,
                }]
            }
            fn store(&self, cookies: &[Cookie]) {
                self.0.lock().unwrap().push(cookies.to_vec());
            }
        }
        let _g = test_lock();
        reset_for_tests();
        let rec: &'static Rec = Box::leak(Box::new(Rec(Mutex::new(Vec::new()))));
        let prev = host();
        set_host(rec);
        set_h("https://example.com/", "session=1").unwrap();
        set_h("https://example.com/", "kept=1; Max-Age=60").unwrap();
        assert_eq!(sent("https://example.com/"), "old=disk; session=1; kept=1", "loaded from the host");
        let last = rec.0.lock().unwrap().last().cloned().unwrap();
        let names: Vec<_> = last.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["old", "kept"], "the session cookie stayed in memory");
        *HOST.write().unwrap() = prev;
        reset_for_tests();
    }

    #[test]
    fn eviction_drops_the_least_recently_used_past_the_per_domain_limit() {
        let _g = test_lock();
        reset_for_tests();
        for i in 0..(MAX_PER_DOMAIN + 5) {
            set_h("https://many.example/", &format!("c{i}=1")).unwrap();
        }
        let n = get(None).iter().filter(|c| c.domain == "many.example").count();
        assert_eq!(n, MAX_PER_DOMAIN);
        assert!(!sent("https://many.example/").contains("c0=1"), "the oldest went first");
        reset_for_tests();
    }
}
