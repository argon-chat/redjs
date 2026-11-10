//! Context identity: a monotonic id per context, and the one teardown fan-out.
//!
//! # The bug this exists to close
//!
//! Every per-context registry in redjs is keyed by the context's ADDRESS — the
//! `JSGlobalObject*` — because that is what every host function receives. An
//! address is a fine key while the context lives, and a wrong one after: when a
//! context is released, its address can be handed to the next context, which
//! then *inherits* the dead one's entries. Three concrete ways that went wrong:
//!
//! - `webapi::install` treats "a `WebState` exists for this address" as "already
//!   installed" and returns early — so a new context at a recycled address got
//!   **no web API at all**, silently;
//! - `errors::install_rejection` likewise skipped arming the rejection hook;
//! - the scheduler's timers/rAF lists for the dead context — protected values in
//!   a heap that no longer exists — would be fired into the new one.
//!
//! Two mechanisms, both here:
//!
//! 1. **A [`ContextId`]**: a `u64` minted from one process-wide counter, never 0
//!    and never reused. It is what anything that outlives a single call keys on
//!    when the delivery can arrive *later* — the permission-change fan-out today,
//!    IndexedDB's per-client inboxes and Workers' agents next. A late delivery
//!    for a dead id is dropped; it can never land in a newcomer.
//! 2. **[`purge`]**: one explicit list of every address-keyed registry, cleared
//!    for an address the moment that address stops meaning the context it was
//!    keyed for. It runs from [`forget`] (`redjs_context_release`) and from
//!    [`register`] / [`ensure`] when they find a registration for an address that
//!    belongs to someone else.
//!
//! # How a recycled address is recognised
//!
//! `redjs_context_create` calls [`register`]: an address that is already in the
//! table was released behind our back, so its old state is purged before the new
//! id is minted. An embedder that creates contexts with the raw JSC C API instead
//! is covered too: every install entry point calls [`ensure`], which checks a
//! **mark** — the id, stored on the global object under a private name minted for
//! that context. A different global at the same address does not carry the mark
//! (a private name is a fresh symbol per context; no script can read or forge it),
//! so a mismatch means "same address, different context" and purges.
//!
//! What a purge does NOT do: unprotect values. The registries hold JSC values
//! protected in the context they were keyed for; by the time a purge runs that
//! heap is either gone (release) or was never ours (recycling), so the only
//! correct thing to do with those `u64`s is to forget them.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::glue;

/// A context's identity for the lifetime of the process: minted once, never 0,
/// never handed to another context — unlike the address.
///
/// This is also the id Workers' design calls an `AgentId` (m25 workers-design
/// §2.4 C1): a worker agent IS a context, so the same counter serves both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContextId(pub u64);

/// What a registry is told when an address stops meaning its context.
#[derive(Clone, Copy, Debug)]
pub struct Gone {
    /// The address the registries keyed (`g as usize`).
    pub key: usize,
    /// The id that address WAS — for registries keyed by [`ContextId`].
    pub id: ContextId,
}

static NEXT: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct Entry {
    id: u64,
    /// Private-name atom the id is stored under on this context's global.
    mark: usize,
}

fn table() -> &'static Mutex<HashMap<usize, Entry>> {
    static T: OnceLock<Mutex<HashMap<usize, Entry>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

const ENC_UNDEFINED: u64 = 0xa;
const TAG_NUMBER: u64 = 0xfffe_0000_0000_0000;

fn enc_id(id: u64) -> u64 {
    // ids stay far below 2^53, so the double is exact.
    ((id as f64).to_bits() as i64).wrapping_add(1i64 << 49) as u64
}

fn dec_number(v: u64) -> Option<f64> {
    if (v & TAG_NUMBER) == TAG_NUMBER {
        return Some(v as u32 as i32 as f64);
    }
    if (v & TAG_NUMBER) != 0 {
        return Some(f64::from_bits((v as i64).wrapping_sub(1i64 << 49) as u64));
    }
    None
}

/// Mints a fresh id for `g` and marks its global with it.
unsafe fn adopt(g: *mut c_void) -> ContextId {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let name = "redjs#contextId";
    let mark = glue::rjs_atom_private(g, name.as_ptr(), name.len());
    // ReadOnly | DontEnum | DontDelete — and a private name on top, so neither
    // enumeration nor any script can see it.
    glue::rjs_define(g, glue::rjs_global(g), mark, enc_id(id), 1 | 2 | 4);
    table().lock().unwrap().insert(g as usize, Entry { id, mark: mark as usize });
    ContextId(id)
}

unsafe fn mark_matches(g: *mut c_void, e: Entry) -> bool {
    let mut out = ENC_UNDEFINED;
    if glue::rjs_get(g, glue::rjs_global(g), e.mark as *mut c_void, &mut out) != 0 {
        return false;
    }
    dec_number(out) == Some(e.id as f64)
}

/// Registers a context that was JUST created. If the address is already in the
/// table, the context it belonged to was released without telling us: its
/// state is purged first, so nothing of it leaks into this one.
///
/// # Safety
/// `g` must be a live context on the calling thread.
pub unsafe fn register(g: *mut c_void) -> ContextId {
    let stale = table().lock().unwrap().remove(&(g as usize));
    if let Some(old) = stale {
        purge(Gone { key: g as usize, id: ContextId(old.id) });
    }
    adopt(g)
}

/// The id of `g`, registering it on first sight — and, if the address is known
/// but the global does not carry that registration's mark, treating it as the
/// recycled address it is (purge, then a fresh id). Every install entry point
/// calls this first, so a context created with the raw C API is covered too.
///
/// # Safety
/// `g` must be a live context on the calling thread.
pub unsafe fn ensure(g: *mut c_void) -> ContextId {
    let e = table().lock().unwrap().get(&(g as usize)).copied();
    match e {
        Some(e) if mark_matches(g, e) => ContextId(e.id),
        Some(e) => {
            table().lock().unwrap().remove(&(g as usize));
            purge(Gone { key: g as usize, id: ContextId(e.id) });
            adopt(g)
        }
        None => adopt(g),
    }
}

/// The id of a registered context. A plain table read — no JS — so it is safe
/// on any hot path; `None` for an address nobody registered.
pub fn id_of(g: *mut c_void) -> Option<ContextId> {
    table().lock().unwrap().get(&(g as usize)).map(|e| ContextId(e.id))
}

/// `g` is going away (`redjs_context_release`): forget everything keyed by it.
pub fn forget(g: *mut c_void) {
    let e = table().lock().unwrap().remove(&(g as usize));
    if let Some(e) = e {
        purge(Gone { key: g as usize, id: ContextId(e.id) });
    }
}

/// THE fan-out: every registry in redjs that is keyed by a context, in one
/// list, so "did we miss one?" is answered by reading one function. A module
/// that adds an address-keyed registry adds its `forget` here.
fn purge(gone: Gone) {
    crate::errors::forget(gone);
    crate::web::forget(gone);
    crate::webapi::forget(gone);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Context;

    #[test]
    fn ids_are_monotonic_and_never_reused() {
        let a = Context::new();
        let b = Context::new();
        let ia = id_of(a.raw()).expect("redjs_context_create registers");
        let ib = id_of(b.raw()).expect("redjs_context_create registers");
        assert_ne!(ia, ib);
        assert!(ia.0 >= 1 && ib.0 >= 1, "0 is never an id");
        // `ensure` on a live, marked context is the identity.
        assert_eq!(unsafe { ensure(a.raw()) }, ia);
        assert_eq!(unsafe { ensure(b.raw()) }, ib);
    }

    /// The latent bug itself: a context that arrives at an address whose old
    /// registrations survived. `register` is exactly what
    /// `redjs_context_create` calls for the newcomer, so calling it again on a
    /// live address is the recycled-address case with the release step elided
    /// (the release itself fastfails in this prebuilt — README "Known issues").
    #[test]
    fn a_recycled_address_starts_from_nothing() {
        let c = Context::new();
        let g = c.raw();
        unsafe {
            crate::ffi::redjs_web_install(g);
            crate::ffi::redjs_webshim_install(g, core::ptr::null(), 0, core::ptr::null(), 0);
        }
        let before = id_of(g).unwrap();
        assert!(crate::webapi::util::has_state(g), "installed");
        assert!(crate::errors::rejection_armed(g));
        assert!(crate::web::has_scheduler(g));

        let after = unsafe { register(g) };
        assert_ne!(after, before, "a newcomer never inherits the old id");
        assert!(after.0 > before.0, "ids only grow");
        // Every address-keyed registry forgot the old context — so the next
        // `webapi::install` RUNS instead of returning early on a stale entry.
        assert!(!crate::webapi::util::has_state(g), "stale WebState must be gone");
        assert!(!crate::errors::rejection_armed(g), "stale rejection registration must be gone");
        assert!(!crate::web::has_scheduler(g), "stale timers/rAF must be gone");
    }

    /// The raw-C-API path: same address, a global that does not carry this
    /// registration's mark. Simulated by deleting the mark — the observable
    /// difference between "my context" and "a different context here".
    #[test]
    fn a_global_without_the_mark_is_a_different_context() {
        let c = Context::new();
        let g = c.raw();
        let first = unsafe { ensure(g) };
        let mark = table().lock().unwrap().get(&(g as usize)).unwrap().mark;
        unsafe {
            let e = Entry { id: first.0, mark };
            assert!(mark_matches(g, e));
            // No script can reach a private name; `rjs_define` is `putDirect`,
            // which overwrites whatever the attributes say.
            glue::rjs_define(g, glue::rjs_global(g), mark as *mut c_void, enc_id(first.0 + 1_000_000), 0);
            assert!(!mark_matches(g, e));
            let second = ensure(g);
            assert_ne!(second, first);
            assert_eq!(ensure(g), second, "stable once re-marked");
        }
    }
}
