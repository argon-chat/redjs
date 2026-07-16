//! URL→file resolution for fetch/Image (port of the player's
//! FetchApi.ResolveLocalPath) + content-type guessing + data: URI decoding.

use core::ffi::c_void;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Default, Clone)]
pub struct Roots {
    pub scripts: PathBuf,
    pub assets: PathBuf,
}

fn roots() -> &'static Mutex<HashMap<usize, Roots>> {
    static R: OnceLock<Mutex<HashMap<usize, Roots>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn set_roots(g: *mut c_void, scripts: &str, assets: &str) {
    roots()
        .lock()
        .unwrap()
        .insert(g as usize, Roots { scripts: PathBuf::from(scripts), assets: PathBuf::from(assets) });
}

pub fn get_roots(g: *mut c_void) -> Roots {
    roots().lock().unwrap().get(&(g as usize)).cloned().unwrap_or_default()
}

// --- host content provider (W-HIVE) ----------------------------------------
// The player routes every content-relative local read through its single
// C#-side IContentProvider (DeHive `game.hb` bank or loose files), registered
// per context via `redjs_content_provider_set`. Standard C ABI — these are
// called BY Rust, not by JSC, so no SysV trampoline is involved.

/// Receives the full decoded bytes of one read. Called exactly once per hit.
pub type ContentSinkFn = unsafe extern "C" fn(sink_ctx: *mut c_void, ptr: *const u8, len: usize);

/// Host read: UTF-8 content-relative path in, `sink` called once with the
/// decoded bytes on success. Returns 0 on hit, non-zero on miss (the caller
/// then falls back to on-disk root resolution).
pub type ContentReadFn =
    unsafe extern "C" fn(path_ptr: *const u8, path_len: usize, sink: ContentSinkFn, sink_ctx: *mut c_void) -> i32;

fn providers() -> &'static Mutex<HashMap<usize, ContentReadFn>> {
    static P: OnceLock<Mutex<HashMap<usize, ContentReadFn>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn set_content_provider(g: *mut c_void, read: Option<ContentReadFn>) {
    let mut map = providers().lock().unwrap();
    match read {
        Some(f) => {
            map.insert(g as usize, f);
        }
        None => {
            map.remove(&(g as usize));
        }
    }
}

fn get_content_provider(g: *mut c_void) -> Option<ContentReadFn> {
    providers().lock().unwrap().get(&(g as usize)).copied()
}

unsafe extern "C" fn vec_sink(sink_ctx: *mut c_void, ptr: *const u8, len: usize) {
    if len == 0 {
        return;
    }
    let vec = &mut *(sink_ctx as *mut Vec<u8>);
    vec.extend_from_slice(core::slice::from_raw_parts(ptr, len));
}

/// URL → provider key: query/hash stripped, separators normalized, `./` and
/// leading `/` trimmed — the same cleaning `resolve_local` applies before its
/// root join. None for `file:///` and absolute host paths (host-filesystem
/// forms, never content keys).
fn clean_relative(url: &str) -> Option<String> {
    let mut clean = url;
    if let Some(cut) = clean.find(['?', '#']) {
        clean = &clean[..cut];
    }
    if clean.starts_with("file:///") {
        return None;
    }
    let clean = clean.replace('\\', "/");
    let mut clean = clean.as_str();
    while let Some(rest) = clean.strip_prefix("./") {
        clean = rest;
    }
    let clean = clean.trim_start_matches('/');
    if clean.is_empty() || Path::new(clean).is_absolute() {
        return None;
    }
    Some(clean.to_string())
}

pub fn guess_content_type(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "json" => "application/json",
        "js" | "mjs" => "text/javascript",
        "wasm" => "application/wasm",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

/// Strip query/hash + file://, then resolve against scripts dir, assets dir.
pub fn resolve_local(r: &Roots, url: &str) -> Option<PathBuf> {
    let mut clean = url;
    if let Some(cut) = clean.find(['?', '#']) {
        clean = &clean[..cut];
    }
    if let Some(rest) = clean.strip_prefix("file:///") {
        return Some(PathBuf::from(rest.replace("%20", " ")));
    }
    let clean = clean.replace('\\', "/");
    let mut clean = clean.as_str();
    while let Some(rest) = clean.strip_prefix("./") {
        clean = rest;
    }
    let clean = clean.trim_start_matches('/');
    if clean.is_empty() {
        return None;
    }
    let p = Path::new(clean);
    if p.is_absolute() && p.exists() {
        return Some(p.to_path_buf());
    }
    for base in [&r.scripts, &r.assets] {
        if base.as_os_str().is_empty() {
            continue;
        }
        let candidate = base.join(clean);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    Some(r.scripts.join(clean)) // useful 404 path
}

pub struct Fetched {
    pub ok: bool,
    pub status: i32,
    pub status_text: &'static str,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Local-file / data:-URI fetch. http(s) → status 0 (network error, spec:
/// fetch resolves, response.ok = false handled by the caller shape).
pub fn fetch_url(g: *mut c_void, url: &str) -> Fetched {
    if let Some(rest) = url.strip_prefix("data:") {
        return fetch_data_uri(rest);
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Fetched { ok: false, status: 0, status_text: "", mime: "text/plain".into(), bytes: Vec::new() };
    }
    // Host content provider (W-HIVE): content-relative reads are served by the player's single
    // IContentProvider (bank or loose). A miss falls through to the on-disk resolution below
    // (absolute paths, dev layouts without a provider).
    if let Some(read) = get_content_provider(g) {
        if let Some(key) = clean_relative(url) {
            let mut bytes: Vec<u8> = Vec::new();
            let rc = unsafe { read(key.as_ptr(), key.len(), vec_sink, &mut bytes as *mut Vec<u8> as *mut c_void) };
            if rc == 0 {
                return Fetched {
                    ok: true,
                    status: 200,
                    status_text: "OK",
                    mime: guess_content_type(&key).to_string(),
                    bytes,
                };
            }
        }
    }
    let r = get_roots(g);
    match resolve_local(&r, url).and_then(|p| std::fs::read(&p).ok().map(|b| (p, b))) {
        Some((path, bytes)) => Fetched {
            ok: true,
            status: 200,
            status_text: "OK",
            mime: guess_content_type(&path.to_string_lossy()).to_string(),
            bytes,
        },
        None => Fetched { ok: false, status: 404, status_text: "Not Found", mime: "text/plain".into(), bytes: Vec::new() },
    }
}

fn fetch_data_uri(rest: &str) -> Fetched {
    let Some(comma) = rest.find(',') else {
        return Fetched { ok: false, status: 0, status_text: "", mime: "text/plain".into(), bytes: Vec::new() };
    };
    let meta = &rest[..comma];
    let payload = &rest[comma + 1..];
    let is_b64 = meta.to_ascii_lowercase().ends_with(";base64");
    let mime = meta.split(';').next().unwrap_or("").to_string();
    let mime = if mime.is_empty() { "text/plain".to_string() } else { mime };
    let bytes = if is_b64 {
        match b64_decode(payload) {
            Some(b) => b,
            None => return Fetched { ok: false, status: 0, status_text: "", mime, bytes: Vec::new() },
        }
    } else {
        percent_decode(payload)
    };
    Fetched { ok: true, status: 200, status_text: "OK", mime, bytes }
}

pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        buf = (buf << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

pub fn b64_encode(bytes: &[u8]) -> String {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triplet = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[(triplet >> 18) as usize & 63] as char);
        out.push(B64[(triplet >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(triplet >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[triplet as usize & 63] as char } else { '=' });
    }
    out
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h << 4 | l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}
