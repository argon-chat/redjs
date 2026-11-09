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

    extern "C" {
        static bun_icu_zstd_dict: u8;
        static bun_icu_zstd_dict_size: u32;
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
            let dict_size = core::ptr::addr_of!(bun_icu_zstd_dict_size).read() as usize;
            let ddict = if dict_size > 0 {
                zstd_sys::ZSTD_createDDict(core::ptr::addr_of!(bun_icu_zstd_dict) as *const c_void, dict_size) as usize
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
