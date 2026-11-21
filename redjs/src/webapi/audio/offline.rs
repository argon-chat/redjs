//! `OfflineAudioContext`: a context nothing plays. Its stream hands the renderer to the context
//! (`output::OfflineOut`), and after `startRendering()` the pump pulls the graph through it a
//! render quantum at a time, running the timelines and the scheduled starts before each block,
//! so its clock is the frames rendered. A long rendering is spread over pump ticks.

use std::time::Duration;

use super::output::{OfflineOut, Renderer};
use super::*;

/// The frames of one block, the spec's render quantum.
const QUANTUM: usize = 128;
/// How long one pump tick renders for before it lets the page go on.
pub(super) const SLICE: Duration = Duration::from_millis(8);

pub(super) struct Offline {
    pub channels: u32,
    pub sample_rate: u32,
    pub length: usize,
    pub rendered: usize,
    pub rendering: bool,
    started: bool,
    data: Vec<Vec<f32>>,
    block: Vec<f32>,
    renderer: Arc<Mutex<Option<Renderer>>>,
    /// The promise of `startRendering()`'s resolve function and the context, protected while
    /// it renders.
    resolve: u64,
    wrapper: u64,
}

// SAFETY: the two JS values are only used on the VM's thread, like every wrapper the engines keep.
unsafe impl Send for Offline {}

impl Offline {
    fn new(channels: u32, length: usize, sample_rate: u32) -> Self {
        Self {
            channels,
            sample_rate,
            length,
            rendered: 0,
            rendering: false,
            started: false,
            data: vec![Vec::with_capacity(length); channels as usize],
            block: vec![0.0; QUANTUM * channels as usize],
            renderer: Arc::new(Mutex::new(None)),
            resolve: UNDEF,
            wrapper: UNDEF,
        }
    }

    pub fn out(&self) -> OfflineOut {
        OfflineOut {
            channels: self.channels,
            sample_rate: self.sample_rate,
            renderer: Arc::clone(&self.renderer),
        }
    }
}

pub(super) enum Step {
    /// Not an offline context, or not rendering.
    Idle,
    More,
    Finished,
}

/// Render the next block of a context that is rendering. The pump has just run the timelines
/// and shipped the graph for the time this block starts at.
pub(super) fn render_block(eng: &mut Engine) -> Step {
    let Some(o) = eng.offline.as_mut().filter(|o| o.rendering) else {
        return Step::Idle;
    };
    let frames = QUANTUM.min(o.length - o.rendered);
    let channels = o.channels as usize;
    if let Some(renderer) = o.renderer.lock().unwrap().as_mut() {
        renderer.render(&mut o.block, channels, QUANTUM);
    } else {
        o.block.fill(0.0);
    }
    for (c, channel) in o.data.iter_mut().enumerate() {
        channel.extend(o.block.chunks(channels).take(frames).map(|frame| frame[c]));
    }
    o.rendered += frames;
    if o.rendered < o.length {
        return Step::More;
    }
    o.rendering = false;
    Step::Finished
}

/// The rendering is done: the buffer goes to the promise and to `complete`, and the context closes.
pub(super) unsafe fn complete(g: *mut c_void, engine: u64) {
    let (data, rate, resolve, wrapper) = {
        let mut gl = glob().lock().unwrap();
        let Some(eng) = gl.engines.get_mut(&engine) else {
            return;
        };
        eng.state = ST_CLOSED;
        eng.fw.stop_stream();
        eng.running = false;
        let rate = eng.sample_rate;
        let Some(o) = eng.offline.as_mut() else {
            return;
        };
        (core::mem::take(&mut o.data), rate, o.resolve, o.wrapper)
    };
    let buffer = wrap_buffer(
        g,
        Arc::new(PlanarBuffer {
            data,
            sample_rate: rate,
        }),
    );
    let _ = call(g, resolve, UNDEF, &[buffer]);
    let event = super::super::events::new_trusted_event(g, "complete");
    define(g, event, "renderedBuffer", buffer, 0);
    super::super::events::dispatch(g, wrapper, event);
    state_changed(g, wrapper);
    crate::ffi::redjs_unprotect(g, resolve);
    crate::ffi::redjs_unprotect(g, wrapper);
}

/// `new OfflineAudioContext(numberOfChannels, length, sampleRate)` or
/// `new OfflineAudioContext({ numberOfChannels, length, sampleRate })`.
#[crate::host_fn]
unsafe fn offline_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let what = "Failed to construct 'OfflineAudioContext'";
    let first = cf_arg(cf, 0);
    let mut numbers = [1.0, f64::NAN, f64::NAN];
    if super::super::blob::is_object(g, first) {
        for (slot, name) in numbers
            .iter_mut()
            .zip(["numberOfChannels", "length", "sampleRate"])
        {
            let Ok(value) = try_get(g, first, name) else {
                return UNDEF;
            };
            if value != UNDEF {
                let Ok(number) = to_number(g, value) else {
                    return UNDEF;
                };
                *slot = number;
            }
        }
    } else {
        for (i, slot) in numbers.iter_mut().enumerate() {
            let Ok(number) = to_number(g, cf_arg(cf, i as u32)) else {
                return UNDEF;
            };
            *slot = number;
        }
    }
    let [channels, length, rate] = numbers;
    if length.is_nan() || rate.is_nan() || channels.is_nan() {
        return throw_type_error(
            g,
            &format!(
                "{what}: the number of channels, the length and the sample rate are required."
            ),
        );
    }
    let refused = if !(1.0..=mixing::MAX_CHANNELS as f64).contains(&channels) {
        Some(format!(
            "the number of channels ({channels}) is outside the range [1, {}].",
            mixing::MAX_CHANNELS
        ))
    } else if !(length >= 1.0 && length <= u32::MAX as f64) {
        Some(format!(
            "the number of frames ({length}) must be at least 1."
        ))
    } else if !(3000.0..=768_000.0).contains(&rate) {
        Some(format!(
            "the sample rate ({rate}) is outside the range [3000, 768000]."
        ))
    } else {
        None
    };
    if let Some(reason) = refused {
        return super::super::domexception::throw(
            g,
            "NotSupportedError",
            &format!("{what}: {reason}"),
        );
    }
    let offline = Offline::new(channels as u32, length as usize, rate as u32);
    let obj = construct_context(g, cf, classes(g).offline.proto, Some(offline));
    if is_cell(obj) {
        define(g, slots_of(g, obj), "oncomplete", NULL, 0);
    }
    obj
}

#[crate::host_fn]
unsafe fn start_rendering(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(h) = this_engine_handle(g, cf) else {
        return rejected_promise(g, 1, "OfflineAudioContext.startRendering on wrong this");
    };
    let promise_ctor = get(g, glue::rjs_global(g), "Promise");
    let Ok(capability) = call(g, get(g, promise_ctor, "withResolvers"), promise_ctor, &[]) else {
        return UNDEF;
    };
    let resolve = get(g, capability, "resolve");
    {
        let mut gl = glob().lock().unwrap();
        let Some(eng) = gl.engines.get_mut(&h) else {
            return UNDEF;
        };
        let Some(o) = eng.offline.as_mut().filter(|o| !o.started) else {
            drop(gl);
            return super::super::domexception::rejected(
                g,
                "InvalidStateError",
                "Failed to execute 'startRendering' on 'OfflineAudioContext': cannot call startRendering more than once.",
            );
        };
        o.started = true;
        o.rendering = true;
        o.resolve = resolve;
        o.wrapper = cf_this(cf);
        eng.state = ST_RUNNING;
    }
    crate::ffi::redjs_protect(g, resolve);
    crate::ffi::redjs_protect(g, cf_this(cf));
    state_changed(g, cf_this(cf));
    get(g, capability, "promise")
}

#[crate::host_fn]
unsafe fn get_length(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(h) = this_engine_handle(g, cf) else {
        return throw_type_error(g, "OfflineAudioContext.length on wrong this");
    };
    let gl = glob().lock().unwrap();
    let length = gl
        .engines
        .get(&h)
        .and_then(|e| e.offline.as_ref())
        .map_or(0, |o| o.length);
    enc_f64(length as f64)
}

/// A rendering cannot be paused here: `suspend(time)` and `resume()` are refused.
#[crate::host_fn]
unsafe fn not_supported(g: *mut c_void, _cf: *mut c_void) -> u64 {
    super::super::domexception::rejected(
        g,
        "NotSupportedError",
        "An OfflineAudioContext renders in one piece on this host: suspend() and resume() are not supported.",
    )
}

#[crate::host_fn]
unsafe fn get_oncomplete(g: *mut c_void, cf: *mut c_void) -> u64 {
    get(g, slots_of(g, cf_this(cf)), "oncomplete")
}

#[crate::host_fn]
unsafe fn set_oncomplete(g: *mut c_void, cf: *mut c_void) -> u64 {
    let handler = cf_arg(cf, 0);
    let handler = if is_cell(handler) { handler } else { NULL };
    define(g, slots_of(g, cf_this(cf)), "oncomplete", handler, 0);
    UNDEF
}

pub(super) unsafe fn install(g: *mut c_void, context_proto: u64) -> Class {
    define_class(
        g,
        &ClassSpec {
            name: "OfflineAudioContext",
            arity: 1,
            construct: Some(offline_construct),
            parent_proto: context_proto,
            methods: &[
                ("startRendering", 0, start_rendering),
                ("suspend", 1, not_supported),
                ("resume", 0, not_supported),
            ],
            getters: &[("length", get_length)],
            accessors: &[("oncomplete", get_oncomplete, set_oncomplete)],
        },
    )
}
