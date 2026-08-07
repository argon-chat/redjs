//! The WHATWG Streams queueing and backpressure state machine, in Rust
//! (`RedReadableCore` / `RedWritableCore`), plus the queue-with-sizes it rests
//! on (Streams Standard §8.1).
//!
//! # The split, and why it is where it is
//!
//! Streams is the one API on the list where the division is not obvious, so
//! here is the reasoning and the cost.
//!
//! The user-facing protocol is promises over **user callbacks**: `pull`,
//! `write`, `transform` and `size` are functions the author supplies, and every
//! method hands back a promise. None of that can leave JS — a promise is a JS
//! object, and a chunk is an arbitrary JS value.
//!
//! What *can* leave JS, and does here, is everything that decides: the queue
//! and its running total, the readable/writable state machines, the
//! close/error/in-flight bookkeeping, and the backpressure computation. Those
//! are the parts with genuinely tricky invariants — and they are testable
//! without a JS context at all, which the tests below do.
//!
//! **The cost.** The obvious objection is per-chunk boundary crossings, which
//! this repo has measured as the dominant cost elsewhere. That measurement is
//! from the UI layout seam, which goes JS↔wasm with allocation on both sides.
//! It does not apply here: these are `extern "sysv64"` host functions called
//! directly by JSC, ~2 ns each (`src/web.rs`), so a chunk costs a handful of
//! nanoseconds of decision-making. Chunk VALUES never cross at all — they stay
//! in a JS array held by a private slot on the controller, so there is no
//! marshalling and no GC protect/unprotect per chunk either.
//!
//! **What was NOT moved, and why.** The promise sequencing itself stays in JS
//! (`src/streams.js`). Doing it from Rust means creating promises, storing
//! resolve/reject pairs, and attaching reaction handlers through JSC by hand
//! for `pipeTo`, `tee` and the writable in-flight-request dance. That is several
//! times more unsafe Rust for identical observable behaviour, in the part of the
//! repo the ground rules single out as hard to verify. The state that can be
//! got wrong silently is in Rust; the plumbing that fails loudly if it is wrong
//! is in JS.

use core::ffi::c_void;
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

use super::util::*;
use crate::capi;

// ---------------------------------------------------------------------------
// Queue with sizes (spec §8.1)
// ---------------------------------------------------------------------------

/// Only the SIZES live here. The chunk values stay in a JS array on the
/// controller, so a chunk never crosses the boundary and never needs a GC
/// protect — the queue and the values are kept in lockstep by index.
#[derive(Default, Debug)]
pub struct SizeQueue {
    sizes: VecDeque<f64>,
    total: f64,
}

#[derive(Debug, PartialEq)]
pub enum EnqueueError {
    /// Spec §8.1 EnqueueValueWithSize: "If size is not a finite, non-negative
    /// number, throw a RangeError".
    BadSize,
}

impl SizeQueue {
    pub fn len(&self) -> usize {
        self.sizes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.sizes.is_empty()
    }
    pub fn total(&self) -> f64 {
        self.total
    }

    pub fn enqueue(&mut self, size: f64) -> Result<(), EnqueueError> {
        if !size.is_finite() || size < 0.0 {
            return Err(EnqueueError::BadSize);
        }
        self.sizes.push_back(size);
        self.total += size;
        Ok(())
    }

    /// Spec §8.1 DequeueValue: the total is FORCED to 0 when the queue empties.
    /// Without that, repeated enqueue/dequeue of fractional sizes leaves a
    /// residue that drifts the desired size away from the high-water mark
    /// forever — a stream that quietly stops asking for data.
    pub fn dequeue(&mut self) -> bool {
        match self.sizes.pop_front() {
            Some(size) => {
                self.total -= size;
                if self.sizes.is_empty() || self.total < 0.0 {
                    self.total = 0.0;
                }
                true
            }
            None => false,
        }
    }

    pub fn peek(&self) -> Option<f64> {
        self.sizes.front().copied()
    }

    pub fn reset(&mut self) {
        self.sizes.clear();
        self.total = 0.0;
    }
}

// ---------------------------------------------------------------------------
// ReadableStream
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ReadableState {
    Readable = 0,
    Closed = 1,
    Errored = 2,
}

#[derive(Debug)]
pub struct ReadableCore {
    pub state: ReadableState,
    pub queue: SizeQueue,
    pub hwm: f64,
    pub started: bool,
    pub close_requested: bool,
    pub pulling: bool,
    pub pull_again: bool,
    pub disturbed: bool,
    pub locked: bool,
}

impl ReadableCore {
    pub fn new(hwm: f64) -> ReadableCore {
        ReadableCore {
            state: ReadableState::Readable,
            queue: SizeQueue::default(),
            hwm,
            started: false,
            close_requested: false,
            pulling: false,
            pull_again: false,
            disturbed: false,
            locked: false,
        }
    }

    /// Spec: ReadableStreamDefaultControllerCanCloseOrEnqueue.
    pub fn can_close_or_enqueue(&self) -> bool {
        !self.close_requested && self.state == ReadableState::Readable
    }

    /// Spec: ReadableStreamDefaultControllerGetDesiredSize. `None` is the
    /// spec's `null` (errored); a closed stream is 0, NOT the high-water mark.
    pub fn desired_size(&self) -> Option<f64> {
        match self.state {
            ReadableState::Errored => None,
            ReadableState::Closed => Some(0.0),
            ReadableState::Readable => Some(self.hwm - self.queue.total()),
        }
    }

    /// Spec: ReadableStreamDefaultControllerShouldCallPull.
    ///
    /// `read_requests` is the number of unfulfilled `read()` promises. The
    /// clause that needs it is the one that keeps a zero-high-water-mark stream
    /// alive: such a stream has `desiredSize == 0` and would never pull again,
    /// except that a waiting reader makes it pull exactly one chunk.
    pub fn should_call_pull(&self, read_requests: usize) -> bool {
        if !self.can_close_or_enqueue() {
            return false;
        }
        if !self.started {
            return false;
        }
        if self.locked && read_requests > 0 {
            return true;
        }
        matches!(self.desired_size(), Some(d) if d > 0.0)
    }

    /// Begin a pull if one is warranted. `false` means "already pulling" — the
    /// re-entrancy guard, which also records that another pull is owed.
    pub fn begin_pull(&mut self, read_requests: usize) -> bool {
        if !self.should_call_pull(read_requests) {
            return false;
        }
        if self.pulling {
            self.pull_again = true;
            return false;
        }
        self.pulling = true;
        true
    }

    /// A pull's promise settled. `true` means pull again immediately — a chunk
    /// was requested while the previous pull was in flight, and dropping it is
    /// a stream that stalls with a reader waiting.
    pub fn end_pull(&mut self, read_requests: usize) -> bool {
        self.pulling = false;
        if self.pull_again {
            self.pull_again = false;
            return self.begin_pull(read_requests);
        }
        false
    }

    pub fn enqueue(&mut self, size: f64) -> Result<(), EnqueueError> {
        self.queue.enqueue(size)
    }

    pub fn dequeue(&mut self) -> bool {
        self.queue.dequeue()
    }

    /// Spec: ReadableStreamDefaultControllerClose. `true` means the stream
    /// transitions to closed NOW; `false` means the queue still has chunks and
    /// the close happens when it drains.
    pub fn request_close(&mut self) -> bool {
        self.close_requested = true;
        self.queue.is_empty()
    }

    /// A chunk was taken; `true` when that was the last one of a close-requested
    /// stream, which is the deferred close finally landing.
    pub fn close_if_drained(&mut self) -> bool {
        self.close_requested && self.queue.is_empty()
    }
}

// ---------------------------------------------------------------------------
// WritableStream
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WritableState {
    Writable = 0,
    Erroring = 1,
    Errored = 2,
    Closed = 3,
}

#[derive(Debug)]
pub struct WritableCore {
    pub state: WritableState,
    pub queue: SizeQueue,
    pub hwm: f64,
    pub started: bool,
    pub backpressure: bool,
    pub locked: bool,
    pub in_flight_write: bool,
    pub in_flight_close: bool,
    pub close_requested: bool,
}

impl WritableCore {
    pub fn new(hwm: f64) -> WritableCore {
        WritableCore {
            state: WritableState::Writable,
            queue: SizeQueue::default(),
            hwm,
            started: false,
            // Spec: a writable stream starts with backpressure applied until
            // `start` settles, so a writer's `ready` is pending from the outset.
            backpressure: true,
            locked: false,
            in_flight_write: false,
            in_flight_close: false,
            close_requested: false,
        }
    }

    /// Spec: WritableStreamDefaultControllerGetDesiredSize.
    pub fn desired_size(&self) -> f64 {
        self.hwm - self.queue.total()
    }

    /// Spec: WritableStreamDefaultControllerGetBackpressure.
    fn compute_backpressure(&self) -> bool {
        if !self.started {
            return true;
        }
        self.desired_size() <= 0.0
    }

    /// Recompute; `true` when the value CHANGED, which is the only time the
    /// writer's `ready` promise has to be replaced or resolved. Returning it
    /// unconditionally would replace `ready` on every write and strand whoever
    /// was awaiting the old one.
    pub fn update_backpressure(&mut self) -> bool {
        let next = self.compute_backpressure();
        if next == self.backpressure {
            return false;
        }
        self.backpressure = next;
        true
    }

    pub fn enqueue(&mut self, size: f64) -> Result<(), EnqueueError> {
        self.queue.enqueue(size)
    }

    pub fn dequeue(&mut self) -> bool {
        self.queue.dequeue()
    }
}

// ---------------------------------------------------------------------------
// The JS-visible cores
// ---------------------------------------------------------------------------

struct StreamClasses {
    readable_cls: capi::JSClassRef,
    readable: Class,
    writable_cls: capi::JSClassRef,
    writable: Class,
}
unsafe impl Send for StreamClasses {}
unsafe impl Sync for StreamClasses {}

fn registry() -> &'static Mutex<HashMap<usize, &'static StreamClasses>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static StreamClasses>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> &'static StreamClasses {
    registry().lock().unwrap().get(&(g as usize)).copied().expect("webapi::streams not installed")
}

unsafe fn this_readable<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut ReadableCore> {
    host_data::<ReadableCore>(g, classes(g).readable_cls, cf_this(cf))
}
unsafe fn this_writable<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut WritableCore> {
    host_data::<WritableCore>(g, classes(g).writable_cls, cf_this(cf))
}

unsafe fn proto_for(g: *mut c_void, cf: *mut c_void, fallback: u64) -> u64 {
    let nt = cf_new_target(cf);
    let p = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
    if is_cell(p) { p } else { fallback }
}

unsafe extern "sysv64" fn readable_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let hwm = arg_num(g, cf, 0, 1.0);
    let proto = proto_for(g, cf, c.readable.proto);
    host_create(g, c.readable_cls, proto, ReadableCore::new(hwm))
}

unsafe extern "sysv64" fn writable_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let hwm = arg_num(g, cf, 0, 1.0);
    let proto = proto_for(g, cf, c.writable.proto);
    host_create(g, c.writable_cls, proto, WritableCore::new(hwm))
}

/// Boolean field accessors, generated so a getter cannot exist without its
/// setter reaching the same field.
macro_rules! bool_field {
    ($core:ident, $this:path, $($get:ident, $set:ident, $field:ident;)*) => {
        $(
            unsafe extern "sysv64" fn $get(g: *mut c_void, cf: *mut c_void) -> u64 {
                match $this(g, cf) {
                    Some(d) => enc_bool(d.$field),
                    None => throw_type_error(g, concat!(stringify!($core), " getter on wrong this")),
                }
            }
            unsafe extern "sysv64" fn $set(g: *mut c_void, cf: *mut c_void) -> u64 {
                let v = truthy_flag(cf_arg(cf, 0));
                match $this(g, cf) {
                    Some(d) => {
                        d.$field = v;
                        UNDEF
                    }
                    None => throw_type_error(g, concat!(stringify!($core), " setter on wrong this")),
                }
            }
        )*
    };
}

bool_field!(ReadableCore, this_readable,
    r_get_started, r_set_started, started;
    r_get_locked, r_set_locked, locked;
    r_get_disturbed, r_set_disturbed, disturbed;
);

bool_field!(WritableCore, this_writable,
    w_get_started, w_set_started, started;
    w_get_locked, w_set_locked, locked;
    w_get_in_flight_write, w_set_in_flight_write, in_flight_write;
    w_get_in_flight_close, w_set_in_flight_close, in_flight_close;
    w_get_close_requested, w_set_close_requested, close_requested;
);

unsafe extern "sysv64" fn r_get_state(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_readable(g, cf) {
        Some(d) => enc_i32(d.state as i32),
        None => throw_type_error(g, "ReadableCore.state on wrong this"),
    }
}

unsafe extern "sysv64" fn r_set_state(g: *mut c_void, cf: *mut c_void) -> u64 {
    let v = arg_num(g, cf, 0, 0.0) as i32;
    let Some(d) = this_readable(g, cf) else {
        return throw_type_error(g, "ReadableCore.state on wrong this");
    };
    d.state = match v {
        1 => ReadableState::Closed,
        2 => ReadableState::Errored,
        _ => ReadableState::Readable,
    };
    UNDEF
}

unsafe extern "sysv64" fn r_get_close_requested(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_readable(g, cf) {
        Some(d) => enc_bool(d.close_requested),
        None => throw_type_error(g, "ReadableCore.closeRequested on wrong this"),
    }
}

unsafe extern "sysv64" fn r_get_desired_size(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_readable(g, cf) {
        Some(d) => match d.desired_size() {
            Some(v) => enc_f64(v),
            None => NULL,
        },
        None => throw_type_error(g, "ReadableCore.desiredSize on wrong this"),
    }
}

unsafe extern "sysv64" fn r_get_can_close_or_enqueue(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_readable(g, cf) {
        Some(d) => enc_bool(d.can_close_or_enqueue()),
        None => throw_type_error(g, "ReadableCore.canCloseOrEnqueue on wrong this"),
    }
}

unsafe extern "sysv64" fn r_get_queue_length(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_readable(g, cf) {
        Some(d) => enc_i32(d.queue.len() as i32),
        None => throw_type_error(g, "ReadableCore.queueLength on wrong this"),
    }
}

/// 0 ok, 1 the size was not a finite non-negative number (the caller throws a
/// RangeError), 2 the stream cannot accept chunks any more.
unsafe extern "sysv64" fn r_enqueue(g: *mut c_void, cf: *mut c_void) -> u64 {
    let size = arg_num(g, cf, 0, 1.0);
    let Some(d) = this_readable(g, cf) else {
        return throw_type_error(g, "ReadableCore.enqueue on wrong this");
    };
    if !d.can_close_or_enqueue() {
        return enc_i32(2);
    }
    match d.enqueue(size) {
        Ok(()) => enc_i32(0),
        Err(EnqueueError::BadSize) => enc_i32(1),
    }
}

unsafe extern "sysv64" fn r_dequeue(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_readable(g, cf) {
        Some(d) => enc_bool(d.dequeue()),
        None => throw_type_error(g, "ReadableCore.dequeue on wrong this"),
    }
}

unsafe extern "sysv64" fn r_request_close(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_readable(g, cf) {
        Some(d) => enc_bool(d.request_close()),
        None => throw_type_error(g, "ReadableCore.requestClose on wrong this"),
    }
}

unsafe extern "sysv64" fn r_close_if_drained(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_readable(g, cf) {
        Some(d) => enc_bool(d.close_if_drained()),
        None => throw_type_error(g, "ReadableCore.closeIfDrained on wrong this"),
    }
}

unsafe extern "sysv64" fn r_begin_pull(g: *mut c_void, cf: *mut c_void) -> u64 {
    let n = arg_num(g, cf, 0, 0.0).max(0.0) as usize;
    match this_readable(g, cf) {
        Some(d) => enc_bool(d.begin_pull(n)),
        None => throw_type_error(g, "ReadableCore.beginPull on wrong this"),
    }
}

unsafe extern "sysv64" fn r_end_pull(g: *mut c_void, cf: *mut c_void) -> u64 {
    let n = arg_num(g, cf, 0, 0.0).max(0.0) as usize;
    match this_readable(g, cf) {
        Some(d) => enc_bool(d.end_pull(n)),
        None => throw_type_error(g, "ReadableCore.endPull on wrong this"),
    }
}

unsafe extern "sysv64" fn r_reset_queue(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_readable(g, cf) {
        Some(d) => {
            d.queue.reset();
            UNDEF
        }
        None => throw_type_error(g, "ReadableCore.resetQueue on wrong this"),
    }
}

// ---- writable ---------------------------------------------------------------

unsafe extern "sysv64" fn w_get_state(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_writable(g, cf) {
        Some(d) => enc_i32(d.state as i32),
        None => throw_type_error(g, "WritableCore.state on wrong this"),
    }
}

unsafe extern "sysv64" fn w_set_state(g: *mut c_void, cf: *mut c_void) -> u64 {
    let v = arg_num(g, cf, 0, 0.0) as i32;
    let Some(d) = this_writable(g, cf) else {
        return throw_type_error(g, "WritableCore.state on wrong this");
    };
    d.state = match v {
        1 => WritableState::Erroring,
        2 => WritableState::Errored,
        3 => WritableState::Closed,
        _ => WritableState::Writable,
    };
    UNDEF
}

unsafe extern "sysv64" fn w_get_desired_size(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_writable(g, cf) {
        Some(d) => enc_f64(d.desired_size()),
        None => throw_type_error(g, "WritableCore.desiredSize on wrong this"),
    }
}

unsafe extern "sysv64" fn w_get_backpressure(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_writable(g, cf) {
        Some(d) => enc_bool(d.backpressure),
        None => throw_type_error(g, "WritableCore.backpressure on wrong this"),
    }
}

unsafe extern "sysv64" fn w_update_backpressure(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_writable(g, cf) {
        Some(d) => enc_bool(d.update_backpressure()),
        None => throw_type_error(g, "WritableCore.updateBackpressure on wrong this"),
    }
}

unsafe extern "sysv64" fn w_get_queue_length(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_writable(g, cf) {
        Some(d) => enc_i32(d.queue.len() as i32),
        None => throw_type_error(g, "WritableCore.queueLength on wrong this"),
    }
}

unsafe extern "sysv64" fn w_enqueue(g: *mut c_void, cf: *mut c_void) -> u64 {
    let size = arg_num(g, cf, 0, 1.0);
    let Some(d) = this_writable(g, cf) else {
        return throw_type_error(g, "WritableCore.enqueue on wrong this");
    };
    match d.enqueue(size) {
        Ok(()) => enc_i32(0),
        Err(EnqueueError::BadSize) => enc_i32(1),
    }
}

unsafe extern "sysv64" fn w_dequeue(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_writable(g, cf) {
        Some(d) => enc_bool(d.dequeue()),
        None => throw_type_error(g, "WritableCore.dequeue on wrong this"),
    }
}

unsafe extern "sysv64" fn w_reset_queue(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_writable(g, cf) {
        Some(d) => {
            d.queue.reset();
            UNDEF
        }
        None => throw_type_error(g, "WritableCore.resetQueue on wrong this"),
    }
}

pub unsafe fn install(g: *mut c_void) {
    let readable_cls = make_host_class::<ReadableCore>("RedReadableCore");
    let readable = define_class(g, &ClassSpec {
        name: "RedReadableCore",
        arity: 1,
        construct: Some(readable_construct),
        parent_proto: 0,
        methods: &[
            ("enqueue", 1, r_enqueue),
            ("dequeue", 0, r_dequeue),
            ("requestClose", 0, r_request_close),
            ("closeIfDrained", 0, r_close_if_drained),
            ("beginPull", 1, r_begin_pull),
            ("endPull", 1, r_end_pull),
            ("resetQueue", 0, r_reset_queue),
        ],
        getters: &[
            ("desiredSize", r_get_desired_size),
            ("canCloseOrEnqueue", r_get_can_close_or_enqueue),
            ("closeRequested", r_get_close_requested),
            ("queueLength", r_get_queue_length),
        ],
        accessors: &[
            ("state", r_get_state, r_set_state),
            ("started", r_get_started, r_set_started),
            ("locked", r_get_locked, r_set_locked),
            ("disturbed", r_get_disturbed, r_set_disturbed),
        ],
    });

    let writable_cls = make_host_class::<WritableCore>("RedWritableCore");
    let writable = define_class(g, &ClassSpec {
        name: "RedWritableCore",
        arity: 1,
        construct: Some(writable_construct),
        parent_proto: 0,
        methods: &[
            ("enqueue", 1, w_enqueue),
            ("dequeue", 0, w_dequeue),
            ("updateBackpressure", 0, w_update_backpressure),
            ("resetQueue", 0, w_reset_queue),
        ],
        getters: &[
            ("desiredSize", w_get_desired_size),
            ("backpressure", w_get_backpressure),
            ("queueLength", w_get_queue_length),
        ],
        accessors: &[
            ("state", w_get_state, w_set_state),
            ("started", w_get_started, w_set_started),
            ("locked", w_get_locked, w_set_locked),
            ("inFlightWrite", w_get_in_flight_write, w_set_in_flight_write),
            ("inFlightClose", w_get_in_flight_close, w_set_in_flight_close),
            ("closeRequested", w_get_close_requested, w_set_close_requested),
        ],
    });

    let boxed: &'static StreamClasses =
        Box::leak(Box::new(StreamClasses { readable_cls, readable, writable_cls, writable }));
    registry().lock().unwrap().insert(g as usize, boxed);
}

/// The promise-shaped surface over the cores above. Evaluated AFTER
/// `compression::install` because `CompressionStream` wraps `RedZip`.
const STREAMS_JS: &str = include_str!("../streams.js");

pub unsafe fn install_surface(g: *mut c_void) -> i32 {
    let mut out = 0u64;
    let url = "redjs/streams.js";
    let rc = crate::glue::rjs_eval8(
        g,
        STREAMS_JS.as_ptr(),
        STREAMS_JS.len(),
        url.as_ptr(),
        url.len(),
        &mut out,
    );
    if rc == 1 {
        let (mut msg, mut stack) = (0u64, 0u64);
        crate::glue::rjs_error_info(g, out, &mut msg, &mut stack);
        let m = read_string(g, msg).unwrap_or_default();
        let s = read_string(g, stack).unwrap_or_default();
        eprintln!("[redjs] streams.js failed: {m}\n{s}");
        return 1;
    }
    0
}

// ---------------------------------------------------------------------------
// Tests — the state machine, no JS context needed
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_running_total_is_forced_to_zero_when_the_queue_empties() {
        // Fractional sizes leave a residue otherwise, and the residue only ever
        // grows: desiredSize drifts below the high-water mark and the stream
        // stops asking for data with nothing to show for it.
        let mut q = SizeQueue::default();
        for _ in 0..1000 {
            q.enqueue(0.1).unwrap();
            q.dequeue();
        }
        assert_eq!(q.total(), 0.0, "residue after 1000 round trips");
    }

    #[test]
    fn a_bad_size_is_rejected_rather_than_poisoning_the_total() {
        let mut q = SizeQueue::default();
        assert_eq!(q.enqueue(f64::NAN), Err(EnqueueError::BadSize));
        assert_eq!(q.enqueue(f64::INFINITY), Err(EnqueueError::BadSize));
        assert_eq!(q.enqueue(-1.0), Err(EnqueueError::BadSize));
        // A NaN total is permanent: every later comparison against it is false,
        // so the stream never applies backpressure again.
        assert_eq!(q.total(), 0.0);
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn desired_size_tracks_the_high_water_mark_minus_the_queue() {
        let mut c = ReadableCore::new(3.0);
        assert_eq!(c.desired_size(), Some(3.0));
        c.enqueue(1.0).unwrap();
        assert_eq!(c.desired_size(), Some(2.0));
        c.enqueue(5.0).unwrap();
        assert_eq!(c.desired_size(), Some(-3.0), "desiredSize goes negative, it does not clamp");
    }

    #[test]
    fn a_closed_stream_reports_zero_and_an_errored_one_reports_null() {
        let mut c = ReadableCore::new(10.0);
        c.state = ReadableState::Closed;
        assert_eq!(c.desired_size(), Some(0.0), "a closed stream must not still advertise capacity");
        c.state = ReadableState::Errored;
        assert_eq!(c.desired_size(), None);
    }

    #[test]
    fn pull_is_not_called_before_start_settles() {
        let mut c = ReadableCore::new(1.0);
        assert!(!c.should_call_pull(0), "pulled before started");
        c.started = true;
        assert!(c.should_call_pull(0));
    }

    #[test]
    fn a_zero_high_water_mark_stream_still_pulls_for_a_waiting_reader() {
        // The clause that is easy to drop. Without it a `new ReadableStream(src,
        // { highWaterMark: 0 })` never calls pull at all and every read() hangs.
        let mut c = ReadableCore::new(0.0);
        c.started = true;
        assert!(!c.should_call_pull(0), "an idle zero-HWM stream should not pull");
        c.locked = true;
        assert!(c.should_call_pull(1), "a waiting reader must provoke a pull");
    }

    #[test]
    fn a_close_requested_stream_does_not_pull_again() {
        let mut c = ReadableCore::new(10.0);
        c.started = true;
        c.request_close();
        assert!(!c.should_call_pull(1));
        assert!(!c.can_close_or_enqueue());
    }

    #[test]
    fn a_pull_in_flight_is_remembered_and_replayed_exactly_once() {
        // Re-entrancy: pull() is async, and demand arriving while it is in
        // flight must not start a second concurrent pull, nor be forgotten.
        let mut c = ReadableCore::new(1.0);
        c.started = true;
        assert!(c.begin_pull(0), "first pull should start");
        assert!(!c.begin_pull(0), "a second concurrent pull must not start");
        assert!(c.pull_again, "the missed demand must be recorded");
        assert!(c.end_pull(0), "the recorded demand replays");
        assert!(!c.end_pull(0), "…and only once");
    }

    #[test]
    fn enqueueing_over_the_mark_stops_the_pulling() {
        let mut c = ReadableCore::new(1.0);
        c.started = true;
        assert!(c.begin_pull(0));
        c.enqueue(1.0).unwrap();
        assert!(!c.end_pull(0), "the queue is full; nothing more should be pulled");
    }

    #[test]
    fn close_with_a_full_queue_defers_until_it_drains() {
        // `close()` on a stream with queued chunks must let the reader have them
        // first. Closing immediately silently truncates the stream.
        let mut c = ReadableCore::new(10.0);
        c.enqueue(1.0).unwrap();
        c.enqueue(1.0).unwrap();
        assert!(!c.request_close(), "close must not land while chunks are queued");
        assert!(!c.close_if_drained());
        c.dequeue();
        assert!(!c.close_if_drained());
        c.dequeue();
        assert!(c.close_if_drained(), "the close should land as the queue empties");
    }

    #[test]
    fn close_on_an_empty_queue_lands_immediately() {
        let mut c = ReadableCore::new(10.0);
        assert!(c.request_close());
    }

    #[test]
    fn a_writable_stream_starts_with_backpressure_applied() {
        // A writer's `ready` must be pending until start() settles, or a write
        // can land before the sink exists.
        let c = WritableCore::new(1.0);
        assert!(c.backpressure);
        assert!(!c.started);
    }

    #[test]
    fn backpressure_changes_only_report_when_they_actually_change() {
        // The return value drives replacing the writer's `ready` promise.
        // Reporting a change on every write strands whoever awaited the old one.
        let mut c = WritableCore::new(4.0);
        c.started = true;
        assert!(c.update_backpressure(), "start settling releases backpressure");
        assert!(!c.backpressure);
        c.enqueue(1.0).unwrap();
        assert!(!c.update_backpressure(), "still under the mark: no change");
        c.enqueue(3.0).unwrap();
        assert!(c.update_backpressure(), "reaching the mark applies backpressure");
        assert!(c.backpressure);
        c.dequeue();
        assert!(c.update_backpressure(), "draining releases it again");
    }

    #[test]
    fn backpressure_engages_exactly_at_the_high_water_mark_not_past_it() {
        // Spec: backpressure is `desiredSize <= 0`, so a queue that exactly
        // reaches the mark is already full. `< 0` would admit one chunk too many
        // on every stream in the program.
        let mut c = WritableCore::new(1.0);
        c.started = true;
        c.update_backpressure();
        c.enqueue(1.0).unwrap();
        assert!(c.update_backpressure());
        assert_eq!(c.desired_size(), 0.0);
        assert!(c.backpressure);
    }

    #[test]
    fn a_writable_stream_reports_backpressure_while_unstarted_whatever_the_queue() {
        let mut c = WritableCore::new(100.0);
        assert!(!c.update_backpressure(), "unstarted is already back-pressured");
        assert!(c.backpressure);
    }
}

/// The whole surface, through JS. The state machine tests above cannot see the
/// promise sequencing, and the promise sequencing is where a stream hangs.
#[cfg(test)]
mod jsc_tests {
    use crate::api::Context;
    use crate::webapi::testing::*;

    /// Run an async body and return its resolved value stringified. A body that
    /// never settles comes back as `<pending>` — which is what makes "the test
    /// passed because nothing happened" impossible here.
    fn run(c: &Context, body: &str) -> String {
        s(
            c,
            &format!(
                "globalThis.__out = '<pending>'; (async () => {{ {body} }})().then(\
                   v => {{ globalThis.__out = String(v); }}, \
                   e => {{ globalThis.__out = 'ERROR: ' + ((e && e.message) || e); }}); 'started'"
            ),
        );
        // Repeatedly, because each drain can queue more reactions.
        for _ in 0..64 {
            c.drain_microtasks();
        }
        s(c, "globalThis.__out")
    }

    fn expect(c: &Context, body: &str, want: &str) {
        let got = run(c, body);
        assert_eq!(got, want, "body: {body}");
    }

    #[test]
    fn the_harness_itself_can_report_a_stall() {
        // If this ever passes with '1', every other test in this module is
        // meaningless — they all rest on `<pending>` being observable.
        let c = ctx();
        assert_eq!(run(&c, "await new Promise(() => {}); return 1;"), "<pending>");
    }

    #[test]
    fn a_reader_gets_the_enqueued_chunks_and_then_done() {
        let c = ctx();
        expect(
            &c,
            "const rs = new ReadableStream({ start(c) { c.enqueue('a'); c.enqueue('b'); c.close(); } });
             const r = rs.getReader();
             const out = [];
             for (;;) { const { value, done } = await r.read(); if (done) break; out.push(value); }
             return out.join('');",
            "ab",
        );
    }

    #[test]
    fn close_with_queued_chunks_delivers_them_before_done() {
        // The deferred close. Closing eagerly truncates the stream and the
        // consumer sees a short file with no error anywhere.
        let c = ctx();
        expect(
            &c,
            "const rs = new ReadableStream({ start(c) { for (const x of [1,2,3]) c.enqueue(x); c.close(); } },
                                            { highWaterMark: 10 });
             const r = rs.getReader();
             const out = [];
             for (;;) { const { value, done } = await r.read(); if (done) break; out.push(value); }
             return out.join(',');",
            "1,2,3",
        );
    }

    #[test]
    fn pull_is_driven_by_demand_and_stops_at_the_high_water_mark() {
        // A stream with nobody reading must fill to the mark and then STOP.
        // Running away is an unbounded queue; stopping short is a stall.
        let c = ctx();
        expect(
            &c,
            "let pulls = 0;
             new ReadableStream({ pull(c) { pulls++; c.enqueue(pulls); } }, { highWaterMark: 3 });
             for (let i = 0; i < 20; i++) await Promise.resolve();
             return pulls;",
            "3",
        );
        // …and one more read frees exactly one slot, so exactly one more pull.
        expect(
            &c,
            "let pulls = 0;
             const rs = new ReadableStream({ pull(c) { pulls++; c.enqueue(pulls); } }, { highWaterMark: 3 });
             for (let i = 0; i < 20; i++) await Promise.resolve();
             await rs.getReader().read();
             for (let i = 0; i < 20; i++) await Promise.resolve();
             return pulls;",
            "4",
        );
    }

    #[test]
    fn a_zero_high_water_mark_stream_still_delivers_to_a_waiting_reader() {
        // Without the read-request clause in ShouldCallPull this hangs forever.
        let c = ctx();
        expect(
            &c,
            "let n = 0;
             const rs = new ReadableStream({ pull(c) { c.enqueue(++n); } }, { highWaterMark: 0 });
             const r = rs.getReader();
             const a = await r.read();
             const b = await r.read();
             return a.value + ',' + b.value;",
            "1,2",
        );
    }

    #[test]
    fn a_pull_that_arrives_during_an_in_flight_pull_is_replayed() {
        let c = ctx();
        expect(
            &c,
            "let n = 0;
             const rs = new ReadableStream({
               pull(c) { return Promise.resolve().then(() => { c.enqueue(++n); }); }
             }, { highWaterMark: 0 });
             const r = rs.getReader();
             const out = [];
             for (let i = 0; i < 3; i++) out.push((await r.read()).value);
             return out.join(',');",
            "1,2,3",
        );
    }

    #[test]
    fn an_error_reaches_a_pending_read_and_every_later_one() {
        let c = ctx();
        expect(
            &c,
            "const rs = new ReadableStream({ start(c) { c.error(new Error('boom')); } });
             const r = rs.getReader();
             try { await r.read(); return 'no throw'; } catch (e) { return e.message; }",
            "boom",
        );
        expect(
            &c,
            "let ctl; const rs = new ReadableStream({ start(c) { ctl = c; } });
             const r = rs.getReader();
             const p = r.read();
             ctl.error(new Error('late'));
             try { await p; return 'no throw'; } catch (e) { return e.message; }",
            "late",
        );
    }

    #[test]
    fn cancel_reaches_the_underlying_source() {
        let c = ctx();
        expect(
            &c,
            "let seen = null;
             const rs = new ReadableStream({ cancel(reason) { seen = reason; } });
             await rs.cancel('because');
             return seen;",
            "because",
        );
    }

    #[test]
    fn a_locked_stream_refuses_a_second_reader_and_refuses_to_cancel() {
        let c = ctx();
        expect(
            &c,
            "const rs = new ReadableStream({});
             rs.getReader();
             let a = 'no'; try { rs.getReader(); } catch (e) { a = 'locked'; }
             let b = 'no'; try { await rs.cancel(); } catch (e) { b = 'locked'; }
             return a + ',' + b + ',' + rs.locked;",
            "locked,locked,true",
        );
    }

    #[test]
    fn async_iteration_walks_the_stream() {
        let c = ctx();
        expect(
            &c,
            "const rs = new ReadableStream({ start(c) { c.enqueue('x'); c.enqueue('y'); c.close(); } },
                                            { highWaterMark: 10 });
             let out = '';
             for await (const chunk of rs) out += chunk;
             return out;",
            "xy",
        );
    }

    #[test]
    fn tee_gives_both_branches_every_chunk() {
        let c = ctx();
        expect(
            &c,
            "const rs = new ReadableStream({ start(c) { c.enqueue(1); c.enqueue(2); c.close(); } },
                                            { highWaterMark: 10 });
             const [a, b] = rs.tee();
             const drain = async (s) => { const r = s.getReader(); const out = [];
               for (;;) { const { value, done } = await r.read(); if (done) break; out.push(value); }
               return out.join(''); };
             const [x, y] = await Promise.all([drain(a), drain(b)]);
             return x + '/' + y;",
            "12/12",
        );
    }

    #[test]
    fn writes_reach_the_sink_in_order() {
        let c = ctx();
        expect(
            &c,
            "const seen = [];
             const ws = new WritableStream({ write(chunk) { seen.push(chunk); } });
             const w = ws.getWriter();
             await w.write('a'); await w.write('b'); await w.close();
             return seen.join('');",
            "ab",
        );
    }

    #[test]
    fn ready_reflects_backpressure_and_releases_as_the_queue_drains() {
        // The observable half of the backpressure machine. With hwm 1 and a sink
        // that only settles when told, two queued writes must leave `ready`
        // pending — and it must stay pending until the queue actually DRAINS,
        // not merely until the first write completes (the second is in flight by
        // then and still occupies the mark).
        let c = ctx();
        expect(
            &c,
            "let release;
             const ws = new WritableStream({ write() { return new Promise(r => { release = r; }); } },
                                            { highWaterMark: 1 });
             const w = ws.getWriter();
             await w.ready;
             w.write('a');
             w.write('b');
             let settled = false;
             w.ready.then(() => { settled = true; });
             const turns = async () => { for (let i = 0; i < 10; i++) await Promise.resolve(); };
             await turns();
             const blockedAtStart = !settled;
             release();                 // 'a' completes; 'b' goes in flight
             await turns();
             const stillBlocked = !settled;
             release();                 // 'b' completes; the queue is empty
             await turns();
             return blockedAtStart + ',' + stillBlocked + ',' + settled;",
            "true,true,true",
        );
    }

    #[test]
    fn writing_after_close_rejects_and_closed_resolves() {
        let c = ctx();
        expect(
            &c,
            "const ws = new WritableStream({});
             const w = ws.getWriter();
             await w.close();
             await w.closed;
             try { await w.write('x'); return 'no throw'; } catch (e) { return 'rejected'; }",
            "rejected",
        );
    }

    #[test]
    fn a_failing_sink_rejects_the_write_and_the_stream() {
        let c = ctx();
        expect(
            &c,
            "const ws = new WritableStream({ write() { throw new Error('sink died'); } });
             const w = ws.getWriter();
             let first = '';
             try { await w.write('a'); } catch (e) { first = e.message; }
             let second = '';
             try { await w.write('b'); } catch (e) { second = e.message; }
             return first + '/' + second;",
            "sink died/sink died",
        );
    }

    #[test]
    fn abort_reaches_the_sink() {
        let c = ctx();
        expect(
            &c,
            "let seen = null;
             const ws = new WritableStream({ abort(r) { seen = r; } });
             await ws.abort('stop');
             return seen;",
            "stop",
        );
    }

    #[test]
    fn a_transform_stream_transforms() {
        let c = ctx();
        expect(
            &c,
            "const ts = new TransformStream({ transform(chunk, c) { c.enqueue(chunk * 2); } });
             const w = ts.writable.getWriter();
             const r = ts.readable.getReader();
             w.write(1); w.write(2); w.close();
             const out = [];
             for (;;) { const { value, done } = await r.read(); if (done) break; out.push(value); }
             return out.join(',');",
            "2,4",
        );
    }

    #[test]
    fn a_transform_with_no_transform_function_is_the_identity() {
        let c = ctx();
        expect(
            &c,
            "const ts = new TransformStream();
             const w = ts.writable.getWriter();
             const r = ts.readable.getReader();
             w.write('p'); w.close();
             return (await r.read()).value;",
            "p",
        );
    }

    #[test]
    fn a_transform_flush_can_emit_a_final_chunk() {
        let c = ctx();
        expect(
            &c,
            "const ts = new TransformStream({ transform(c2, c) { c.enqueue(c2); }, flush(c) { c.enqueue('!'); } });
             const w = ts.writable.getWriter();
             const r = ts.readable.getReader();
             w.write('a'); w.close();
             const out = [];
             for (;;) { const { value, done } = await r.read(); if (done) break; out.push(value); }
             return out.join('');",
            "a!",
        );
    }

    #[test]
    fn pipe_to_moves_everything_and_closes_the_destination() {
        let c = ctx();
        expect(
            &c,
            "const seen = []; let closed = false;
             const rs = new ReadableStream({ start(c) { c.enqueue(1); c.enqueue(2); c.close(); } },
                                            { highWaterMark: 10 });
             const ws = new WritableStream({ write(x) { seen.push(x); }, close() { closed = true; } });
             await rs.pipeTo(ws);
             return seen.join(',') + '/' + closed;",
            "1,2/true",
        );
    }

    #[test]
    fn pipe_through_chains_a_transform() {
        let c = ctx();
        expect(
            &c,
            "const rs = new ReadableStream({ start(c) { c.enqueue('a'); c.enqueue('b'); c.close(); } },
                                            { highWaterMark: 10 });
             const up = new TransformStream({ transform(x, c) { c.enqueue(x.toUpperCase()); } });
             const r = rs.pipeThrough(up).getReader();
             let out = '';
             for (;;) { const { value, done } = await r.read(); if (done) break; out += value; }
             return out;",
            "AB",
        );
    }

    #[test]
    fn queuing_strategies_supply_a_mark_and_a_detachable_size() {
        let c = ctx();
        expect(
            &c,
            "const cs = new CountQueuingStrategy({ highWaterMark: 5 });
             const bs = new ByteLengthQueuingStrategy({ highWaterMark: 16 });
             const size = bs.size;
             return cs.highWaterMark + ',' + cs.size('anything') + ',' + bs.highWaterMark + ',' +
                    size(new Uint8Array(7));",
            "5,1,16,7",
        );
    }

    #[test]
    fn a_byte_length_strategy_actually_drives_the_desired_size() {
        let c = ctx();
        expect(
            &c,
            "let ctl;
             new ReadableStream({ start(c) { ctl = c; } }, new ByteLengthQueuingStrategy({ highWaterMark: 100 }));
             ctl.enqueue(new Uint8Array(30));
             return ctl.desiredSize;",
            "70",
        );
    }

    #[test]
    fn a_bad_high_water_mark_is_a_range_error() {
        let c = ctx();
        let msg = throws(&c, "new ReadableStream({}, { highWaterMark: -1 })");
        assert!(msg.contains("non-negative"), "{msg}");
        let msg2 = throws(&c, "new WritableStream({}, { highWaterMark: NaN })");
        assert!(msg2.contains("non-negative"), "{msg2}");
    }

    #[test]
    fn a_size_that_is_not_a_number_errors_the_stream_rather_than_corrupting_the_queue() {
        // A NaN running total is permanent: every later comparison is false and
        // the stream never applies backpressure again.
        let c = ctx();
        expect(
            &c,
            "let ctl;
             const rs = new ReadableStream({ start(c) { ctl = c; } }, { size: () => NaN, highWaterMark: 5 });
             let threw = '';
             try { ctl.enqueue('x'); } catch (e) { threw = e.constructor.name; }
             const r = rs.getReader();
             let readErr = '';
             try { await r.read(); } catch (e) { readErr = 'errored'; }
             return threw + '/' + readErr;",
            "RangeError/errored",
        );
    }

    #[test]
    fn byte_streams_and_byob_readers_refuse_by_name() {
        let c = ctx();
        let msg = throws(&c, "new ReadableStream({ type: 'bytes' })");
        assert!(msg.contains("byte streams"), "{msg}");
        assert!(msg.contains("Uint8Array"), "{msg}");
        let msg2 = throws(&c, "new ReadableStream({}).getReader({ mode: 'byob' })");
        assert!(msg2.contains("BYOB"), "{msg2}");
    }

    #[test]
    fn blob_stream_now_returns_a_readable_stream() {
        // It threw "not supported in the player" before this module existed.
        let c = ctx();
        expect(
            &c,
            "const r = new Blob(['hello']).stream().getReader();
             const { value } = await r.read();
             return new TextDecoder().decode(value);",
            "hello",
        );
    }

    #[test]
    fn the_classes_are_on_the_global_and_brand_correctly() {
        let c = ctx();
        ok(&c, "new ReadableStream({}) instanceof ReadableStream");
        ok(&c, "new WritableStream({}) instanceof WritableStream");
        ok(&c, "new TransformStream() instanceof TransformStream");
        ok(&c, "new ReadableStream({}).getReader() instanceof ReadableStreamDefaultReader");
        // The controllers are not constructible (spec: [Exposed] with no ctor).
        let msg = throws(&c, "new ReadableStreamDefaultController()");
        assert!(msg.contains("Illegal constructor"), "{msg}");
    }
}
