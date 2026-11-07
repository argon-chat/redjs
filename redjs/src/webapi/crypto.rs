//! Native `crypto`: getRandomValues over the OS CSPRNG (`getrandom` →
//! BCryptGenRandom on Windows — actually secure, unlike the old Math.random
//! JS stub), spec-shaped randomUUID (v4), and `crypto.subtle.digest`
//! (SHA-1/256/384/512 via RustCrypto) for asset hashing / cache keys.

use core::ffi::c_void;

use sha1::Digest;

use super::util::*;
use crate::glue;

unsafe extern "sysv64" fn get_random_values(g: *mut c_void, cf: *mut c_void) -> u64 {
    let view = cf_arg(cf, 0);
    let mut ta = crate::ffi::RjsTaInfo::default();
    if glue::rjs_typedarray_info(g, view, &mut ta) != 0 {
        return throw_type_error(g, "crypto.getRandomValues: argument must be an integer typed array");
    }
    // Spec: integer-typed arrays only — a Float16/32/64Array or DataView is
    // "a TypeMismatchError DOMException" (a non-view is the IDL TypeError
    // above) — and at most 65536 bytes, past which "a QuotaExceededError".
    if matches!(ta.kind, 8 | 9 | 12 | 13) {
        return super::domexception::throw(
            g,
            "TypeMismatchError",
            "Failed to execute 'getRandomValues' on 'Crypto': The provided ArrayBufferView is of type \
             float or a DataView, which is not an integer array type.",
        );
    }
    if ta.byte_length > 65536 {
        let msg = format!(
            "Failed to execute 'getRandomValues' on 'Crypto': The ArrayBufferView's byte length ({}) \
             exceeds the number of bytes of entropy available via this API (65536).",
            ta.byte_length
        );
        glue::rjs_throw(g, super::domexception::create_quota_exceeded(g, &msg, Some(65536.0), Some(ta.byte_length as f64)));
        return UNDEF;
    }
    if ta.detached == 0 && !ta.data.is_null() && ta.byte_length > 0 {
        let buf = core::slice::from_raw_parts_mut(ta.data as *mut u8, ta.byte_length as usize);
        let _ = getrandom::fill(buf);
    }
    view // spec: returns the same array
}

unsafe extern "sysv64" fn random_uuid(g: *mut c_void, _cf: *mut c_void) -> u64 {
    let mut b = [0u8; 16];
    let _ = getrandom::fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10
    let s = format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    );
    js_string(g, &s)
}

/// crypto.subtle.digest(algorithm, data) → Promise<ArrayBuffer>.
/// `algorithm` is a name string or `{ name }` per spec.
unsafe extern "sysv64" fn subtle_digest(g: *mut c_void, cf: *mut c_void) -> u64 {
    let alg_v = cf_arg(cf, 0);
    // Spec: AlgorithmIdentifier = name string | { name } dictionary.
    // (read_string ToString-coerces objects, so check the kind explicitly.)
    let alg_v = if glue::rjs_kind(g, alg_v) == 8 { get(g, alg_v, "name") } else { alg_v };
    let alg = read_string(g, alg_v).unwrap_or_default();
    let Some(bytes) = bufferish(g, cf_arg(cf, 1)) else {
        return rejected_promise(g, 1, "crypto.subtle.digest: data must be a BufferSource");
    };
    let hash: Vec<u8> = match alg.trim().to_ascii_uppercase().as_str() {
        "SHA-1" => sha1::Sha1::digest(bytes).to_vec(),
        "SHA-256" => sha2::Sha256::digest(bytes).to_vec(),
        "SHA-384" => sha2::Sha384::digest(bytes).to_vec(),
        "SHA-512" => sha2::Sha512::digest(bytes).to_vec(),
        other => {
            return super::domexception::rejected(
                g,
                "NotSupportedError",
                &format!("crypto.subtle.digest: unsupported algorithm \"{other}\""),
            )
        }
    };
    // Spec returns an ArrayBuffer; build it as a fresh Uint8Array's buffer.
    let u8v = make_u8(g, &hash);
    resolved_promise(g, get(g, u8v, "buffer"))
}

pub unsafe fn install(g: *mut c_void) {
    let global = glue::rjs_global(g);
    let crypto = new_object(g, UNDEF);

    let def_fn = |target: u64, name: &str, arity: u32, f: HostFn| {
        let n16: Vec<u16> = name.encode_utf16().collect();
        let mut func = UNDEF;
        glue::rjs_function_create(g, n16.as_ptr(), n16.len(), arity, f as *const c_void, &mut func);
        define(g, target, name, func, 2);
    };
    def_fn(crypto, "getRandomValues", 1, get_random_values);
    def_fn(crypto, "randomUUID", 0, random_uuid);

    let subtle = new_object(g, UNDEF);
    def_fn(subtle, "digest", 2, subtle_digest);
    define(g, crypto, "subtle", subtle, 1 | 2);

    define(g, global, "crypto", crypto, 2);
}
