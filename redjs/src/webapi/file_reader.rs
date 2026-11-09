//! FileReader's read state and packaging live in Rust, over the native Blob pool.
//! Only operation IDs cross worker queues. JS results/handlers are traced private
//! slots; active readers are rooted until their final event or abort completes.
use super::{blob, domexception, serialize, util::*, xhr};
use crate::{capi, glue};
use core::ffi::c_void;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
};

const EMPTY: u8 = 0;
const LOADING: u8 = 1;
const DONE: u8 = 2;
#[derive(Default)]
struct Reader {
    state: u8,
    flight: u64,
}
struct Classes {
    class: capi::JSClassRef,
    reader: Class,
    slots: *mut c_void,
    buffer_getter: u64,
}
unsafe impl Send for Classes {}
unsafe impl Sync for Classes {}
fn registry() -> &'static Mutex<HashMap<usize, &'static Classes>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static Classes>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}
fn classes(g: *mut c_void) -> &'static Classes {
    registry()
        .lock()
        .unwrap()
        .get(&(g as usize))
        .copied()
        .expect("FileReader not installed")
}
fn flights() -> &'static Mutex<HashMap<(usize, u64), u64>> {
    static R: OnceLock<Mutex<HashMap<(usize, u64), u64>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}
static NEXT: AtomicU64 = AtomicU64::new(1);
pub(crate) fn forget(gone: crate::context::Gone) {
    registry().lock().unwrap().remove(&gone.key);
    flights()
        .lock()
        .unwrap()
        .retain(|(key, _), _| *key != gone.key);
}
unsafe fn reader<'a>(g: *mut c_void, object: u64) -> Option<&'a mut Reader> {
    host_data(g, classes(g).class, object)
}
unsafe fn bag(g: *mut c_void, object: u64) -> u64 {
    get_a(g, object, classes(g).slots)
}
unsafe fn slot(g: *mut c_void, object: u64, key: &str) -> u64 {
    get(g, bag(g, object), key)
}
unsafe fn set_slot(g: *mut c_void, object: u64, key: &str, value: u64) {
    if is_cell(value) {
        crate::ffi::redjs_protect(g, value);
    }
    define(g, bag(g, object), key, value, 0);
    if is_cell(value) {
        crate::ffi::redjs_unprotect(g, value);
    }
}
unsafe fn checked(g: *mut c_void, object: u64, method: &str) -> bool {
    if reader(g, object).is_some() {
        true
    } else {
        throw_type_error(
            g,
            &format!("Illegal invocation: FileReader.{method} called on a non-FileReader"),
        );
        false
    }
}
fn object_for(g: *mut c_void, id: u64) -> Option<u64> {
    flights().lock().unwrap().get(&(g as usize, id)).copied()
}
unsafe fn end(g: *mut c_void, object: u64) -> bool {
    let Some(data) = reader(g, object) else {
        return false;
    };
    let id = std::mem::take(&mut data.flight);
    id != 0
        && flights()
            .lock()
            .unwrap()
            .remove(&(g as usize, id))
            .is_some()
}
unsafe fn loading(g: *mut c_void, object: u64) -> bool {
    reader(g, object).is_some_and(|data| data.state == LOADING)
}
#[crate::host_fn]
unsafe fn construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let class = classes(g);
    let Ok(proto) = try_get(g, cf_new_target(cf), "prototype") else {
        return UNDEF;
    };
    let object = host_create(
        g,
        class.class,
        if blob::is_object(g, proto) {
            proto
        } else {
            class.reader.proto
        },
        Reader::default(),
    );
    crate::ffi::redjs_protect(g, object);
    let values = new_object(g, UNDEF);
    crate::ffi::redjs_protect(g, values);
    glue::rjs_define(g, object, class.slots, values, 0);
    define(g, values, "result", NULL, 0);
    define(g, values, "error", NULL, 0);
    crate::ffi::redjs_unprotect(g, values);
    crate::ffi::redjs_unprotect(g, object);
    object
}
#[crate::host_fn]
unsafe fn ready_state(g: *mut c_void, cf: *mut c_void) -> u64 {
    match reader(g, cf_this(cf)) {
        Some(data) => enc_i32(data.state as i32),
        None => throw_type_error(g, "FileReader.readyState called on a non-FileReader"),
    }
}
macro_rules! getter {
    ($name:ident, $key:literal) => {
        #[crate::host_fn]
        unsafe fn $name(g: *mut c_void, cf: *mut c_void) -> u64 {
            let object = cf_this(cf);
            if !checked(g, object, $key) {
                return UNDEF;
            }
            slot(g, object, $key)
        }
    };
}
getter!(result, "result");
getter!(error, "error");
macro_rules! handler {
    ($get:ident, $set:ident, $key:literal) => {
        #[crate::host_fn]
        unsafe fn $get(g: *mut c_void, cf: *mut c_void) -> u64 {
            let object = cf_this(cf);
            if !checked(g, object, $key) {
                return UNDEF;
            }
            let value = slot(g, object, $key);
            if blob::is_object(g, value) {
                value
            } else {
                NULL
            }
        }
        #[crate::host_fn]
        unsafe fn $set(g: *mut c_void, cf: *mut c_void) -> u64 {
            let object = cf_this(cf);
            if !checked(g, object, $key) {
                return UNDEF;
            }
            let value = cf_arg(cf, 0);
            set_slot(
                g,
                object,
                $key,
                if blob::is_object(g, value) {
                    value
                } else {
                    NULL
                },
            );
            UNDEF
        }
    };
}
handler!(get_loadstart, set_loadstart, "onloadstart");
handler!(get_progress, set_progress, "onprogress");
handler!(get_load, set_load, "onload");
handler!(get_abort, set_abort, "onabort");
handler!(get_error, set_error, "onerror");
handler!(get_loadend, set_loadend, "onloadend");

#[derive(Clone, Copy)]
enum Mode {
    ArrayBuffer,
    BinaryString,
    Text,
    DataUrl,
}
fn encoding(label: Option<&str>) -> Option<&'static encoding_rs::Encoding> {
    label
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        .filter(|encoding| *encoding != encoding_rs::REPLACEMENT)
}
unsafe fn package(
    g: *mut c_void,
    bytes: &[u8],
    mode: Mode,
    mime: &str,
    label: Option<&str>,
) -> Result<u64, blob::ReadError> {
    let value = match mode {
        Mode::ArrayBuffer => {
            let bytes = make_u8(g, bytes);
            if !is_cell(bytes) {
                return Err(blob::ReadError::OutOfMemory);
            }
            crate::ffi::redjs_protect(g, bytes);
            let buffer = call(g, classes(g).buffer_getter, bytes, &[]);
            crate::ffi::redjs_unprotect(g, bytes);
            buffer.map_err(|_| blob::ReadError::OutOfMemory)?
        }
        Mode::BinaryString => js_string16(
            g,
            &bytes
                .iter()
                .map(|byte| u16::from(*byte))
                .collect::<Vec<_>>(),
        ),
        Mode::DataUrl => js_string(
            g,
            &format!(
                "data:{};base64,{}",
                if mime.is_empty() {
                    "application/octet-stream"
                } else {
                    mime
                },
                super::fsutil::b64_encode(bytes)
            ),
        ),
        Mode::Text => {
            let charset = xhr::charset_of(mime);
            let encoding = encoding(label)
                .or_else(|| encoding(charset.as_deref()))
                .unwrap_or(encoding_rs::UTF_8);
            // Encoding::decode performs BOM sniffing first, overriding the label.
            let (text, _, _) = encoding.decode(bytes);
            js_string(g, &text)
        }
    };
    if is_cell(value) {
        Ok(value)
    } else {
        Err(blob::ReadError::OutOfMemory)
    }
}
unsafe fn terminal(g: *mut c_void, object: u64, event: &str, loaded: u64) {
    reader(g, object).unwrap().state = DONE;
    let owed = end(g, object);
    xhr::fire_progress(g, object, event, loaded, loaded);
    if !loading(g, object) {
        xhr::fire_progress(g, object, "loadend", loaded, loaded);
    }
    if owed {
        crate::ffi::redjs_unprotect(g, object);
    }
}
unsafe fn finish(
    g: *mut c_void,
    id: u64,
    read: Result<Vec<u8>, blob::ReadError>,
    mode: Mode,
    mime: &str,
    label: Option<&str>,
) {
    let Some(object) = object_for(g, id) else {
        return;
    };
    let read = match read {
        Ok(bytes) => {
            xhr::fire_progress(
                g,
                object,
                "progress",
                bytes.len() as u64,
                bytes.len() as u64,
            );
            if object_for(g, id).is_none() {
                return;
            }
            package(g, &bytes, mode, mime, label).map(|value| (value, bytes.len() as u64))
        }
        Err(error) => Err(error),
    };
    match read {
        Ok((value, loaded)) => {
            set_slot(g, object, "result", value);
            terminal(g, object, "load", loaded);
        }
        Err(error) => {
            set_slot(g, object, "result", NULL);
            set_slot(g, object, "error", error.to_js(g));
            terminal(g, object, "error", 0);
        }
    }
}
unsafe fn read(g: *mut c_void, cf: *mut c_void, mode: Mode, name: &str) -> u64 {
    let object = cf_this(cf);
    if !checked(g, object, name) {
        return UNDEF;
    }
    if cf_argc(cf) == 0 {
        return throw_type_error(g, &format!("Failed to execute '{name}' on 'FileReader': 1 argument required, but only 0 present."));
    }
    let Some(blob) = blob::data_of(g, cf_arg(cf, 0)) else {
        return throw_type_error(
            g,
            &format!(
                "Failed to execute '{name}' on 'FileReader': parameter 1 is not of type 'Blob'."
            ),
        );
    };
    let range = blob.range();
    let mime = blob.mime.clone();
    let label = if matches!(mode, Mode::Text) && cf_arg(cf, 1) != UNDEF {
        let Ok(string) = to_js_string(g, cf_arg(cf, 1)) else {
            return UNDEF;
        };
        read_string(g, string)
    } else {
        None
    };
    // Recheck after WebIDL ToString, which may reenter this reader.
    if loading(g, object) {
        let error = domexception::create(g, "InvalidStateError", &format!("Failed to execute '{name}' on 'FileReader': The object is already busy reading Blobs."));
        glue::rjs_throw(g, error);
        return UNDEF;
    }
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    {
        let data = reader(g, object).unwrap();
        data.state = LOADING;
        data.flight = id;
    }
    set_slot(g, object, "result", NULL);
    set_slot(g, object, "error", NULL);
    crate::ffi::redjs_protect(g, object);
    flights().lock().unwrap().insert((g as usize, id), object);
    // loadstart is a task. Abort before it runs cancels the read without doing I/O.
    crate::web::schedule_native(
        g,
        0.0,
        Box::new(move |g| unsafe {
            let Some(object) = object_for(g, id) else {
                return;
            };
            xhr::fire_progress(g, object, "loadstart", 0, 0);
            if object_for(g, id).is_none() {
                return;
            }
            blob::read_async(
                g,
                range,
                Box::new(move |g, bytes| finish(g, id, bytes, mode, &mime, label.as_deref())),
            );
        }),
    );
    UNDEF
}
#[crate::host_fn]
unsafe fn read_array_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    read(g, cf, Mode::ArrayBuffer, "readAsArrayBuffer")
}
#[crate::host_fn]
unsafe fn read_binary_string(g: *mut c_void, cf: *mut c_void) -> u64 {
    read(g, cf, Mode::BinaryString, "readAsBinaryString")
}
#[crate::host_fn]
unsafe fn read_text(g: *mut c_void, cf: *mut c_void) -> u64 {
    read(g, cf, Mode::Text, "readAsText")
}
#[crate::host_fn]
unsafe fn read_data_url(g: *mut c_void, cf: *mut c_void) -> u64 {
    read(g, cf, Mode::DataUrl, "readAsDataURL")
}
#[crate::host_fn]
unsafe fn abort(g: *mut c_void, cf: *mut c_void) -> u64 {
    let object = cf_this(cf);
    if !checked(g, object, "abort") {
        return UNDEF;
    }
    set_slot(g, object, "result", NULL);
    if !loading(g, object) {
        return UNDEF;
    }
    terminal(g, object, "abort", 0);
    UNDEF
}
pub unsafe fn install(g: *mut c_void, event_target_proto: u64) {
    // Capture the intrinsic getter before app code can replace TypedArray.buffer.
    let object = get(g, glue::rjs_global(g), "Object");
    let typed = get(g, get(g, glue::rjs_global(g), "Uint8Array"), "prototype");
    let typed_proto =
        capi::JSObjectGetPrototype(g, typed as usize as capi::JSObjectRef) as usize as u64;
    let descriptor = call(
        g,
        get(g, object, "getOwnPropertyDescriptor"),
        object,
        &[typed_proto, js_string(g, "buffer")],
    )
    .expect("TypedArray.buffer intrinsic descriptor");
    crate::ffi::redjs_protect(g, descriptor);
    let buffer_getter = get(g, descriptor, "get");
    crate::ffi::redjs_protect(g, buffer_getter);
    crate::ffi::redjs_unprotect(g, descriptor);
    let class = make_host_class::<Reader>("FileReader");
    let reader = define_class(
        g,
        &ClassSpec {
            name: "FileReader",
            arity: 0,
            construct: Some(construct),
            parent_proto: event_target_proto,
            methods: &[
                ("readAsArrayBuffer", 1, read_array_buffer),
                ("readAsBinaryString", 1, read_binary_string),
                ("readAsText", 1, read_text),
                ("readAsDataURL", 1, read_data_url),
                ("abort", 0, abort),
            ],
            getters: &[
                ("readyState", ready_state),
                ("result", result),
                ("error", error),
            ],
            accessors: &[
                ("onloadstart", get_loadstart, set_loadstart),
                ("onprogress", get_progress, set_progress),
                ("onload", get_load, set_load),
                ("onabort", get_abort, set_abort),
                ("onerror", get_error, set_error),
                ("onloadend", get_loadend, set_loadend),
            ],
        },
    );
    for (name, value) in [("EMPTY", EMPTY), ("LOADING", LOADING), ("DONE", DONE)] {
        define(g, reader.ctor, name, enc_i32(value as i32), 1 | 4);
        define(g, reader.proto, name, enc_i32(value as i32), 1 | 4);
    }
    serialize::refuse_platform_prototype(g, reader.proto, "FileReader");
    let state = Box::leak(Box::new(Classes {
        class,
        reader,
        slots: atom_priv(g, "redjs#fileReader"),
        buffer_getter,
    }));
    registry().lock().unwrap().insert(g as usize, state);
}
#[cfg(test)]
#[path = "file_reader/tests.rs"]
mod tests;
