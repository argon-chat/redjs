//! The one MIME table: extension → type and type → extensions.
//!
//! Three consumers read it, and that is the point of having one:
//!
//! - `File.type` for a file the host hands over (a picker, a drop, `appFs`) —
//!   [`type_for_name`];
//! - the `Content-Type` of a local `fetch()` — [`super::fsutil::guess_content_type`],
//!   which falls back to `application/octet-stream` where a `File` says `""`;
//! - an `accept="image/*,.pdf"` / FSA `types` list turned into a picker
//!   filter — [`exts_for`], with `type/*` wildcards.
//!
//! # Where the rows come from
//!
//! Chromium's own hard-coded mappings (`net/base/mime_util.cc`, primary and
//! secondary) first, so the types a page sees for the common extensions are the
//! ones Chrome reports — `xml` is `text/xml`, `m4a` is `audio/x-m4a`, `js` is
//! `text/javascript`. Then the IANA / mime-db spelling for what Chromium leaves
//! to the platform (fonts, Office, archives, 3D models, `md`).
//!
//! # The registry, and why only in one direction
//!
//! Chromium on Windows asks the registry in both directions. Here only the
//! REVERSE lookup does (`MIME\Database\Content Type\<type>` → `Extension`, read
//! only, per-user `HKCU\Software\Classes` first, then the merged `HKCR`), which
//! is what turns an `accept` type the table does not know — say
//! `audio/x-ms-wma` — into a filter that finds `.wma` files. The forward
//! direction stays table-only on purpose: `HKCR\.ts` says `.ts` is an MPEG
//! transport stream on every Windows box, and a `File.type` that depends on
//! which media player is installed is not one an app can test against. The
//! consequence is a named divergence (DESKTOP.md D-F15): an extension outside
//! this table gets `File.type === ""` where Chrome may report the registry's
//! type.

/// `(extension, type)`, lowercase. The FIRST row for an extension is its type;
/// a type's extensions are its rows in this order (the first is the one a save
/// dialog would suggest).
const TABLE: &[(&str, &str)] = &[
    // -- the web ------------------------------------------------------------
    ("html", "text/html"),
    ("htm", "text/html"),
    ("shtml", "text/html"),
    ("shtm", "text/html"),
    ("xhtml", "application/xhtml+xml"),
    ("xht", "application/xhtml+xml"),
    ("xhtm", "application/xhtml+xml"),
    ("css", "text/css"),
    ("js", "text/javascript"),
    ("mjs", "text/javascript"),
    ("json", "application/json"),
    ("map", "application/json"),
    ("jsonld", "application/ld+json"),
    ("webmanifest", "application/manifest+json"),
    ("wasm", "application/wasm"),
    ("xml", "text/xml"),
    ("xsl", "text/xml"),
    ("xslt", "text/xml"),
    ("xbl", "text/xml"),
    ("rss", "application/rss+xml"),
    ("atom", "application/atom+xml"),
    ("rdf", "application/rdf+xml"),
    ("mht", "multipart/related"),
    ("mhtml", "multipart/related"),
    ("crx", "application/x-chrome-extension"),
    // -- text ---------------------------------------------------------------
    ("txt", "text/plain"),
    ("text", "text/plain"),
    ("log", "text/plain"),
    ("ini", "text/plain"),
    ("conf", "text/plain"),
    ("csv", "text/csv"),
    ("tsv", "text/tab-separated-values"),
    ("md", "text/markdown"),
    ("markdown", "text/markdown"),
    ("ics", "text/calendar"),
    ("vtt", "text/vtt"),
    ("srt", "application/x-subrip"),
    ("sh", "text/x-sh"),
    ("yaml", "application/yaml"),
    ("yml", "application/yaml"),
    ("toml", "application/toml"),
    ("rtf", "application/rtf"),
    ("eml", "message/rfc822"),
    // -- images -------------------------------------------------------------
    ("png", "image/png"),
    ("apng", "image/apng"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("jfif", "image/jpeg"),
    ("pjpeg", "image/jpeg"),
    ("pjp", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("avif", "image/avif"),
    ("svg", "image/svg+xml"),
    ("svgz", "image/svg+xml"),
    ("bmp", "image/bmp"),
    ("ico", "image/x-icon"),
    ("tif", "image/tiff"),
    ("tiff", "image/tiff"),
    ("xbm", "image/x-xbitmap"),
    ("heic", "image/heic"),
    ("heif", "image/heif"),
    ("jxl", "image/jxl"),
    ("psd", "image/vnd.adobe.photoshop"),
    // -- audio --------------------------------------------------------------
    ("mp3", "audio/mpeg"),
    ("wav", "audio/wav"),
    ("ogg", "audio/ogg"),
    ("oga", "audio/ogg"),
    ("opus", "audio/ogg"),
    ("flac", "audio/flac"),
    ("m4a", "audio/x-m4a"),
    ("aac", "audio/aac"),
    ("weba", "audio/webm"),
    ("mid", "audio/midi"),
    ("midi", "audio/midi"),
    ("aif", "audio/x-aiff"),
    ("aiff", "audio/x-aiff"),
    // -- video --------------------------------------------------------------
    ("mp4", "video/mp4"),
    ("m4v", "video/mp4"),
    ("webm", "video/webm"),
    ("ogv", "video/ogg"),
    ("ogm", "video/ogg"),
    ("mpeg", "video/mpeg"),
    ("mpg", "video/mpeg"),
    ("mov", "video/quicktime"),
    ("avi", "video/x-msvideo"),
    ("mkv", "video/x-matroska"),
    ("wmv", "video/x-ms-wmv"),
    ("3gp", "video/3gpp"),
    ("m3u8", "application/x-mpegurl"),
    // -- fonts --------------------------------------------------------------
    ("woff", "font/woff"),
    ("woff2", "font/woff2"),
    ("ttf", "font/ttf"),
    ("otf", "font/otf"),
    ("eot", "application/vnd.ms-fontobject"),
    // -- documents ----------------------------------------------------------
    ("pdf", "application/pdf"),
    ("doc", "application/msword"),
    ("dot", "application/msword"),
    ("docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
    ("xls", "application/vnd.ms-excel"),
    ("xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
    ("ppt", "application/vnd.ms-powerpoint"),
    ("pptx", "application/vnd.openxmlformats-officedocument.presentationml.presentation"),
    ("odt", "application/vnd.oasis.opendocument.text"),
    ("ods", "application/vnd.oasis.opendocument.spreadsheet"),
    ("odp", "application/vnd.oasis.opendocument.presentation"),
    ("epub", "application/epub+zip"),
    ("ps", "application/postscript"),
    ("eps", "application/postscript"),
    ("ai", "application/postscript"),
    // -- archives -----------------------------------------------------------
    ("zip", "application/zip"),
    ("gz", "application/gzip"),
    ("tgz", "application/gzip"),
    ("tar", "application/x-tar"),
    ("7z", "application/x-7z-compressed"),
    ("rar", "application/vnd.rar"),
    ("bz2", "application/x-bzip2"),
    ("xz", "application/x-xz"),
    ("zst", "application/zstd"),
    // -- binaries and certificates -----------------------------------------
    ("bin", "application/octet-stream"),
    ("exe", "application/x-msdownload"),
    ("dll", "application/x-msdownload"),
    ("apk", "application/vnd.android.package-archive"),
    ("cer", "application/x-x509-ca-cert"),
    ("crt", "application/x-x509-ca-cert"),
    ("p7s", "application/pkcs7-signature"),
    // -- 3D -----------------------------------------------------------------
    ("gltf", "model/gltf+json"),
    ("glb", "model/gltf-binary"),
    ("obj", "model/obj"),
    ("stl", "model/stl"),
    ("usdz", "model/vnd.usdz+zip"),
];

/// Spellings an `accept` attribute uses for a type the table spells
/// differently. Reverse direction only: a `File` never reports an alias.
const ALIASES: &[(&str, &str)] = &[
    ("image/jpg", "image/jpeg"),
    ("image/pjpeg", "image/jpeg"),
    ("image/x-png", "image/png"),
    ("image/vnd.microsoft.icon", "image/x-icon"),
    ("audio/mp3", "audio/mpeg"),
    ("audio/x-wav", "audio/wav"),
    ("audio/wave", "audio/wav"),
    ("audio/x-flac", "audio/flac"),
    ("audio/mp4", "audio/x-m4a"),
    ("application/javascript", "text/javascript"),
    ("application/x-javascript", "text/javascript"),
    ("application/xml", "text/xml"),
    ("application/x-gzip", "application/gzip"),
    ("text/x-markdown", "text/markdown"),
];

/// The table's type for an extension (without the dot, any case).
pub fn type_for_extension(ext: &str) -> Option<&'static str> {
    let ext = ext.strip_prefix('.').unwrap_or(ext);
    TABLE.iter().find(|(e, _)| e.eq_ignore_ascii_case(ext)).map(|(_, t)| *t)
}

/// The type for a file NAME (or a path, or a URL key): the extension after the
/// last `.` of the last path segment. `None` when there is none or the table
/// does not know it — `File.type` is then `""`, as in a browser.
pub fn type_for_name(name: &str) -> Option<&'static str> {
    let leaf = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let (_, ext) = leaf.rsplit_once('.')?;
    if ext.is_empty() {
        return None;
    }
    type_for_extension(ext)
}

/// A MIME type's essence: `type/subtype`, lowercased, parameters and
/// whitespace dropped. `None` for anything that is not one (no `/`, an empty
/// half, a character outside the RFC 9110 token set) — which is also what
/// keeps a malformed `accept` token out of a registry path.
pub fn essence(mime: &str) -> Option<String> {
    let head = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let (t, s) = head.split_once('/')?;
    let token = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
    (token(t) && token(s)).then_some(head)
}

/// The extensions (lowercase, no dot) a type names, preferred first; for
/// `type/*`, every extension of that top-level type. Empty for an unknown or
/// malformed type, and for `*/*` — "anything" is not a filter.
///
/// On Windows an exact type also gets the registry's `Extension` for it
/// (Chromium's `GetExtensionsForMimeType` does the same), deduplicated.
pub fn exts_for(mime: &str) -> Vec<String> {
    let Some(m) = essence(mime) else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    let mut push = |e: &str| {
        if !out.iter().any(|x| x == e) {
            out.push(e.to_string());
        }
    };
    if let Some(top) = m.strip_suffix("/*") {
        if top == "*" {
            return Vec::new();
        }
        let prefix = format!("{top}/");
        for (e, t) in TABLE {
            if t.starts_with(&prefix) {
                push(e);
            }
        }
        return out;
    }
    let canonical = ALIASES.iter().find(|(a, _)| *a == m).map(|(_, t)| *t).unwrap_or(m.as_str());
    for (e, t) in TABLE {
        if *t == canonical {
            push(e);
        }
    }
    #[cfg(windows)]
    if let Some(e) = registry::extension_for(&m) {
        push(&e);
    }
    out
}

/// Every row, for a caller that builds its own view of the table (a picker's
/// "All supported types" entry, a test).
pub fn table() -> &'static [(&'static str, &'static str)] {
    TABLE
}

#[cfg(windows)]
mod registry {
    //! `MIME\Database\Content Type\<type>` → its `Extension` value. READ ONLY:
    //! nothing here opens a key for writing. Raw advapi32, no dependency, the
    //! same way `web.rs` reaches kernel32.

    use core::ffi::c_void;
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    type Hkey = isize;
    /// The predefined handles are sign-extended 32-bit values on Win64.
    const HKEY_CLASSES_ROOT: Hkey = 0x8000_0000u32 as i32 as isize;
    const HKEY_CURRENT_USER: Hkey = 0x8000_0001u32 as i32 as isize;
    const RRF_RT_REG_SZ: u32 = 0x0000_0002;
    const ERROR_SUCCESS: i32 = 0;

    #[link(name = "advapi32")]
    extern "system" {
        fn RegGetValueW(
            hkey: Hkey,
            sub_key: *const u16,
            value: *const u16,
            flags: u32,
            ty: *mut u32,
            data: *mut c_void,
            cb: *mut u32,
        ) -> i32;
    }

    fn read(root: Hkey, sub: &str) -> Option<String> {
        let sub16: Vec<u16> = sub.encode_utf16().chain([0]).collect();
        let name16: Vec<u16> = "Extension".encode_utf16().chain([0]).collect();
        let mut buf = [0u16; 64];
        let mut cb = (buf.len() * 2) as u32;
        let rc = unsafe {
            RegGetValueW(
                root,
                sub16.as_ptr(),
                name16.as_ptr(),
                RRF_RT_REG_SZ,
                core::ptr::null_mut(),
                buf.as_mut_ptr() as *mut c_void,
                &mut cb,
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let n = (cb as usize / 2).min(buf.len());
        let s = String::from_utf16_lossy(&buf[..n]);
        let s = s.trim_end_matches('\0');
        // `.ext` and nothing stranger: the value is somebody else's data.
        let ext = s.strip_prefix('.')?.to_ascii_lowercase();
        let sane = !ext.is_empty() && ext.len() <= 16 && ext.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        sane.then_some(ext)
    }

    /// `essence` is already a validated `type/subtype`, so it cannot climb out
    /// of the key it names. Cached: an `accept` attribute is re-read on every
    /// picker open, and the answer does not change while we run.
    pub(super) fn extension_for(essence: &str) -> Option<String> {
        static CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        if let Some(hit) = cache.lock().unwrap().get(essence) {
            return hit.clone();
        }
        let sub = format!("MIME\\Database\\Content Type\\{essence}");
        let found = read(HKEY_CURRENT_USER, &format!("Software\\Classes\\{sub}")).or_else(|| read(HKEY_CLASSES_ROOT, &sub));
        cache.lock().unwrap().insert(essence.to_string(), found.clone());
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_common_extensions_get_chromiums_types() {
        assert_eq!(type_for_extension("png"), Some("image/png"));
        assert_eq!(type_for_extension("PNG"), Some("image/png"), "case-insensitive");
        assert_eq!(type_for_extension(".jpg"), Some("image/jpeg"), "a leading dot is fine");
        assert_eq!(type_for_extension("jpeg"), Some("image/jpeg"));
        assert_eq!(type_for_extension("js"), Some("text/javascript"));
        assert_eq!(type_for_extension("xml"), Some("text/xml"), "Chromium's, not mime-db's application/xml");
        assert_eq!(type_for_extension("m4a"), Some("audio/x-m4a"));
        assert_eq!(type_for_extension("svg"), Some("image/svg+xml"));
        assert_eq!(type_for_extension("csv"), Some("text/csv"));
        assert_eq!(type_for_extension("nope"), None);
        assert_eq!(type_for_extension(""), None);
    }

    #[test]
    fn a_name_is_typed_by_the_extension_of_its_last_segment() {
        assert_eq!(type_for_name("photo.JPG"), Some("image/jpeg"));
        assert_eq!(type_for_name("C:\\a.b\\report.pdf"), Some("application/pdf"));
        assert_eq!(type_for_name("dir.d/notes"), None, "a dot in a folder is not an extension");
        assert_eq!(type_for_name("archive.tar.gz"), Some("application/gzip"));
        assert_eq!(type_for_name("README"), None);
        assert_eq!(type_for_name("trailing."), None);
        assert_eq!(type_for_name("assets/a.png"), Some("image/png"));
    }

    #[test]
    fn the_table_is_about_a_hundred_and_ten_rows_and_every_row_round_trips() {
        assert!(TABLE.len() >= 110, "{} rows", TABLE.len());
        for (e, t) in TABLE {
            assert_eq!(e.to_ascii_lowercase(), *e, "{e}: rows are lowercase");
            // The FIRST row for an extension is its type…
            let first = TABLE.iter().find(|(x, _)| x == e).unwrap().1;
            assert_eq!(type_for_extension(e), Some(first), "{e}");
            assert_eq!(first, *t, "{e} is listed twice with different types");
            // …and the type's extensions include it.
            assert!(exts_for(t).iter().any(|x| x == e), "{t} does not list {e}");
            assert_eq!(essence(t).as_deref(), Some(*t), "{t} is a valid essence");
        }
    }

    #[test]
    fn reverse_lookups_are_ordered_and_take_parameters_and_aliases() {
        let jpeg = exts_for("image/jpeg");
        assert_eq!(jpeg[..2], ["jpg".to_string(), "jpeg".to_string()], "{jpeg:?}");
        assert!(exts_for("Text/Plain; charset=utf-8").contains(&"txt".to_string()));
        assert!(exts_for("image/jpg").contains(&"jpg".to_string()), "a common misspelling in accept=");
        assert!(exts_for("application/javascript").contains(&"js".to_string()));
        assert!(exts_for("not a type").is_empty());
        assert!(exts_for("image/").is_empty());
        assert!(exts_for("image/png\\..\\x").is_empty(), "a backslash never reaches a registry path");
    }

    #[test]
    fn wildcards_cover_their_top_level_type_only() {
        let images = exts_for("image/*");
        for e in ["png", "jpg", "jpeg", "gif", "webp", "svg", "avif", "bmp", "ico"] {
            assert!(images.iter().any(|x| x == e), "image/* lacks {e}: {images:?}");
        }
        assert!(!images.iter().any(|x| x == "mp4"), "{images:?}");
        let video = exts_for("VIDEO/*");
        assert!(video.contains(&"mp4".to_string()) && video.contains(&"webm".to_string()), "{video:?}");
        assert!(exts_for("audio/*").contains(&"mp3".to_string()));
        assert!(exts_for("*/*").is_empty(), "anything is not a filter");
    }

    /// The fallback is real: `audio/x-ms-wma` is not in the table, and every
    /// Windows install registers it with `.wma` (HKLM\Software\Classes\MIME).
    /// Nothing is written: the module never opens a key for writing.
    #[cfg(windows)]
    #[test]
    fn an_unknown_type_falls_back_to_the_registry() {
        assert!(TABLE.iter().all(|(_, t)| *t != "audio/x-ms-wma"));
        assert_eq!(exts_for("audio/x-ms-wma"), vec!["wma".to_string()]);
        assert!(exts_for("application/x-vn-test-no-such-type").is_empty());
        // The table's answer comes first; the registry only adds.
        let png = exts_for("image/png");
        assert_eq!(png.first().map(String::as_str), Some("png"));
        assert_eq!(png.iter().filter(|e| *e == "png").count(), 1, "deduplicated: {png:?}");
    }
}
