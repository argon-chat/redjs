//! What crosses an `AudioWorkletNode`'s port: a copy of the value as bytes. The worklet's VM has
//! none of the web API, so this is not the structured serializer: it carries primitives,
//! strings, arrays, plain objects, `ArrayBuffer`s and typed arrays, and refuses the rest.

use core::ffi::c_void;

use crate::ffi::RjsTaInfo;
use crate::glue;
use crate::webapi::util::*;

const T_UNDEFINED: u8 = 0;
const T_NULL: u8 = 1;
const T_FALSE: u8 = 2;
const T_TRUE: u8 = 3;
const T_NUMBER: u8 = 4;
const T_STRING: u8 = 5;
const T_ARRAY: u8 = 6;
const T_OBJECT: u8 = 7;
const T_VIEW: u8 = 8;
const T_BUFFER: u8 = 9;

const MAX_DEPTH: u32 = 64;
/// The element size of each `RjsTaKind`, 1 (Int8Array) to 11 (BigUint64Array).
const ELEMENT: [usize; 12] = [0, 1, 1, 1, 2, 2, 4, 4, 4, 8, 8, 8];

/// `Err` is the reason, for a `DataCloneError`.
pub(super) unsafe fn encode(g: *mut c_void, v: u64) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(32);
    write(g, v, &mut out, 0)?;
    Ok(out)
}

unsafe fn write_string(g: *mut c_void, v: u64, out: &mut Vec<u8>) -> Result<(), String> {
    let units = read_string16(g, v).ok_or("a string could not be read")?;
    out.extend_from_slice(&(units.len() as u32).to_le_bytes());
    for unit in units {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    Ok(())
}

unsafe fn write(g: *mut c_void, v: u64, out: &mut Vec<u8>, depth: u32) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err("the value is nested too deeply, or refers to itself".into());
    }
    match glue::rjs_kind(g, v) {
        0 => out.push(T_UNDEFINED),
        1 => out.push(T_NULL),
        2 => out.push(if v == TRUE { T_TRUE } else { T_FALSE }),
        3 | 4 => {
            out.push(T_NUMBER);
            out.extend_from_slice(&read_number(g, v).to_le_bytes());
        }
        5 => {
            out.push(T_STRING);
            write_string(g, v, out)?;
        }
        10 => {
            let len = array_len(g, v);
            out.push(T_ARRAY);
            out.extend_from_slice(&len.to_le_bytes());
            for i in 0..len {
                write(g, get_i(g, v, i), out, depth + 1)?;
            }
        }
        11 => {
            let mut info = RjsTaInfo::default();
            if glue::rjs_typedarray_info(g, v, &mut info) != 0
                || info.detached != 0
                || !(1..=11).contains(&info.kind)
            {
                return Err("a DataView or a detached view cannot be sent".into());
            }
            out.push(T_VIEW);
            out.push(info.kind as u8);
            out.extend_from_slice(&info.length.to_le_bytes());
            if info.byte_length != 0 {
                out.extend_from_slice(core::slice::from_raw_parts(
                    info.data as *const u8,
                    info.byte_length as usize,
                ));
            }
        }
        12 => {
            let mut info = crate::ffi::RjsAbInfo::default();
            if glue::rjs_arraybuffer_info(g, v, &mut info) != 0
                || info.detached != 0
                || info.byte_length > u32::MAX as u64
            {
                return Err("a detached ArrayBuffer cannot be sent".into());
            }
            out.push(T_BUFFER);
            out.extend_from_slice(&(info.byte_length as u32).to_le_bytes());
            if info.byte_length != 0 {
                out.extend_from_slice(core::slice::from_raw_parts(
                    info.data as *const u8,
                    info.byte_length as usize,
                ));
            }
        }
        8 => {
            let mut brand = glue::RjsBrandInfo::default();
            if glue::rjs_brand(g, v, &mut brand) != glue::brand::PLAIN {
                return Err(
                    "only plain objects, arrays, ArrayBuffers and typed arrays can be sent".into(),
                );
            }
            let mut keys = UNDEF;
            if glue::rjs_own_keys(g, v, &mut keys) != 0 {
                return Err("the object's keys could not be read".into());
            }
            let len = array_len(g, keys);
            out.push(T_OBJECT);
            out.extend_from_slice(&len.to_le_bytes());
            for i in 0..len {
                let key = get_i(g, keys, i);
                write_string(g, key, out)?;
                let mut value = UNDEF;
                if glue::rjs_get_v(g, v, key, &mut value) != 0 {
                    return Err("a property could not be read".into());
                }
                write(g, value, out, depth + 1)?;
            }
        }
        _ => return Err("functions, symbols, BigInts and promises cannot be sent".into()),
    }
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.bytes.len() < n {
            return None;
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Some(head)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    unsafe fn string(&mut self, g: *mut c_void) -> Option<u64> {
        let len = self.u32()? as usize;
        let units: Vec<u16> = self
            .take(len.checked_mul(2)?)?
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        Some(js_string16(g, &units))
    }

    unsafe fn view(&mut self, g: *mut c_void, kind: u32, length: usize) -> Option<u64> {
        let size = *ELEMENT.get(kind as usize).filter(|&&size| size != 0)?;
        let bytes = self.take(length.checked_mul(size)?)?;
        let mut array = UNDEF;
        if crate::ffi::redjs_typedarray_create(g, kind, length, &mut array) != 0 {
            return None;
        }
        let mut info = RjsTaInfo::default();
        if !bytes.is_empty() {
            if glue::rjs_typedarray_info(g, array, &mut info) != 0 || info.data.is_null() {
                return None;
            }
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), info.data as *mut u8, bytes.len());
        }
        Some(array)
    }

    unsafe fn value(&mut self, g: *mut c_void) -> Option<u64> {
        Some(match *self.take(1)?.first()? {
            T_UNDEFINED => UNDEF,
            T_NULL => NULL,
            T_FALSE => FALSE,
            T_TRUE => TRUE,
            T_NUMBER => enc_f64(f64::from_le_bytes(self.take(8)?.try_into().ok()?)),
            T_STRING => self.string(g)?,
            T_ARRAY => {
                let len = self.u32()?;
                let array = new_array(g);
                for i in 0..len {
                    let item = self.value(g)?;
                    set_i(g, array, i, item);
                }
                array
            }
            T_OBJECT => {
                let len = self.u32()?;
                let object = new_object(g, UNDEF);
                for _ in 0..len {
                    let key = self.string(g)?;
                    let value = self.value(g)?;
                    glue::rjs_define_v(g, object, key, value);
                }
                object
            }
            T_VIEW => {
                let kind = *self.take(1)?.first()? as u32;
                let length = self.u32()? as usize;
                self.view(g, kind, length)?
            }
            T_BUFFER => {
                let length = self.u32()? as usize;
                let bytes = self.view(g, 2, length)?;
                get(g, bytes, "buffer")
            }
            _ => return None,
        })
    }
}

/// `None` for bytes this did not write.
pub(super) unsafe fn decode(g: *mut c_void, bytes: &[u8]) -> Option<u64> {
    Reader { bytes }.value(g)
}
