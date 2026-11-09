//! Native `crypto`: getRandomValues over the OS CSPRNG (`getrandom` →
//! BCryptGenRandom on Windows — actually secure, unlike the old Math.random
//! JS stub), spec-shaped randomUUID (v4), and `crypto.subtle.digest`
//! (SHA-1/256/384/512 via RustCrypto) for asset hashing / cache keys.

use core::ffi::c_void;

use sha1::Digest;

use super::util::*;
use crate::glue;

// Private slot bags carry their receiver, so Object.create(crypto) cannot
// inherit a brand and a Proxy around it cannot substitute its own identity.
unsafe fn crypto_bag(g: *mut c_void, receiver: u64) -> Option<u64> {
    if !is_cell(receiver) {
        return None;
    }
    let bag = get_a(g, receiver, state(g).s_crypto);
    (is_cell(bag) && get(g, bag, "receiver") == receiver).then_some(bag)
}
unsafe fn is_subtle(g: *mut c_void, receiver: u64) -> bool {
    is_cell(receiver) && get_a(g, receiver, state(g).s_subtle_crypto) == receiver
}
#[crate::host_fn]
unsafe fn get_subtle(g: *mut c_void, frame: *mut c_void) -> u64 {
    let Some(bag) = crypto_bag(g, cf_this(frame)) else {
        return throw_type_error(g, "Illegal invocation");
    };
    get(g, bag, "subtle")
}
#[crate::host_fn]
unsafe fn get_random_values(g: *mut c_void, cf: *mut c_void) -> u64 {
    if crypto_bag(g, cf_this(cf)).is_none() {
        return throw_type_error(g, "Illegal invocation");
    }
    let view = cf_arg(cf, 0);
    let mut ta = crate::ffi::RjsTaInfo::default();
    if glue::rjs_typedarray_info(g, view, &mut ta) != 0 {
        return throw_type_error(
            g,
            "crypto.getRandomValues: argument must be an integer typed array",
        );
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
        glue::rjs_throw(
            g,
            super::domexception::create_quota_exceeded(
                g,
                &msg,
                Some(65536.0),
                Some(ta.byte_length as f64),
            ),
        );
        return UNDEF;
    }
    if ta.detached == 0 && !ta.data.is_null() && ta.byte_length > 0 {
        let buf = core::slice::from_raw_parts_mut(ta.data as *mut u8, ta.byte_length as usize);
        let _ = getrandom::fill(buf);
    }
    view // spec: returns the same array
}

#[crate::host_fn]
unsafe fn random_uuid(g: *mut c_void, cf: *mut c_void) -> u64 {
    if crypto_bag(g, cf_this(cf)).is_none() {
        return throw_type_error(g, "Illegal invocation");
    }
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
#[crate::host_fn]
unsafe fn subtle_digest(g: *mut c_void, cf: *mut c_void) -> u64 {
    if !is_subtle(g, cf_this(cf)) {
        return throw_type_error(g, "Illegal invocation");
    }
    let alg_v = cf_arg(cf, 0);
    // Spec: AlgorithmIdentifier = name string | { name } dictionary.
    // (read_string ToString-coerces objects, so check the kind explicitly.)
    let alg_v = if glue::rjs_kind(g, alg_v) == 8 {
        get(g, alg_v, "name")
    } else {
        alg_v
    };
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
    let subtle_class = define_class(
        g,
        &ClassSpec {
            name: "SubtleCrypto",
            arity: 0,
            construct: None,
            parent_proto: 0,
            methods: &[("digest", 2, subtle_digest)],
            getters: &[],
            accessors: &[],
        },
    );
    let crypto_class = define_class(
        g,
        &ClassSpec {
            name: "Crypto",
            arity: 0,
            construct: None,
            parent_proto: 0,
            methods: &[
                ("getRandomValues", 1, get_random_values),
                ("randomUUID", 0, random_uuid),
            ],
            getters: &[("subtle", get_subtle)],
            accessors: &[],
        },
    );
    let subtle = new_object(g, subtle_class.proto);
    glue::rjs_define(g, subtle, state(g).s_subtle_crypto, subtle, 0);
    let crypto = new_object(g, crypto_class.proto);
    let bag = new_object(g, UNDEF);
    define(g, bag, "receiver", crypto, 0);
    define(g, bag, "subtle", subtle, 0);
    glue::rjs_define(g, crypto, state(g).s_crypto, bag, 0);
    define(g, glue::rjs_global(g), "crypto", crypto, 2);
}

#[cfg(test)]
mod tests {
    use super::super::testing::{ctx, ok};
    #[test]
    fn crypto_singletons_have_native_interfaces_and_keep_their_operations() {
        let context = ctx();
        ok(&context, "crypto instanceof Crypto && crypto.subtle instanceof SubtleCrypto && Object.getPrototypeOf(crypto) === Crypto.prototype && Object.getPrototypeOf(crypto.subtle) === SubtleCrypto.prototype");
        ok(&context, "Object.prototype.toString.call(crypto) === '[object Crypto]' && Object.prototype.toString.call(crypto.subtle) === '[object SubtleCrypto]'");
        ok(&context, "(() => { const a = new Uint8Array(16); return crypto.getRandomValues(a) === a && /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(crypto.randomUUID()) && crypto.subtle.digest('SHA-256', a) instanceof Promise; })()");
    }
    #[test]
    fn native_digest_keeps_its_known_sha256_bytes() {
        let context = ctx();
        context.eval("crypto.subtle.digest('SHA-256', new Uint8Array([1,2,3])).then(bytes => { globalThis.__digestHex = Array.from(new Uint8Array(bytes), byte => byte.toString(16).padStart(2, '0')).join(''); })").unwrap();
        context.drain_microtasks();
        assert_eq!(context.to_rust_string(context.global_get("__digestHex").unwrap()).unwrap(), "039058c6f2c0cb492c533b0a4d14ef77cc0f78abccced5287d84a1a2011cfb81");
    }

    #[test]
    fn crypto_constructors_and_forged_receivers_are_rejected() {
        let context = ctx();
        ok(&context, "(() => { const attempts = [() => Crypto(), () => new Crypto(), () => SubtleCrypto(), () => new SubtleCrypto(), () => Crypto.prototype.randomUUID.call({}), () => Crypto.prototype.randomUUID.call(Object.create(crypto)), () => Crypto.prototype.getRandomValues.call(Object.create(Crypto.prototype), new Uint8Array(1)), () => Object.getOwnPropertyDescriptor(Crypto.prototype, 'subtle').get.call({}), () => SubtleCrypto.prototype.digest.call(Object.create(crypto.subtle), 'SHA-256', new Uint8Array(1))]; return attempts.every(f => { try { f(); return false; } catch (e) { return e instanceof TypeError; } }); })()");
    }
}
