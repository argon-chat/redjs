//! Native `FormData` (XMLHttpRequest Standard §5) — the ordered entry list, its
//! multi-map semantics, and the `multipart/form-data` encoding algorithm, all
//! in Rust.
//!
//! ## Where the values live
//!
//! Text entries are Rust `String`s. A `Blob` value is NOT copied into Rust:
//! the object itself is kept in a JS array behind a private-name slot on the
//! `FormData` instance, and the Rust entry holds an index into it. That is the
//! same shape `EventTarget`'s `[[listeners]]` uses, and it buys two things a
//! byte-copy would lose — `fd.get(k)` returns the *identical* object the caller
//! appended (which the spec requires), and the blob is traced by the GC instead
//! of needing a protect with no safe release point in a finaliser.
//!
//! ## Deliberate narrowings
//!
//! - `new FormData(form)` builds the entry list from an **HTMLFormElement**
//!   (spec §5, "constructing a FormData object"). This runtime has no real form
//!   elements — `document.createElement` produces an inert object with no
//!   controls — so a form argument is REFUSED by name rather than silently
//!   producing an empty FormData, which would look like a form that submitted
//!   nothing.
//!
//! "Create an entry" is the spec's: a bare `Blob` becomes a new `File` named
//! `"blob"`, a filename makes a new `File` of that name over the same bytes,
//! and only a `File` appended WITHOUT a filename comes back as the identical
//! object. A path-backed File is read when the body is encoded (A-F2).

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::blob;
use super::util::*;
use crate::capi;
use crate::glue;

// ---------------------------------------------------------------------------
// The entry list (no JS)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum EntryValue {
    Text(String),
    /// `slot` indexes the instance's JS-side value array.
    File { slot: u32, filename: String, mime: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub value: EntryValue,
}

#[derive(Default)]
pub struct FormDataData {
    pub list: Vec<Entry>,
    /// Next free index in the JS-side value array. Never reused: `delete` drops
    /// the entry but leaves the slot, because reusing an index would silently
    /// re-point a still-live iterator at somebody else's blob.
    pub next_slot: u32,
}

impl FormDataData {
    pub fn append(&mut self, name: String, value: EntryValue) {
        self.list.push(Entry { name, value });
    }

    pub fn get(&self, name: &str) -> Option<&EntryValue> {
        self.list.iter().find(|e| e.name == name).map(|e| &e.value)
    }

    pub fn get_all(&self, name: &str) -> Vec<&EntryValue> {
        self.list.iter().filter(|e| e.name == name).map(|e| &e.value).collect()
    }

    pub fn has(&self, name: &str) -> bool {
        self.list.iter().any(|e| e.name == name)
    }

    pub fn delete(&mut self, name: &str) {
        self.list.retain(|e| e.name != name);
    }

    /// Spec §"set": replace the FIRST entry with this name and drop the rest,
    /// keeping the first one's POSITION. Appending after a delete instead would
    /// reorder the form, and multipart order is observable to the server.
    pub fn set(&mut self, name: String, value: EntryValue) {
        match self.list.iter().position(|e| e.name == name) {
            Some(i) => {
                self.list[i].value = value;
                let n = name.clone();
                let mut seen = false;
                self.list.retain(|e| {
                    if e.name != n {
                        return true;
                    }
                    if seen {
                        return false;
                    }
                    seen = true;
                    true
                });
            }
            None => self.append(name, value),
        }
    }
}

/// Spec §"escape a multipart/form-data name": newlines are normalised to CRLF
/// first, then `"`, CR and LF become percent-escapes. Not URL-encoding — only
/// these three, and only after normalisation.
pub fn escape_multipart_name(s: &str) -> String {
    let normalised = normalize_newlines(s);
    let mut out = String::with_capacity(normalised.len());
    for ch in normalised.chars() {
        match ch {
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            '"' => out.push_str("%22"),
            c => out.push(c),
        }
    }
    out
}

/// Lone CR and lone LF become CRLF; existing CRLF is left alone.
fn normalize_newlines(s: &str) -> String {
    let b: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            '\r' => {
                out.push_str("\r\n");
                if i + 1 < b.len() && b[i + 1] == '\n' {
                    i += 1;
                }
            }
            '\n' => out.push_str("\r\n"),
            c => out.push(c),
        }
        i += 1;
    }
    out
}

/// A boundary that cannot occur inside base-16 payload text. 16 random hex
/// digits, the same shape browsers use.
pub fn make_boundary() -> String {
    let mut bytes = [0u8; 8];
    // A failed RNG must not silently produce a CONSTANT boundary — two requests
    // sharing one would be a body that the server splits wrongly. Fall back to
    // the clock, which is at least unique per call.
    if getrandom::fill(&mut bytes).is_err() {
        let t = crate::web::now_ms().to_bits();
        bytes.copy_from_slice(&t.to_le_bytes());
    }
    let mut s = String::from("----RedJsFormBoundary");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// The WHATWG "multipart/form-data encoding algorithm". `file_bytes` resolves a
/// file entry's slot to its bytes; it returns `None` for a slot whose blob is
/// gone, and that entry is then encoded with an EMPTY body rather than skipped,
/// so the field still exists on the server side.
pub fn encode_multipart(
    list: &[Entry],
    boundary: &str,
    mut file_bytes: impl FnMut(u32) -> Option<Vec<u8>>,
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for e in list {
        out.extend_from_slice(b"--");
        out.extend_from_slice(boundary.as_bytes());
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(b"Content-Disposition: form-data; name=\"");
        out.extend_from_slice(escape_multipart_name(&e.name).as_bytes());
        out.push(b'"');
        match &e.value {
            EntryValue::Text(v) => {
                out.extend_from_slice(b"\r\n\r\n");
                out.extend_from_slice(normalize_newlines(v).as_bytes());
            }
            EntryValue::File { slot, filename, mime } => {
                out.extend_from_slice(b"; filename=\"");
                out.extend_from_slice(escape_multipart_name(filename).as_bytes());
                out.extend_from_slice(b"\"\r\nContent-Type: ");
                let ct = if mime.is_empty() { "application/octet-stream" } else { mime };
                out.extend_from_slice(ct.as_bytes());
                out.extend_from_slice(b"\r\n\r\n");
                if let Some(bytes) = file_bytes(*slot) {
                    out.extend_from_slice(&bytes);
                }
            }
        }
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"--");
    out.extend_from_slice(boundary.as_bytes());
    out.extend_from_slice(b"--\r\n");
    out
}

// ---------------------------------------------------------------------------
// The JS class
// ---------------------------------------------------------------------------

struct FdClasses {
    cls: capi::JSClassRef,
    class: Class,
    iter_cls: capi::JSClassRef,
    iter_proto: u64,
    /// [[values]] → JS Array holding the appended Blob objects.
    s_values: *mut c_void,
    /// iterator [[target]] → the FormData it walks.
    s_target: *mut c_void,
}
unsafe impl Send for FdClasses {}
unsafe impl Sync for FdClasses {}

struct FdIter {
    idx: usize,
    kind: u8, // 0 entries, 1 keys, 2 values
}

fn registry() -> &'static Mutex<HashMap<usize, &'static FdClasses>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static FdClasses>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> &'static FdClasses {
    registry().lock().unwrap().get(&(g as usize)).copied().expect("webapi::formdata not installed")
}

pub(crate) fn forget(gone: crate::context::Gone) {
    registry().lock().unwrap().remove(&gone.key);
}

unsafe fn this_fd<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut FormDataData> {
    host_data::<FormDataData>(g, classes(g).cls, cf_this(cf))
}

/// The instance's JS-side blob array, created on first use.
unsafe fn values_of(g: *mut c_void, this: u64, create: bool) -> u64 {
    let c = classes(g);
    let v = get_a(g, this, c.s_values);
    if is_cell(v) {
        return v;
    }
    if !create {
        return UNDEF;
    }
    let arr = new_array(g);
    glue::rjs_define(g, this, c.s_values, arr, 0);
    arr
}

/// Spec §"create an entry": a value that is not a `File` becomes a new File
/// over the same bytes named `"blob"`; with a filename, the value becomes a
/// new File of that name over the same bytes (a File keeps its type,
/// `lastModified` and path backing — Chromium's `File::Clone`); a File with
/// no filename is stored as itself. Anything else is stringified.
unsafe fn make_entry_value(
    g: *mut c_void,
    this: u64,
    value: u64,
    filename_arg: u64,
) -> Result<EntryValue, String> {
    if let Some(b) = blob::data_of(g, value) {
        let named = (filename_arg != UNDEF).then(|| read_string(g, filename_arg).unwrap_or_default());
        let value = match (&b.file, named) {
            (Some(_), None) => value,
            (meta, named) => {
                let file = match meta {
                    Some(m) => blob::FileMeta { name: named.unwrap_or_default(), ..m.clone() },
                    None => blob::FileMeta {
                        name: named.unwrap_or_else(|| "blob".to_string()),
                        last_modified: super::file::now_ms(),
                        relative_path: String::new(),
                        backed: false,
                    },
                };
                let data = blob::BlobData { source: b.source.clone(), offset: b.offset, len: b.len, mime: b.mime.clone(), file: Some(file) };
                super::file::from_data(g, data)
            }
        };
        let Some(b) = blob::data_of(g, value) else {
            return Err("FormData: the File could not be created".into());
        };
        let (mime, filename) = (b.mime.clone(), b.file.as_ref().map(|m| m.name.clone()).unwrap_or_default());
        let arr = values_of(g, this, true);
        let Some(d) = host_data::<FormDataData>(g, classes(g).cls, this) else {
            return Err("FormData: lost its entry list".into());
        };
        let slot = d.next_slot;
        d.next_slot += 1;
        set_i(g, arr, slot, value);
        return Ok(EntryValue::File { slot, filename, mime });
    }
    if filename_arg != UNDEF {
        // Spec: the 3-argument form is only defined for Blob values.
        return Err(
            "FormData.append/set: a filename is only allowed when the value is a Blob".into()
        );
    }
    Ok(EntryValue::Text(read_string(g, value).unwrap_or_default()))
}

/// An entry value back as a JS value.
unsafe fn entry_to_js(g: *mut c_void, this: u64, v: &EntryValue) -> u64 {
    match v {
        EntryValue::Text(s) => js_string(g, s),
        EntryValue::File { slot, .. } => {
            let arr = values_of(g, this, false);
            if is_cell(arr) {
                get_i(g, arr, *slot)
            } else {
                NULL
            }
        }
    }
}

#[crate::host_fn]
unsafe fn construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let form = cf_arg(cf, 0);
    if form != UNDEF && form != NULL {
        return throw_type_error(
            g,
            "new FormData(form) builds the entry list from an HTMLFormElement, and this runtime \
             has no form elements — document.createElement returns an inert object with no \
             controls. Construct an empty FormData and append() the fields instead.",
        );
    }
    let proto = {
        let nt = cf_new_target(cf);
        let p = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
        if is_cell(p) { p } else { c.class.proto }
    };
    host_create(g, c.cls, proto, FormDataData::default())
}

unsafe fn append_or_set(g: *mut c_void, cf: *mut c_void, replace: bool) -> u64 {
    let this = cf_this(cf);
    let label = if replace { "set" } else { "append" };
    if this_fd(g, cf).is_none() {
        return throw_type_error(g, &format!("FormData.prototype.{label} called on a non-FormData"));
    }
    let name = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let value = match make_entry_value(g, this, cf_arg(cf, 1), cf_arg(cf, 2)) {
        Ok(v) => v,
        Err(e) => return throw_type_error(g, &e),
    };
    let Some(d) = this_fd(g, cf) else {
        return throw_type_error(g, &format!("FormData.prototype.{label} called on a non-FormData"));
    };
    if replace {
        d.set(name, value);
    } else {
        d.append(name, value);
    }
    UNDEF
}

#[crate::host_fn]
unsafe fn fd_append(g: *mut c_void, cf: *mut c_void) -> u64 {
    append_or_set(g, cf, false)
}

#[crate::host_fn]
unsafe fn fd_set(g: *mut c_void, cf: *mut c_void) -> u64 {
    append_or_set(g, cf, true)
}

#[crate::host_fn]
unsafe fn fd_get(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let name = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let Some(d) = this_fd(g, cf) else {
        return throw_type_error(g, "FormData.prototype.get called on a non-FormData");
    };
    match d.get(&name).cloned() {
        Some(v) => entry_to_js(g, this, &v),
        None => NULL,
    }
}

#[crate::host_fn]
unsafe fn fd_get_all(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let name = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let Some(d) = this_fd(g, cf) else {
        return throw_type_error(g, "FormData.prototype.getAll called on a non-FormData");
    };
    let vals: Vec<EntryValue> = d.get_all(&name).into_iter().cloned().collect();
    let arr = new_array(g);
    for (i, v) in vals.iter().enumerate() {
        let jv = entry_to_js(g, this, v);
        set_i(g, arr, i as u32, jv);
    }
    arr
}

#[crate::host_fn]
unsafe fn fd_has(g: *mut c_void, cf: *mut c_void) -> u64 {
    let name = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    match this_fd(g, cf) {
        Some(d) => enc_bool(d.has(&name)),
        None => throw_type_error(g, "FormData.prototype.has called on a non-FormData"),
    }
}

#[crate::host_fn]
unsafe fn fd_delete(g: *mut c_void, cf: *mut c_void) -> u64 {
    let name = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    match this_fd(g, cf) {
        Some(d) => {
            d.delete(&name);
            UNDEF
        }
        None => throw_type_error(g, "FormData.prototype.delete called on a non-FormData"),
    }
}

#[crate::host_fn]
unsafe fn fd_for_each(g: *mut c_void, cf: *mut c_void) -> u64 {
    let this = cf_this(cf);
    let cb = cf_arg(cf, 0);
    let this_arg = cf_arg(cf, 1);
    let Some(d) = this_fd(g, cf) else {
        return throw_type_error(g, "FormData.prototype.forEach called on a non-FormData");
    };
    let snapshot = d.list.clone();
    for e in snapshot {
        let v = entry_to_js(g, this, &e.value);
        let k = js_string(g, &e.name);
        if call(g, cb, this_arg, &[v, k, this]).is_err() {
            break;
        }
    }
    UNDEF
}

// ---- iteration -------------------------------------------------------------

unsafe fn make_iter(g: *mut c_void, cf: *mut c_void, kind: u8) -> u64 {
    let c = classes(g);
    let this = cf_this(cf);
    if this_fd(g, cf).is_none() {
        return throw_type_error(g, "FormData iterator called on a non-FormData");
    }
    let it = host_create(g, c.iter_cls, c.iter_proto, FdIter { idx: 0, kind });
    glue::rjs_define(g, it, c.s_target, this, 0);
    it
}

#[crate::host_fn]
unsafe fn fd_entries(g: *mut c_void, cf: *mut c_void) -> u64 {
    make_iter(g, cf, 0)
}
#[crate::host_fn]
unsafe fn fd_keys(g: *mut c_void, cf: *mut c_void) -> u64 {
    make_iter(g, cf, 1)
}
#[crate::host_fn]
unsafe fn fd_values(g: *mut c_void, cf: *mut c_void) -> u64 {
    make_iter(g, cf, 2)
}

#[crate::host_fn]
unsafe fn iter_self(_g: *mut c_void, cf: *mut c_void) -> u64 {
    cf_this(cf)
}

#[crate::host_fn]
unsafe fn iter_next(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let this_v = cf_this(cf);
    let Some(it) = host_data::<FdIter>(g, c.iter_cls, this_v) else {
        return throw_type_error(g, "next called on a non-FormData-iterator");
    };
    let target = get_a(g, this_v, c.s_target);
    let result = new_object(g, UNDEF);
    // Walks the LIVE list by index, so mutations during iteration are observed
    // — the same behaviour URLSearchParams has here and browsers have there.
    let entry = host_data::<FormDataData>(g, c.cls, target).and_then(|d| {
        let e = d.list.get(it.idx).cloned();
        if e.is_some() {
            it.idx += 1;
        }
        e
    });
    match entry {
        Some(e) => {
            let value = match it.kind {
                1 => js_string(g, &e.name),
                2 => entry_to_js(g, target, &e.value),
                _ => {
                    let pair = new_array(g);
                    set_i(g, pair, 0, js_string(g, &e.name));
                    let v = entry_to_js(g, target, &e.value);
                    set_i(g, pair, 1, v);
                    pair
                }
            };
            define(g, result, "value", value, 0);
            define(g, result, "done", FALSE, 0);
        }
        None => {
            define(g, result, "value", UNDEF, 0);
            define(g, result, "done", TRUE, 0);
        }
    }
    result
}

// ---- multipart -------------------------------------------------------------

/// The encoded body of a `FormData` value, plus the `Content-Type` it needs.
/// `None` when `v` is not a FormData. Used by `Response`'s BodyInit handling.
/// `Some(Err)` when a path-backed File in it can no longer be read — a
/// browser fails that request too, rather than sending a part it could not
/// read.
pub unsafe fn encode_body(g: *mut c_void, v: u64) -> Option<Result<(Vec<u8>, String), String>> {
    let c = classes(g);
    let list = host_data::<FormDataData>(g, c.cls, v)?.list.clone();
    let arr = values_of(g, v, false);
    let boundary = make_boundary();
    let mut failed: Option<blob::ReadError> = None;
    let bytes = encode_multipart(&list, &boundary, |slot| {
        if !is_cell(arr) || failed.is_some() {
            return None;
        }
        match blob::bytes_of(g, get_i(g, arr, slot), "FormData (a multipart body)")? {
            Ok(b) => Some(b),
            Err(e) => {
                failed = Some(e);
                None
            }
        }
    });
    if let Some(e) = failed {
        return Some(Err(blob::sync_read_failed("FormData (a multipart body)", e)));
    }
    Some(Ok((bytes, format!("multipart/form-data; boundary={boundary}"))))
}

pub unsafe fn install(g: *mut c_void) {
    let cls = make_host_class::<FormDataData>("FormData");
    let iter_cls = make_host_class::<FdIter>("FormData Iterator");
    let s_values = atom_priv(g, "redjs#formDataValues");
    let s_target = atom_priv(g, "redjs#formDataIterTarget");

    let iter_proto = new_object(g, UNDEF);
    crate::ffi::redjs_protect(g, iter_proto);
    define(g, iter_proto, "next", make_fn(g, "next", 0, iter_next), 2);
    let it_atom = glue::rjs_atom_iterator(g);
    glue::rjs_define(g, iter_proto, it_atom, make_fn(g, "[Symbol.iterator]", 0, iter_self), 2);

    let class = define_class(g, &ClassSpec {
        name: "FormData",
        arity: 0,
        construct: Some(construct),
        parent_proto: 0,
        methods: &[
            ("append", 3, fd_append),
            ("set", 3, fd_set),
            ("get", 1, fd_get),
            ("getAll", 1, fd_get_all),
            ("has", 1, fd_has),
            ("delete", 1, fd_delete),
            ("forEach", 2, fd_for_each),
            ("entries", 0, fd_entries),
            ("keys", 0, fd_keys),
            ("values", 0, fd_values),
        ],
        getters: &[],
        accessors: &[],
    });
    // Spec: FormData's [Symbol.iterator] IS `entries`.
    glue::rjs_define(g, class.proto, it_atom, get(g, class.proto, "entries"), 2);
    glue::rjs_atom_free(g, it_atom);

    let boxed: &'static FdClasses =
        Box::leak(Box::new(FdClasses { cls, class, iter_cls, iter_proto, s_values, s_target }));
    registry().lock().unwrap().insert(g as usize, boxed);
}

// ---------------------------------------------------------------------------
// Tests — the entry list and the encoder, no JS context needed
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> EntryValue {
        EntryValue::Text(s.into())
    }

    #[test]
    fn append_keeps_duplicates_in_order() {
        let mut fd = FormDataData::default();
        fd.append("a".into(), text("1"));
        fd.append("a".into(), text("2"));
        assert_eq!(fd.get(&"a".to_string()), Some(&text("1")));
        assert_eq!(fd.get_all("a"), vec![&text("1"), &text("2")]);
    }

    #[test]
    fn set_replaces_in_place_rather_than_moving_to_the_end() {
        // Multipart ORDER is observable to the server, so "delete then append"
        // is a different form from "set" even though both end with one entry.
        let mut fd = FormDataData::default();
        fd.append("a".into(), text("1"));
        fd.append("b".into(), text("2"));
        fd.append("a".into(), text("3"));
        fd.set("a".into(), text("9"));
        let names: Vec<&str> = fd.list.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(fd.get("a"), Some(&text("9")));
    }

    #[test]
    fn set_on_an_absent_name_appends() {
        let mut fd = FormDataData::default();
        fd.append("a".into(), text("1"));
        fd.set("z".into(), text("9"));
        assert_eq!(fd.list.last().unwrap().name, "z");
    }

    #[test]
    fn delete_removes_every_entry_with_that_name() {
        let mut fd = FormDataData::default();
        fd.append("a".into(), text("1"));
        fd.append("a".into(), text("2"));
        fd.append("b".into(), text("3"));
        fd.delete("a");
        assert!(!fd.has("a"));
        assert!(fd.has("b"));
    }

    #[test]
    fn names_are_escaped_not_url_encoded() {
        // Only ", CR and LF are escaped. Percent-encoding the whole name would
        // corrupt every field with a bracket in it, which is how array-style
        // form fields are written.
        assert_eq!(escape_multipart_name("items[0]"), "items[0]");
        assert_eq!(escape_multipart_name("a\"b"), "a%22b");
        assert_eq!(escape_multipart_name("a\nb"), "a%0D%0Ab");
        assert_eq!(escape_multipart_name("a\r\nb"), "a%0D%0Ab");
    }

    #[test]
    fn a_quote_in_a_name_cannot_break_out_of_the_header() {
        // The reason the escape exists: an unescaped quote ends the `name="…"`
        // parameter and lets the field inject its own headers.
        let list = vec![Entry { name: "x\"; filename=\"evil".into(), value: text("v") }];
        let out = encode_multipart(&list, "B", |_| None);
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains("filename=\"evil\""), "{s}");
        assert!(s.contains("name=\"x%22; filename=%22evil\""), "{s}");
    }

    #[test]
    fn the_encoding_has_the_specified_frame() {
        let list = vec![Entry { name: "a".into(), value: text("1") }];
        let s = String::from_utf8(encode_multipart(&list, "BOUND", |_| None)).unwrap();
        assert_eq!(s, "--BOUND\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n1\r\n--BOUND--\r\n");
    }

    #[test]
    fn a_file_entry_carries_a_filename_and_a_content_type() {
        let list = vec![Entry {
            name: "f".into(),
            value: EntryValue::File { slot: 0, filename: "a.txt".into(), mime: "text/plain".into() },
        }];
        let s = String::from_utf8(encode_multipart(&list, "B", |_| Some(b"hi".to_vec()))).unwrap();
        assert!(s.contains("name=\"f\"; filename=\"a.txt\""), "{s}");
        assert!(s.contains("Content-Type: text/plain"), "{s}");
        assert!(s.ends_with("\r\nhi\r\n--B--\r\n"), "{s}");
    }

    #[test]
    fn a_file_with_no_type_defaults_to_octet_stream() {
        let list = vec![Entry {
            name: "f".into(),
            value: EntryValue::File { slot: 0, filename: "x".into(), mime: String::new() },
        }];
        let s = String::from_utf8(encode_multipart(&list, "B", |_| Some(vec![]))).unwrap();
        assert!(s.contains("Content-Type: application/octet-stream"), "{s}");
    }

    #[test]
    fn a_missing_blob_still_emits_its_field() {
        // Dropping the part entirely would make the server see a form with a
        // field missing, which is indistinguishable from the user not filling
        // it in. An empty body says "there was a file and it was empty".
        let list = vec![Entry {
            name: "f".into(),
            value: EntryValue::File { slot: 7, filename: "x".into(), mime: String::new() },
        }];
        let s = String::from_utf8(encode_multipart(&list, "B", |_| None)).unwrap();
        assert!(s.contains("name=\"f\""), "{s}");
    }

    #[test]
    fn text_values_are_newline_normalised_but_not_escaped() {
        let list = vec![Entry { name: "a".into(), value: text("one\ntwo") }];
        let s = String::from_utf8(encode_multipart(&list, "B", |_| None)).unwrap();
        assert!(s.contains("\r\n\r\none\r\ntwo\r\n"), "{s:?}");
    }

    #[test]
    fn boundaries_differ_between_calls() {
        // A constant boundary is a body the server splits at the wrong places
        // as soon as two requests are in flight.
        assert_ne!(make_boundary(), make_boundary());
    }
}

#[cfg(test)]
mod jsc_tests {
    use crate::webapi::testing::*;

    #[test]
    fn the_multi_map_surface_works_from_js() {
        let c = ctx();
        ok(&c, "(() => { const f = new FormData(); f.append('a','1'); f.append('a','2'); \
                 return f.get('a') === '1' && f.getAll('a').join() === '1,2' && f.has('a'); })()");
        ok(&c, "(() => { const f = new FormData(); f.append('a','1'); f.delete('a'); return f.get('a') === null; })()");
    }

    #[test]
    fn it_is_iterable_and_symbol_iterator_is_entries() {
        let c = ctx();
        ok(&c, "FormData.prototype[Symbol.iterator] === FormData.prototype.entries");
        ok(&c, "(() => { const f = new FormData(); f.append('a','1'); f.append('b','2'); \
                 return [...f].map(p => p.join(':')).join() === 'a:1,b:2'; })()");
        ok(&c, "(() => { const f = new FormData(); f.append('a','1'); return [...f.keys()].join() === 'a'; })()");
    }

    #[test]
    fn a_file_without_a_filename_comes_back_as_the_same_object() {
        // The identity the spec requires, and the reason blobs are held in a JS
        // array rather than copied into Rust.
        let c = ctx();
        ok(
            &c,
            "(() => { const x = new File(['hi'], 'a.txt'); const f = new FormData(); \
              f.append('file', x); return f.get('file') === x; })()",
        );
    }

    #[test]
    fn a_blob_or_a_renamed_file_becomes_a_new_file_over_the_same_bytes() {
        // XHR §5 "create an entry", steps 3.1 and 3.2 — what Chromium does too.
        let c = ctx();
        ok(
            &c,
            "(() => { const b = new Blob(['hi'], { type: 'text/plain' }); const f = new FormData(); \
              f.append('a', b); f.append('b', b, 'named.txt'); \
              const x = f.get('a'), y = f.get('b'); \
              return x instanceof File && x !== b && x.name === 'blob' && x.type === 'text/plain' && x.size === 2 \
                  && y instanceof File && y.name === 'named.txt' && typeof x.lastModified === 'number'; })()",
        );
        ok(
            &c,
            "(() => { const x = new File(['hey'], 'orig.txt', { type: 'text/x', lastModified: 42 }); const f = new FormData(); \
              f.set('k', x, 'renamed.txt'); const y = f.get('k'); \
              return y !== x && y.name === 'renamed.txt' && y.type === 'text/x' && y.lastModified === 42 && y.size === 3; })()",
        );
    }

    #[test]
    fn a_form_element_argument_refuses_by_name() {
        let c = ctx();
        let msg = throws(&c, "new FormData(document.createElement('form'))");
        assert!(msg.contains("HTMLFormElement"), "{msg}");
        assert!(msg.contains("append"), "{msg}");
    }

    #[test]
    fn a_filename_without_a_blob_is_rejected() {
        let c = ctx();
        let msg = throws(&c, "new FormData().append('a', 'text', 'name.txt')");
        assert!(msg.contains("only allowed when the value is a Blob"), "{msg}");
    }

    #[test]
    fn a_form_data_body_encodes_through_response() {
        // The encoder's real consumer: Response accepts FormData as a BodyInit
        // and sets the multipart content type with the generated boundary.
        let c = ctx();
        s(
            &c,
            "globalThis.out = null; globalThis.ct = null; \
             (() => { const f = new FormData(); f.append('a', 'one'); f.append('b', 'two'); \
               const r = new Response(f); globalThis.ct = r.headers.get('content-type'); \
               r.text().then(t => { globalThis.out = t; }); })(); 'started'",
        );
        c.drain_microtasks();
        // `out` staying null would mean the promise never settled — the exact
        // way an async assertion passes without ever running.
        ok(&c, "typeof globalThis.out === 'string'");
        ok(&c, "globalThis.ct.startsWith('multipart/form-data; boundary=')");
        ok(
            &c,
            "(() => { const b = globalThis.ct.split('boundary=')[1]; const t = globalThis.out; \
              return t.startsWith('--' + b) && t.includes('name=\"a\"') && t.includes('two') \
                  && t.endsWith('--' + b + '--\\r\\n'); })()",
        );
    }
}
