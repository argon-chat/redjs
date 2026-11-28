//! Trusted, versioned JavaScript bytecode caches.
//!
//! JSC's bytecode decoder assumes compiler-produced input. The envelope detects
//! stale/corrupt local artifacts; its checksum is not authentication. Importing
//! downloaded or attacker-writable bytes remains unsafe even with a valid hash.

use core::ffi::c_void;
use sha2::{Digest, Sha256};

use crate::api::{Context, Error, Result, Value};
use crate::glue;

const MAGIC: &[u8; 8] = b"RJSBC001";
const HEADER_LENGTH: usize = 96;
const FORMAT_VERSION: u32 = 1;
const MAX_PAYLOAD_LENGTH: usize = 256 * 1024 * 1024;

/// Owned compiler-produced bytecode and its checked serialization envelope.
/// Keep the original source for evaluation, stack traces and Function#toString.
#[derive(Clone, Debug)]
pub struct Bytecode {
    envelope: Vec<u8>,
    key: [u8; 32],
}

impl Bytecode {
    /// A local artifact suitable for storage alongside the original source.
    pub fn as_bytes(&self) -> &[u8] {
        &self.envelope
    }

    pub fn cache_key(&self) -> [u8; 32] {
        self.key
    }

    /// Imports a trusted local artifact after checking its envelope and the key
    /// computed by [`Context::bytecode_cache_key`] for the intended source/URL.
    /// Returns `None` for stale, truncated, oversized or corrupted artifacts.
    ///
    /// # Safety
    /// The payload must originate from this embedding's JSC compiler and have
    /// remained within the application's trusted artifact/cache boundary. The
    /// checksum does not make arbitrary bytes safe for JSC's native decoder.
    pub unsafe fn from_trusted_bytes(bytes: &[u8], expected_key: [u8; 32]) -> Option<Self> {
        validate_envelope(bytes, expected_key)?;
        Some(Self {
            envelope: bytes.to_vec(),
            key: expected_key,
        })
    }

    fn payload(&self) -> &[u8] {
        &self.envelope[HEADER_LENGTH..]
    }

    fn from_generated(payload: &[u8], key: [u8; 32], depth: u32) -> Option<Self> {
        if payload.is_empty() || payload.len() > MAX_PAYLOAD_LENGTH {
            return None;
        }
        let mut envelope = vec![0; HEADER_LENGTH];
        envelope[..8].copy_from_slice(MAGIC);
        envelope[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        envelope[12..16].copy_from_slice(&depth.to_le_bytes());
        envelope[16..24].copy_from_slice(&(payload.len() as u64).to_le_bytes());
        envelope[24..56].copy_from_slice(&key);
        envelope.extend_from_slice(payload);
        let checksum = envelope_checksum(&envelope);
        envelope[56..88].copy_from_slice(&checksum);
        Some(Self { envelope, key })
    }
}

fn envelope_checksum(bytes: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(&bytes[..56]);
    hash.update(&bytes[88..]);
    hash.finalize().into()
}

fn validate_envelope(bytes: &[u8], expected_key: [u8; 32]) -> Option<()> {
    if bytes.len() < HEADER_LENGTH || bytes.get(..8)? != MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(bytes.get(8..12)?.try_into().ok()?);
    let payload_length = u64::from_le_bytes(bytes.get(16..24)?.try_into().ok()?);
    let payload_length = usize::try_from(payload_length).ok()?;
    if version != FORMAT_VERSION
        || payload_length == 0
        || payload_length > MAX_PAYLOAD_LENGTH
        || HEADER_LENGTH.checked_add(payload_length)? != bytes.len()
        || bytes.get(24..56)? != expected_key
        || bytes.get(88..96)? != [0; 8]
        || bytes.get(56..88)? != envelope_checksum(bytes)
    {
        return None;
    }
    Some(())
}

struct GeneratedPayload(*mut c_void);

impl Drop for GeneratedPayload {
    fn drop(&mut self) {
        unsafe { glue::rjs_bytecode_free(self.0) }
    }
}

impl Context {
    /// Exact source/URL, pinned engine/build, architecture, bytecode version and
    /// effective JSC options. Includes the source type to separate script and module bytecode.
    pub fn bytecode_cache_key(&self, source: &str, url: &str) -> [u8; 32] {
        self.source_cache_key(source, url, false)
    }

    pub fn module_bytecode_cache_key(&self, source: &str, url: &str) -> [u8; 32] {
        self.source_cache_key(source, url, true)
    }

    fn source_cache_key(&self, source: &str, url: &str, module: bool) -> [u8; 32] {
        let mut options = vec![0u8; 4096];
        loop {
            let required = unsafe {
                glue::rjs_bytecode_cache_options(self.raw(), options.as_mut_ptr(), options.len())
            };
            if required <= options.len() {
                options.truncate(required);
                break;
            }
            options.resize(required, 0);
        }
        let mut hash = Sha256::new();
        hash.update(if module {
            &b"redjs-module-bytecode-v1\0"[..]
        } else {
            &b"redjs-classic-bytecode-v1\0"[..]
        });
        hash.update(FORMAT_VERSION.to_le_bytes());
        hash.update(usize::BITS.to_le_bytes());
        hash.update(std::env::consts::ARCH.as_bytes());
        hash.update(std::env::consts::OS.as_bytes());
        hash.update(include_bytes!("../pins.txt"));
        hash.update(unsafe { glue::rjs_bytecode_cache_version() }.to_le_bytes());
        for bytes in [options.as_slice(), source.as_bytes(), url.as_bytes()] {
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        hash.finalize().into()
    }

    /// Compiles the program's top-level code without running any JavaScript.
    /// Nested functions keep normal lazy compilation, bounding cold-cache cost.
    pub fn compile_bytecode(&self, source: &str, url: &str) -> Result<Bytecode> {
        self.compile_bytecode_with_depth(source, url, 0)
    }

    /// Compiles the program and all nested functions. Intended for build-time
    /// generation, or explicit measurements; large bundles cost more to compile
    /// and store than a program-only cache.
    pub fn compile_bytecode_recursive(&self, source: &str, url: &str) -> Result<Bytecode> {
        self.compile_bytecode_with_depth(source, url, u32::MAX)
    }

    /// Compiles at most `depth` levels of nested function bodies. Zero caches
    /// the program only; one also caches an outer bundle IIFE. Functions beyond
    /// the bound remain lazy. `u32::MAX` requests recursive compilation.
    pub fn compile_bytecode_with_depth(
        &self,
        source: &str,
        url: &str,
        depth: u32,
    ) -> Result<Bytecode> {
        self.compile_source_bytecode(source, url, depth, false)
    }

    pub fn compile_module_bytecode(&self, source: &str, url: &str) -> Result<Bytecode> {
        self.compile_source_bytecode(source, url, 0, true)
    }

    fn compile_source_bytecode(
        &self,
        source: &str,
        url: &str,
        depth: u32,
        module: bool,
    ) -> Result<Bytecode> {
        let key = self.source_cache_key(source, url, module);
        let mut handle = core::ptr::null_mut();
        let mut bytes = core::ptr::null();
        let mut length = 0;
        let mut out = 0;
        let status = unsafe {
            glue::rjs_bytecode_generate_for_source(
                self.raw(),
                source.as_ptr(),
                source.len(),
                url.as_ptr(),
                url.len(),
                module.into(),
                depth,
                &mut handle,
                &mut bytes,
                &mut length,
                &mut out,
            )
        };
        let _owned = GeneratedPayload(handle);
        status_result(status, out)?;
        if bytes.is_null() || length == 0 || length > MAX_PAYLOAD_LENGTH {
            return Err(Error::BadArg);
        }
        let payload = unsafe { core::slice::from_raw_parts(bytes, length) };
        Bytecode::from_generated(payload, key, depth).ok_or(Error::BadArg)
    }

    /// Evaluates exactly once. An absent/mismatched cache falls back to source;
    /// a JS exception from evaluation is returned without executing it again.
    /// The boolean reports whether JSC accepted the cache's version/source key.
    /// JSC receives its own owned copy, so the cache can be dropped immediately
    /// after this call even when nested functions execute much later.
    pub fn eval_named_with_bytecode(
        &self,
        source: &str,
        url: &str,
        bytecode: Option<&Bytecode>,
    ) -> Result<(Value, bool)> {
        self.eval_source_with_bytecode(source, url, bytecode, false)
    }

    /// Evaluates a bundled ES module. The promise settles after its top-level await completes.
    pub fn eval_module(
        &self,
        source: &str,
        url: &str,
        bytecode: Option<&Bytecode>,
    ) -> Result<(Value, bool)> {
        self.eval_source_with_bytecode(source, url, bytecode, true)
    }

    fn eval_source_with_bytecode(
        &self,
        source: &str,
        url: &str,
        bytecode: Option<&Bytecode>,
        module: bool,
    ) -> Result<(Value, bool)> {
        let bytecode =
            bytecode.filter(|cache| cache.key == self.source_cache_key(source, url, module));
        let payload = bytecode.map_or(&[][..], Bytecode::payload);
        let mut out = 0;
        let mut used = 0;
        let status = unsafe {
            glue::rjs_eval8_cached_for_source(
                self.raw(),
                source.as_ptr(),
                source.len(),
                url.as_ptr(),
                url.len(),
                module.into(),
                payload.as_ptr(),
                payload.len(),
                &mut used,
                &mut out,
            )
        };
        status_result(status, out).map(|value| (value, used != 0))
    }
}

fn status_result(status: i32, out: u64) -> Result<Value> {
    match status {
        0 => Ok(Value(out)),
        1 => Err(Error::Exception(Value(out))),
        2 => Err(Error::BadArg),
        3 => Err(Error::Exhausted),
        code => Err(Error::Engine(code)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_await_resumes_once_with_and_without_cached_bytecode() {
        let url = "app://redjs/app.js";
        let source = "export const value = await new Promise(r => globalThis.resume = r); globalThis.result = value;";
        let compiler = Context::new();
        let cache = compiler.compile_module_bytecode(source, url).unwrap();
        assert_ne!(cache.cache_key(), compiler.bytecode_cache_key(source, url));
        // Darwin can share the VM across globals. Exercise the provider before a
        // source evaluation can populate that VM's in-memory code cache.
        for cached in [Some(&cache), None] {
            let ctx = Context::new();
            let (promise, used) = ctx.eval_module(source, url, cached).unwrap();
            assert_eq!(used, cached.is_some());
            ctx.protect(promise);
            ctx.drain_microtasks();
            assert_eq!(ctx.promise_info(promise).unwrap().0, 0);
            ctx.eval("resume(42)").unwrap();
            ctx.drain_microtasks();
            assert_eq!(ctx.promise_info(promise).unwrap().0, 1);
            assert_eq!(ctx.global_get("result").unwrap().as_number(), Some(42.0));
            ctx.unprotect(promise);
        }
    }

    #[test]
    fn registered_modules_link_live_bindings_and_return_the_namespace() {
        let ctx = Context::new();
        ctx.provide_module(
            "sk://dev/dep",
            "export let v=await Promise.resolve(40); export function inc(){v++}",
        )
        .unwrap();
        ctx.provide_module(
            "sk://dev/main",
            "import {v,inc} from 'sk://dev/dep'; inc(); globalThis.result=v; export {v};",
        )
        .unwrap();
        let promise = ctx.import_module("sk://dev/main").unwrap();
        ctx.protect(promise);
        ctx.drain_microtasks();
        let (state, value) = ctx.promise_info(promise).unwrap();
        assert_eq!(state, 1, "{:?}", ctx.to_rust_string(value));
        assert_eq!(ctx.global_get("result").unwrap().as_number(), Some(41.0));
        assert_eq!(
            ctx.get(value, &ctx.atom("v")).unwrap().as_number(),
            Some(41.0)
        );
        ctx.forget_module("sk://dev/main");
        ctx.unprotect(promise);
    }

    #[test]
    fn reloading_the_same_module_url_evaluates_the_new_source_once() {
        let ctx = Context::new();
        let url = "sk://app.prod.js";
        for version in [1, 2] {
            let code = format!("globalThis.runs = (globalThis.runs ?? 0) + 1; export const v = await Promise.resolve({version}); globalThis.v = v;");
            let (promise, _) = ctx.eval_module(&code, url, None).unwrap();
            ctx.protect(promise);
            ctx.drain_microtasks();
            assert_eq!(ctx.promise_info(promise).unwrap().0, 1);
            assert_eq!(
                ctx.global_get("runs").unwrap().as_number(),
                Some(version as f64)
            );
            assert_eq!(
                ctx.global_get("v").unwrap().as_number(),
                Some(version as f64)
            );
            ctx.unprotect(promise);
        }
    }

    // Envelope tests never give synthetic payloads to JSC's trusted decoder.
    #[test]
    fn envelope_detects_corruption_and_stale_keys() {
        let cache = Bytecode::from_generated(b"compiler-produced-placeholder", [7; 32], 0).unwrap();
        assert!(validate_envelope(cache.as_bytes(), [7; 32]).is_some());
        assert!(validate_envelope(cache.as_bytes(), [8; 32]).is_none());
        for offset in 0..cache.as_bytes().len() {
            let mut corrupted = cache.as_bytes().to_vec();
            corrupted[offset] ^= 1;
            assert!(
                validate_envelope(&corrupted, [7; 32]).is_none(),
                "offset {offset}"
            );
        }
        for length in 0..cache.as_bytes().len() {
            assert!(validate_envelope(&cache.as_bytes()[..length], [7; 32]).is_none());
        }
        let mut extended = cache.as_bytes().to_vec();
        extended.push(0);
        assert!(validate_envelope(&extended, [7; 32]).is_none());
    }

    #[test]
    fn envelope_rejects_oversized_length_without_allocating() {
        let cache = Bytecode::from_generated(b"x", [7; 32], 0).unwrap();
        let mut bytes = cache.as_bytes().to_vec();
        bytes[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(validate_envelope(&bytes, [7; 32]).is_none());
    }
}
