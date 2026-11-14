//! Experimental native AOT image compiler/loader for the pinned JSC engine.
//! This module does not accept bytecode caches or Bun standalone executables.

use crate::api::{Context, Error, Value};
use core::ffi::c_void;
use sha2::{Digest, Sha256};
use std::path::Path;

/// Exact experimental frontend/wire-table provenance.
pub const FRONTEND_REVISION: &str = "7ca0042d3ba23ed60fd9cd8af1a1ce6ec3f8d074";
pub const ENGINE_VERSION: &str = env!("REDJS_AOT_ENGINE_VERSION");
pub const BUILD_TABLE_ID: &str = env!("REDJS_AOT_TABLE_ID");
#[cfg(windows)]
pub const COMPILE_OPTIONS: &str = concat!(
    env!("REDJS_AOT_TARGET_ARCH"),
    ";references=narrow;image=pic;threads=1;admit=FilesRunByHost;default-options@",
    env!("REDJS_AOT_ENGINE_VERSION")
);
#[cfg(unix)]
pub const COMPILE_OPTIONS: &str = concat!(
    env!("REDJS_AOT_TARGET_ARCH"),
    ";os=",
    env!("REDJS_AOT_TARGET_OS"),
    ";references=narrow;image=pic;threads=1;admit=FilesRunByHost;default-options@",
    env!("REDJS_AOT_ENGINE_VERSION")
);
pub fn digest_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Hash a trusted image without retaining a second image-sized allocation.
/// Interrupted reads are retried; all other I/O failures remain observable.
pub fn digest_reader(mut reader: impl std::io::Read) -> std::io::Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(format!("{:x}", hash.finalize())),
            Ok(length) => hash.update(&buffer[..length]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Runtime-only heap capacity profile for a selected native AOT process.
/// This does not change native-image identity, reference width or JIT defaults.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u32)]
pub enum MemoryProfile {
    /// Preserve the pinned engine's normal options and external overrides.
    #[default]
    Default = 0,
    /// 256 MiB Structure heap and a 1 GiB narrow-reference cell cage.
    /// Capacity limits: an exhausted heap can fail allocations earlier than
    /// the engine-default profile. Select before any JSC initialization.
    Compact = 1,
}

/// The options and initial image-view protection actually selected by a loader.
/// An engine-default structure_heap_size_kb of zero means automatic sizing;
/// these are configured option values, not committed/resident byte counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemorySettings {
    pub profile: MemoryProfile,
    pub structure_heap_size_kb: u64,
    pub marked_block_cage_size_mb: u64,
    pub initial_image_read_only: bool,
}

fn anchor_icu() {
    core::hint::black_box(crate::icu::bun_icu_maybe_decompress as *const ());
}

unsafe extern "C" {
    fn rjs_aot_compile(program: *const u8, length: usize, identity: *const u8) -> *mut c_void;
    fn rjs_aot_compilation_image(handle: *mut c_void, bytes: *mut *const u8) -> usize;
    fn rjs_aot_compilation_entries(handle: *mut c_void, entries: *mut *const u32) -> usize;
    fn rjs_aot_compilation_diagnostic(handle: *mut c_void, bytes: *mut *const u8) -> usize;
    fn rjs_aot_compilation_free(handle: *mut c_void);
    fn rjs_aot_load_file_with_profile(
        path: *const u16,
        length: usize,
        identity: *const u8,
        profile: u32,
    ) -> u32;
    fn rjs_aot_load_handle_with_profile(file: isize, identity: *const u8, profile: u32) -> u32;
    fn rjs_aot_memory_settings(
        structure_heap_kb: *mut u64,
        cell_cage_mb: *mut u64,
        read_only: *mut u32,
    ) -> u32;
    fn rjs_aot_create_context() -> *mut c_void;
    fn rjs_aot_run(context: *mut c_void, module: u32, result: *mut u64) -> i32;
    fn rjs_aot_is_active() -> i32;
    fn rjs_aot_admits_host_files() -> i32;
    fn rjs_aot_forget_context(context: *mut c_void);
}

#[derive(Debug)]
pub struct CompiledImage {
    pub bytes: Vec<u8>,
    pub entries: Vec<u32>,
    pub identity: [u8; 16],
    pub diagnostics: String,
}

struct Compilation(*mut c_void);
impl Drop for Compilation {
    fn drop(&mut self) {
        unsafe { rjs_aot_compilation_free(self.0) }
    }
}

/// Compiles trusted frontend IR into machine code without running its program.
///
/// # Safety
/// The bytes must come from the exact pinned frontend/wire schema and be kept
/// inside the build's trust boundary. Native compiler input is not user content.
pub unsafe fn compile_ir(program: &[u8]) -> Result<CompiledImage, String> {
    anchor_icu();
    let mut hash = Sha256::new();
    hash.update(b"skathi-aot-image-v1\0");
    hash.update(include_bytes!("../../pins.txt"));
    hash.update(BUILD_TABLE_ID);
    hash.update(COMPILE_OPTIONS);
    hash.update(program);
    let digest = hash.finalize();
    let identity: [u8; 16] = digest[..16].try_into().expect("SHA-256 has 32 bytes");
    let handle =
        Compilation(unsafe { rjs_aot_compile(program.as_ptr(), program.len(), identity.as_ptr()) });
    if handle.0.is_null() {
        return Err("native AOT compiler did not initialize".into());
    }
    let mut diagnostic = std::ptr::null();
    let diagnostic_length = unsafe { rjs_aot_compilation_diagnostic(handle.0, &mut diagnostic) };
    let diagnostics = if diagnostic_length == 0 {
        String::new()
    } else {
        String::from_utf8_lossy(unsafe {
            std::slice::from_raw_parts(diagnostic, diagnostic_length)
        })
        .into_owned()
    };
    let mut image = std::ptr::null();
    let length = unsafe { rjs_aot_compilation_image(handle.0, &mut image) };
    if image.is_null() || length == 0 {
        return Err(format!("native AOT compilation failed:\n{diagnostics}"));
    }
    let mut entries = std::ptr::null();
    let entry_count = unsafe { rjs_aot_compilation_entries(handle.0, &mut entries) };
    let entries = if entry_count == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(entries, entry_count) }.to_vec()
    };
    Ok(CompiledImage {
        bytes: unsafe { std::slice::from_raw_parts(image, length) }.to_vec(),
        entries,
        identity,
        diagnostics,
    })
}

/// Selects a trusted native image before the first redjs context is created.
///
/// # Safety
/// Loading executes native machine code. The file and identity must belong to
/// the application's trusted build artifacts. Use a separate AOT process: this
/// changes process-wide JSC options and cannot be undone for a JIT fallback.
/// The host must check the artifact manifest against ENGINE_VERSION,
/// BUILD_TABLE_ID and COMPILE_OPTIONS before loading. An identity from an
/// unrelated artifact is not an ABI compatibility check.
pub unsafe fn load_image(path: &Path, identity: [u8; 16]) -> Result<(), String> {
    unsafe { load_image_with_profile(path, identity, MemoryProfile::Default) }
}

/// Selects a trusted native image and its runtime memory capacity profile.
/// Compact must be selected before JSC initialization; conflicting options or
/// attempts to change a previously initialized profile fail explicitly.
///
/// # Safety
/// The same native-code trust and exact manifest/ABI checks as [`load_image`]
/// apply. Compact is a capacity tradeoff, not an image compatibility change.
/// Options cannot be undone for a JIT fallback after initialization/acceptance.
pub unsafe fn load_image_with_profile(
    path: &Path,
    identity: [u8; 16],
    profile: MemoryProfile,
) -> Result<(), String> {
    anchor_icu();
    #[cfg(unix)]
    {
        let file = std::fs::File::open(path)
            .map_err(|error| format!("cannot open native AOT image: {error}"))?;
        return unsafe { load_image_file_with_profile(&file, identity, profile) };
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        let verdict = unsafe {
            rjs_aot_load_file_with_profile(
                wide.as_ptr(),
                wide.len(),
                identity.as_ptr(),
                profile as u32,
            )
        };
        if verdict == 1 {
            Ok(())
        } else {
            Err(format!(
                "native AOT image rejected (verdict {verdict}, memory profile {profile:?})"
            ))
        }
    }
}

/// Loads the exact already-verified file object, without reopening its path.
/// The loader duplicates its handle; the caller can close its File on success.
/// The file must have read and execute access. Hash and validate the trusted
/// manifest through this same file while denying writes before calling.
///
/// # Safety
/// The native-code trust and exact ABI requirements of [`load_image`] apply.
/// A read checksum is not authentication. The caller must keep the verified
/// file immutable for the native image lifetime. On Windows, deny write sharing;
/// the retained duplicate preserves it. On Unix, publish immutable artifacts
/// and do not modify the retained inode. A retained path alone is not a file identity.
pub unsafe fn load_image_file_with_profile(
    file: &std::fs::File,
    identity: [u8; 16],
    profile: MemoryProfile,
) -> Result<(), String> {
    anchor_icu();
    #[cfg(unix)]
    use std::os::fd::AsRawFd;
    #[cfg(windows)]
    use std::os::windows::io::AsRawHandle;
    #[cfg(windows)]
    let descriptor = file.as_raw_handle() as isize;
    #[cfg(unix)]
    let descriptor = file.as_raw_fd() as isize;
    let verdict =
        unsafe { rjs_aot_load_handle_with_profile(descriptor, identity.as_ptr(), profile as u32) };
    if verdict == 1 {
        Ok(())
    } else {
        Err(format!(
            "native AOT image rejected (verdict {verdict}, memory profile {profile:?})"
        ))
    }
}

/// Read-only diagnostics for the image on this VM thread; no initialization.
pub fn memory_settings() -> Option<MemorySettings> {
    let (mut structure_heap_size_kb, mut marked_block_cage_size_mb, mut read_only) = (0, 0, 0);
    let profile = match unsafe {
        rjs_aot_memory_settings(
            &mut structure_heap_size_kb,
            &mut marked_block_cage_size_mb,
            &mut read_only,
        )
    } {
        0 => MemoryProfile::Default,
        1 => MemoryProfile::Compact,
        _ => return None,
    };
    Some(MemorySettings {
        profile,
        structure_heap_size_kb,
        marked_block_cage_size_mb,
        initial_image_read_only: read_only != 0,
    })
}

/// The native backend is selected on this VM thread.
pub fn is_active() -> bool {
    unsafe { rjs_aot_is_active() != 0 }
}

/// The selected image was compiled for a host that runs code of its own before the entry: preludes, native globals.
pub fn admits_host_files() -> bool {
    unsafe { rjs_aot_admits_host_files() != 0 }
}

pub(crate) unsafe fn forget_context(context: *mut c_void) {
    unsafe { rjs_aot_forget_context(context) }
}

pub(crate) fn create_context_if_active() -> *mut c_void {
    unsafe { rjs_aot_create_context() }
}

#[derive(Debug)]
pub enum ModuleEvaluation {
    Fulfilled(Value),
    Pending(Value),
}

impl Context {
    /// Starts a one-based native entry module after the host registered all JS globals.
    /// A pending top-level await returns its promise, never completed success.
    pub fn run_aot_module(&self, module: u32) -> crate::api::Result<ModuleEvaluation> {
        let mut result = 0;
        match unsafe { rjs_aot_run(self.raw(), module, &mut result) } {
            0 => Ok(ModuleEvaluation::Fulfilled(Value(result))),
            3 => Ok(ModuleEvaluation::Pending(Value(result))),
            1 => Err(Error::Exception(Value(result))),
            2 => Err(Error::BadArg),
            status => Err(Error::Engine(status)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Read};
    struct Chunked {
        bytes: Vec<u8>,
        offset: usize,
        interrupted: bool,
    }
    impl Read for Chunked {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = 3.min(out.len()).min(self.bytes.len() - self.offset);
            out[..count].copy_from_slice(&self.bytes[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        }
    }
    #[test]
    fn streaming_digest_matches_empty_and_chunked_images() {
        assert_eq!(digest_reader(io::empty()).unwrap(), digest_hex(&[]));
        let bytes = (0..100_000).map(|x| (x % 251) as u8).collect::<Vec<_>>();
        assert_eq!(
            digest_reader(Chunked {
                bytes: bytes.clone(),
                offset: 0,
                interrupted: false
            })
            .unwrap(),
            digest_hex(&bytes)
        );
    }
    #[test]
    fn streaming_digest_preserves_reader_errors() {
        struct Failed;
        impl Read for Failed {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "image read denied",
                ))
            }
        }
        let error = digest_reader(Failed).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "image read denied");
    }
    #[test]
    fn profile_is_typed_and_does_not_change_compile_identity() {
        assert_eq!(MemoryProfile::default(), MemoryProfile::Default);
        assert_eq!(MemoryProfile::Default as u32, 0);
        assert_eq!(MemoryProfile::Compact as u32, 1);
        assert!(COMPILE_OPTIONS.contains("references=narrow"));
        assert!(COMPILE_OPTIONS.contains(";admit=FilesRunByHost;"));
        assert!(!COMPILE_OPTIONS.contains("Compact"));
    }
    #[test]
    fn invalid_ffi_profile_is_rejected_before_initialization_or_file_access() {
        let verdict = unsafe {
            rjs_aot_load_file_with_profile(std::ptr::null(), 0, std::ptr::null(), u32::MAX)
        };
        assert_eq!(verdict, 11); // pinned ImageVerdict::OptionsConflict
    }
    #[test]
    fn original_c_loader_symbol_remains_available() {
        unsafe extern "C" {
            fn rjs_aot_load_file(path: *const u16, length: usize, identity: *const u8) -> u32;
        }
        assert_eq!(
            unsafe { rjs_aot_load_file(std::ptr::null(), 0, std::ptr::null()) },
            6
        );
    }
    #[test]
    fn invalid_handles_and_profiles_are_rejected_before_initialization() {
        let identity = [0u8; 16];
        #[cfg(windows)] // Unix descriptor zero is a valid file (stdin).
        assert_eq!(
            unsafe { rjs_aot_load_handle_with_profile(0, identity.as_ptr(), 0) },
            17
        );
        #[cfg(unix)]
        assert_eq!(
            unsafe { rjs_aot_load_handle_with_profile(isize::MAX, identity.as_ptr(), 0) },
            17
        );
        assert_eq!(
            unsafe { rjs_aot_load_handle_with_profile(-1, identity.as_ptr(), 1) },
            17
        );
        assert_eq!(
            unsafe { rjs_aot_load_handle_with_profile(0, identity.as_ptr(), u32::MAX) },
            11
        );
        assert!(memory_settings().is_none());
    }
}
