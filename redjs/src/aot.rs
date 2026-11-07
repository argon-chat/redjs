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
pub const COMPILE_OPTIONS: &str = concat!(
    "x86_64;references=narrow;image=pic;threads=1;default-options@",
    env!("REDJS_AOT_ENGINE_VERSION")
);
pub fn digest_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
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
    fn rjs_aot_load_file(path: *const u16, length: usize, identity: *const u8) -> u32;
    fn rjs_aot_create_context() -> *mut c_void;
    fn rjs_aot_run(context: *mut c_void, module: u32, result: *mut u64) -> i32;
    fn rjs_aot_is_active() -> i32;
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
    hash.update(b"vue-native-aot-image-v1\0");
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
    anchor_icu();
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    let verdict = unsafe { rjs_aot_load_file(wide.as_ptr(), wide.len(), identity.as_ptr()) };
    if verdict == 1 {
        Ok(())
    } else {
        Err(format!("native AOT image rejected (verdict {verdict})"))
    }
}

/// The native backend is selected on this VM thread.
pub fn is_active() -> bool {
    unsafe { rjs_aot_is_active() != 0 }
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
