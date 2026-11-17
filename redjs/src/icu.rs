//! ICU data decompression hook for the bun-webkit prebuilt.
//!
//! oven-sh/WebKit's ICU build (icu/udata-decompress-hook.patch) repacks the
//! common data archive: display-name resource items (curr/lang/region/unit/
//! zone, non-en) are stored as raw zstd frames, and the patched udata.cpp
//! calls a WEAK `bun_icu_maybe_decompress` hook between TOC lookup and item
//! validation. Without a strong definition the weak default (null) leaves
//! compressed items raw and Intl/toLocale* fail (u_init: U_FILE_ACCESS_ERROR,
//! udat_open: allocation/parse errors).
//!
//! This is the Rust twin of Bun's `bun_icu_decompress.cpp`: zstd-decompress
//! per item using the dictionary shipped inside sicudt.lib, cache the buffer
//! for the process lifetime keyed by the item's .rodata address, pass through
//! anything that doesn't start with the zstd magic.

use std::sync::atomic::{AtomicBool, Ordering};

/// The ICU data this build links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Linked {
    /// Every locale (the prebuilt's own data).
    Full,
    /// The root locale only (the `icu-root` feature); [`set_common_data`] adds the rest.
    Root,
    /// The operating system's ICU (macOS).
    System,
}

impl Linked {
    pub fn as_str(self) -> &'static str {
        match self {
            Linked::Full => "full",
            Linked::Root => "root",
            Linked::System => "system",
        }
    }
}

pub const LINKED: Linked = if cfg!(target_os = "macos") {
    Linked::System
} else if cfg!(redjs_icu_root) {
    Linked::Root
} else {
    Linked::Full
};

static INITIALIZED: AtomicBool = AtomicBool::new(false);

pub(crate) fn mark_initialized() {
    INITIALIZED.store(true, Ordering::Release);
}

/// The linked package's name, e.g. `icudt79l`; its full data file is `<name>.dat`. `None` with the system ICU.
pub fn package() -> Option<String> {
    #[cfg(target_os = "macos")]
    return None;
    #[cfg(not(target_os = "macos"))]
    // SAFETY: the linked package is a valid ICU package (the prebuilt's own, or the `icu-root`
    // subset of it); only its header, first TOC entry and first name are read.
    unsafe {
        let linked = crate::glue::rjs_icu_linked_data() as *const u8;
        let size = u16::from_le_bytes([*linked, *linked.add(1)]) as usize;
        let start = core::slice::from_raw_parts(linked, size + 8);
        if read_u32(start, size) == 0 {
            return None;
        }
        let name = core::ffi::CStr::from_ptr(linked.add(size + read_u32(start, size + 4) as usize).cast());
        name.to_str().ok()?.split_once('/').map(|(p, _)| p.to_owned())
    }
}

/// Hands ICU a common-data package of this engine's ICU (`<package>.dat`, as the prebuilt stores
/// it), searched before the linked data. Only before the first context.
pub fn set_common_data(data: &'static [u8]) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let _ = data;
        return Err("this engine uses the system ICU, which takes no data".into());
    }
    #[cfg(not(target_os = "macos"))]
    {
        if INITIALIZED.load(Ordering::Acquire) {
            return Err("the engine is already initialized; ICU data is set before the first context".into());
        }
        if data.as_ptr() as usize % 16 != 0 {
            return Err("ICU data must be 16-byte aligned".into());
        }
        let toc = Toc::read(data).ok_or("not an ICU common-data package")?;
        let want = package().ok_or("the linked ICU data has no package name")?;
        let have = toc.package().ok_or("the ICU package is empty")?;
        if have != want {
            return Err(format!("the ICU data is {have}, this engine's ICU reads {want}"));
        }
        if let Some(item) = toc.first_compressed() {
            let mut length = item.len() as i32;
            let decoded = unsafe { bun_icu_maybe_decompress(item.as_ptr().cast(), &mut length) };
            if decoded == item.as_ptr().cast() {
                return Err("the ICU data is compressed with another dictionary than this engine's".into());
            }
        }
        unsafe { crate::glue::rjs_icu_set_common_data(data.as_ptr().cast()) };
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
struct Toc<'a> {
    data: &'a [u8],
    header: usize,
    count: usize,
}

#[cfg(not(target_os = "macos"))]
fn read_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

#[cfg(not(target_os = "macos"))]
impl<'a> Toc<'a> {
    fn read(data: &'a [u8]) -> Option<Self> {
        if data.len() < 32 || data[2] != 0xda || data[3] != 0x27 || &data[12..16] != b"CmnD" {
            return None;
        }
        let header = u16::from_le_bytes([data[0], data[1]]) as usize;
        if header + 4 > data.len() {
            return None;
        }
        let count = read_u32(data, header) as usize;
        if header + 4 + 8 * count > data.len() {
            return None;
        }
        let toc = Toc { data, header, count };
        (0..count).all(|i| toc.name(i).is_some() && toc.offset(i) < data.len()).then_some(toc)
    }

    fn offset(&self, i: usize) -> usize {
        self.header + read_u32(self.data, self.header + 8 + 8 * i) as usize
    }

    fn name(&self, i: usize) -> Option<&'a str> {
        let at = self.header + read_u32(self.data, self.header + 4 + 8 * i) as usize;
        let rest = self.data.get(at..)?;
        core::str::from_utf8(&rest[..rest.iter().position(|&b| b == 0)?]).ok()
    }

    fn package(&self) -> Option<&'a str> {
        (self.count > 0).then(|| self.name(0)?.split_once('/').map(|(p, _)| p)).flatten()
    }

    fn item(&self, i: usize) -> &'a [u8] {
        let start = self.offset(i);
        let end = if i + 1 < self.count { self.offset(i + 1) } else { self.data.len() };
        &self.data[start..end.max(start)]
    }

    fn first_compressed(&self) -> Option<&'a [u8]> {
        (0..self.count)
            .map(|i| self.item(i))
            .find(|item| item.len() >= 4 && read_u32(item, 0) == 0xFD2F_B528)
    }
}

// macOS uses the operating system's libicucore and has no compressed data
// dictionary. Keep the embedding anchor available without referring to the
// static-ICU symbols that only the Windows/Linux prebuilts provide.
#[cfg(target_os = "macos")]
#[no_mangle]
pub unsafe extern "C" fn bun_icu_maybe_decompress(
    pointer: *const core::ffi::c_void,
    _length: *mut i32,
) -> *const core::ffi::c_void {
    pointer
}

#[cfg(not(target_os = "macos"))]
pub use compressed::bun_icu_maybe_decompress;

#[cfg(not(target_os = "macos"))]
mod compressed {
    use core::ffi::c_void;
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    #[cfg(not(redjs_engine_dll))]
    extern "C" {
        static bun_icu_zstd_dict: u8;
        static bun_icu_zstd_dict_size: u32;
    }
    // The engine DLL (build.rs `REDJS_ENGINE_DLL`) hands the dictionary over
    // through a function: data does not cross an import library.
    #[cfg(redjs_engine_dll)]
    extern "C" {
        fn rjs_icu_zstd_dict(size: *mut u32) -> *const c_void;
    }

    /// The dictionary sicudt.lib carries, and its size.
    unsafe fn dictionary() -> (*const c_void, usize) {
        #[cfg(not(redjs_engine_dll))]
        return (
            core::ptr::addr_of!(bun_icu_zstd_dict) as *const c_void,
            core::ptr::addr_of!(bun_icu_zstd_dict_size).read() as usize,
        );
        #[cfg(redjs_engine_dll)]
        {
            let mut size = 0;
            let dictionary = rjs_icu_zstd_dict(&mut size);
            (dictionary, size as usize)
        }
    }

    const ZSTD_MAGIC: u32 = 0xFD2F_B528;
    const ZSTD_CONTENTSIZE_UNKNOWN: u64 = u64::MAX; // -1
    const ZSTD_CONTENTSIZE_ERROR: u64 = u64::MAX - 1; // -2

    struct State {
        dctx: usize,  // *mut ZSTD_DCtx
        ddict: usize, // *mut ZSTD_DDict (0 when no dict)
        cache: HashMap<usize, (usize, i32)>,
    }

    unsafe impl Send for State {}

    fn state() -> &'static Mutex<State> {
        static S: OnceLock<Mutex<State>> = OnceLock::new();
        S.get_or_init(|| unsafe {
            let dctx = zstd_sys::ZSTD_createDCtx() as usize;
            let (dict, dict_size) = dictionary();
            let ddict = if dict_size > 0 {
                zstd_sys::ZSTD_createDDict(dict, dict_size) as usize
            } else {
                0
            };
            Mutex::new(State { dctx, ddict, cache: HashMap::new() })
        })
    }

    /// Called by the patched ICU udata.cpp for every common-data item lookup.
    /// `length` is the TOC-derived item length (may be <= 0 for the last item).
    #[no_mangle]
    pub unsafe extern "C" fn bun_icu_maybe_decompress(p: *const c_void, length: *mut i32) -> *const c_void {
        if p.is_null() {
            return p;
        }
        let magic = (p as *const u32).read_unaligned();
        if magic != ZSTD_MAGIC {
            return p; // raw ICU item (0xda27 header) — the common case
        }

        let mut st = state().lock().unwrap();

        let toc_len = if length.is_null() { 0 } else { *length };
        let bound = if toc_len > 0 { toc_len as usize } else { 1usize << 20 };

        if let Some(&(buf, dlen)) = st.cache.get(&(p as usize)) {
            if !length.is_null() {
                *length = dlen;
            }
            return buf as *const c_void;
        }

        let clen = zstd_sys::ZSTD_findFrameCompressedSize(p, bound);
        if zstd_sys::ZSTD_isError(clen) != 0 {
            return p;
        }
        let dlen = zstd_sys::ZSTD_getFrameContentSize(p, clen);
        if dlen == ZSTD_CONTENTSIZE_UNKNOWN || dlen == ZSTD_CONTENTSIZE_ERROR || dlen == 0 {
            return p;
        }

        let mut buf = vec![0u8; dlen as usize].into_boxed_slice();
        let r = if st.ddict != 0 {
            zstd_sys::ZSTD_decompress_usingDDict(
                st.dctx as *mut zstd_sys::ZSTD_DCtx,
                buf.as_mut_ptr() as *mut c_void,
                dlen as usize,
                p,
                clen,
                st.ddict as *const zstd_sys::ZSTD_DDict,
            )
        } else {
            zstd_sys::ZSTD_decompressDCtx(
                st.dctx as *mut zstd_sys::ZSTD_DCtx,
                buf.as_mut_ptr() as *mut c_void,
                dlen as usize,
                p,
                clen,
            )
        };
        if zstd_sys::ZSTD_isError(r) != 0 {
            return p;
        }

        let leaked = Box::leak(buf);
        st.cache.insert(p as usize, (leaked.as_ptr() as usize, dlen as i32));
        if !length.is_null() {
            *length = dlen as i32;
        }
        leaked.as_ptr() as *const c_void
    }
}
