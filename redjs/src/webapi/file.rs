//! `File` and `FileList` (W3C File API §4, §5).
//!
//! A `File` is a [`Blob`](super::blob) with a name, a `lastModified` and a
//! `webkitRelativePath` — the same [`BlobData`] with [`FileMeta`] set, in a
//! host class whose parent is the Blob class, on `File.prototype` (whose
//! prototype is `Blob.prototype`), so every Blob method accepts it and
//! `file instanceof Blob` holds.
//!
//! Script makes Files with `new File(bits, name, options)`: in memory, or
//! sharing the source of a single Blob part (`new File([picked], "renamed")`
//! is still path-backed and still lazy). The HOST makes them from disk with
//! [`create_file`] (a picker, a drop, `appFs`): nothing is read until a
//! consumer asks, and then under Chromium's snapshot rule (see the Blob
//! module docs).
//!
//! `FileList` is immutable: made once by the host ([`create_file_list`]) from
//! Files, never constructible from script. Its indices are own data
//! properties, defined once — `writable: false, enumerable: true,
//! configurable: false`. A browser's FileList is a legacy platform object
//! whose index properties report `configurable: true` yet refuse deletion,
//! and which refuses a write to ANY index; neither is expressible without a
//! Proxy, which would break the brand check. Reads, iteration, `item()`,
//! `length`, deletion and writes to the defined indices behave as in a
//! browser; DESKTOP.md D-F16 names the rest.

use core::ffi::c_void;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use super::blob::{self, BlobData, DiskFile, FileMeta, Source};
use super::util::*;
use crate::glue;

/// What a FileList instance carries natively; the Files themselves live in a
/// private slot (a JS array), where the collector sees them.
pub struct FileListData {
    pub len: u32,
}

struct FileClasses {
    file: Class,
    /// The File host class: a subclass of the Blob one (see [`install`]).
    file_cls: crate::capi::JSClassRef,
    list: Class,
    list_cls: crate::capi::JSClassRef,
    /// The private slot holding a FileList's array of Files.
    s_items: *mut c_void,
    /// `Object.defineProperty` as installed (the index properties).
    define_property: u64,
}

unsafe impl Send for FileClasses {}
unsafe impl Sync for FileClasses {}

fn registry() -> &'static Mutex<HashMap<usize, &'static FileClasses>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static FileClasses>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> Option<&'static FileClasses> {
    registry().lock().unwrap().get(&(g as usize)).copied()
}

/// The classes were protected in a heap that no longer exists.
pub(crate) fn forget(gone: crate::context::Gone) {
    registry().lock().unwrap().remove(&gone.key);
}

/// Milliseconds since the epoch, floored — `lastModified` of a moment.
fn epoch_ms(t: SystemTime) -> f64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as f64,
        Err(e) => -(e.duration().as_millis() as f64) - if e.duration().subsec_nanos() % 1_000_000 != 0 { 1.0 } else { 0.0 },
    }
}

/// `lastModified` of a File made now (the constructor's default, FormData's
/// "blob" File).
pub(crate) fn now_ms() -> f64 {
    epoch_ms(SystemTime::now())
}

/// WebIDL `long long` (no `[Clamp]`, no `[EnforceRange]`): NaN and the
/// infinities are 0, the rest truncated and wrapped into 64 bits.
fn to_long_long(d: f64) -> f64 {
    if !d.is_finite() {
        return 0.0;
    }
    const TWO_64: f64 = 18_446_744_073_709_551_616.0;
    let t = d.trunc();
    if t.abs() < TWO_64 / 2.0 {
        // In range already — and `rem_euclid` of a small negative would round
        // to 2^64 in f64 and wrap to 0.
        return if t == 0.0 { 0.0 } else { t };
    }
    let t = t.rem_euclid(TWO_64);
    let t = if t >= TWO_64 / 2.0 { t - TWO_64 } else { t };
    if t == 0.0 { 0.0 } else { t }
}

unsafe fn file_data<'a>(g: *mut c_void, v: u64) -> Option<(&'a BlobData, &'a FileMeta)> {
    let b = blob::data_of(g, v)?;
    let m = b.file.as_ref()?;
    Some((b, m))
}

// ---------------------------------------------------------------------------
// File
// ---------------------------------------------------------------------------

unsafe extern "sysv64" fn file_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(c) = classes(g) else { return throw_type_error(g, "File is not installed in this context") };
    let argc = cf_argc(cf);
    if argc < 2 {
        return throw_type_error(g, &format!("Failed to construct 'File': 2 arguments required, but only {argc} present."));
    }
    // Arguments in order (WebIDL), then the dictionary members in order:
    // `endings`, `type` (BlobPropertyBag), then `lastModified`.
    let Ok(parts) = blob::convert_parts(g, cf_arg(cf, 0), "File") else { return UNDEF };
    let Ok(name) = to_js_string(g, cf_arg(cf, 1)) else { return UNDEF };
    let name = read_string(g, name).unwrap_or_default();
    let options = cf_arg(cf, 2);
    let Ok((mime, native)) = blob::convert_bag(g, options, "File") else { return UNDEF };
    let mut last_modified = now_ms();
    if blob::is_object(g, options) {
        let Ok(lm) = try_get(g, options, "lastModified") else { return UNDEF };
        if lm != UNDEF {
            let Ok(d) = to_number(g, lm) else { return UNDEF };
            last_modified = to_long_long(d);
        }
    }
    let range = blob::assemble(parts, native, "the File constructor");
    let proto = {
        let nt = cf_new_target(cf);
        let p = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
        if is_cell(p) { p } else { c.file.proto }
    };
    let file = FileMeta { name, last_modified, relative_path: String::new(), backed: false };
    host_create(g, c.file_cls, proto, BlobData { source: range.source, offset: range.offset, len: range.len, mime, file: Some(file) })
}

unsafe extern "sysv64" fn get_name(g: *mut c_void, cf: *mut c_void) -> u64 {
    match file_data(g, cf_this(cf)) {
        Some((_, m)) => {
            let s = m.name.clone();
            js_string(g, &s)
        }
        None => throw_type_error(g, "File.prototype.name called on a non-File"),
    }
}

unsafe extern "sysv64" fn get_last_modified(g: *mut c_void, cf: *mut c_void) -> u64 {
    match file_data(g, cf_this(cf)) {
        Some((_, m)) => enc_f64(m.last_modified),
        None => throw_type_error(g, "File.prototype.lastModified called on a non-File"),
    }
}

unsafe extern "sysv64" fn get_relative_path(g: *mut c_void, cf: *mut c_void) -> u64 {
    match file_data(g, cf_this(cf)) {
        Some((_, m)) => {
            let s = m.relative_path.clone();
            js_string(g, &s)
        }
        None => throw_type_error(g, "File.prototype.webkitRelativePath called on a non-File"),
    }
}

/// The size and mtime a path-backed File is checked against on every read.
pub fn snapshot(path: &Path) -> std::io::Result<(u64, Option<SystemTime>)> {
    let m = std::fs::metadata(path)?;
    Ok((m.len(), m.modified().ok()))
}

/// A path-backed `File` — the host's way in (picker results, drops, `appFs`).
/// Nothing is read here; `size` and `mtime` are the snapshot every later read
/// is checked against ([`snapshot`] takes one). `mime: None` looks the type up
/// from `name`'s extension ([`super::mime`]); an unknown one is `""`, as in a
/// browser. `lastModified` is the mtime in ms, or now when the filesystem
/// keeps none (Chromium does the same).
///
/// # Safety
/// `g` must be a live context with the web API installed, on its thread.
pub unsafe fn create_file(
    g: *mut c_void,
    path: &Path,
    name: &str,
    mime: Option<&str>,
    size: u64,
    mtime: Option<SystemTime>,
    relative_path: &str,
) -> u64 {
    let Some(c) = classes(g) else { return UNDEF };
    let mime = match mime {
        Some(m) => m.to_ascii_lowercase(),
        None => super::mime::type_for_name(name).unwrap_or("").to_string(),
    };
    let last_modified = epoch_ms(mtime.unwrap_or_else(SystemTime::now));
    let disk = DiskFile { path: path.to_path_buf(), size, mtime };
    let data = BlobData {
        source: Source::Disk(Arc::new(disk)),
        offset: 0,
        len: size,
        mime,
        file: Some(FileMeta { name: name.to_string(), last_modified, relative_path: relative_path.to_string(), backed: true }),
    };
    host_create(g, c.file_cls, c.file.proto, data)
}

/// [`create_file`] for a path as it is now: stat'ed here, named after its
/// last component. `Err` when it cannot be stat'ed or is a directory.
///
/// # Safety
/// As [`create_file`].
pub unsafe fn file_from_path(g: *mut c_void, path: &Path, relative_path: &str) -> std::io::Result<u64> {
    let m = std::fs::metadata(path)?;
    if m.is_dir() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "a directory is not a File"));
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(create_file(g, path, &name, None, m.len(), m.modified().ok(), relative_path))
}

/// A `File` over existing data (the serializer's way back in).
///
/// # Safety
/// As [`create_file`].
pub(crate) unsafe fn from_data(g: *mut c_void, data: BlobData) -> u64 {
    let Some(c) = classes(g) else { return UNDEF };
    host_create(g, c.file_cls, c.file.proto, data)
}

/// Is `v` a File?
///
/// # Safety
/// As [`create_file`].
pub unsafe fn is_file(g: *mut c_void, v: u64) -> bool {
    file_data(g, v).is_some()
}

/// The path behind a File the HOST made from disk — what Electron's
/// `webUtils.getPathForFile` answers. `None` for a script-made File, even one
/// that shares a picked file's bytes, as in Electron.
///
/// # Safety
/// As [`create_file`].
pub unsafe fn path_of(g: *mut c_void, v: u64) -> Option<PathBuf> {
    let (b, m) = file_data(g, v)?;
    match &b.source {
        Source::Disk(f) if m.backed => Some(f.path.clone()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// FileList
// ---------------------------------------------------------------------------

unsafe fn list_data<'a>(g: *mut c_void, c: &FileClasses, v: u64) -> Option<&'a mut FileListData> {
    host_data::<FileListData>(g, c.list_cls, v)
}

/// A FileList of `files` (each must be a File; anything else is skipped).
///
/// # Safety
/// As [`create_file`]; the values must be live in `g`.
pub unsafe fn create_file_list(g: *mut c_void, files: &[u64]) -> u64 {
    let Some(c) = classes(g) else { return UNDEF };
    let files: Vec<u64> = files.iter().copied().filter(|&f| is_file(g, f)).collect();
    let list = host_create(g, c.list_cls, c.list.proto, FileListData { len: files.len() as u32 });
    let arr = new_array(g);
    for (i, &f) in files.iter().enumerate() {
        set_i(g, arr, i as u32, f);
        let desc = new_object(g, NULL);
        define(g, desc, "value", f, 0);
        define(g, desc, "enumerable", TRUE, 0);
        let _ = call(g, c.define_property, UNDEF, &[list, enc_i32(i as i32), desc]);
    }
    glue::rjs_define(g, list, c.s_items, arr, 1 | 2 | 4);
    list
}

/// Is `v` a FileList?
///
/// # Safety
/// As [`create_file`].
pub unsafe fn is_file_list(g: *mut c_void, v: u64) -> bool {
    classes(g).is_some_and(|c| list_data(g, c, v).is_some())
}

/// A FileList's Files, in order. The values stay reachable through the list
/// itself for as long as the caller holds it.
///
/// # Safety
/// As [`create_file`].
pub unsafe fn file_list_items(g: *mut c_void, v: u64) -> Option<Vec<u64>> {
    let c = classes(g)?;
    let n = list_data(g, c, v)?.len;
    let arr = get_a(g, v, c.s_items);
    Some((0..n).map(|i| get_i(g, arr, i)).collect())
}

unsafe extern "sysv64" fn list_length(g: *mut c_void, cf: *mut c_void) -> u64 {
    match classes(g).and_then(|c| list_data(g, c, cf_this(cf))) {
        Some(d) => enc_f64(d.len as f64),
        None => throw_type_error(g, "FileList.prototype.length called on a non-FileList"),
    }
}

unsafe extern "sysv64" fn list_item(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(c) = classes(g) else { return UNDEF };
    let this = cf_this(cf);
    let Some(len) = list_data(g, c, this).map(|d| d.len) else {
        return throw_type_error(g, "FileList.prototype.item called on a non-FileList");
    };
    if cf_argc(cf) < 1 {
        return throw_type_error(g, "Failed to execute 'item' on 'FileList': 1 argument required, but only 0 present.");
    }
    // `unsigned long`: ToNumber, then modulo 2^32.
    let Ok(d) = to_number(g, cf_arg(cf, 0)) else { return UNDEF };
    let i = if d.is_finite() { d.trunc().rem_euclid(4_294_967_296.0) as u32 } else { 0 };
    if i >= len {
        return NULL;
    }
    get_i(g, get_a(g, this, c.s_items), i)
}

unsafe fn to_string_tag(g: *mut c_void, proto: u64, tag: &str) {
    let a = glue::rjs_atom_wellknown(g, 1);
    if !a.is_null() {
        glue::rjs_define(g, proto, a, js_string(g, tag), 1 | 2);
        glue::rjs_atom_free(g, a);
    }
}

/// After `blob::install` (File.prototype's prototype is Blob.prototype).
pub unsafe fn install(g: *mut c_void) {
    let st = state(g);
    let file = define_class(g, &ClassSpec {
        name: "File",
        arity: 2,
        construct: Some(file_construct),
        parent_proto: st.blob.proto,
        methods: &[],
        getters: &[("name", get_name), ("lastModified", get_last_modified), ("webkitRelativePath", get_relative_path)],
        accessors: &[],
    });
    // `Object.getPrototypeOf(File) === Blob`, as for every derived interface.
    crate::capi::JSObjectSetPrototype(g, file.ctor as usize as crate::capi::JSObjectRef, st.blob.ctor as usize as crate::capi::JSValueRef);
    to_string_tag(g, file.proto, "File");
    // A File instance is of its own host class, whose parent is the Blob
    // class: `JSValueIsObjectOfClass` walks the parent chain, so every Blob
    // brand check accepts a File, while JSC names the object after its own
    // class — with the Blob class itself, `Object.prototype.toString` said
    // "[object Blob]" for a File whatever its @@toStringTag. NO finalizer:
    // JSC runs every finalizer up the chain, and the Blob class's already
    // drops the `BlobData`; a second would free it twice.
    let file_cls = {
        let name = std::ffi::CString::new("File").unwrap();
        let mut def = crate::capi::JSClassDefinition::empty();
        def.class_name = name.as_ptr();
        def.parent_class = st.blob_class;
        crate::capi::JSClassCreate(&def)
    };

    let list_cls = make_host_class::<FileListData>("FileList");
    let list = define_class(g, &ClassSpec {
        name: "FileList",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[("item", 1, list_item)],
        getters: &[("length", list_length)],
        accessors: &[],
    });
    to_string_tag(g, list.proto, "FileList");
    // WebIDL: an interface with an indexed getter and `length` iterates as
    // an array does — its @@iterator IS %Array.prototype.values%.
    let global = glue::rjs_global(g);
    let values = get(g, get(g, get(g, global, "Array"), "prototype"), "values");
    let it = glue::rjs_atom_iterator(g);
    glue::rjs_define(g, list.proto, it, values, 2);
    glue::rjs_atom_free(g, it);

    let define_property = get(g, get(g, global, "Object"), "defineProperty");
    crate::ffi::redjs_protect(g, define_property);
    let s_items = atom_priv(g, "redjs#filelist");
    let boxed: &'static FileClasses = Box::leak(Box::new(FileClasses { file, file_cls, list, list_cls, s_items, define_property }));
    registry().lock().unwrap().insert(g as usize, boxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_long_wraps_like_webidl() {
        assert_eq!(to_long_long(f64::NAN), 0.0);
        assert_eq!(to_long_long(f64::INFINITY), 0.0);
        assert_eq!(to_long_long(-1.9), -1.0);
        assert_eq!(to_long_long(1.5e3), 1500.0);
        assert_eq!(to_long_long(18_446_744_073_709_551_616.0 + 4096.0), 4096.0);
    }

    #[test]
    fn epoch_ms_floors_on_both_sides_of_the_epoch() {
        use std::time::Duration;
        assert_eq!(epoch_ms(UNIX_EPOCH + Duration::from_micros(1_500)), 1.0);
        assert_eq!(epoch_ms(UNIX_EPOCH - Duration::from_micros(1_500)), -2.0);
        assert_eq!(epoch_ms(UNIX_EPOCH - Duration::from_millis(2)), -2.0);
    }
}

/// Shared by the File-layer tests here and in `blob` / `url` / `uievents`:
/// a context with the scheduler (path-backed reads settle from the pump), an
/// async runner that pumps, and temp files that clean up after themselves.
#[cfg(test)]
pub(crate) mod testing {
    use crate::api::Context;
    use crate::webapi::testing::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    pub fn file_ctx() -> Context {
        let c = ctx();
        unsafe { crate::web::install(c.raw()) };
        c
    }

    /// Runs an async body to completion, pumping the scheduler and the
    /// microtask queue. `ERROR: name: message` when it rejects.
    pub fn run(c: &Context, body: &str) -> String {
        s(
            c,
            &format!(
                "globalThis.__out = '<pending>'; (async () => {{ {body} }})().then(\
                   v => {{ globalThis.__out = String(v); }}, \
                   e => {{ globalThis.__out = 'ERROR: ' + ((e && e.name ? e.name + ': ' : '') + ((e && e.message) || e)); }}); 'started'"
            ),
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            unsafe {
                crate::web::pump(c.raw(), crate::web::PUMP_TIMERS | crate::web::PUMP_MICROTASKS, core::ptr::null_mut());
            }
            c.drain_microtasks();
            let out = s(c, "globalThis.__out");
            if out != "<pending>" {
                return out;
            }
            if Instant::now() > deadline {
                return "<timed out>".into();
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A file under the OS temp dir, removed on drop.
    pub struct TempFile(pub PathBuf);

    impl TempFile {
        pub fn new(name: &str, bytes: &[u8]) -> TempFile {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!("redjs-file-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join(name);
            std::fs::write(&p, bytes).unwrap();
            TempFile(p)
        }

        /// Defines `globalThis[global]` as a path-backed File over this file.
        pub fn expose(&self, c: &Context, global: &str) {
            unsafe {
                let g = c.raw();
                let f = super::file_from_path(g, &self.0, "").unwrap();
                crate::webapi::util::define(g, crate::glue::rjs_global(g), global, f, 0);
            }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            if let Some(d) = self.0.parent() {
                let _ = std::fs::remove_dir(d);
            }
        }
    }
}

#[cfg(test)]
mod jsc_tests {
    use super::testing::*;
    use crate::webapi::testing::*;

    #[test]
    fn the_constructor_follows_the_idl() {
        let c = ctx();
        ok(&c, "File.length === 2 && Object.getPrototypeOf(File) === Blob && Object.getPrototypeOf(File.prototype) === Blob.prototype");
        assert_eq!(
            s(&c, "[Object.getPrototypeOf(new File([], 'x')) === File.prototype, String(File.prototype[Symbol.toStringTag]), \
                    Object.getOwnPropertySymbols(File.prototype).length, Object.prototype.toString.call(new File([], 'x'))].join()"),
            "true,File,1,[object File]"
        );
        ok(&c, "(() => { const f = new File(['ab', new Uint8Array([99])], 'a.TXT', { type: 'Text/Plain', lastModified: 1234.9 }); \
                 return f instanceof File && f instanceof Blob && f.name === 'a.TXT' && f.type === 'text/plain' && f.size === 3 \
                   && f.lastModified === 1234 && f.webkitRelativePath === ''; })()");
        ok(&c, "(() => { const t = Date.now(); const f = new File([], 'n'); return f.lastModified >= t && f.lastModified <= Date.now(); })()");
        ok(&c, "new File([], 'x', { lastModified: NaN }).lastModified === 0");
        ok(&c, "new File([], 'x', { lastModified: -5 }).lastModified === -5");
        assert_eq!(throws(&c, "new File([])"), "Failed to construct 'File': 2 arguments required, but only 1 present.");
        assert!(throws(&c, "new File([], 'x', 'nope')").contains("FilePropertyBag"));
        assert!(throws(&c, "File([], 'x')").contains("new"));
        // A getter brand-checks: a Blob is not a File.
        assert!(throws(&c, "Object.getOwnPropertyDescriptor(File.prototype, 'name').get.call(new Blob([]))").contains("non-File"));
        // A subclass keeps its prototype and is still a File.
        ok(&c, "(() => { class F extends File {} const f = new F(['x'], 'n'); return f instanceof F && f.name === 'n' && f.size === 1; })()");
    }

    #[test]
    fn one_blob_part_shares_and_a_slice_of_a_file_is_a_blob() {
        let c = file_ctx();
        let t = TempFile::new("shared.bin", b"0123456789");
        t.expose(&c, "picked");
        // `new File([picked], …)` is still path-backed: deleting the file now
        // makes its read fail, which a copy would not.
        ok(&c, "(() => { globalThis.renamed = new File([picked], 'renamed.bin', { type: 'a/b' }); \
                 globalThis.part = picked.slice(2, 5, 'X/Y'); return renamed.size === 10 && part.size === 3; })()");
        ok(&c, "!(part instanceof File) && part instanceof Blob && part.type === 'x/y'");
        assert_eq!(run(&c, "return await part.text();"), "234");
        assert_eq!(run(&c, "return await renamed.text();"), "0123456789");
        assert_eq!(run(&c, "return [picked.name, picked.size, picked.type, typeof picked.lastModified].join();"), "shared.bin,10,application/octet-stream,number");
        drop(t);
        assert!(run(&c, "return await renamed.text();").starts_with("ERROR: NotFoundError"));
    }

    #[test]
    fn a_path_backed_file_reads_off_the_js_thread_and_honours_the_snapshot() {
        let c = file_ctx();
        let t = TempFile::new("snap.txt", "\u{FEFF}héllo".as_bytes());
        t.expose(&c, "f");
        ok(&c, "f.type === 'text/plain' && f.size === 9");
        // The read is asynchronous: nothing has settled before the pump runs.
        ok(&c, "(() => { globalThis.early = 'none'; f.text().then(v => { early = v; }); return early === 'none'; })()");
        assert_eq!(run(&c, "return await f.text();"), "héllo", "UTF-8 decode drops the BOM");
        assert_eq!(run(&c, "const b = await f.bytes(); return b instanceof Uint8Array && b.length === 9 && b[0] === 0xEF;"), "true");
        assert_eq!(run(&c, "const b = await f.arrayBuffer(); return b instanceof ArrayBuffer && b.byteLength === 9;"), "true");
        // Change it after the File was made: Chromium's snapshot rule.
        std::fs::write(&t.0, b"changed!!!").unwrap();
        assert_eq!(
            run(&c, "return await f.text();"),
            "ERROR: NotReadableError: The requested file could not be read, typically due to permission problems that have occurred after a reference to a file was acquired."
        );
        ok(&c, "(() => { globalThis.rej = null; f.arrayBuffer().catch(e => { rej = e; }); return true; })()");
        assert_eq!(run(&c, "await new Promise(r => setTimeout(r, 5)); return rej instanceof DOMException && rej.code === 0;"), "true");
    }

    #[test]
    fn reads_beyond_the_cap_queue_rather_than_fail() {
        let c = file_ctx();
        let t = TempFile::new("many.bin", &[7u8; 4096]);
        t.expose(&c, "f");
        assert_eq!(
            run(&c, "const all = await Promise.all(Array.from({ length: 40 }, (_, i) => f.slice(i, i + 10).bytes())); \
                     return all.every(b => b.length === 10 && b[0] === 7);"),
            "true"
        );
        assert!(super::super::blob::peak_reads_in_flight() <= super::super::blob::MAX_READS_IN_FLIGHT);
    }

    #[test]
    fn stream_yields_64_kib_chunks_for_memory_and_disk() {
        let c = file_ctx();
        let body: Vec<u8> = (0..150_000u32).map(|i| (i % 251) as u8).collect();
        let t = TempFile::new("big.bin", &body);
        t.expose(&c, "f");
        let read_all = "async (b) => { const r = b.stream().getReader(); const sizes = []; let sum = 0, ok = true, at = 0; \
                          for (;;) { const { done, value } = await r.read(); if (done) break; sizes.push(value.length); \
                            for (let i = 0; i < value.length; i++) if (value[i] !== (at + i) % 251) ok = false; at += value.length; sum += value.length; } \
                          return sizes.join('+') + '=' + sum + (ok ? ' ok' : ' BAD'); }";
        assert_eq!(run(&c, &format!("return await ({read_all})(f);")), "65536+65536+18928=150000 ok");
        assert_eq!(
            run(&c, &format!("const m = new Uint8Array(150000); for (let i = 0; i < m.length; i++) m[i] = i % 251; return await ({read_all})(new Blob([m]));")),
            "65536+65536+18928=150000 ok"
        );
        assert_eq!(run(&c, "const r = new Blob([]).stream().getReader(); return (await r.read()).done;"), "true");
        // A page that replaces ReadableStream later does not change what stream() returns.
        assert_eq!(
            run(&c, "const RS = ReadableStream; globalThis.ReadableStream = function () { throw new Error('patched'); }; \
                     try { return new Blob(['x']).stream() instanceof RS; } finally { globalThis.ReadableStream = RS; }"),
            "true"
        );
    }

    #[test]
    fn file_list_is_a_read_only_indexed_list() {
        let c = ctx();
        unsafe {
            let g = c.raw();
            let a = c.eval("new File(['a'], 'a.txt')").unwrap().0;
            let b = c.eval("new File(['bb'], 'b.txt')").unwrap().0;
            let list = super::create_file_list(g, &[a, b]);
            crate::webapi::util::define(g, crate::glue::rjs_global(g), "list", list, 0);
            assert!(super::is_file_list(g, list));
            assert_eq!(super::file_list_items(g, list).unwrap().len(), 2);
        }
        ok(&c, "list instanceof FileList && list.length === 2 && list[0].name === 'a.txt' && list.item(1) === list[1] && list.item(2) === null && list[2] === undefined");
        ok(&c, "Object.prototype.toString.call(list) === '[object FileList]' && FileList.prototype[Symbol.iterator] === Array.prototype.values");
        ok(&c, "[...list].map(f => f.name).join() === 'a.txt,b.txt' && Object.keys(list).join() === '0,1'");
        ok(&c, "list.item(4294967296) === list[0] && list.item('1') === list[1]");
        ok(&c, "(() => { const d = Object.getOwnPropertyDescriptor(list, 0); return d.enumerable && !d.writable; })()");
        ok(&c, "(() => { list[0] = 1; delete list[1]; return list[0] instanceof File && list[1] instanceof File; })()");
        assert!(throws(&c, "'use strict'; list[0] = 1").contains("read"), "a write to an index fails");
        assert_eq!(throws(&c, "list.item()"), "Failed to execute 'item' on 'FileList': 1 argument required, but only 0 present.");
        assert!(throws(&c, "new FileList()").contains("Illegal constructor"));
    }

    #[test]
    fn files_and_file_lists_clone_and_post() {
        let c = file_ctx();
        let t = TempFile::new("clone.txt", b"by reference");
        t.expose(&c, "picked");
        unsafe {
            let g = c.raw();
            let a = c.eval("new File(['mem'], 'm.txt', { type: 'text/x', lastModified: 77 })").unwrap().0;
            let p = c.eval("picked").unwrap().0;
            let list = super::create_file_list(g, &[a, p]);
            crate::webapi::util::define(g, crate::glue::rjs_global(g), "list", list, 0);
        }
        ok(&c, "(() => { const r = structuredClone({ list, again: list[0] }); \
                 return r.list instanceof FileList && r.list !== list && r.list.length === 2 && r.again === r.list[0] \
                   && r.list[0] instanceof File && r.list[0].name === 'm.txt' && r.list[0].type === 'text/x' && r.list[0].lastModified === 77 \
                   && r.list[1].name === 'clone.txt'; })()");
        // By reference: the clone of a path-backed File reads the file (no copy was made).
        assert_eq!(run(&c, "return await structuredClone(picked).text();"), "by reference");
        assert_eq!(
            run(&c, "const rx = new BroadcastChannel('files'), tx = new BroadcastChannel('files'); \
                     const got = new Promise(r => { rx.onmessage = e => r(e.data); }); tx.postMessage({ f: picked }); \
                     const d = await got; rx.close(); tx.close(); \
                     return [d.f instanceof File, d.f !== picked, d.f.name, await d.f.text()].join();"),
            "true,true,clone.txt,by reference"
        );
    }

    #[test]
    fn storage_inlines_files_and_reads_a_path_backed_one_now() {
        use crate::webapi::serialize::{deserialize_storage, serialize, tag, Options};
        let c = file_ctx();
        let t = TempFile::new("stored.txt", b"on disk");
        t.expose(&c, "picked");
        unsafe {
            let g = c.raw();
            let ser = |src: &str| {
                let v = c.eval(src).unwrap().0;
                serialize(g, v, &Options { for_storage: true, transfer: &[] }).map_err(|e| c.error_info(crate::api::Value(e)).0)
            };
            // The wire format of 0x31, pinned.
            let s = ser("new File(['ab'], 'n', { type: 't/x', lastModified: 5 })").unwrap();
            let mut want = vec![b'R', b'J', b'S', 1, 1, tag::FILE, 0x08, 3, b't', b'/', b'x', 2, b'a', b'b', 0x08, 1, b'n'];
            want.extend(5f64.to_le_bytes());
            want.extend([0x08, 0]);
            assert_eq!(s.bytes, want);
            // A path-backed File is read at serialization time and stored as bytes.
            let s = ser("({ f: picked, l: [picked] })").unwrap();
            assert!(s.attachments.is_empty());
            std::fs::write(&t.0, b"now different").unwrap();
            let v = deserialize_storage(g, &s.bytes).unwrap();
            crate::webapi::util::define(g, crate::glue::rjs_global(g), "fromDisk", v, 0);
            ok(&c, "fromDisk.f instanceof File && fromDisk.f.name === 'stored.txt' && fromDisk.l[0] === fromDisk.f");
            assert_eq!(run(&c, "return await fromDisk.f.text();"), "on disk", "the stored copy is independent of the file");
            // The file changed since `picked` was made: storing it now fails.
            let e = ser("picked").unwrap_err();
            assert!(e.contains("could not be read"), "{e}");
            // A FileList whose item is not a File, or a non-integral lastModified, is corrupt.
            for bad in [
                vec![b'R', b'J', b'S', 1, 1, tag::FILE_LIST, 1, 0x05, 0],
                {
                    let mut b = vec![b'R', b'J', b'S', 1, 1, tag::FILE, 0x08, 0, 0, 0x08, 0];
                    b.extend(0.5f64.to_le_bytes());
                    b.extend([0x08, 0]);
                    b
                },
            ] {
                assert!(deserialize_storage(g, &bad).is_err(), "{bad:?}");
            }
        }
    }

    #[test]
    fn a_large_synchronous_read_says_so_once() {
        let c = file_ctx();
        let t = TempFile::new("large.bin", &vec![1u8; (17 << 20) as usize]);
        t.expose(&c, "big");
        ok(&c, "new Response(big) instanceof Response && new Response(big) instanceof Response");
        let said: Vec<String> =
            super::super::blob::advisories().into_iter().filter(|m| m.contains("A-F2") && m.contains("fetch/Request/Response body")).collect();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("17 MiB") && said[0].contains("stream()"), "{said:?}");
    }

    #[test]
    fn path_of_answers_for_host_made_files_only() {
        let c = ctx();
        let t = TempFile::new("p.txt", b"x");
        t.expose(&c, "picked");
        unsafe {
            let g = c.raw();
            let picked = c.eval("picked").unwrap().0;
            assert_eq!(super::path_of(g, picked).as_deref(), Some(t.0.as_path()));
            let made = c.eval("new File([picked], 'copy.txt')").unwrap().0;
            assert_eq!(super::path_of(g, made), None, "a script-made File has no path, as in Electron");
            let blob = c.eval("new Blob(['x'])").unwrap().0;
            assert!(!super::is_file(g, blob) && super::path_of(g, blob).is_none());
        }
    }
}
