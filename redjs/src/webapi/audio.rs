//! Native Web Audio API (the game-engine subset) over the Firewheel
//! audio-graph engine.
//!
//! Web Audio is itself a DAG of processing nodes, so the mapping onto
//! Firewheel is direct:
//!
//! | Web Audio                | Firewheel                                    |
//! |--------------------------|----------------------------------------------|
//! | `AudioContext`           | `FirewheelContext` + cpal output stream      |
//! | `destination`            | master `VolumeNode` → graph out              |
//! | `GainNode`               | `VolumeNode`                                 |
//! | `StereoPannerNode`       | `VolumePanNode`                              |
//! | `PannerNode` + listener  | `WebPannerNode` (webapi/hrtf.rs): spec distance models + cones + listener basis; REAL HRTF via Chromium's HRIR set, or equal-power |
//! | `BiquadFilterNode`       | `SvfNode<2>`                                 |
//! | `AudioBufferSourceNode`  | `SamplerNode`                                |
//! | `AudioBuffer`            | planar `f32` `SampleResource`                |
//! | `OscillatorNode`         | `OscNode` (webapi/osc.rs): PolyBLEP-limited  |
//! | `RedStreamSourceNode`    | `StreamWriterNode` (webapi/pcm.rs) — ours     |
//! | `decodeAudioData`        | symphonium (resampled to the stream rate)    |
//! | `currentTime`            | the audio-stream sample clock (seconds)      |
//!
//! `AudioParam` automation (`setValueAtTime`/`linearRampToValueAtTime`/
//! `exponentialRampToValueAtTime`/`setTargetAtTime`) is evaluated on every pump
//! tick (once per frame) and forwarded as Firewheel param events; the nodes'
//! built-in ~15 ms smoothers turn the per-frame steps into artifact-free ramps.
//! Ends of playback are detected from the sampler's shared playback state and
//! fire `onended`.
//!
//! Known deviations (documented, deliberate for v1):
//! - `createView`-style object identity rules aside, `suspend()` mutes the
//!   master bus instead of pausing the stream, so `currentTime` keeps
//!   advancing while suspended.
//! - `AnalyserNode` is REAL (W-MIX MIX-13): a bypass monitor node captures the
//!   last `MONITOR_WINDOW` mono-mixed samples into a lock-free ring; the
//!   getters serve true time-domain data and a Blackman-windowed radix-2 FFT
//!   with spec smoothing. Deviations: `fftSize` caps at 4096 (spec: 32768) and
//!   reads may tear across a processing-block boundary (fine for meters).
//! - Scheduling (`start`/`stop`, automation) lands on the pump tick at or after
//!   the requested time, not on the exact sample.
//! - No `OfflineAudioContext`, `AudioWorklet`, `ConvolverNode`, `DelayNode`,
//!   `WaveShaperNode`, `DynamicsCompressorNode`, `ChannelSplitter`/`Merger`, or
//!   `PeriodicWave`. Each is absent rather than stubbed, so
//!   `typeof ctx.createDelay === 'function'` tells the truth.
//! - `createMediaElementSource` is a silent pass-through that says so — see
//!   `ctx_create_media_element_source`.

use core::ffi::c_void;
use std::collections::HashMap;
use std::num::{NonZeroU32, NonZeroUsize};
use std::ops::Range;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use firewheel::{
    collector::ArcGc,
    diff::{Diff, PathBuilder},
    event::NodeEventType,
    node::NodeID,
    nodes::{
        sampler::{PlayFrom, RepeatMode, SamplerConfig, SamplerNode},
        svf::{SvfNode, SvfType},
        volume::VolumeNode,
        volume_pan::VolumePanNode,
    },
    sample_resource::{SampleResource, SampleResourceInfo},
    FirewheelContext, Volume,
};

use firewheel::nodes::stream::{
    writer::{StreamWriterConfig, StreamWriterNode, StreamWriterState},
    ResamplingChannelConfig,
};

use super::hrtf::{HrtfDatabase, WebPannerConfig, WebPannerNode};
use super::mediadevices;
use super::osc::{OscNode, OscShared, Wave};
use super::pcm::{self, PcmStreamConfig};
use super::util::*;
use crate::glue;

// ---------------------------------------------------------------------------
// AudioParam kinds
// ---------------------------------------------------------------------------

const P_GAIN: u8 = 0; // GainNode.gain (also BiquadFilter.gain shares SvfNode)
const P_PAN: u8 = 1; // StereoPannerNode.pan
const P_RATE: u8 = 2; // AudioBufferSourceNode.playbackRate
const P_FREQ: u8 = 3; // BiquadFilterNode.frequency
const P_Q: u8 = 4; // BiquadFilterNode.Q
const P_POS_X: u8 = 6; // PannerNode.positionX
const P_POS_Y: u8 = 7;
const P_POS_Z: u8 = 8;
const P_INERT: u8 = 9; // detune / … — stored, no effect
const P_ORIENT_X: u8 = 10; // PannerNode.orientationX (cone direction)
const P_ORIENT_Y: u8 = 11;
const P_ORIENT_Z: u8 = 12;
const P_DETUNE: u8 = 13; // OscillatorNode.detune (cents) — a REAL detune, unlike P_INERT
const P_L_POS_X: u8 = 100; // AudioListener.positionX (param.node = engine handle)
const P_L_POS_Y: u8 = 101;
const P_L_POS_Z: u8 = 102;
const P_L_FWD_X: u8 = 103; // AudioListener.forwardX (rotates panning space)
const P_L_FWD_Y: u8 = 104;
const P_L_FWD_Z: u8 = 105;
const P_L_UP_X: u8 = 106;
const P_L_UP_Y: u8 = 107;
const P_L_UP_Z: u8 = 108;

// ---------------------------------------------------------------------------
// AudioBuffer backing store (planar f32, shared with playing samplers)
// ---------------------------------------------------------------------------

pub struct PlanarBuffer {
    pub data: Vec<Vec<f32>>,
    pub sample_rate: u32,
}

/// The sampler-facing view of a buffer (`ArcGc` needs a sized owner).
struct SharedSample(Arc<PlanarBuffer>);

impl SampleResourceInfo for SharedSample {
    fn num_channels(&self) -> NonZeroUsize {
        NonZeroUsize::new(self.0.data.len().max(1)).unwrap()
    }

    fn len_frames(&self) -> u64 {
        self.0.data.first().map(|c| c.len() as u64).unwrap_or(0)
    }

    fn sample_rate(&self) -> Option<NonZeroU32> {
        NonZeroU32::new(self.0.sample_rate)
    }
}

impl SampleResource for SharedSample {
    fn fill_buffers(&self, buffers: &mut [&mut [f32]], buffer_range: Range<usize>, start_frame: u64) {
        for (ch, out) in self.0.data.iter().zip(buffers.iter_mut()) {
            let out = &mut out[buffer_range.clone()];
            let start = (start_frame as usize).min(ch.len());
            let n = out.len().min(ch.len() - start);
            out[..n].copy_from_slice(&ch[start..start + n]);
            out[n..].fill(0.0);
        }
    }
}

// ---------------------------------------------------------------------------
// AudioParam automation timeline (evaluated once per pump tick)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Tl {
    Set { t: f64, v: f64 },
    Lin { t: f64, v: f64 },
    /// `exponentialRampToValueAtTime` — a real geometric ramp, not a linear
    /// stand-in. It matters for exactly the thing it is used for: a gain fade
    /// that sounds even to the ear is exponential in amplitude, and a linear
    /// substitute is audibly back-loaded.
    Exp { t: f64, v: f64 },
    Target { t: f64, v: f64, tau: f64 },
}

impl Tl {
    fn time(&self) -> f64 {
        match *self {
            Tl::Set { t, .. } | Tl::Lin { t, .. } | Tl::Exp { t, .. } | Tl::Target { t, .. } => t,
        }
    }
}

struct Timeline {
    evs: Vec<Tl>,
    /// Param value at the moment the first event was scheduled.
    start_v: f64,
}

impl Timeline {
    /// (value at `now`, finished)
    fn eval(&self, now: f64) -> (f64, bool) {
        let mut cur = self.start_v;
        let mut cur_t = f64::NEG_INFINITY;
        for (i, ev) in self.evs.iter().enumerate() {
            match *ev {
                Tl::Set { t, v } => {
                    if t <= now {
                        cur = v;
                        cur_t = t;
                    } else {
                        return (cur, false);
                    }
                }
                Tl::Lin { t, v } => {
                    if t <= now {
                        cur = v;
                        cur_t = t;
                    } else {
                        let t0 = cur_t.max(0.0);
                        let k = if t > t0 { ((now - t0) / (t - t0)).clamp(0.0, 1.0) } else { 1.0 };
                        return (cur + (v - cur) * k, false);
                    }
                }
                Tl::Exp { t, v } => {
                    if t <= now {
                        cur = v;
                        cur_t = t;
                    } else {
                        let t0 = cur_t.max(0.0);
                        let k = if t > t0 { ((now - t0) / (t - t0)).clamp(0.0, 1.0) } else { 1.0 };
                        // v(t) = v0 · (v1/v0)^k. Defined only for a same-signed
                        // pair of non-zero endpoints; the spec leaves the rest
                        // undefined and every browser degrades to linear rather
                        // than producing a NaN, so we do too.
                        let ok = (cur > 0.0 && v > 0.0) || (cur < 0.0 && v < 0.0);
                        let val = if ok { cur * (v / cur).powf(k) } else { cur + (v - cur) * k };
                        return (val, false);
                    }
                }
                Tl::Target { t, v, tau } => {
                    if t > now {
                        return (cur, false);
                    }
                    // A target holds until the next event (spec) — but the
                    // engine only ever ends automation with a target, so
                    // treat it as terminal unless followed by another event.
                    if i + 1 < self.evs.len() && self.evs[i + 1].time() <= now {
                        cur = v; // jumped past by a later event
                        cur_t = t;
                        continue;
                    }
                    let val = v + (cur - v) * (-(now - t) / tau.max(1e-4)).exp();
                    return (val, (val - v).abs() < 1e-4);
                }
            }
        }
        (cur, true)
    }
}

// ---------------------------------------------------------------------------
// AnalyserNode backing (W-MIX MIX-13): a bypass monitor node captures the last
// MONITOR_WINDOW mono-mixed samples into a lock-free ring shared with the main
// thread; the analysis getters (time-domain + FFT) read it on demand.
// ---------------------------------------------------------------------------

/// Native max `fftSize` (power of two). The spec allows up to 32768; games use
/// 256–2048 — 4096 keeps the per-analyser footprint at 16 KB.
const MONITOR_WINDOW: usize = 4096;

struct MonitorRing {
    /// f32 bits, mono mix of the inputs. Torn reads across a processing-block
    /// boundary are acceptable for analysis purposes.
    samples: Box<[AtomicU32]>,
    cursor: AtomicUsize,
}

impl MonitorRing {
    fn new() -> Self {
        Self {
            samples: (0..MONITOR_WINDOW).map(|_| AtomicU32::new(0)).collect(),
            cursor: AtomicUsize::new(0),
        }
    }

    /// Copy the most recent `out.len()` samples, oldest → newest.
    fn read_latest(&self, out: &mut [f32]) {
        let cur = self.cursor.load(Ordering::Acquire);
        let n = out.len().min(MONITOR_WINDOW);
        let start = cur.wrapping_sub(n);
        for (i, slot) in out.iter_mut().take(n).enumerate() {
            let bits = self.samples[start.wrapping_add(i) & (MONITOR_WINDOW - 1)].load(Ordering::Relaxed);
            *slot = f32::from_bits(bits);
        }
    }
}

#[derive(Clone)]
struct MonitorNode {
    ring: ArcGc<MonitorRing>,
}

impl firewheel::node::AudioNode for MonitorNode {
    type Configuration = firewheel::node::EmptyConfig;

    fn info(&self, _config: &Self::Configuration) -> firewheel::node::AudioNodeInfo {
        firewheel::node::AudioNodeInfo::new()
            .debug_name("webaudio_analyser")
            .channel_config(firewheel::channel_config::ChannelConfig {
                num_inputs: firewheel::channel_config::ChannelCount::new(2).unwrap(),
                num_outputs: firewheel::channel_config::ChannelCount::new(2).unwrap(),
            })
    }

    fn construct_processor(
        &self,
        _config: &Self::Configuration,
        _cx: firewheel::node::ConstructProcessorContext,
    ) -> impl firewheel::node::AudioNodeProcessor {
        MonitorProcessor { ring: ArcGc::clone(&self.ring) }
    }
}

struct MonitorProcessor {
    ring: ArcGc<MonitorRing>,
}

impl firewheel::node::AudioNodeProcessor for MonitorProcessor {
    fn process(
        &mut self,
        info: &firewheel::node::ProcInfo,
        buffers: firewheel::node::ProcBuffers,
        _events: &mut firewheel::event::ProcEvents,
        _extra: &mut firewheel::node::ProcExtra,
    ) -> firewheel::node::ProcessStatus {
        let n_ch = buffers.inputs.len().max(1) as f32;
        let mut cur = self.ring.cursor.load(Ordering::Relaxed);
        for i in 0..info.frames {
            let mut s = 0.0f32;
            for (ch, buf) in buffers.inputs.iter().enumerate() {
                if !info.in_silence_mask.is_channel_silent(ch) {
                    s += buf[i];
                }
            }
            self.ring.samples[cur & (MONITOR_WINDOW - 1)].store((s / n_ch).to_bits(), Ordering::Relaxed);
            cur = cur.wrapping_add(1);
        }
        self.ring.cursor.store(cur, Ordering::Release);
        firewheel::node::ProcessStatus::Bypass
    }
}

/// Blackman window (spec α = 0.16), applied before the FFT.
fn blackman(n: usize, len: usize) -> f32 {
    const A0: f32 = 0.42;
    const A1: f32 = 0.5;
    const A2: f32 = 0.08;
    let x = core::f32::consts::TAU * n as f32 / len as f32;
    A0 - A1 * x.cos() + A2 * (2.0 * x).cos()
}

/// In-place iterative radix-2 Cooley-Tukey. `re.len()` must be a power of two.
fn fft_in_place(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two() && im.len() == n);

    // Bit-reversal permutation
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    let mut len = 2;
    while len <= n {
        let ang = -core::f32::consts::TAU / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        let half = len / 2;
        let mut i = 0;
        while i < n {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..half {
                let (ur, ui) = (re[i + k], im[i + k]);
                let (br, bi) = (re[i + k + half], im[i + k + half]);
                let (vr, vi) = (br * cr - bi * ci, br * ci + bi * cr);
                re[i + k] = ur + vr;
                im[i + k] = ui + vi;
                re[i + k + half] = ur - vr;
                im[i + k + half] = ui - vi;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
            i += len;
        }
        len <<= 1;
    }
}

// ---------------------------------------------------------------------------
// Engine / node state
// ---------------------------------------------------------------------------

enum Params {
    /// The context's destination (the engine-owned master VolumeNode).
    Dest,
    Gain(VolumeNode),
    Pan(VolumePanNode),
    Svf(Box<SvfNode<2>>),
    /// PannerNode: equal-power / HRTF panner (see webapi/hrtf.rs).
    Spatial(WebPannerNode),
    Sampler(Box<SamplerNode>),
    /// AnalyserNode: bypass monitor capturing samples for the analysis getters.
    Analyser {
        ring: ArcGc<MonitorRing>,
        /// Smoothed magnitude spectrum (spec `smoothingTimeConstant`), sized on first use.
        smooth: Vec<f32>,
    },
    /// OscillatorNode: params shared with the audio thread (webapi/osc.rs).
    Osc(ArcGc<OscShared>),
    /// RedStreamSourceNode: the graph end of a streaming-PCM push (webapi/pcm.rs).
    /// The producer end lives in `pcm`'s own registry, keyed by the node handle.
    PcmStream(PcmStreamConfig),
    /// Generic no-op nodes: audio passes through untouched.
    Passthrough,
}

/// Web Audio distance models (spec formulas implemented in `distance_gain`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum DistModel {
    Inverse,
    Linear,
    Exponential,
}

struct NodeRec {
    engine: u64,
    id: NodeID,
    params: Params,
    /// Channel widths of the Firewheel node, mirrored here so `connect` can
    /// build a port list Firewheel will accept without reaching into the graph.
    /// Everything is stereo except `RedStreamSourceNode`.
    out_ch: u32,
    in_ch: u32,
    /// Outgoing connections (dst node handles) for `disconnect()`.
    outs: Vec<u64>,
    /// PannerNode world position.
    pos: [f32; 3],
    /// PannerNode orientation (cone direction; Web Audio default +x).
    orient: [f32; 3],
    /// PannerNode cone: [innerAngle°, outerAngle°, outerGain].
    cone: [f32; 3],
    /// PannerNode distance attenuation: model + [refDistance, maxDistance, rolloffFactor].
    dist_model: DistModel,
    dist: [f32; 3],
    // -- sampler bookkeeping --
    buffer: Option<Arc<PlanarBuffer>>,
    loop_: bool,
    started: bool,
    saw_playing: bool,
    ended: bool,
    /// The JS node object, protected while playing (spec keeps it alive).
    js_obj: u64,
    pending_start: Option<(f64, f64)>, // (when, offset)
    pending_stop: Option<f64>,
}

impl NodeRec {
    fn new(engine: u64, id: NodeID, params: Params) -> Self {
        let (in_ch, out_ch) = match &params {
            // The producer's interleave width IS the node's output width, and
            // a source node has no inputs.
            Params::PcmStream(cfg) => (0, cfg.channels),
            // The oscillator is a source too, but renders dual mono so it wires
            // like everything else.
            Params::Osc(_) => (0, 2),
            _ => (2, 2),
        };
        Self {
            engine,
            id,
            params,
            out_ch,
            in_ch,
            outs: Vec::new(),
            pos: [0.0; 3],
            orient: [1.0, 0.0, 0.0],
            cone: [360.0, 360.0, 0.0],
            dist_model: DistModel::Inverse,
            dist: [1.0, 10_000.0, 1.0], // Web Audio defaults: ref, max, rolloff
            buffer: None,
            loop_: false,
            started: false,
            saw_playing: false,
            ended: false,
            js_obj: 0,
            pending_start: None,
            pending_stop: None,
        }
    }
}

const ST_RUNNING: u8 = 0;
const ST_SUSPENDED: u8 = 1;
const ST_CLOSED: u8 = 2;

struct Engine {
    g: usize,
    fw: FirewheelContext,
    /// A cpal stream is actually running (false on headless machines — the
    /// graph API still works, the clock just doesn't advance).
    running: bool,
    state: u8,
    master: NodeID,
    master_params: VolumeNode,
    listener: [f32; 3],
    /// Listener orientation — rotates the panning space (Web Audio defaults).
    listener_fwd: [f32; 3],
    listener_up: [f32; 3],
    sample_rate: u32,
    /// `AudioContext.sinkId` exactly as JS last set it (`""` = system default).
    /// Kept as the string rather than the resolved device so the getter can
    /// return what was assigned, per spec.
    sink_id: String,
}

impl Engine {
    fn now(&self) -> f64 {
        self.fw.audio_clock().seconds.0
    }
}

/// How long `setSinkId` will wait for the previous output stream to finish
/// tearing down before giving up.
///
/// `FirewheelCtx::stop_stream` drops the CPAL stream, but the processor travels
/// back to the context through a ring buffer written from the audio thread, and
/// `start_stream` refuses until it lands. In practice the CPAL stream's `Drop`
/// has already joined that thread and the wait is zero iterations; the bound
/// exists so a wedged audio thread rejects a promise instead of hanging the VM.
const SINK_SWITCH_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1000);

/// Move an engine's output onto `device` (`None` = system default).
///
/// The node graph survives: `stop_stream` only deactivates the graph, and
/// `start_stream` recompiles the same one. Firewheel's clock survives too — it
/// is rescaled, not reset, if the new device runs at a different rate — so
/// `currentTime` stays monotonic and the `AudioParam` timelines keyed on it
/// (which store absolute times) keep their meaning across the switch.
fn restart_stream(eng: &mut Engine, device: Option<firewheel::cpal::cpal::DeviceId>) -> Result<(), String> {
    use firewheel::cpal::{CpalConfig, CpalOutputConfig};

    if eng.fw.is_audio_stream_running() {
        eng.fw.stop_stream();
    }
    let deadline = std::time::Instant::now() + SINK_SWITCH_TIMEOUT;
    while !eng.fw.can_start_stream() {
        if std::time::Instant::now() >= deadline {
            eng.running = false;
            return Err("the previous output stream did not finish stopping".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    let cfg = CpalConfig {
        output: CpalOutputConfig {
            device_id: device,
            // Keep the rate the context was built at. A device that cannot do
            // it still opens at its own rate (and `sample_rate` below picks
            // that up) — the request just avoids a gratuitous resample.
            desired_sample_rate: Some(eng.sample_rate),
            // NOT the CPAL default. Falling back to the system default here
            // would resolve `setSinkId` while the audio came out of a device
            // the user did not choose.
            fallback: false,
            ..Default::default()
        },
        input: None,
    };
    match eng.fw.start_stream(cfg) {
        Ok(()) => {
            eng.running = true;
            if let Some(rate) = eng.fw.stream_info().map(|i| i.sample_rate.get()) {
                eng.sample_rate = rate;
            }
            Ok(())
        }
        Err(e) => {
            eng.running = false;
            Err(format!("{e:?}"))
        }
    }
}

#[derive(Default)]
struct Glob {
    engines: HashMap<u64, Engine>,
    nodes: HashMap<u64, NodeRec>,
    timelines: HashMap<(u64, u8), Timeline>,
    next: u64,
}

// FirewheelCtx is !Send (its graph owns `Box<dyn DynAudioNode>`), but every
// access goes through this registry's mutex and only ever from the embedder
// thread that owns the JS context (host calls + pump run under the VM lock).
// Same assertion the WebState registry makes.
unsafe impl Send for Glob {}

fn glob() -> &'static Mutex<Glob> {
    static G: OnceLock<Mutex<Glob>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(Glob::default()))
}

/// `REDJS_AUDIO_LOG=1` → diagnostic prints for the whole audio path.
fn audio_log() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("REDJS_AUDIO_LOG").is_ok_and(|v| v == "1"))
}

macro_rules! alog {
    ($($arg:tt)*) => {
        if audio_log() {
            eprintln!("[audio] {}", format_args!($($arg)*));
        }
    };
}

/// Handles whose JS wrappers were finalized; drained on the next pump.
fn dead() -> &'static Mutex<Vec<u64>> {
    static D: OnceLock<Mutex<Vec<u64>>> = OnceLock::new();
    D.get_or_init(|| Mutex::new(Vec::new()))
}

/// Node host data: finalizer queues graph cleanup (GC frees JS wrappers of
/// disconnected nodes; the Firewheel node must go with them).
struct NodeHandle(u64);
impl Drop for NodeHandle {
    fn drop(&mut self) {
        dead().lock().unwrap().push(self.0);
    }
}

/// AudioContext host data: closing the wrapper closes the engine.
struct CtxHandle(u64);
impl Drop for CtxHandle {
    fn drop(&mut self) {
        dead().lock().unwrap().push(self.0);
    }
}

/// AudioListener host data (`0` = owner engine handle). No drop — the
/// listener dies together with its AudioContext.
struct ListenerHandle(#[allow(dead_code)] u64);

struct ParamRef {
    node: u64,
    kind: u8,
    /// Mirror of the last explicitly set value (`param.value` getter).
    last: f64,
}

struct BufData {
    buf: Arc<PlanarBuffer>,
}

// ---------------------------------------------------------------------------
// Per-context JS classes
// ---------------------------------------------------------------------------

pub struct AudioClasses {
    node_cls: crate::capi::JSClassRef,
    ctx_cls: crate::capi::JSClassRef,
    param_cls: crate::capi::JSClassRef,
    buffer_cls: crate::capi::JSClassRef,
    listener_cls: crate::capi::JSClassRef,
    /// Private slot: per-object bag holding AudioParam objects, destination,
    /// onended handler, the JS AudioBuffer, … (GC-owned lifetimes).
    s_audio: *mut c_void,
    node_base: Class,
    ctx: Class,
    gain: Class,
    stereo: Class,
    panner: Class,
    biquad: Class,
    analyser: Class,
    source: Class,
    osc: Class,
    stream_src: Class,
    dest: Class,
    listener: Class,
    param: Class,
    buffer: Class,
}

unsafe impl Send for AudioClasses {}
unsafe impl Sync for AudioClasses {}

fn class_registry() -> &'static Mutex<HashMap<usize, &'static AudioClasses>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static AudioClasses>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn classes(g: *mut c_void) -> &'static AudioClasses {
    class_registry().lock().unwrap().get(&(g as usize)).copied().expect("webapi::audio not installed")
}

/// `crate::context` fan-out. A gone context's engines are silenced and detached
/// from the address (`g = 0`), so a new context that lands on the same address
/// never has another context's `onended` handles pumped into it. Their graph
/// records go when the wrappers' finalizers report them, as before.
pub(crate) fn forget(gone: crate::context::Gone) {
    class_registry().lock().unwrap().remove(&gone.key);
    let mut gl = glob().lock().unwrap();
    for eng in gl.engines.values_mut().filter(|e| e.g == gone.key) {
        if eng.fw.is_audio_stream_running() {
            eng.fw.stop_stream();
        }
        eng.running = false;
        eng.state = ST_CLOSED;
        eng.g = 0;
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

unsafe fn queue_diff<T: Diff>(fw: &mut FirewheelContext, id: NodeID, old: &T, new: &T) {
    let mut evs: Vec<NodeEventType> = Vec::new();
    new.diff(old, PathBuilder::default(), &mut evs);
    for e in evs {
        fw.queue_event_for(id, e);
    }
}

fn v_dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn v_cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Normalize, or `fallback` for (near-)zero vectors.
fn v_norm(v: [f32; 3], fallback: [f32; 3]) -> [f32; 3] {
    let len = v_dot(v, v).sqrt();
    if len < 1e-6 {
        fallback
    } else {
        [v[0] / len, v[1] / len, v[2] / len]
    }
}

/// Web Audio cone gain: the angle between the source's orientation and the
/// source→listener vector, mapped 1 → coneOuterGain across the inner/outer
/// cone halves (spec §PannerNode "sound cones").
fn cone_gain(src_to_listener: [f32; 3], orient: [f32; 3], cone: [f32; 3]) -> f32 {
    let (inner, outer, outer_gain) = (cone[0], cone[1], cone[2].clamp(0.0, 1.0));
    if inner >= 360.0 && outer >= 360.0 {
        return 1.0;
    }
    let dist = v_dot(src_to_listener, src_to_listener).sqrt();
    let olen = v_dot(orient, orient).sqrt();
    if dist < 1e-6 || olen < 1e-6 {
        return 1.0; // degenerate — spec treats as no cone effect
    }
    let cos = (v_dot(src_to_listener, orient) / (dist * olen)).clamp(-1.0, 1.0);
    let angle = cos.acos().to_degrees();
    let half_inner = inner.min(360.0) / 2.0;
    let half_outer = outer.min(360.0) / 2.0;
    if angle <= half_inner {
        1.0
    } else if angle >= half_outer {
        outer_gain
    } else {
        let t = (angle - half_inner) / (half_outer - half_inner).max(1e-6);
        1.0 + (outer_gain - 1.0) * t
    }
}

/// Web Audio distance attenuation (MDN formulas, distances clamped per spec).
fn distance_gain(model: DistModel, dist: f32, ref_d: f32, max_d: f32, rolloff: f32) -> f32 {
    let ref_d = ref_d.max(1e-4);
    match model {
        DistModel::Inverse => {
            let d = dist.max(ref_d);
            ref_d / (ref_d + rolloff.max(0.0) * (d - ref_d))
        }
        DistModel::Linear => {
            if max_d <= ref_d {
                1.0
            } else {
                let d = dist.clamp(ref_d, max_d);
                (1.0 - rolloff.clamp(0.0, 1.0) * (d - ref_d) / (max_d - ref_d)).clamp(0.0, 1.0)
            }
        }
        DistModel::Exponential => {
            let d = dist.max(ref_d);
            (d / ref_d).powf(-rolloff.max(0.0))
        }
    }
}

/// Recompute a panner's derived params — Web Audio azimuth/elevation in the
/// LISTENER's basis (right = fwd×up; azimuth 0 = front, +90 = right) and the
/// combined distance × cone gain — then diff-push. `mutate` applies attribute
/// writes (panningModel etc.) first so everything lands in one event batch.
fn update_spatial_with(eng: &mut Engine, rec: &mut NodeRec, mutate: impl FnOnce(&mut WebPannerNode)) {
    let (pos, orient, cone) = (rec.pos, rec.orient, rec.cone);
    let (dist_model, dist_params) = (rec.dist_model, rec.dist);
    if let Params::Spatial(p) = &mut rec.params {
        let old = *p;
        mutate(p);

        let d = [
            pos[0] - eng.listener[0],
            pos[1] - eng.listener[1],
            pos[2] - eng.listener[2],
        ];
        let fwd = v_norm(eng.listener_fwd, [0.0, 0.0, -1.0]);
        let up = v_norm(eng.listener_up, [0.0, 1.0, 0.0]);
        let right = v_norm(v_cross(fwd, up), [1.0, 0.0, 0.0]);
        let (lx, ly, lz) = (v_dot(d, right), v_dot(d, up), v_dot(d, fwd));

        let dist = v_dot(d, d).sqrt();
        p.azimuth = lx.atan2(lz).to_degrees();
        p.elevation = if dist > 1e-6 {
            (ly / dist).clamp(-1.0, 1.0).asin().to_degrees()
        } else {
            0.0
        };

        let to_listener = [-d[0], -d[1], -d[2]];
        p.gain = distance_gain(dist_model, dist, dist_params[0], dist_params[1], dist_params[2])
            * cone_gain(to_listener, orient, cone);

        unsafe { queue_diff(&mut eng.fw, rec.id, &old, p) };
    }
}

fn update_spatial(eng: &mut Engine, rec: &mut NodeRec) {
    update_spatial_with(eng, rec, |_| {});
}

/// Route a param write to the owning Firewheel node.
fn set_param(engines: &mut HashMap<u64, Engine>, nodes: &mut HashMap<u64, NodeRec>, handle: u64, kind: u8, v: f64) {
    if kind >= P_L_POS_X {
        let Some(eng) = engines.get_mut(&handle) else { return };
        let i = (kind - P_L_POS_X) as usize;
        match i {
            0..=2 => eng.listener[i] = v as f32,
            3..=5 => eng.listener_fwd[i - 3] = v as f32,
            6..=8 => eng.listener_up[i - 6] = v as f32,
            _ => return,
        }
        for rec in nodes.values_mut().filter(|r| r.engine == handle) {
            update_spatial(eng, rec);
        }
        return;
    }
    let Some(rec) = nodes.get_mut(&handle) else { return };
    let Some(eng) = engines.get_mut(&rec.engine) else { return };
    unsafe {
        match (&mut rec.params, kind) {
            (Params::Gain(p), P_GAIN) => {
                let old = *p;
                p.volume = Volume::Linear(v as f32);
                queue_diff(&mut eng.fw, rec.id, &old, p);
            }
            (Params::Pan(p), P_PAN) => {
                let old = *p;
                p.pan = (v as f32).clamp(-1.0, 1.0);
                queue_diff(&mut eng.fw, rec.id, &old, p);
            }
            (Params::Svf(p), P_FREQ) => {
                let old = (**p).clone();
                p.cutoff_hz = (v as f32).clamp(20.0, 20_480.0);
                queue_diff(&mut eng.fw, rec.id, &old, &**p);
            }
            (Params::Svf(p), P_Q) => {
                let old = (**p).clone();
                p.q_factor = (v as f32).max(1e-3);
                queue_diff(&mut eng.fw, rec.id, &old, &**p);
            }
            (Params::Svf(p), P_GAIN) => {
                let old = (**p).clone();
                p.gain = Volume::Decibels(v as f32);
                queue_diff(&mut eng.fw, rec.id, &old, &**p);
            }
            (Params::Sampler(s), P_RATE) => {
                let baseline = (**s).clone();
                s.speed = v.max(0.0);
                queue_diff(&mut eng.fw, rec.id, &baseline, &**s);
            }
            // Oscillator params are plain atomic stores — no event round-trip,
            // and readable back by `param_current` for `.value`.
            (Params::Osc(sh), P_FREQ) => sh.set_frequency(v as f32),
            (Params::Osc(sh), P_DETUNE) => sh.set_detune(v as f32),
            (Params::Spatial(_), P_POS_X..=P_POS_Z) => {
                rec.pos[(kind - P_POS_X) as usize] = v as f32;
                update_spatial(eng, rec);
            }
            (Params::Spatial(_), P_ORIENT_X..=P_ORIENT_Z) => {
                rec.orient[(kind - P_ORIENT_X) as usize] = v as f32;
                update_spatial(eng, rec);
            }
            _ => {}
        }
    }
}

/// Start a scheduled source: the sampler pushes its full state, the oscillator
/// opens its gate. Both are driven by `src_start` / the pump's `pending_start`.
fn do_start(eng: &mut Engine, rec: &mut NodeRec, offset: f64) {
    if let Params::Osc(sh) = &rec.params {
        // `offset` is meaningless for an oscillator (the spec's `start(when)`
        // takes no offset), so it is ignored rather than misinterpreted.
        alog!("osc start node={:?} freq={}", rec.id, sh.frequency());
        sh.start();
        return;
    }
    let Params::Sampler(s) = &mut rec.params else { return };
    alog!(
        "do_start node={:?} buffer_frames={:?} loop={} speed={} offset={offset}",
        rec.id,
        rec.buffer.as_ref().map(|b| b.data.first().map(|c| c.len()).unwrap_or(0)),
        rec.loop_,
        s.speed
    );
    // Param changes go through Diff (NOT the sync_*_event helpers): in
    // firewheel 0.10 `sync_play_event()` emits `ParamData::Bool`, but the
    // derived Patch for `play: Notify<bool>` only accepts
    // `ParamData::any(Notify<bool>)` — the event fails to downcast and is
    // silently dropped, so the sampler never starts. Diff emits the correct
    // payloads for every field (Notify's DerefMut bumps its counter).
    let baseline = (**s).clone();
    if let Some(buf) = &rec.buffer {
        let arc = buf.clone();
        s.set_sample(ArcGc::new_unsized(|| Arc::new(SharedSample(arc)) as Arc<dyn SampleResource>));
    }
    s.repeat_mode = if rec.loop_ { RepeatMode::RepeatEndlessly } else { RepeatMode::PlayOnce };
    s.start_from(if offset > 0.0 { PlayFrom::Seconds(offset) } else { PlayFrom::Seconds(0.0) });
    unsafe { queue_diff(&mut eng.fw, rec.id, &baseline, &**s) };
}

fn do_stop(eng: &mut Engine, rec: &mut NodeRec) {
    if let Params::Osc(sh) = &rec.params {
        sh.stop();
        return;
    }
    let Params::Sampler(s) = &mut rec.params else { return };
    let baseline = (**s).clone();
    s.stop();
    unsafe { queue_diff(&mut eng.fw, rec.id, &baseline, &**s) };
}

unsafe fn this_node_handle(g: *mut c_void, cf: *mut c_void) -> Option<u64> {
    host_data::<NodeHandle>(g, classes(g).node_cls, cf_this(cf)).map(|h| h.0)
}

unsafe fn this_engine_handle(g: *mut c_void, cf: *mut c_void) -> Option<u64> {
    host_data::<CtxHandle>(g, classes(g).ctx_cls, cf_this(cf)).map(|h| h.0)
}

/// Creates the JS wrapper for a node: host object + private slot bag with
/// its eagerly created AudioParam objects (stable identity, GC-tied).
unsafe fn wrap_node(g: *mut c_void, proto: u64, handle: u64, params: &[(&str, u8, f64)]) -> u64 {
    let c = classes(g);
    let obj = host_create(g, c.node_cls, proto, NodeHandle(handle));
    let slots = new_object(g, UNDEF);
    glue::rjs_define(g, obj, c.s_audio, slots, 0);
    for &(name, kind, initial) in params {
        let p = host_create(g, c.param_cls, c.param.proto, ParamRef { node: handle, kind, last: initial });
        define(g, slots, name, p, 0);
    }
    obj
}

unsafe fn slots_of(g: *mut c_void, obj: u64) -> u64 {
    get_a(g, obj, classes(g).s_audio)
}

/// Open the producer→processor channel of a `StreamWriterNode` and publish the
/// producer half in `pcm`'s registry under `handle`.
///
/// Runs at node-creation time, on the JS thread, because it is the only moment
/// the `FirewheelContext` and the node's custom state are both reachable. After
/// this returns, pushes never touch the graph again (see webapi/pcm.rs).
fn open_pcm_channel(eng: &mut Engine, id: NodeID, handle: u64, cfg: PcmStreamConfig) -> bool {
    let (Some(in_rate), Some(out_rate)) =
        (NonZeroU32::new(cfg.sample_rate), NonZeroU32::new(eng.sample_rate))
    else {
        return false;
    };
    let chan = ResamplingChannelConfig {
        latency_seconds: cfg.latency_seconds,
        capacity_seconds: cfg.capacity_seconds,
        ..Default::default()
    };
    let (ev, producer) = {
        let Some(st) = eng.fw.node_state_mut::<StreamWriterState>(id) else { return false };
        let Ok(ev) = st.start_stream(in_rate, out_rate, chan) else { return false };
        // The clone shares the producer `Arc` and the atomics with the state
        // the node owns, so either side closing the stream closes it for both.
        // That is the intent: `close()` from JS and the node's own death must
        // both leave a native pusher holding a handle that reports NotReady
        // rather than a dangling one.
        (ev, st.clone())
    };
    eng.fw.queue_event_for(id, NodeEventType::custom(ev));
    pcm::register(handle, producer, cfg);
    true
}

/// Allocates a node in the graph + registry. Returns the new handle.
fn alloc_node(gl: &mut Glob, engine: u64, params: Params) -> Option<u64> {
    let eng = gl.engines.get_mut(&engine)?;
    if eng.state == ST_CLOSED {
        return None;
    }
    // Taken before the graph node exists: `Params::PcmStream` publishes its
    // producer under this handle, and JS hands the same number to the native
    // side as `streamId`.
    let handle = gl.next;
    gl.next += 1;
    let id = match &params {
        Params::Gain(p) => eng.fw.add_node(*p, None),
        Params::Pan(p) => eng.fw.add_node(*p, None),
        // The HRIR database loads once per stream rate (~136 KB flac decode);
        // None (decode failure) degrades HRTF mode to equal-power inside the node.
        Params::Spatial(p) => eng.fw.add_node(
            *p,
            Some(WebPannerConfig { db: HrtfDatabase::for_rate(eng.sample_rate) }),
        ),
        Params::Svf(p) => eng.fw.add_node((**p).clone(), None),
        Params::Sampler(s) => eng.fw.add_node((**s).clone(), Some(SamplerConfig::default())),
        Params::Analyser { ring, .. } => eng.fw.add_node(MonitorNode { ring: ArcGc::clone(ring) }, None),
        Params::Osc(sh) => eng.fw.add_node(OscNode { shared: ArcGc::clone(sh) }, None),
        Params::PcmStream(cfg) => {
            let ch = firewheel::channel_config::NonZeroChannelCount::new(cfg.channels as u32)
                .unwrap_or(firewheel::channel_config::NonZeroChannelCount::STEREO);
            eng.fw.add_node(
                StreamWriterNode,
                Some(StreamWriterConfig { channels: ch, check_for_silence: true }),
            )
        }
        Params::Passthrough => eng.fw.add_node(VolumeNode::default(), None),
        Params::Dest => eng.master,
    };

    // Streaming PCM needs a second step the other nodes do not: opening the
    // producer→processor channel, and publishing the producer half under
    // `handle` so a native thread can find it (webapi/pcm.rs).
    if let Params::PcmStream(cfg) = &params {
        if !open_pcm_channel(eng, id, handle, *cfg) {
            // The channel could not be opened (no output stream yet, or the
            // node state vanished). Keep the node — it renders silence — but
            // say so, because "connected and mute" is otherwise indetectable.
            alog!("RedStreamSourceNode {handle}: no producer channel (running={})", eng.running);
        }
    }

    gl.nodes.insert(handle, NodeRec::new(engine, id, params));
    Some(handle)
}

// ---------------------------------------------------------------------------
// AudioNode base: connect / disconnect
// ---------------------------------------------------------------------------

const STEREO_PORTS: &[(u32, u32)] = &[(0, 0), (1, 1)];

/// The port pairs to wire between a source with `src_out` output channels and a
/// destination with `dst_in` input channels.
///
/// Everything in this graph is stereo except `RedStreamSourceNode`, which is as
/// wide as the producer's frames. Firewheel's `connect` refuses the WHOLE call
/// if any port index is out of range, so a fixed `[(0,0),(1,1)]` would make a
/// mono stream source connect to nothing at all — silently, since the result is
/// discarded. Deriving the pairs also gets the spec's mono up-mix right: a
/// 1-channel source feeds BOTH inputs of a stereo destination (centre), not
/// just the left one.
fn ports_between(src_out: u32, dst_in: u32) -> &'static [(u32, u32)] {
    match (src_out.max(1).min(2), dst_in.max(1).min(2)) {
        // Mono → stereo: the spec's up-mix, one source port into both inputs.
        (1, 2) => &[(0, 0), (0, 1)],
        // Anything → mono: port 0 only. A true down-mix would need a summing
        // node; nothing in this graph has a mono input today.
        (_, 1) => &[(0, 0)],
        _ => STEREO_PORTS,
    }
}

unsafe extern "sysv64" fn node_connect(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let Some(src) = this_node_handle(g, cf) else {
        return throw_type_error(g, "AudioNode.connect called on wrong this");
    };
    let dst_v = cf_arg(cf, 0);
    let Some(dst) = host_data::<NodeHandle>(g, c.node_cls, dst_v).map(|h| h.0) else {
        return throw_type_error(g, "AudioNode.connect: destination is not an AudioNode");
    };
    let mut gl = glob().lock().unwrap();
    let gl = &mut *gl;
    let (Some(sr), Some(dr)) = (gl.nodes.get(&src), gl.nodes.get(&dst)) else {
        return throw_type_error(g, "AudioNode.connect: node is gone");
    };
    if sr.engine != dr.engine {
        return throw_type_error(g, "AudioNode.connect: nodes belong to different AudioContexts");
    }
    let (sid, did, engine) = (sr.id, dr.id, sr.engine);
    let ports = ports_between(sr.out_ch, dr.in_ch);
    if let Some(eng) = gl.engines.get_mut(&engine) {
        if let Err(e) = eng.fw.connect(sid, did, ports, true) {
            // Loud, because the symptom of a dropped edge is silence with a
            // graph that looks correctly wired from JS.
            eprintln!("[audio] connect failed ({e:?}) — {sid:?} → {did:?} on ports {ports:?}");
        }
    }
    gl.nodes.get_mut(&src).unwrap().outs.push(dst);
    dst_v
}

unsafe extern "sysv64" fn node_disconnect(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let Some(src) = this_node_handle(g, cf) else {
        return throw_type_error(g, "AudioNode.disconnect called on wrong this");
    };
    let only = host_data::<NodeHandle>(g, c.node_cls, cf_arg(cf, 0)).map(|h| h.0);
    let mut gl = glob().lock().unwrap();
    let gl = &mut *gl;
    let Some(sr) = gl.nodes.get_mut(&src) else { return UNDEF };
    let sid = sr.id;
    let engine = sr.engine;
    let src_ch = sr.out_ch;
    let removed: Vec<u64> = match only {
        Some(d) => {
            sr.outs.retain(|&o| o != d);
            vec![d]
        }
        None => core::mem::take(&mut sr.outs),
    };
    if let Some(eng) = gl.engines.get_mut(&engine) {
        for d in removed {
            if let Some(dr) = gl.nodes.get(&d) {
                // Same pairs `connect` used, or the edges do not match.
                let _ = eng.fw.disconnect(sid, dr.id, ports_between(src_ch, dr.in_ch));
            }
        }
    }
    UNDEF
}

// ---------------------------------------------------------------------------
// AudioParam
// ---------------------------------------------------------------------------

unsafe fn this_param<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut ParamRef> {
    host_data::<ParamRef>(g, classes(g).param_cls, cf_this(cf))
}

/// Current committed value of a param (for `.value` + timeline anchoring).
fn param_current(gl: &Glob, node: u64, kind: u8, fallback: f64) -> f64 {
    if kind >= P_L_POS_X {
        if let Some(eng) = gl.engines.get(&node) {
            let i = (kind - P_L_POS_X) as usize;
            match i {
                0..=2 => return eng.listener[i] as f64,
                3..=5 => return eng.listener_fwd[i - 3] as f64,
                6..=8 => return eng.listener_up[i - 6] as f64,
                _ => {}
            }
        }
        return fallback;
    }
    let Some(rec) = gl.nodes.get(&node) else { return fallback };
    match (&rec.params, kind) {
        (Params::Gain(p), P_GAIN) => match p.volume {
            Volume::Linear(v) => v as f64,
            Volume::Decibels(d) => 10f64.powf(d as f64 / 20.0),
        },
        (Params::Pan(p), P_PAN) => p.pan as f64,
        (Params::Svf(p), P_FREQ) => p.cutoff_hz as f64,
        (Params::Svf(p), P_Q) => p.q_factor as f64,
        (Params::Sampler(s), P_RATE) => s.speed,
        (Params::Osc(sh), P_FREQ) => sh.frequency() as f64,
        (Params::Osc(sh), P_DETUNE) => sh.detune() as f64,
        (Params::Spatial(_), P_POS_X..=P_POS_Z) => rec.pos[(kind - P_POS_X) as usize] as f64,
        (Params::Spatial(_), P_ORIENT_X..=P_ORIENT_Z) => rec.orient[(kind - P_ORIENT_X) as usize] as f64,
        _ => fallback,
    }
}

unsafe extern "sysv64" fn param_get_value(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_param(g, cf) {
        Some(p) => {
            let gl = glob().lock().unwrap();
            enc_f64(param_current(&gl, p.node, p.kind, p.last))
        }
        None => throw_type_error(g, "AudioParam.value on wrong this"),
    }
}

unsafe extern "sysv64" fn param_set_value(g: *mut c_void, cf: *mut c_void) -> u64 {
    let v = dec_f64(cf_arg(cf, 0)).unwrap_or(0.0);
    let Some(p) = this_param(g, cf) else {
        return throw_type_error(g, "AudioParam.value on wrong this");
    };
    p.last = v;
    let (node, kind) = (p.node, p.kind);
    let mut gl = glob().lock().unwrap();
    let gl = &mut *gl;
    gl.timelines.remove(&(node, kind));
    set_param(&mut gl.engines, &mut gl.nodes, node, kind, v);
    UNDEF
}

unsafe fn param_schedule(g: *mut c_void, cf: *mut c_void, mk: impl FnOnce(f64, f64) -> Tl) -> u64 {
    let v = dec_f64(cf_arg(cf, 0)).unwrap_or(0.0);
    let t = dec_f64(cf_arg(cf, 1)).unwrap_or(0.0);
    let this = cf_this(cf);
    let Some(p) = this_param(g, cf) else {
        return throw_type_error(g, "AudioParam method on wrong this");
    };
    p.last = v;
    let (node, kind) = (p.node, p.kind);
    let mut gl = glob().lock().unwrap();
    let cur = param_current(&gl, node, kind, v);
    let tl = gl.timelines.entry((node, kind)).or_insert_with(|| Timeline { evs: Vec::new(), start_v: cur });
    let ev = mk(v, t);
    // keep sorted by time (stable for equal timestamps)
    let at = tl.evs.partition_point(|e| e.time() <= ev.time());
    tl.evs.insert(at, ev);
    this
}

unsafe extern "sysv64" fn param_set_value_at(g: *mut c_void, cf: *mut c_void) -> u64 {
    param_schedule(g, cf, |v, t| Tl::Set { t, v })
}

unsafe extern "sysv64" fn param_linear_ramp(g: *mut c_void, cf: *mut c_void) -> u64 {
    param_schedule(g, cf, |v, t| Tl::Lin { t, v })
}

unsafe extern "sysv64" fn param_exp_ramp(g: *mut c_void, cf: *mut c_void) -> u64 {
    param_schedule(g, cf, |v, t| Tl::Exp { t, v })
}

unsafe extern "sysv64" fn param_set_target(g: *mut c_void, cf: *mut c_void) -> u64 {
    let tau = dec_f64(cf_arg(cf, 2)).unwrap_or(0.01);
    param_schedule(g, cf, move |v, t| Tl::Target { t, v, tau })
}

unsafe extern "sysv64" fn param_cancel(g: *mut c_void, cf: *mut c_void) -> u64 {
    let t = dec_f64(cf_arg(cf, 0)).unwrap_or(0.0);
    let this = cf_this(cf);
    let Some(p) = this_param(g, cf) else {
        return throw_type_error(g, "AudioParam.cancelScheduledValues on wrong this");
    };
    let key = (p.node, p.kind);
    let mut gl = glob().lock().unwrap();
    if let Some(tl) = gl.timelines.get_mut(&key) {
        tl.evs.retain(|e| e.time() < t);
        if tl.evs.is_empty() {
            gl.timelines.remove(&key);
        }
    }
    this
}

// ---------------------------------------------------------------------------
// AudioBuffer
// ---------------------------------------------------------------------------

unsafe fn this_buf<'a>(g: *mut c_void, cf: *mut c_void) -> Option<&'a mut BufData> {
    host_data::<BufData>(g, classes(g).buffer_cls, cf_this(cf))
}

unsafe fn wrap_buffer(g: *mut c_void, buf: Arc<PlanarBuffer>) -> u64 {
    let c = classes(g);
    host_create(g, c.buffer_cls, c.buffer.proto, BufData { buf })
}

unsafe extern "sysv64" fn buf_get_sample_rate(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_buf(g, cf) {
        Some(b) => enc_f64(b.buf.sample_rate as f64),
        None => throw_type_error(g, "AudioBuffer.sampleRate on wrong this"),
    }
}

unsafe extern "sysv64" fn buf_get_length(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_buf(g, cf) {
        Some(b) => enc_f64(b.buf.data.first().map(|c| c.len()).unwrap_or(0) as f64),
        None => throw_type_error(g, "AudioBuffer.length on wrong this"),
    }
}

unsafe extern "sysv64" fn buf_get_duration(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_buf(g, cf) {
        Some(b) => {
            let frames = b.buf.data.first().map(|c| c.len()).unwrap_or(0) as f64;
            enc_f64(frames / (b.buf.sample_rate.max(1) as f64))
        }
        None => throw_type_error(g, "AudioBuffer.duration on wrong this"),
    }
}

unsafe extern "sysv64" fn buf_get_channels(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_buf(g, cf) {
        Some(b) => enc_i32(b.buf.data.len() as i32),
        None => throw_type_error(g, "AudioBuffer.numberOfChannels on wrong this"),
    }
}

unsafe extern "sysv64" fn buf_get_channel_data(g: *mut c_void, cf: *mut c_void) -> u64 {
    let i = dec_f64(cf_arg(cf, 0)).unwrap_or(0.0) as usize;
    let Some(b) = this_buf(g, cf) else {
        return throw_type_error(g, "AudioBuffer.getChannelData on wrong this");
    };
    let Some(ch) = b.buf.data.get(i) else {
        return throw_error(g, 2, "AudioBuffer.getChannelData: channel index out of range");
    };
    // A copy, not the live store (spec returns the live buffer; the engine
    // only reads decoded data, and copies keep the sampler race-free).
    let mut out = UNDEF;
    if crate::ffi::redjs_typedarray_create(g, 8 /* f32 */, ch.len(), &mut out) != 0 {
        return UNDEF;
    }
    let mut ta = crate::ffi::RjsTaInfo::default();
    if glue::rjs_typedarray_info(g, out, &mut ta) == 0 && !ta.data.is_null() {
        core::ptr::copy_nonoverlapping(ch.as_ptr(), ta.data as *mut f32, ch.len());
    }
    out
}

unsafe extern "sysv64" fn buf_copy_to_channel(g: *mut c_void, cf: *mut c_void) -> u64 {
    let src = cf_arg(cf, 0);
    let ch = dec_f64(cf_arg(cf, 1)).unwrap_or(0.0) as usize;
    let start = dec_f64(cf_arg(cf, 2)).unwrap_or(0.0) as usize;
    let Some(b) = this_buf(g, cf) else {
        return throw_type_error(g, "AudioBuffer.copyToChannel on wrong this");
    };
    let mut ta = crate::ffi::RjsTaInfo::default();
    if glue::rjs_typedarray_info(g, src, &mut ta) != 0 || ta.data.is_null() || ta.kind != 8 {
        return throw_type_error(g, "AudioBuffer.copyToChannel: source must be a Float32Array");
    }
    let src_slice = core::slice::from_raw_parts(ta.data as *const f32, ta.length as usize);
    // Playing samplers hold Arc clones; make_mut keeps them race-free.
    let data = Arc::make_mut(&mut b.buf);
    if let Some(chan) = data.data.get_mut(ch) {
        let start = start.min(chan.len());
        let n = src_slice.len().min(chan.len() - start);
        chan[start..start + n].copy_from_slice(&src_slice[..n]);
    }
    UNDEF
}

impl Clone for PlanarBuffer {
    fn clone(&self) -> Self {
        Self { data: self.data.clone(), sample_rate: self.sample_rate }
    }
}

// ---------------------------------------------------------------------------
// AudioBufferSourceNode
// ---------------------------------------------------------------------------

unsafe extern "sysv64" fn src_get_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    let slots = slots_of(g, cf_this(cf));
    get(g, slots, "buffer")
}

unsafe extern "sysv64" fn src_set_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let v = cf_arg(cf, 0);
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, "AudioBufferSourceNode.buffer on wrong this");
    };
    let arc = host_data::<BufData>(g, c.buffer_cls, v).map(|b| b.buf.clone());
    {
        let mut gl = glob().lock().unwrap();
        if let Some(rec) = gl.nodes.get_mut(&handle) {
            rec.buffer = arc;
        }
    }
    let slots = slots_of(g, cf_this(cf));
    define(g, slots, "buffer", if is_cell(v) { v } else { NULL }, 0);
    UNDEF
}

unsafe extern "sysv64" fn src_get_loop(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, "AudioBufferSourceNode.loop on wrong this");
    };
    let gl = glob().lock().unwrap();
    enc_bool(gl.nodes.get(&handle).map(|r| r.loop_).unwrap_or(false))
}

unsafe extern "sysv64" fn src_set_loop(g: *mut c_void, cf: *mut c_void) -> u64 {
    let v = cf_arg(cf, 0) == 0x7; // true
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, "AudioBufferSourceNode.loop on wrong this");
    };
    let mut gl = glob().lock().unwrap();
    let gl = &mut *gl;
    if let Some(rec) = gl.nodes.get_mut(&handle) {
        rec.loop_ = v;
        if rec.started {
            if let (Params::Sampler(s), Some(eng)) = (&mut rec.params, gl.engines.get_mut(&rec.engine)) {
                let baseline = (**s).clone();
                s.repeat_mode = if v { RepeatMode::RepeatEndlessly } else { RepeatMode::PlayOnce };
                queue_diff(&mut eng.fw, rec.id, &baseline, &**s);
            }
        }
    }
    UNDEF
}

unsafe extern "sysv64" fn src_get_onended(g: *mut c_void, cf: *mut c_void) -> u64 {
    let slots = slots_of(g, cf_this(cf));
    get(g, slots, "onended")
}

unsafe extern "sysv64" fn src_set_onended(g: *mut c_void, cf: *mut c_void) -> u64 {
    let slots = slots_of(g, cf_this(cf));
    define(g, slots, "onended", cf_arg(cf, 0), 0);
    UNDEF
}

/// `AudioScheduledSourceNode.start(when[, offset])` — shared by
/// `AudioBufferSourceNode` and `OscillatorNode`; `do_start` dispatches on the
/// node's `Params` (`offset` is ignored for an oscillator, which has none).
unsafe extern "sysv64" fn src_start(g: *mut c_void, cf: *mut c_void) -> u64 {
    let when = dec_f64(cf_arg(cf, 0)).unwrap_or(0.0);
    let offset = dec_f64(cf_arg(cf, 1)).unwrap_or(0.0);
    let this = cf_this(cf);
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, "AudioScheduledSourceNode.start on wrong this");
    };
    let mut gl = glob().lock().unwrap();
    let gl = &mut *gl;
    let Some(rec) = gl.nodes.get_mut(&handle) else { return UNDEF };
    if rec.started {
        // Spec: InvalidStateError. A source is start-once, restarting it means
        // creating another one — cheap, by design.
        return super::domexception::throw(g, "InvalidStateError", "start() has already been called on this source");
    }
    rec.started = true;
    // Spec: a playing source is kept alive even without JS references.
    rec.js_obj = this;
    crate::ffi::redjs_protect(g, this);
    let Some(eng) = gl.engines.get_mut(&rec.engine) else { return UNDEF };
    if when <= eng.now() || !eng.running {
        do_start(eng, rec, offset);
    } else {
        rec.pending_start = Some((when, offset));
    }
    UNDEF
}

unsafe extern "sysv64" fn src_stop(g: *mut c_void, cf: *mut c_void) -> u64 {
    let when = dec_f64(cf_arg(cf, 0)).unwrap_or(0.0);
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, "AudioScheduledSourceNode.stop on wrong this");
    };
    let mut gl = glob().lock().unwrap();
    let gl = &mut *gl;
    let Some(rec) = gl.nodes.get_mut(&handle) else { return UNDEF };
    let Some(eng) = gl.engines.get_mut(&rec.engine) else { return UNDEF };
    if when <= eng.now() || !eng.running {
        rec.pending_start = None;
        do_stop(eng, rec);
    } else {
        rec.pending_stop = Some(when);
    }
    UNDEF
}

// ---------------------------------------------------------------------------
// OscillatorNode.type
// ---------------------------------------------------------------------------

unsafe extern "sysv64" fn osc_get_type(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, "OscillatorNode.type on wrong this");
    };
    let gl = glob().lock().unwrap();
    let name = match gl.nodes.get(&handle).map(|r| &r.params) {
        Some(Params::Osc(sh)) => sh.wave().name(),
        _ => return throw_type_error(g, "OscillatorNode.type on a node that is not an oscillator"),
    };
    drop(gl);
    js_string(g, name)
}

unsafe extern "sysv64" fn osc_set_type(g: *mut c_void, cf: *mut c_void) -> u64 {
    let raw = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, "OscillatorNode.type on wrong this");
    };
    let Some(w) = Wave::from_name(&raw) else {
        // Named, not silently coerced: a `'custom'` assignment that quietly
        // kept playing a sine would be a sound bug, and there is no
        // `createPeriodicWave` on this runtime to make it real.
        return throw_type_error(
            g,
            &format!(
                "OscillatorNode.type: '{raw}' is not one of 'sine' | 'square' | 'sawtooth' | \
                 'triangle'. 'custom' needs createPeriodicWave(), which this runtime does not \
                 implement — synthesize the waveform into an AudioBuffer instead."
            ),
        );
    };
    let mut gl = glob().lock().unwrap();
    match gl.nodes.get_mut(&handle).map(|r| &r.params) {
        Some(Params::Osc(sh)) => sh.set_wave(w),
        _ => return throw_type_error(g, "OscillatorNode.type on a node that is not an oscillator"),
    }
    UNDEF
}

// ---------------------------------------------------------------------------
// RedStreamSourceNode (webapi/pcm.rs) — the streaming-PCM seam
// ---------------------------------------------------------------------------

/// The `pcm::PcmSink` for `this`, or `None` if the node is not a stream source
/// or has been closed.
unsafe fn this_sink(g: *mut c_void, cf: *mut c_void) -> Option<pcm::PcmSink> {
    pcm::sink(this_node_handle(g, cf)?)
}

/// `node.push(Float32Array)` — the JS-side counterpart of `PcmSink::push_interleaved`.
///
/// Present mostly so the seam is testable and demoable without writing native
/// code; the production producer is native and never crosses the VM lock.
/// Returns the number of frames accepted (0 when the graph is not consuming).
unsafe extern "sysv64" fn stream_push(g: *mut c_void, cf: *mut c_void) -> u64 {
    let src = cf_arg(cf, 0);
    let Some(sink) = this_sink(g, cf) else {
        // A closed node accepting pushes silently would look identical to a
        // working one that is muted. Say which it is.
        return throw_error(g, 0, "RedStreamSourceNode.push: the stream is closed");
    };
    let mut ta = crate::ffi::RjsTaInfo::default();
    if glue::rjs_typedarray_info(g, src, &mut ta) != 0 || ta.data.is_null() || ta.kind != 8 {
        return throw_type_error(g, "RedStreamSourceNode.push: expected an interleaved Float32Array");
    }
    let data = core::slice::from_raw_parts(ta.data as *const f32, ta.length as usize);
    let frames = data.len() / sink.channels().max(1) as usize;
    let accepted = match sink.push_interleaved(data) {
        pcm::PushOutcome::Ok | pcm::PushOutcome::UnderflowCorrected { .. } => frames,
        pcm::PushOutcome::Overflow { frames_pushed } => frames_pushed,
        pcm::PushOutcome::NotReady => 0,
    };
    enc_f64(accepted as f64)
}

/// `node.close()` — retire the producer. The node keeps its place in the graph
/// (and renders silence) until it is disconnected and collected; only the push
/// side goes away, so a native producer still holding a `PcmSink` starts
/// getting `NotReady` instead of writing into a stream nobody reads.
unsafe extern "sysv64" fn stream_close(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Some(h) = this_node_handle(g, cf) {
        pcm::unregister(h);
    }
    UNDEF
}

/// Seconds of audio buffered ahead of the device; `null` when not streaming.
unsafe extern "sysv64" fn stream_get_queued(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_sink(g, cf).and_then(|s| s.occupied_seconds()) {
        Some(v) => enc_f64(v),
        None => NULL,
    }
}

unsafe extern "sysv64" fn stream_get_available(g: *mut c_void, cf: *mut c_void) -> u64 {
    enc_f64(this_sink(g, cf).map(|s| s.available_frames()).unwrap_or(0) as f64)
}

/// Read-AND-CLEAR, exactly like the native side: reading tells you whether it
/// happened since your last read, so a meter cannot latch on forever.
unsafe extern "sysv64" fn stream_get_underflowed(g: *mut c_void, cf: *mut c_void) -> u64 {
    enc_bool(this_sink(g, cf).map(|s| s.underflowed()).unwrap_or(false))
}

unsafe extern "sysv64" fn stream_get_overflowed(g: *mut c_void, cf: *mut c_void) -> u64 {
    enc_bool(this_sink(g, cf).map(|s| s.overflowed()).unwrap_or(false))
}

/// Is the graph actually pulling frames? False while the context has no output
/// stream, or before the processor's first block.
unsafe extern "sysv64" fn stream_get_ready(g: *mut c_void, cf: *mut c_void) -> u64 {
    enc_bool(this_sink(g, cf).map(|s| s.is_ready()).unwrap_or(false))
}

// ---------------------------------------------------------------------------
// BiquadFilterNode.type / AnalyserNode extras
// ---------------------------------------------------------------------------

unsafe extern "sysv64" fn biquad_get_type(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, "BiquadFilterNode.type on wrong this");
    };
    let gl = glob().lock().unwrap();
    let s = match gl.nodes.get(&handle).map(|r| &r.params) {
        Some(Params::Svf(p)) => match p.filter_type {
            SvfType::Lowpass | SvfType::LowpassX2 => "lowpass",
            SvfType::Highpass | SvfType::HighpassX2 => "highpass",
            SvfType::Bandpass => "bandpass",
            SvfType::LowShelf => "lowshelf",
            SvfType::HighShelf => "highshelf",
            _ => "peaking",
        },
        _ => "lowpass",
    };
    js_string(g, s)
}

unsafe extern "sysv64" fn biquad_set_type(g: *mut c_void, cf: *mut c_void) -> u64 {
    let ty = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, "BiquadFilterNode.type on wrong this");
    };
    let mut gl = glob().lock().unwrap();
    let gl = &mut *gl;
    let Some(rec) = gl.nodes.get_mut(&handle) else { return UNDEF };
    let Some(eng) = gl.engines.get_mut(&rec.engine) else { return UNDEF };
    if let Params::Svf(p) = &mut rec.params {
        let old = (**p).clone();
        p.filter_type = match ty.as_str() {
            "highpass" => SvfType::Highpass,
            "bandpass" => SvfType::Bandpass,
            "lowshelf" => SvfType::LowShelf,
            "highshelf" => SvfType::HighShelf,
            "peaking" => SvfType::Bell,
            _ => SvfType::Lowpass,
        };
        queue_diff(&mut eng.fw, rec.id, &old, &**p);
    }
    UNDEF
}

// ---------------------------------------------------------------------------
// PannerNode attributes (W-MIX MIX-14/20): distance model + cone + panning model
// ---------------------------------------------------------------------------

/// Read a PannerNode value under the glob lock (0.0 for non-panner `this`).
unsafe fn panner_read(g: *mut c_void, cf: *mut c_void, read: impl FnOnce(&NodeRec) -> f64) -> u64 {
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, "PannerNode attribute on wrong this");
    };
    let gl = glob().lock().unwrap();
    match gl.nodes.get(&handle) {
        Some(rec) if matches!(rec.params, Params::Spatial(_)) => enc_f64(read(rec)),
        _ => enc_f64(0.0),
    }
}

/// Mutate the panner's record fields, then re-derive + diff-push.
unsafe fn panner_write(g: *mut c_void, cf: *mut c_void, apply: impl FnOnce(&mut NodeRec)) -> u64 {
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, "PannerNode attribute on wrong this");
    };
    let mut gl = glob().lock().unwrap();
    let gl = &mut *gl;
    let Some(rec) = gl.nodes.get_mut(&handle) else { return UNDEF };
    let Some(eng) = gl.engines.get_mut(&rec.engine) else { return UNDEF };
    apply(rec);
    update_spatial(eng, rec);
    UNDEF
}

/// [refDistance, maxDistance, rolloffFactor] and [inner°, outer°, outerGain] accessors.
macro_rules! panner_f32_accessor {
    ($get:ident, $set:ident, $field:ident, $idx:expr, $clamp:expr) => {
        unsafe extern "sysv64" fn $get(g: *mut c_void, cf: *mut c_void) -> u64 {
            panner_read(g, cf, |rec| rec.$field[$idx] as f64)
        }
        unsafe extern "sysv64" fn $set(g: *mut c_void, cf: *mut c_void) -> u64 {
            let v = dec_f64(cf_arg(cf, 0)).unwrap_or(0.0) as f32;
            panner_write(g, cf, |rec| rec.$field[$idx] = ($clamp)(v))
        }
    };
}
panner_f32_accessor!(panner_get_ref, panner_set_ref, dist, 0, |v: f32| v.max(1e-4));
panner_f32_accessor!(panner_get_max, panner_set_max, dist, 1, |v: f32| v.max(0.0));
panner_f32_accessor!(panner_get_rolloff, panner_set_rolloff, dist, 2, |v: f32| v.max(0.0));
panner_f32_accessor!(panner_get_cone_inner, panner_set_cone_inner, cone, 0, |v: f32| v);
panner_f32_accessor!(panner_get_cone_outer, panner_set_cone_outer, cone, 1, |v: f32| v);
panner_f32_accessor!(panner_get_cone_gain, panner_set_cone_gain, cone, 2, |v: f32| v);

unsafe extern "sysv64" fn panner_get_distance_model(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, "PannerNode.distanceModel on wrong this");
    };
    let gl = glob().lock().unwrap();
    let s = match gl.nodes.get(&handle).map(|r| r.dist_model) {
        Some(DistModel::Linear) => "linear",
        Some(DistModel::Exponential) => "exponential",
        _ => "inverse",
    };
    js_string(g, s)
}

unsafe extern "sysv64" fn panner_set_distance_model(g: *mut c_void, cf: *mut c_void) -> u64 {
    let ty = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let model = match ty.as_str() {
        "linear" => DistModel::Linear,
        "exponential" => DistModel::Exponential,
        _ => DistModel::Inverse,
    };
    panner_write(g, cf, |rec| rec.dist_model = model)
}

/// Real HRTF (W-MIX MIX-20): switches the node's panning mode live.
unsafe extern "sysv64" fn panner_get_panning_model(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, "PannerNode.panningModel on wrong this");
    };
    let gl = glob().lock().unwrap();
    let hrtf = matches!(gl.nodes.get(&handle).map(|r| &r.params), Some(Params::Spatial(p)) if p.hrtf);
    js_string(g, if hrtf { "HRTF" } else { "equalpower" })
}

unsafe extern "sysv64" fn panner_set_panning_model(g: *mut c_void, cf: *mut c_void) -> u64 {
    let ty = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let hrtf = ty == "HRTF";
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, "PannerNode.panningModel on wrong this");
    };
    let mut gl = glob().lock().unwrap();
    let gl = &mut *gl;
    let Some(rec) = gl.nodes.get_mut(&handle) else { return UNDEF };
    let Some(eng) = gl.engines.get_mut(&rec.engine) else { return UNDEF };
    update_spatial_with(eng, rec, |p| p.hrtf = hrtf);
    UNDEF
}

/// A typed-array argument as a mutable slice (`kind`: 2 = u8, 8 = f32).
unsafe fn analyser_out<'a, T>(g: *mut c_void, cf: *mut c_void, kind: u32) -> Option<&'a mut [T]> {
    let arr = cf_arg(cf, 0);
    let mut ta = crate::ffi::RjsTaInfo::default();
    if glue::rjs_typedarray_info(g, arr, &mut ta) == 0 && !ta.data.is_null() && ta.kind == kind {
        Some(core::slice::from_raw_parts_mut(ta.data as *mut T, ta.length as usize))
    } else {
        None
    }
}

/// A valid analyser `fftSize`: power of two in 32..=MONITOR_WINDOW.
fn valid_fft_size(v: f64) -> Option<usize> {
    let n = v as usize;
    (v.fract() == 0.0 && (32..=MONITOR_WINDOW).contains(&n) && n.is_power_of_two()).then_some(n)
}

unsafe fn analyser_slot_f64(g: *mut c_void, cf: *mut c_void, name: &str, default: f64) -> f64 {
    let slots = slots_of(g, cf_this(cf));
    dec_f64(get(g, slots, name)).unwrap_or(default)
}

unsafe fn analyser_fft_size(g: *mut c_void, cf: *mut c_void) -> usize {
    valid_fft_size(analyser_slot_f64(g, cf, "fftSize", 2048.0)).unwrap_or(2048)
}

/// The last `fftSize` mono samples of THIS analyser's ring (oldest → newest).
unsafe fn analyser_time_domain(g: *mut c_void, cf: *mut c_void) -> Option<Vec<f32>> {
    let handle = this_node_handle(g, cf)?;
    let n = analyser_fft_size(g, cf);
    let gl = glob().lock().unwrap();
    match gl.nodes.get(&handle).map(|r| &r.params) {
        Some(Params::Analyser { ring, .. }) => {
            let mut buf = vec![0.0f32; n];
            ring.read_latest(&mut buf);
            Some(buf)
        }
        _ => None,
    }
}

/// Smoothed magnitude spectrum (fftSize/2 linear magnitudes): Blackman window
/// → radix-2 FFT → |X[k]|/N → exponential smoothing (spec §AnalyserNode).
unsafe fn analyser_spectrum(g: *mut c_void, cf: *mut c_void) -> Option<Vec<f32>> {
    let handle = this_node_handle(g, cf)?;
    let n = analyser_fft_size(g, cf);
    let tau = analyser_slot_f64(g, cf, "smoothingTimeConstant", 0.8).clamp(0.0, 1.0) as f32;

    let mut gl = glob().lock().unwrap();
    let Some(rec) = gl.nodes.get_mut(&handle) else { return None };
    let Params::Analyser { ring, smooth } = &mut rec.params else { return None };

    let mut re = vec![0.0f32; n];
    ring.read_latest(&mut re);
    for (i, v) in re.iter_mut().enumerate() {
        *v *= blackman(i, n);
    }
    let mut im = vec![0.0f32; n];
    fft_in_place(&mut re, &mut im);

    if smooth.len() != n / 2 {
        smooth.clear();
        smooth.resize(n / 2, 0.0);
    }
    for k in 0..n / 2 {
        let mag = (re[k] * re[k] + im[k] * im[k]).sqrt() / n as f32;
        smooth[k] = tau * smooth[k] + (1.0 - tau) * mag;
    }
    Some(smooth.clone())
}

unsafe extern "sysv64" fn analyser_float_time(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(out) = analyser_out::<f32>(g, cf, 8) else { return UNDEF };
    match analyser_time_domain(g, cf) {
        Some(data) => {
            let n = out.len().min(data.len());
            out[..n].copy_from_slice(&data[..n]);
        }
        None => out.fill(0.0),
    }
    UNDEF
}

unsafe extern "sysv64" fn analyser_byte_time(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(out) = analyser_out::<u8>(g, cf, 2) else { return UNDEF };
    match analyser_time_domain(g, cf) {
        Some(data) => {
            for (o, &v) in out.iter_mut().zip(data.iter()) {
                *o = ((128.0 * (1.0 + v)).round()).clamp(0.0, 255.0) as u8;
            }
        }
        None => out.fill(128),
    }
    UNDEF
}

unsafe extern "sysv64" fn analyser_float_freq(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(out) = analyser_out::<f32>(g, cf, 8) else { return UNDEF };
    match analyser_spectrum(g, cf) {
        Some(mags) => {
            for (o, &m) in out.iter_mut().zip(mags.iter()) {
                *o = 20.0 * m.log10(); // m == 0 → -inf, matching browsers
            }
        }
        None => out.fill(f32::NEG_INFINITY),
    }
    UNDEF
}

unsafe extern "sysv64" fn analyser_byte_freq(g: *mut c_void, cf: *mut c_void) -> u64 {
    let min_db = analyser_slot_f64(g, cf, "minDecibels", -100.0) as f32;
    let max_db = analyser_slot_f64(g, cf, "maxDecibels", -30.0) as f32;
    let range = (max_db - min_db).max(1e-6);
    let Some(out) = analyser_out::<u8>(g, cf, 2) else { return UNDEF };
    match analyser_spectrum(g, cf) {
        Some(mags) => {
            for (o, &m) in out.iter_mut().zip(mags.iter()) {
                let db = 20.0 * m.log10();
                *o = (255.0 * (db - min_db) / range).clamp(0.0, 255.0) as u8;
            }
        }
        None => out.fill(0),
    }
    UNDEF
}

unsafe extern "sysv64" fn analyser_get_fft(g: *mut c_void, cf: *mut c_void) -> u64 {
    enc_i32(analyser_fft_size(g, cf) as i32)
}

unsafe extern "sysv64" fn analyser_set_fft(g: *mut c_void, cf: *mut c_void) -> u64 {
    let v = dec_f64(cf_arg(cf, 0)).unwrap_or(f64::NAN);
    if valid_fft_size(v).is_none() {
        return throw_type_error(g, "AnalyserNode.fftSize must be a power of two in [32, 4096]");
    }
    let slots = slots_of(g, cf_this(cf));
    define(g, slots, "fftSize", cf_arg(cf, 0), 0);
    UNDEF
}

unsafe extern "sysv64" fn analyser_bin_count(g: *mut c_void, cf: *mut c_void) -> u64 {
    enc_i32((analyser_fft_size(g, cf) / 2) as i32)
}

// smoothingTimeConstant / minDecibels / maxDecibels — plain slot-backed accessors
macro_rules! analyser_slot_accessor {
    ($get:ident, $set:ident, $name:literal, $default:expr) => {
        unsafe extern "sysv64" fn $get(g: *mut c_void, cf: *mut c_void) -> u64 {
            enc_f64(analyser_slot_f64(g, cf, $name, $default))
        }
        unsafe extern "sysv64" fn $set(g: *mut c_void, cf: *mut c_void) -> u64 {
            let slots = slots_of(g, cf_this(cf));
            define(g, slots, $name, cf_arg(cf, 0), 0);
            UNDEF
        }
    };
}
analyser_slot_accessor!(analyser_get_smoothing, analyser_set_smoothing, "smoothingTimeConstant", 0.8);
analyser_slot_accessor!(analyser_get_min_db, analyser_set_min_db, "minDecibels", -100.0);
analyser_slot_accessor!(analyser_get_max_db, analyser_set_max_db, "maxDecibels", -30.0);

// ---------------------------------------------------------------------------
// Slot getters (AudioParam / object properties)
// ---------------------------------------------------------------------------

macro_rules! slot_getter {
    ($fn_name:ident, $slot:literal) => {
        unsafe extern "sysv64" fn $fn_name(g: *mut c_void, cf: *mut c_void) -> u64 {
            let slots = slots_of(g, cf_this(cf));
            get(g, slots, $slot)
        }
    };
}

slot_getter!(get_slot_gain, "gain");
slot_getter!(get_slot_pan, "pan");
slot_getter!(get_slot_playback_rate, "playbackRate");
slot_getter!(get_slot_detune, "detune");
slot_getter!(get_slot_frequency, "frequency");
slot_getter!(get_slot_q, "Q");
slot_getter!(get_slot_pos_x, "positionX");
slot_getter!(get_slot_pos_y, "positionY");
slot_getter!(get_slot_pos_z, "positionZ");
slot_getter!(get_slot_orient_x, "orientationX");
slot_getter!(get_slot_orient_y, "orientationY");
slot_getter!(get_slot_orient_z, "orientationZ");
slot_getter!(get_slot_forward_x, "forwardX");
slot_getter!(get_slot_forward_y, "forwardY");
slot_getter!(get_slot_forward_z, "forwardZ");
slot_getter!(get_slot_up_x, "upX");
slot_getter!(get_slot_up_y, "upY");
slot_getter!(get_slot_up_z, "upZ");
slot_getter!(get_slot_destination, "destination");
slot_getter!(get_slot_listener, "listener");
slot_getter!(get_slot_stream_id, "streamId");
slot_getter!(get_slot_stream_rate, "sampleRate");
slot_getter!(get_slot_stream_channels, "channelCount");

// ---------------------------------------------------------------------------
// AudioContext
// ---------------------------------------------------------------------------

unsafe extern "sysv64" fn ctx_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);

    let mut fw = FirewheelContext::new(firewheel::FirewheelConfig::default());
    let master = fw.add_node(VolumeNode::default(), None);
    let out = fw.graph_out_node_id();
    let _ = fw.connect(master, out, STEREO_PORTS, true);
    let running = match fw.start_stream(firewheel::cpal::CpalConfig::default()) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("[audio] AudioContext: output stream failed ({e:?}) — graph runs silent");
            false
        }
    };
    let sample_rate = fw.stream_info().map(|i| i.sample_rate.get()).unwrap_or(48_000);
    alog!("AudioContext created: running={running} rate={sample_rate}");

    let engine_handle;
    let dest_handle;
    {
        let mut gl = glob().lock().unwrap();
        engine_handle = gl.next;
        gl.next += 1;
        gl.engines.insert(
            engine_handle,
            Engine {
                g: g as usize,
                fw,
                running,
                state: ST_RUNNING,
                master,
                master_params: VolumeNode::default(),
                listener: [0.0; 3],
                listener_fwd: [0.0, 0.0, -1.0],
                listener_up: [0.0, 1.0, 0.0],
                sample_rate,
                sink_id: String::new(),
            },
        );
        dest_handle = alloc_node(&mut gl, engine_handle, Params::Dest).unwrap();
    }

    let nt = cf_new_target(cf);
    let mut proto = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
    if !is_cell(proto) {
        proto = c.ctx.proto;
    }
    let obj = host_create(g, c.ctx_cls, proto, CtxHandle(engine_handle));

    let slots = new_object(g, UNDEF);
    glue::rjs_define(g, obj, c.s_audio, slots, 0);

    let dest = wrap_node(g, c.dest.proto, dest_handle, &[]);
    define(g, slots, "destination", dest, 0);

    // The listener: a host object owned by this context, its params write
    // through to the engine's listener position.
    let listener = host_create(g, c.listener_cls, c.listener.proto, ListenerHandle(engine_handle));
    let lslots = new_object(g, UNDEF);
    glue::rjs_define(g, listener, c.s_audio, lslots, 0);
    let lparams: &[(&str, u8, f64)] = &[
        ("positionX", P_L_POS_X, 0.0),
        ("positionY", P_L_POS_Y, 0.0),
        ("positionZ", P_L_POS_Z, 0.0),
        ("forwardX", P_L_FWD_X, 0.0),
        ("forwardY", P_L_FWD_Y, 0.0),
        ("forwardZ", P_L_FWD_Z, -1.0),
        ("upX", P_L_UP_X, 0.0),
        ("upY", P_L_UP_Y, 1.0),
        ("upZ", P_L_UP_Z, 0.0),
    ];
    for &(name, kind, initial) in lparams {
        let p = host_create(g, c.param_cls, c.param.proto, ParamRef { node: engine_handle, kind, last: initial });
        define(g, lslots, name, p, 0);
    }
    define(g, slots, "listener", listener, 0);

    obj
}

unsafe extern "sysv64" fn ctx_get_current_time(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(h) = this_engine_handle(g, cf) else {
        return throw_type_error(g, "AudioContext.currentTime on wrong this");
    };
    let gl = glob().lock().unwrap();
    enc_f64(gl.engines.get(&h).map(|e| e.now()).unwrap_or(0.0))
}

unsafe extern "sysv64" fn ctx_get_sample_rate(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(h) = this_engine_handle(g, cf) else {
        return throw_type_error(g, "AudioContext.sampleRate on wrong this");
    };
    let gl = glob().lock().unwrap();
    enc_f64(gl.engines.get(&h).map(|e| e.sample_rate).unwrap_or(48_000) as f64)
}

unsafe extern "sysv64" fn ctx_get_state(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(h) = this_engine_handle(g, cf) else {
        return throw_type_error(g, "AudioContext.state on wrong this");
    };
    let gl = glob().lock().unwrap();
    let s = match gl.engines.get(&h).map(|e| e.state) {
        Some(ST_SUSPENDED) => "suspended",
        Some(ST_CLOSED) | None => "closed",
        _ => "running",
    };
    js_string(g, s)
}

unsafe extern "sysv64" fn ctx_get_sink_id(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(h) = this_engine_handle(g, cf) else {
        return throw_type_error(g, "AudioContext.sinkId on wrong this");
    };
    let gl = glob().lock().unwrap();
    let id = gl.engines.get(&h).map(|e| e.sink_id.clone()).unwrap_or_default();
    drop(gl);
    js_string(g, &id)
}

/// `AudioContext.setSinkId(sinkId)` — the whole reason this runtime needs no
/// `<audio>` element to route output. Maps onto re-opening the CPAL output
/// stream on the selected device, keeping the node graph and the clock.
///
/// Refusals are named rather than silent, because the failure this replaces
/// (playing out of the wrong speakers) is invisible from JS:
/// - a non-string argument → `TypeError` (the spec's `AudioSinkOptions`
///   overload, the `{type:'none'}` silent sink, is not implemented);
/// - an id no longer in `enumerateDevices()` → `NotFoundError`;
/// - a closed context → `InvalidStateError`;
/// - a device that will not open → the CPAL error, after restoring the
///   previous device so the failure is not also a silence.
unsafe extern "sysv64" fn ctx_set_sink_id(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(h) = this_engine_handle(g, cf) else {
        return rejected_promise(g, 1, "AudioContext.setSinkId on wrong this");
    };
    let arg = cf_arg(cf, 0);
    if glue::rjs_kind(g, arg) != 5 {
        return rejected_promise(
            g,
            1,
            "AudioContext.setSinkId: sinkId must be a device id string (AudioSinkOptions — the \
             {type:'none'} silent sink — is not implemented)",
        );
    }
    let requested = read_string(g, arg).unwrap_or_default();

    let device = match mediadevices::resolve_output(&requested) {
        Ok(d) => d,
        Err(()) => {
            return super::domexception::rejected(
                g,
                "NotFoundError",
                &format!("AudioContext.setSinkId: no audio output device with id '{requested}'"),
            )
        }
    };

    // Everything that touches the engine happens under the lock; nothing that
    // re-enters JS does.
    enum Outcome {
        Ok,
        Closed,
        Failed(String),
    }
    let outcome = {
        let mut gl = glob().lock().unwrap();
        match gl.engines.get_mut(&h) {
            None => Outcome::Closed,
            Some(eng) if eng.state == ST_CLOSED => Outcome::Closed,
            Some(eng) => {
                let previous = mediadevices::resolve_output(&eng.sink_id).ok().flatten();
                if previous == device && eng.running {
                    // Same endpoint (e.g. "" → "default"): record the new
                    // spelling, but do not interrupt the stream for it.
                    eng.sink_id = requested.clone();
                    Outcome::Ok
                } else {
                    match restart_stream(eng, device) {
                        Ok(()) => {
                            eng.sink_id = requested.clone();
                            alog!("setSinkId -> '{}' (rate={})", eng.sink_id, eng.sample_rate);
                            Outcome::Ok
                        }
                        Err(e) => {
                            // Do not leave the user in silence because their
                            // new choice failed: put the old one back.
                            let restored = restart_stream(eng, previous);
                            if restored.is_err() {
                                eprintln!(
                                    "[audio] setSinkId: '{requested}' failed ({e}) and the previous \
                                     device could not be reopened either — output is stopped"
                                );
                            }
                            Outcome::Failed(e)
                        }
                    }
                }
            }
        }
    };

    match outcome {
        Outcome::Ok => resolved_promise(g, UNDEF),
        Outcome::Closed => {
            super::domexception::rejected(g, "InvalidStateError", "AudioContext.setSinkId: the context is closed")
        }
        Outcome::Failed(e) => rejected_promise(
            g,
            0,
            &format!("AudioContext.setSinkId: could not open output device '{requested}': {e}"),
        ),
    }
}

/// suspend() mutes the master bus / resume() restores it (the stream keeps
/// running so `currentTime` keeps advancing — see module docs).
unsafe fn ctx_set_suspended(g: *mut c_void, cf: *mut c_void, suspended: bool) -> u64 {
    let Some(h) = this_engine_handle(g, cf) else {
        return rejected_promise(g, 1, "AudioContext method on wrong this");
    };
    let mut gl = glob().lock().unwrap();
    if let Some(eng) = gl.engines.get_mut(&h) {
        if eng.state == ST_CLOSED {
            // Spec: "reject promise with an InvalidStateError".
            return super::domexception::rejected(g, "InvalidStateError", "AudioContext is closed");
        }
        eng.state = if suspended { ST_SUSPENDED } else { ST_RUNNING };
        let old = eng.master_params;
        eng.master_params.volume = Volume::Linear(if suspended { 0.0 } else { 1.0 });
        let (id, new_params) = (eng.master, eng.master_params);
        queue_diff(&mut eng.fw, id, &old, &new_params);
    }
    drop(gl);
    resolved_promise(g, UNDEF)
}

unsafe extern "sysv64" fn ctx_resume(g: *mut c_void, cf: *mut c_void) -> u64 {
    ctx_set_suspended(g, cf, false)
}

unsafe extern "sysv64" fn ctx_suspend(g: *mut c_void, cf: *mut c_void) -> u64 {
    ctx_set_suspended(g, cf, true)
}

unsafe extern "sysv64" fn ctx_close(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(h) = this_engine_handle(g, cf) else {
        return rejected_promise(g, 1, "AudioContext.close on wrong this");
    };
    let mut gl = glob().lock().unwrap();
    if let Some(eng) = gl.engines.get_mut(&h) {
        eng.state = ST_CLOSED;
        eng.fw.stop_stream();
        eng.running = false;
    }
    drop(gl);
    resolved_promise(g, UNDEF)
}

/// Shared body of the create* factories.
unsafe fn ctx_create_node(g: *mut c_void, cf: *mut c_void, params: Params, proto: u64, jsparams: &[(&str, u8, f64)]) -> u64 {
    let Some(h) = this_engine_handle(g, cf) else {
        return throw_type_error(g, "AudioContext factory called on wrong this");
    };
    let handle = {
        let mut gl = glob().lock().unwrap();
        match alloc_node(&mut gl, h, params) {
            Some(x) => x,
            None => return throw_error(g, 0, "AudioContext is closed"),
        }
    };
    wrap_node(g, proto, handle, jsparams)
}

unsafe extern "sysv64" fn ctx_create_gain(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    ctx_create_node(g, cf, Params::Gain(VolumeNode::default()), c.gain.proto, &[("gain", P_GAIN, 1.0)])
}

unsafe extern "sysv64" fn ctx_create_stereo_panner(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    ctx_create_node(g, cf, Params::Pan(VolumePanNode::default()), c.stereo.proto, &[("pan", P_PAN, 0.0)])
}

unsafe extern "sysv64" fn ctx_create_panner(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    // WebPannerNode (webapi/hrtf.rs): equal-power by default per spec, real
    // HRTF when panningModel='HRTF'. Distance/cone params live in NodeRec with
    // Web Audio defaults and are baked into the node's gain on the pump side.
    ctx_create_node(
        g,
        cf,
        Params::Spatial(WebPannerNode::default()),
        c.panner.proto,
        &[
            ("positionX", P_POS_X, 0.0),
            ("positionY", P_POS_Y, 0.0),
            ("positionZ", P_POS_Z, 0.0),
            ("orientationX", P_ORIENT_X, 1.0),
            ("orientationY", P_ORIENT_Y, 0.0),
            ("orientationZ", P_ORIENT_Z, 0.0),
        ],
    )
}

unsafe extern "sysv64" fn ctx_create_biquad(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let svf = SvfNode::<2> { cutoff_hz: 350.0, ..Default::default() };
    ctx_create_node(
        g,
        cf,
        Params::Svf(Box::new(svf)),
        c.biquad.proto,
        &[
            ("frequency", P_FREQ, 350.0),
            ("Q", P_Q, 1.0),
            ("gain", P_GAIN, 0.0),
            ("detune", P_INERT, 0.0),
        ],
    )
}

unsafe extern "sysv64" fn ctx_create_analyser(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let params = Params::Analyser { ring: ArcGc::new(MonitorRing::new()), smooth: Vec::new() };
    ctx_create_node(g, cf, params, c.analyser.proto, &[])
}

/// `createMediaElementSource` — a SILENT pass-through, and it says so.
///
/// This is the one node on this host that cannot be made real by writing more
/// DSP: on both embedders that have media elements at all, `<video>`/`<audio>`
/// decode *and play out* inside the platform's own media pipeline (Media
/// Foundation's `IMFMediaEngine` on Windows), which owns its own output stream
/// and never hands the graph a sample. Routing it here would mean re-decoding
/// the file a second time, out of sync with the picture.
///
/// It still returns a connectable node — `try { ctx.createMediaElementSource(el) }`
/// is the standard capability probe and throwing would break callers that then
/// fall back correctly — but a one-shot advisory names the situation, because
/// the failure it replaces (a graph that is connected and simply mute) has no
/// other symptom.
unsafe extern "sysv64" fn ctx_create_media_element_source(g: *mut c_void, cf: *mut c_void) -> u64 {
    static WARNED: OnceLock<()> = OnceLock::new();
    WARNED.get_or_init(|| {
        eprintln!(
            "[audio] advisory: createMediaElementSource() returns a SILENT pass-through on this \
             runtime. A <video>/<audio> element decodes and plays out through the platform media \
             engine, not through the Web Audio graph, so its samples never reach this node — \
             anything connected downstream (analyser, gain, panner) will see nothing but zeros. \
             For audio you want in the graph, fetch the bytes and use decodeAudioData() + \
             AudioBufferSourceNode, or push PCM through createRedStreamSource()."
        );
    });
    let c = classes(g);
    ctx_create_node(g, cf, Params::Passthrough, c.node_base.proto, &[])
}

/// `createOscillator()` — the spec's tone generator (webapi/osc.rs).
unsafe extern "sysv64" fn ctx_create_oscillator(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let shared = ArcGc::new(OscShared::default());
    let obj = ctx_create_node(
        g,
        cf,
        Params::Osc(shared),
        c.osc.proto,
        &[("frequency", P_FREQ, 440.0), ("detune", P_DETUNE, 0.0)],
    );
    if is_cell(obj) {
        define(g, slots_of(g, obj), "onended", NULL, 0);
    }
    obj
}

/// `createRedStreamSource(options)` — NOT a browser API, and named so you can
/// tell at a glance.
///
/// The browser's equivalent seam is `createMediaStreamSource(stream)`, which
/// presumes a `MediaStream` the engine pulls from. This runtime has no
/// `MediaStream` (see webapi/mediadevices.rs) and the producers that need this
/// — a WebRTC track, a native decoder — already hold decoded `f32` frames and
/// want to push. Calling that `createMediaStreamSource` would be a different
/// API wearing a standard name; see webapi/pcm.rs for the contract.
///
/// `options`: `{ sampleRate?, channelCount?, latencySeconds?, capacitySeconds? }`.
unsafe extern "sysv64" fn ctx_create_red_stream_source(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let opts = cf_arg(cf, 0);
    let num = |name: &str, dflt: f64| -> f64 {
        if is_cell(opts) {
            dec_f64(get(g, opts, name)).filter(|v| v.is_finite()).unwrap_or(dflt)
        } else {
            dflt
        }
    };
    let d = PcmStreamConfig::default();
    let cfg = PcmStreamConfig {
        sample_rate: num("sampleRate", d.sample_rate as f64) as u32,
        channels: num("channelCount", d.channels as f64) as u32,
        latency_seconds: num("latencySeconds", d.latency_seconds),
        capacity_seconds: num("capacitySeconds", d.capacity_seconds),
    }
    .sanitized();

    let obj = ctx_create_node(g, cf, Params::PcmStream(cfg), c.stream_src.proto, &[]);
    if is_cell(obj) {
        // The id a native producer looks the sink up by — the node handle,
        // which is also what `pcm::register` filed the producer under.
        if let Some(h) = host_data::<NodeHandle>(g, c.node_cls, obj).map(|x| x.0) {
            let slots = slots_of(g, obj);
            define(g, slots, "streamId", enc_f64(h as f64), 0);
            define(g, slots, "sampleRate", enc_f64(cfg.sample_rate as f64), 0);
            define(g, slots, "channelCount", enc_f64(cfg.channels as f64), 0);
        }
    }
    obj
}

unsafe extern "sysv64" fn ctx_create_buffer_source(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let obj = ctx_create_node(
        g,
        cf,
        Params::Sampler(Box::new(SamplerNode::default())),
        c.source.proto,
        &[("playbackRate", P_RATE, 1.0), ("detune", P_INERT, 0.0)],
    );
    if is_cell(obj) {
        let slots = slots_of(g, obj);
        define(g, slots, "buffer", NULL, 0);
        define(g, slots, "onended", NULL, 0);
    }
    obj
}

unsafe extern "sysv64" fn ctx_create_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    let channels = (dec_f64(cf_arg(cf, 0)).unwrap_or(1.0) as usize).clamp(1, 32);
    let length = dec_f64(cf_arg(cf, 1)).unwrap_or(0.0).max(0.0) as usize;
    let rate = dec_f64(cf_arg(cf, 2)).unwrap_or(48_000.0).max(1.0) as u32;
    if this_engine_handle(g, cf).is_none() {
        return throw_type_error(g, "AudioContext.createBuffer on wrong this");
    }
    wrap_buffer(g, Arc::new(PlanarBuffer { data: vec![vec![0.0; length]; channels], sample_rate: rate }))
}

// ---- decodeAudioData -------------------------------------------------------

/// The formats this build can actually decode, for the error message. A
/// rejection that only says "unsupported" sends the caller to look for a bug in
/// their file; one that names the list sends them to re-encode it.
pub fn supported_decode_formats() -> &'static [&'static str] {
    // Kept in sync with the symphonium features in Cargo.toml by hand — there
    // is no way to enumerate a dependency's enabled features at run time.
    #[cfg(feature = "audio-codecs-extra")]
    {
        &[
            "WAV/RIFF (PCM, IEEE float)", "AIFF", "CAF", "FLAC", "MP3", "OGG/Vorbis", "MP4/M4A (AAC-LC)",
            "MKV", "ALAC", "ADPCM",
        ]
    }
    #[cfg(not(feature = "audio-codecs-extra"))]
    {
        &["WAV/RIFF (PCM, IEEE float)", "FLAC", "MP3", "OGG/Vorbis"]
    }
}

pub(crate) fn decode_bytes(bytes: Vec<u8>, target_rate: Option<u32>) -> Result<PlanarBuffer, String> {
    static LOADER: OnceLock<Mutex<symphonium::SymphoniumLoader>> = OnceLock::new();
    let loader = LOADER.get_or_init(|| Mutex::new(symphonium::SymphoniumLoader::new()));
    let mut loader = loader.lock().unwrap();
    let decoded = loader
        .load_from_source(
            Box::new(std::io::Cursor::new(bytes)),
            None,
            target_rate.and_then(NonZeroU32::new),
            symphonium::ResampleQuality::default(),
            None,
        )
        .map_err(|e| {
            format!(
                "decodeAudioData could not decode this data ({e}). \
                 Decodable here: {}. Not decodable in any build: Opus, HE-AAC/AAC+ (the AAC-LC \
                 core decodes, SBR/PS do not), and anything DRM-protected.",
                supported_decode_formats().join(", ")
            )
        })?;
    let frames = decoded.frames();
    let mut data = Vec::with_capacity(decoded.channels());
    for ch in 0..decoded.channels() {
        let mut v = vec![0.0f32; frames];
        let _ = decoded.fill_channel(ch, 0, &mut v);
        data.push(v);
    }
    Ok(PlanarBuffer { data, sample_rate: decoded.sample_rate().get() })
}

unsafe extern "sysv64" fn ctx_decode_audio_data(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(h) = this_engine_handle(g, cf) else {
        return rejected_promise(g, 1, "AudioContext.decodeAudioData on wrong this");
    };
    let Some(bytes) = bufferish(g, cf_arg(cf, 0)).map(|b| b.to_vec()) else {
        return rejected_promise(g, 1, "decodeAudioData: expected an ArrayBuffer");
    };
    if bytes.is_empty() {
        // Nothing to decode is undecodable: Chrome and Firefox both reject an
        // empty buffer with EncodingError.
        return super::domexception::rejected(g, "EncodingError", "decodeAudioData: empty buffer");
    }
    let target_rate = {
        let gl = glob().lock().unwrap();
        gl.engines.get(&h).filter(|e| e.running).map(|e| e.sample_rate)
    };

    let (mut res, mut rej, mut p) = (0u64, 0u64, 0u64);
    if crate::ffi::redjs_promise_create(g, &mut res, &mut rej, &mut p) != 0 {
        return UNDEF;
    }
    let success_cb = cf_arg(cf, 1);
    let error_cb = cf_arg(cf, 2);
    for v in [res, rej, success_cb, error_cb] {
        if is_cell(v) {
            crate::ffi::redjs_protect(g, v);
        }
    }

    // Spec: decoding is async. The decode itself runs on the pump tick
    // (native task), which for game-sized SFX is a couple of milliseconds.
    crate::web::schedule_native(g, 0.0, Box::new(move |g| {
        match decode_bytes(bytes, target_rate) {
            Ok(buf) => {
                let js_buf = wrap_buffer(g, Arc::new(buf));
                if is_cell(success_cb) {
                    let _ = call(g, success_cb, UNDEF, &[js_buf]);
                }
                let _ = call(g, res, UNDEF, &[js_buf]);
            }
            Err(msg) => {
                // Spec: "an EncodingError DOMException".
                let err = super::domexception::create(g, "EncodingError", &msg);
                if is_cell(error_cb) {
                    let _ = call(g, error_cb, UNDEF, &[err]);
                }
                let _ = call(g, rej, UNDEF, &[err]);
            }
        }
        for v in [res, rej, success_cb, error_cb] {
            if is_cell(v) {
                crate::ffi::redjs_unprotect(g, v);
            }
        }
    }));
    p
}

// ---------------------------------------------------------------------------
// Pump hook: graph upkeep, param timelines, scheduled starts/stops, onended
// ---------------------------------------------------------------------------

pub unsafe fn pump(g: *mut c_void) {
    let c_s_audio;
    {
        let reg = class_registry().lock().unwrap();
        match reg.get(&(g as usize)) {
            Some(c) => c_s_audio = c.s_audio,
            None => return, // audio never installed for this context
        }
    }

    let mut ended: Vec<u64> = Vec::new();
    {
        let mut gl = glob().lock().unwrap();
        let gl = &mut *gl;

        // Finalized wrappers → graph cleanup.
        let dead_list: Vec<u64> = core::mem::take(&mut *dead().lock().unwrap());
        for h in dead_list {
            if let Some(mut eng) = gl.engines.remove(&h) {
                eng.fw.stop_stream();
                // Retire this context's streaming-PCM producers BEFORE the node
                // records go, or a native pusher keeps a handle whose graph
                // side no longer exists.
                for (nh, r) in gl.nodes.iter() {
                    if r.engine == h && matches!(r.params, Params::PcmStream(_)) {
                        pcm::unregister(*nh);
                    }
                }
                gl.nodes.retain(|_, r| r.engine != h);
                gl.timelines.retain(|(n, _), _| *n != h);
                continue;
            }
            if let Some(rec) = gl.nodes.remove(&h) {
                if matches!(rec.params, Params::PcmStream(_)) {
                    pcm::unregister(h);
                }
                if !matches!(rec.params, Params::Dest) {
                    if let Some(eng) = gl.engines.get_mut(&rec.engine) {
                        let _ = eng.fw.remove_node(rec.id);
                    }
                }
                gl.timelines.retain(|(n, _), _| *n != h);
            }
        }

        let Glob { engines, nodes, timelines, .. } = gl;

        let handles: Vec<u64> = engines
            .iter()
            .filter(|(_, e)| e.g == g as usize && e.state != ST_CLOSED)
            .map(|(&h, _)| h)
            .collect();

        for eh in handles {
            let now = {
                let eng = engines.get_mut(&eh).unwrap();
                if let Err(e) = eng.fw.update() {
                    // Always loud: a swallowed update error is silent audio.
                    eprintln!("[audio] firewheel update error (engine {eh}): {e:?}");
                }
                eng.now()
            };

            // Param timelines (per frame; node smoothers interpolate between ticks).
            let due: Vec<((u64, u8), f64, bool)> = timelines
                .iter()
                .filter_map(|(&(node, kind), tl)| {
                    // listener params key by engine handle
                    let belongs = if kind >= P_L_POS_X {
                        node == eh
                    } else {
                        nodes.get(&node).map(|r| r.engine == eh).unwrap_or(false)
                    };
                    if !belongs {
                        return None;
                    }
                    let (v, done) = tl.eval(now);
                    Some(((node, kind), v, done))
                })
                .collect();
            for (key, v, done) in due {
                set_param(engines, nodes, key.0, key.1, v);
                if done {
                    timelines.remove(&key);
                }
            }

            // Scheduled starts/stops + ended detection.
            let eng = engines.get_mut(&eh).unwrap();
            for rec in nodes.values_mut().filter(|r| r.engine == eh) {
                if let Some((when, offset)) = rec.pending_start {
                    if when <= now {
                        rec.pending_start = None;
                        do_start(eng, rec, offset);
                    }
                }
                if let Some(when) = rec.pending_stop {
                    if when <= now {
                        rec.pending_stop = None;
                        do_stop(eng, rec);
                    }
                }
                if rec.started && !rec.ended && matches!(rec.params, Params::Sampler(_)) {
                    // Playback state lives on the node's SamplerState handle
                    // (shared with the audio thread), not on the param struct.
                    if let Some(st) = eng.fw.node_state::<firewheel::nodes::sampler::SamplerState>(rec.id) {
                        if st.playing() {
                            rec.saw_playing = true;
                        }
                        if rec.saw_playing && st.stopped() {
                            rec.ended = true;
                            alog!("source ended node={:?}", rec.id);
                            if rec.js_obj != 0 {
                                ended.push(rec.js_obj);
                                rec.js_obj = 0;
                            }
                        }
                    }
                }
                if rec.started && !rec.ended {
                    if let Params::Osc(sh) = &rec.params {
                        // With a live stream the processor raises `finished`
                        // when its fade-out completes. WITHOUT one the
                        // processor never runs at all, so `stop()` having been
                        // called IS the end — otherwise `onended` would simply
                        // never fire on a machine with no output device, and
                        // code that awaits it would hang rather than degrade.
                        // `saw_playing` is what keeps a not-yet-due scheduled
                        // start from reading as an end.
                        if sh.is_playing() {
                            rec.saw_playing = true;
                        }
                        let done = if eng.running {
                            sh.is_finished()
                        } else {
                            rec.saw_playing && !sh.is_playing()
                        };
                        if done {
                            rec.ended = true;
                            alog!("oscillator ended node={:?}", rec.id);
                            if rec.js_obj != 0 {
                                ended.push(rec.js_obj);
                                rec.js_obj = 0;
                            }
                        }
                    }
                }
            }
        }
    }

    // JS callbacks outside the lock.
    for obj in ended {
        let slots = get_a(g, obj, c_s_audio);
        let cb = get(g, slots, "onended");
        if is_cell(cb) {
            let ev = new_object(g, UNDEF);
            define(g, ev, "type", js_string(g, "ended"), 0);
            define(g, ev, "target", obj, 0);
            let _ = call(g, cb, obj, &[ev]);
        }
        crate::ffi::redjs_unprotect(g, obj);
    }
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

pub unsafe fn install(g: *mut c_void) {
    let s_audio = atom_priv(g, "redjs#audio");

    let node_cls = make_host_class::<NodeHandle>("AudioNode");
    let ctx_cls = make_host_class::<CtxHandle>("AudioContext");
    let param_cls = make_host_class::<ParamRef>("AudioParam");
    let buffer_cls = make_host_class::<BufData>("AudioBuffer");
    let listener_cls = make_host_class::<ListenerHandle>("AudioListener");

    let node_base = define_class(g, &ClassSpec {
        name: "AudioNode",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[("connect", 1, node_connect), ("disconnect", 1, node_disconnect)],
        getters: &[],
        accessors: &[],
    });

    let param = define_class(g, &ClassSpec {
        name: "AudioParam",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[
            ("setValueAtTime", 2, param_set_value_at),
            ("linearRampToValueAtTime", 2, param_linear_ramp),
            ("exponentialRampToValueAtTime", 2, param_exp_ramp),
            ("setTargetAtTime", 3, param_set_target),
            ("cancelScheduledValues", 1, param_cancel),
        ],
        getters: &[],
        accessors: &[("value", param_get_value, param_set_value)],
    });

    let buffer = define_class(g, &ClassSpec {
        name: "AudioBuffer",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[("getChannelData", 1, buf_get_channel_data), ("copyToChannel", 2, buf_copy_to_channel)],
        getters: &[
            ("sampleRate", buf_get_sample_rate),
            ("length", buf_get_length),
            ("duration", buf_get_duration),
            ("numberOfChannels", buf_get_channels),
        ],
        accessors: &[],
    });

    let dest = define_class(g, &ClassSpec {
        name: "AudioDestinationNode",
        arity: 0,
        construct: None,
        parent_proto: node_base.proto,
        methods: &[],
        getters: &[],
        accessors: &[],
    });

    let listener = define_class(g, &ClassSpec {
        name: "AudioListener",
        arity: 0,
        construct: None,
        parent_proto: 0,
        methods: &[],
        getters: &[
            ("positionX", get_slot_pos_x),
            ("positionY", get_slot_pos_y),
            ("positionZ", get_slot_pos_z),
            ("forwardX", get_slot_forward_x),
            ("forwardY", get_slot_forward_y),
            ("forwardZ", get_slot_forward_z),
            ("upX", get_slot_up_x),
            ("upY", get_slot_up_y),
            ("upZ", get_slot_up_z),
        ],
        accessors: &[],
    });

    let gain = define_class(g, &ClassSpec {
        name: "GainNode",
        arity: 0,
        construct: None,
        parent_proto: node_base.proto,
        methods: &[],
        getters: &[("gain", get_slot_gain)],
        accessors: &[],
    });

    let stereo = define_class(g, &ClassSpec {
        name: "StereoPannerNode",
        arity: 0,
        construct: None,
        parent_proto: node_base.proto,
        methods: &[],
        getters: &[("pan", get_slot_pan)],
        accessors: &[],
    });

    let panner = define_class(g, &ClassSpec {
        name: "PannerNode",
        arity: 0,
        construct: None,
        parent_proto: node_base.proto,
        methods: &[],
        getters: &[
            ("positionX", get_slot_pos_x),
            ("positionY", get_slot_pos_y),
            ("positionZ", get_slot_pos_z),
            ("orientationX", get_slot_orient_x),
            ("orientationY", get_slot_orient_y),
            ("orientationZ", get_slot_orient_z),
        ],
        accessors: &[
            ("distanceModel", panner_get_distance_model, panner_set_distance_model),
            ("panningModel", panner_get_panning_model, panner_set_panning_model),
            ("refDistance", panner_get_ref, panner_set_ref),
            ("maxDistance", panner_get_max, panner_set_max),
            ("rolloffFactor", panner_get_rolloff, panner_set_rolloff),
            ("coneInnerAngle", panner_get_cone_inner, panner_set_cone_inner),
            ("coneOuterAngle", panner_get_cone_outer, panner_set_cone_outer),
            ("coneOuterGain", panner_get_cone_gain, panner_set_cone_gain),
        ],
    });

    let biquad = define_class(g, &ClassSpec {
        name: "BiquadFilterNode",
        arity: 0,
        construct: None,
        parent_proto: node_base.proto,
        methods: &[],
        getters: &[
            ("frequency", get_slot_frequency),
            ("Q", get_slot_q),
            ("gain", get_slot_gain),
            ("detune", get_slot_detune),
        ],
        accessors: &[("type", biquad_get_type, biquad_set_type)],
    });

    let analyser = define_class(g, &ClassSpec {
        name: "AnalyserNode",
        arity: 0,
        construct: None,
        parent_proto: node_base.proto,
        methods: &[
            ("getByteFrequencyData", 1, analyser_byte_freq),
            ("getFloatFrequencyData", 1, analyser_float_freq),
            ("getByteTimeDomainData", 1, analyser_byte_time),
            ("getFloatTimeDomainData", 1, analyser_float_time),
        ],
        getters: &[("frequencyBinCount", analyser_bin_count)],
        accessors: &[
            ("fftSize", analyser_get_fft, analyser_set_fft),
            ("smoothingTimeConstant", analyser_get_smoothing, analyser_set_smoothing),
            ("minDecibels", analyser_get_min_db, analyser_set_min_db),
            ("maxDecibels", analyser_get_max_db, analyser_set_max_db),
        ],
    });

    let source = define_class(g, &ClassSpec {
        name: "AudioBufferSourceNode",
        arity: 0,
        construct: None,
        parent_proto: node_base.proto,
        methods: &[("start", 2, src_start), ("stop", 1, src_stop)],
        getters: &[("playbackRate", get_slot_playback_rate), ("detune", get_slot_detune)],
        accessors: &[
            ("buffer", src_get_buffer, src_set_buffer),
            ("loop", src_get_loop, src_set_loop),
            ("onended", src_get_onended, src_set_onended),
        ],
    });

    // `start`/`stop`/`onended` are the AudioScheduledSourceNode surface, shared
    // verbatim with the sampler — `src_start`/`src_stop` dispatch on the node's
    // `Params`, so one implementation drives both.
    let osc = define_class(g, &ClassSpec {
        name: "OscillatorNode",
        arity: 0,
        construct: None,
        parent_proto: node_base.proto,
        methods: &[("start", 1, src_start), ("stop", 1, src_stop)],
        getters: &[("frequency", get_slot_frequency), ("detune", get_slot_detune)],
        accessors: &[("type", osc_get_type, osc_set_type), ("onended", src_get_onended, src_set_onended)],
    });

    let stream_src = define_class(g, &ClassSpec {
        name: "RedStreamSourceNode",
        arity: 0,
        construct: None,
        parent_proto: node_base.proto,
        methods: &[("push", 1, stream_push), ("close", 0, stream_close)],
        getters: &[
            ("streamId", get_slot_stream_id),
            ("sampleRate", get_slot_stream_rate),
            ("channelCount", get_slot_stream_channels),
            ("queuedSeconds", stream_get_queued),
            ("availableFrames", stream_get_available),
            ("underflowed", stream_get_underflowed),
            ("overflowed", stream_get_overflowed),
            ("ready", stream_get_ready),
        ],
        accessors: &[],
    });

    let ctx = define_class(g, &ClassSpec {
        name: "AudioContext",
        arity: 0,
        construct: Some(ctx_construct),
        parent_proto: 0,
        methods: &[
            ("createGain", 0, ctx_create_gain),
            ("createStereoPanner", 0, ctx_create_stereo_panner),
            ("createPanner", 0, ctx_create_panner),
            ("createBiquadFilter", 0, ctx_create_biquad),
            ("createAnalyser", 0, ctx_create_analyser),
            ("createBufferSource", 0, ctx_create_buffer_source),
            ("createOscillator", 0, ctx_create_oscillator),
            ("createBuffer", 3, ctx_create_buffer),
            ("createMediaElementSource", 1, ctx_create_media_element_source),
            // Non-standard by name on purpose — see ctx_create_red_stream_source.
            ("createRedStreamSource", 1, ctx_create_red_stream_source),
            ("decodeAudioData", 1, ctx_decode_audio_data),
            ("setSinkId", 1, ctx_set_sink_id),
            ("resume", 0, ctx_resume),
            ("suspend", 0, ctx_suspend),
            ("close", 0, ctx_close),
        ],
        getters: &[
            ("currentTime", ctx_get_current_time),
            ("sampleRate", ctx_get_sample_rate),
            ("state", ctx_get_state),
            ("sinkId", ctx_get_sink_id),
            ("destination", get_slot_destination),
            ("listener", get_slot_listener),
        ],
        accessors: &[],
    });

    // webkitAudioContext alias (older engine feature-detection paths).
    let ctor = ctx.ctor;
    define(g, glue::rjs_global(g), "webkitAudioContext", ctor, 2);

    let boxed: &'static AudioClasses = Box::leak(Box::new(AudioClasses {
        node_cls,
        ctx_cls,
        param_cls,
        buffer_cls,
        listener_cls,
        s_audio,
        node_base,
        ctx,
        gain,
        stereo,
        panner,
        biquad,
        analyser,
        source,
        osc,
        stream_src,
        dest,
        listener,
        param,
        buffer,
    }));
    class_registry().lock().unwrap().insert(g as usize, boxed);
}

#[cfg(test)]
mod jsc_tests {
    use crate::api::Context;
    use crate::webapi::testing::*;

    /// Run an async body and return its resolved value stringified. A body that
    /// never settles comes back as `<pending>`, so "the test passed because
    /// nothing happened" is not one of the outcomes.
    fn run(c: &Context, body: &str) -> String {
        s(
            c,
            &format!(
                "globalThis.__out = '<pending>'; (async () => {{ {body} }})().then(\
                   v => {{ globalThis.__out = String(v); }}, \
                   e => {{ globalThis.__out = 'ERROR: ' + \
                     (e instanceof DOMException ? 'DOMException ' + e.name + ': ' : '') + \
                     ((e && e.message) || e); }}); 'started'"
            ),
        );
        // Microtasks alone are not enough for everything under test:
        // `decodeAudioData` settles from a NATIVE task (`web::schedule_native`)
        // that only runs on a real pump, so a decode would read `<pending>`
        // forever. Pump the scheduler, then drain, until it settles.
        for _ in 0..64 {
            unsafe {
                crate::web::pump(
                    c.raw(),
                    crate::web::PUMP_TIMERS | crate::web::PUMP_MICROTASKS,
                    core::ptr::null_mut(),
                )
            };
            if s(c, "globalThis.__out") != "<pending>" {
                break;
            }
        }
        s(c, "globalThis.__out")
    }

    #[test]
    fn the_harness_can_report_a_stall() {
        // Every async test below rests on `<pending>` being observable.
        let c = ctx();
        assert_eq!(run(&c, "await new Promise(() => {}); return 1;"), "<pending>");
    }

    #[test]
    fn a_context_exposes_the_element_free_output_selection_surface() {
        // The feature check the application's audio package performs. If either
        // half is missing it takes the `<audio>`-element path, which on this
        // host has no elements to take.
        let c = ctx();
        ok(&c, "const a = new AudioContext(); const r = ('setSinkId' in a) && ('sinkId' in a); a.close(); r");
        ok(&c, "typeof AudioContext.prototype.setSinkId === 'function'");
        ok(&c, "AudioContext.prototype.setSinkId.length === 1");
    }

    #[test]
    fn sink_id_starts_empty_and_records_what_was_set() {
        // The test a no-op `setSinkId` cannot pass: resolving the promise is
        // not enough, the id has to be observable afterwards. `''` and
        // `'default'` name the same endpoint, so no stream is disturbed —
        // which is what makes this deterministic with or without hardware.
        let c = ctx();
        assert_eq!(
            run(
                &c,
                "const a = new AudioContext();
                 const before = a.sinkId;
                 await a.setSinkId('default');
                 const mid = a.sinkId;
                 await a.setSinkId('');
                 const after = a.sinkId;
                 a.close();
                 return JSON.stringify([before, mid, after]);"
            ),
            r#"["","default",""]"#
        );
    }

    #[test]
    fn an_unknown_sink_is_refused_by_name() {
        // A device id that is not in `enumerateDevices()` must not be silently
        // turned into the default device: that failure is inaudible — the user
        // picks the headset, hears the speakers, and nothing reports an error.
        let c = ctx();
        let got = run(
            &c,
            "const a = new AudioContext();
             try { await a.setSinkId('no-such-endpoint'); return 'resolved'; }
             finally { a.close(); }",
        );
        assert!(got.contains("DOMException NotFoundError"), "{got}");
        assert!(got.contains("no-such-endpoint"), "{got}");
    }

    #[test]
    fn a_non_string_sink_is_a_type_error() {
        let c = ctx();
        for arg in ["42", "null", "undefined", "{ type: 'none' }"] {
            let got = run(
                &c,
                &format!(
                    "const a = new AudioContext();
                     try {{ await a.setSinkId({arg}); return 'resolved'; }}
                     finally {{ a.close(); }}"
                ),
            );
            assert!(got.starts_with("ERROR:"), "setSinkId({arg}) did not reject: {got}");
            assert!(got.contains("must be a device id string"), "setSinkId({arg}): {got}");
        }
    }

    #[test]
    fn a_closed_context_refuses_to_switch() {
        let c = ctx();
        let got = run(
            &c,
            "const a = new AudioContext();
             await a.close();
             await a.setSinkId('');
             return 'resolved';",
        );
        assert!(got.contains("DOMException InvalidStateError"), "{got}");
    }

    #[test]
    fn switching_to_a_real_device_keeps_the_graph_and_the_clock() {
        // Hardware-conditional by construction: the loop body runs once per
        // enumerated endpoint, so on a machine with no audio output it proves
        // only that the enumeration is empty. What it pins where hardware does
        // exist is the part a naive implementation gets wrong — rebuilding the
        // context instead of re-opening its stream, which would lose the node
        // graph, the `destination` identity and the clock.
        let c = ctx();
        let got = run(
            &c,
            "const a = new AudioContext();
             const dest = a.destination;
             const g = a.createGain();
             g.connect(dest);
             const t0 = a.currentTime;
             const ids = (await navigator.mediaDevices.enumerateDevices())
               .map(d => d.deviceId).filter(id => id !== 'default');
             for (const id of ids) {
               await a.setSinkId(id);
               if (a.sinkId !== id) return 'sinkId did not take: ' + a.sinkId;
               if (a.destination !== dest) return 'destination identity lost';
               if (!(a.sampleRate > 0)) return 'sampleRate: ' + a.sampleRate;
               if (a.currentTime < t0) return 'clock went backwards';
               if (a.state !== 'running') return 'state: ' + a.state;
               g.gain.value = 0.5;
               g.disconnect();
               g.connect(dest);
             }
             a.close();
             return 'ok:' + ids.length;",
        );
        assert!(got.starts_with("ok:"), "{got}");
        eprintln!("[setSinkId] round-tripped {} real output device(s)", &got[3..]);
    }

    // -----------------------------------------------------------------------
    // OscillatorNode
    // -----------------------------------------------------------------------

    /// Run the audio pump for roughly `ms`, so the audio thread gets to render
    /// some blocks. Returns whether the context's clock actually advanced —
    /// i.e. whether there is a real output stream. Every signal assertion below
    /// is conditional on that, so the suite is green on a machine with no sound
    /// card AND meaningful on one with.
    fn pump_for(c: &Context, ms: u64) -> bool {
        let before = s(c, "globalThis.__ctx.currentTime");
        for _ in 0..(ms / 10).max(1) {
            unsafe { super::pump(c.raw()) };
            c.drain_microtasks();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        unsafe { super::pump(c.raw()) };
        c.drain_microtasks();
        let after = s(c, "globalThis.__ctx.currentTime");
        before != after && after != "0"
    }

    /// RMS of an analyser's time-domain window, as JS sees it.
    fn analyser_rms(c: &Context, which: &str) -> f64 {
        s(
            c,
            &format!(
                "(() => {{ const b = new Float32Array(1024); {which}.getFloatTimeDomainData(b);
                   let acc = 0; for (const v of b) acc += v * v; return Math.sqrt(acc / b.length); }})()"
            ),
        )
        .parse()
        .unwrap_or(f64::NAN)
    }

    #[test]
    fn an_oscillator_exists_and_has_the_spec_shape() {
        let c = ctx();
        ok(&c, "typeof AudioContext.prototype.createOscillator === 'function'");
        ok(
            &c,
            "{ const a = new AudioContext(); const o = a.createOscillator();
             const r = o instanceof OscillatorNode
                    && o instanceof AudioNode
                    && o.type === 'sine'
                    && o.frequency instanceof AudioParam
                    && o.detune instanceof AudioParam
                    && o.frequency.value === 440
                    && o.detune.value === 0
                    && typeof o.start === 'function'
                    && typeof o.stop === 'function'
                    && typeof o.connect === 'function';
             a.close(); r }",
        );
    }

    #[test]
    fn the_four_waveforms_round_trip_and_custom_is_refused_by_name() {
        let c = ctx();
        ok(
            &c,
            "{ const a = new AudioContext(); const o = a.createOscillator();
               let r = true;
               for (const t of ['square','sawtooth','triangle','sine']) { o.type = t; r = r && o.type === t; }
               a.close(); r }",
        );
        // 'custom' silently behaving as a sine would be an inaudible bug.
        let msg = throws(
            &c,
            "{ const a = new AudioContext(); const o = a.createOscillator();
               try { o.type = 'custom'; } finally { a.close(); } }",
        );
        assert!(msg.contains("custom"), "{msg}");
        assert!(msg.contains("createPeriodicWave"), "the refusal must name what is missing: {msg}");
        let msg = throws(
            &c,
            "{ const a = new AudioContext(); const o = a.createOscillator();
               try { o.type = 'Sine'; } finally { a.close(); } }",
        );
        assert!(msg.contains("'Sine'"), "{msg}");
    }

    #[test]
    fn an_oscillator_starts_once() {
        let c = ctx();
        ok(
            &c,
            "(() => { const a = new AudioContext(); const o = a.createOscillator(); o.start();
               try { o.start(); return false; }
               catch (e) { return e instanceof DOMException && e.name === 'InvalidStateError' && e.code === 11; }
               finally { a.close(); } })()",
        );
    }

    #[test]
    fn a_muted_gain_silences_the_output_but_not_the_pre_gain_tap() {
        // The demo's graph, and the only honest way to test audio on a machine
        // somebody is using: an analyser taps the oscillator BEFORE a gain
        // pinned at 0, a second analyser taps after it. If `gain.value = 0`
        // were a no-op — or the analyser a stub returning zeros — exactly one
        // of these two assertions would fail.
        let c = ctx();
        s(
            &c,
            "globalThis.__ctx = new AudioContext();
             const a = globalThis.__ctx;
             globalThis.__pre = a.createAnalyser();
             globalThis.__post = a.createAnalyser();
             const g = a.createGain(); g.gain.value = 0;
             const o = a.createOscillator(); o.type = 'square'; o.frequency.value = 330;
             o.connect(__pre); __pre.connect(g); g.connect(__post); __post.connect(a.destination);
             o.start(); globalThis.__osc = o; 'wired'",
        );
        let live = pump_for(&c, 300);
        let (pre, post) = (analyser_rms(&c, "__pre"), analyser_rms(&c, "__post"));
        assert!(pre.is_finite() && post.is_finite(), "analyser returned {pre} / {post}");
        if !live {
            eprintln!("[oscillator] no output stream on this machine — signal assertions skipped");
            s(&c, "__ctx.close(); 'closed'");
            return;
        }
        // A full-scale square through a unity path: RMS ~1. Anything above a
        // tenth proves signal, and proves it is not the analyser's zero-fill.
        assert!(pre > 0.1, "the oscillator produced no signal at the pre-gain tap (RMS {pre})");
        assert!(post < 1e-3, "gain 0 did not silence the signal: post-gain RMS {post}");

        s(&c, "__osc.stop(); 'stopped'");
        pump_for(&c, 100);
        assert!(analyser_rms(&c, "__pre") < 1e-3, "stop() left the oscillator running");
        s(&c, "__ctx.close(); 'closed'");
    }

    // -----------------------------------------------------------------------
    // RedStreamSourceNode (streaming PCM)
    // -----------------------------------------------------------------------

    #[test]
    fn a_stream_source_is_a_connectable_node_with_an_id() {
        let c = ctx();
        ok(&c, "typeof AudioContext.prototype.createRedStreamSource === 'function'");
        ok(
            &c,
            "{ const a = new AudioContext();
             const n = a.createRedStreamSource({ sampleRate: 24000, channelCount: 1 });
             const r = n instanceof RedStreamSourceNode
                    && n instanceof AudioNode
                    && typeof n.streamId === 'number' && n.streamId >= 0
                    && n.sampleRate === 24000
                    && n.channelCount === 1
                    && typeof n.push === 'function'
                    && typeof n.close === 'function'
                    && n.connect(a.destination) === a.destination;
             a.close(); r }",
        );
    }

    #[test]
    fn stream_options_are_clamped_rather_than_trusted() {
        // A producer that asks for 999 channels at 1 Hz with no buffer is a bug
        // upstream; building an unusable channel out of it is a crash here.
        let c = ctx();
        ok(
            &c,
            "{ const a = new AudioContext();
               const n = a.createRedStreamSource({ sampleRate: 1, channelCount: 999, latencySeconds: 0 });
               const r = n.sampleRate >= 4000 && n.channelCount <= 16;
               a.close(); r }",
        );
        // No options at all is the common case and must not throw.
        ok(
            &c,
            "{ const a = new AudioContext(); const n = a.createRedStreamSource();
               const r = n.channelCount === 2 && n.sampleRate === 48000; a.close(); r }",
        );
    }

    #[test]
    fn pushing_the_wrong_thing_is_refused_by_name() {
        let c = ctx();
        for arg in ["", "42", "'frames'", "new Uint8Array(8)", "[0.1, 0.2]"] {
            let msg = throws(
                &c,
                &format!(
                    "{{ const a = new AudioContext(); const n = a.createRedStreamSource();
                       try {{ n.push({arg}); }} finally {{ a.close(); }} }}"
                ),
            );
            assert!(msg.contains("Float32Array"), "push({arg}): {msg}");
        }
    }

    #[test]
    fn a_closed_stream_refuses_pushes_instead_of_swallowing_them() {
        // The failure this prevents: a producer that keeps pushing into a dead
        // node forever, hearing nothing and being told nothing.
        let c = ctx();
        ok(
            &c,
            "{ const a = new AudioContext();
               const n = a.createRedStreamSource({ channelCount: 1 });
               const ok1 = typeof n.push(new Float32Array(128)) === 'number';
               n.close();
               let refused = false;
               try { n.push(new Float32Array(128)); } catch (e) { refused = /closed/.test(e.message); }
               a.close(); ok1 && refused }",
        );
    }

    #[test]
    fn a_stream_source_carries_pushed_pcm_into_the_graph() {
        // The LiveKit seam, end to end and inaudible: push a tone as raw PCM,
        // read it back off an analyser, keep the output at gain 0.
        let c = ctx();
        s(
            &c,
            "globalThis.__ctx = new AudioContext();
             const a = globalThis.__ctx;
             globalThis.__an = a.createAnalyser(); __an.fftSize = 2048;
             const mute = a.createGain(); mute.gain.value = 0;
             globalThis.__n = a.createRedStreamSource({ sampleRate: 48000, channelCount: 1, latencySeconds: 0.05 });
             __n.connect(__an); __an.connect(mute); mute.connect(a.destination);
             globalThis.__blk = new Float32Array(960);   // 20 ms
             globalThis.__ph = 0;
             globalThis.__fill = () => {
               for (let i = 0; i < __blk.length; i++) { __blk[i] = Math.sin(__ph); __ph += 2 * Math.PI * 440 / 48000; }
               return __n.push(__blk);
             };
             'wired'",
        );

        // Prime the channel, then keep it fed across the pump ticks.
        let mut accepted_total = 0f64;
        for _ in 0..40 {
            accepted_total += s(&c, "__fill()").parse::<f64>().unwrap_or(0.0);
            unsafe { super::pump(c.raw()) };
            c.drain_microtasks();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let live = s(&c, "__ctx.currentTime") != "0";
        let rms = analyser_rms(&c, "__an");

        if !live {
            // Headless: the processor never runs, so the channel never starts
            // and every push is correctly refused. That IS the contract.
            eprintln!("[stream] no output stream — pushed {accepted_total} frames, playout not asserted");
            s(&c, "__ctx.close(); 'closed'");
            return;
        }
        assert!(accepted_total > 0.0, "the stream accepted no frames at all");
        assert!(rms > 0.1, "pushed PCM never reached the analyser (RMS {rms})");
        s(&c, "__ctx.close(); 'closed'");
    }

    // -----------------------------------------------------------------------
    // createMediaElementSource honesty
    // -----------------------------------------------------------------------

    #[test]
    fn create_media_element_source_still_returns_a_connectable_node() {
        // It must NOT throw — `try { ctx.createMediaElementSource(el) } catch`
        // is the standard capability probe, and code that falls back on the
        // throw would take the wrong branch. The advisory on stderr is where
        // the honesty lives; see the function's doc comment.
        let c = ctx();
        ok(
            &c,
            "{ const a = new AudioContext();
               const n = a.createMediaElementSource({});
               const r = n instanceof AudioNode && n.connect(a.destination) === a.destination;
               a.close(); r }",
        );
    }

    // -----------------------------------------------------------------------
    // decodeAudioData
    // -----------------------------------------------------------------------

    /// A minimal 16-bit PCM WAV: `frames` samples of a full-scale square at
    /// `rate`, mono. Built here rather than checked in so the test explains its
    /// own fixture.
    fn wav_fixture(rate: u32, frames: u32) -> Vec<u8> {
        let data_len = frames * 2;
        let mut v = Vec::with_capacity(44 + data_len as usize);
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
        v.extend_from_slice(&1u16.to_le_bytes()); // PCM
        v.extend_from_slice(&1u16.to_le_bytes()); // mono
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
        v.extend_from_slice(&2u16.to_le_bytes()); // block align
        v.extend_from_slice(&16u16.to_le_bytes()); // bits
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        // 100-frame period square wave — real content, trivially verifiable.
        for i in 0..frames {
            let smp: i16 = if (i / 50) % 2 == 0 { 24_000 } else { -24_000 };
            v.extend_from_slice(&smp.to_le_bytes());
        }
        v
    }

    fn as_js_bytes(v: &[u8]) -> String {
        v.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(",")
    }

    #[test]
    fn decode_audio_data_decodes_a_wav_and_the_samples_are_real() {
        let c = ctx();
        let wav = wav_fixture(48_000, 4_800);
        s(&c, &format!("globalThis.__wav = new Uint8Array([{}]).buffer; 'ok'", as_js_bytes(&wav)));

        let got = run(
            &c,
            "const a = new AudioContext();
             const buf = await a.decodeAudioData(__wav);
             const ch = buf.getChannelData(0);
             let peak = 0, energy = 0;
             for (const v of ch) { peak = Math.max(peak, Math.abs(v)); energy += v * v; }
             const rms = Math.sqrt(energy / ch.length);
             a.close();
             return JSON.stringify({
               ch: buf.numberOfChannels, rate: buf.sampleRate,
               peak: Math.round(peak * 1000) / 1000, rms: Math.round(rms * 1000) / 1000,
             });",
        );
        assert!(!got.starts_with("ERROR"), "{got}");
        // 24000/32768 = 0.732, and a square wave's RMS equals its peak — which
        // a decoder that returned zeros, or halved the amplitude, would fail.
        assert!(got.contains(r#""ch":1"#), "{got}");
        assert!(got.contains(r#""peak":0.732"#), "decoded amplitude is wrong: {got}");
        assert!(got.contains(r#""rms":0.732"#), "a decoded square must have RMS == peak: {got}");
    }

    #[test]
    fn an_undecodable_blob_rejects_by_naming_what_would_work() {
        // "Unsupported" alone sends the caller looking for a bug in their file.
        let c = ctx();
        let got = run(
            &c,
            "const a = new AudioContext();
             try { await a.decodeAudioData(new Uint8Array([1,2,3,4,5,6,7,8,9,10]).buffer); return 'resolved'; }
             finally { a.close(); }",
        );
        assert!(got.starts_with("ERROR"), "garbage decoded?! {got}");
        assert!(got.contains("DOMException EncodingError"), "{got}");
        assert!(got.contains("WAV"), "the rejection must list what IS decodable: {got}");
        assert!(got.contains("Opus"), "the rejection must name the notable absences: {got}");
    }

    #[test]
    fn a_decoded_buffer_plays_through_a_buffer_source() {
        // AudioBuffer → AudioBufferSourceNode → analyser, output muted.
        let c = ctx();
        let wav = wav_fixture(48_000, 48_000);
        s(&c, &format!("globalThis.__wav = new Uint8Array([{}]).buffer; 'ok'", as_js_bytes(&wav)));
        let started = run(
            &c,
            "globalThis.__ctx = new AudioContext();
             const a = globalThis.__ctx;
             globalThis.__an = a.createAnalyser(); __an.fftSize = 2048;
             const mute = a.createGain(); mute.gain.value = 0;
             const buf = await a.decodeAudioData(__wav);
             const src = a.createBufferSource();
             src.buffer = buf; src.loop = true;
             src.connect(__an); __an.connect(mute); mute.connect(a.destination);
             src.start();
             return 'started';",
        );
        assert_eq!(started, "started", "{started}");
        let live = pump_for(&c, 300);
        let rms = analyser_rms(&c, "__an");
        if !live {
            eprintln!("[buffer source] no output stream — playout not asserted");
            s(&c, "__ctx.close(); 'closed'");
            return;
        }
        assert!(rms > 0.1, "the decoded buffer never reached the analyser (RMS {rms})");
        s(&c, "__ctx.close(); 'closed'");
    }

    // -----------------------------------------------------------------------
    // suspend / resume
    // -----------------------------------------------------------------------

    #[test]
    fn suspend_and_resume_round_trip_the_state() {
        let c = ctx();
        assert_eq!(
            run(
                &c,
                "const a = new AudioContext();
                 const s0 = a.state;
                 await a.suspend(); const s1 = a.state;
                 await a.resume();  const s2 = a.state;
                 await a.close();   const s3 = a.state;
                 return JSON.stringify([s0, s1, s2, s3]);"
            ),
            r#"["running","suspended","running","closed"]"#
        );
    }
}

#[cfg(test)]
mod timeline_tests {
    use super::{Timeline, Tl};

    /// `Timeline::eval` is the whole of `AudioParam` automation, and it is pure
    /// arithmetic — so it is testable without a JS context or an audio device.
    fn tl(start: f64, evs: Vec<Tl>) -> Timeline {
        Timeline { evs, start_v: start }
    }

    #[test]
    fn a_linear_ramp_is_linear() {
        let t = tl(0.0, vec![Tl::Set { t: 0.0, v: 0.0 }, Tl::Lin { t: 1.0, v: 1.0 }]);
        for (now, want) in [(0.0, 0.0), (0.25, 0.25), (0.5, 0.5), (0.75, 0.75)] {
            let (v, done) = t.eval(now);
            assert!((v - want).abs() < 1e-6, "at {now}: {v}, want {want}");
            assert!(!done);
        }
        assert_eq!(t.eval(1.0), (1.0, true));
    }

    #[test]
    fn an_exponential_ramp_is_exponential_and_not_a_line() {
        // The bug this pins: `exponentialRampToValueAtTime` used to schedule a
        // LINEAR event. Both curves share their endpoints, so only the middle
        // tells them apart — at the halfway point an exponential 0.001→1 ramp
        // is at sqrt(0.001) ≈ 0.0316, a line is at ~0.5.
        let t = tl(0.001, vec![Tl::Set { t: 0.0, v: 0.001 }, Tl::Exp { t: 1.0, v: 1.0 }]);
        let (v, _) = t.eval(0.5);
        let want = 0.001f64 * (1.0f64 / 0.001).powf(0.5);
        assert!((v - want).abs() < 1e-6, "midpoint {v}, want {want}");
        assert!(v < 0.05, "an exponential ramp must not be halfway up at halfway: {v}");

        // Endpoints still land exactly.
        assert!((t.eval(0.0).0 - 0.001).abs() < 1e-9);
        assert_eq!(t.eval(1.0), (1.0, true));
    }

    #[test]
    fn an_exponential_ramp_through_zero_degrades_to_linear_instead_of_nan() {
        // The spec leaves 0 and sign changes undefined; a NaN escaping into a
        // gain param is silence-or-worse, so this must be a defined value.
        for (from, to) in [(0.0, 1.0), (1.0, 0.0), (-1.0, 1.0)] {
            let t = tl(from, vec![Tl::Set { t: 0.0, v: from }, Tl::Exp { t: 1.0, v: to }]);
            let (v, _) = t.eval(0.5);
            assert!(v.is_finite(), "{from} → {to} produced {v}");
            assert!((v - (from + (to - from) * 0.5)).abs() < 1e-6, "{from} → {to}: {v}");
        }
    }

    #[test]
    fn set_target_decays_toward_its_target_and_reports_done() {
        let t = tl(1.0, vec![Tl::Target { t: 0.0, v: 0.0, tau: 0.1 }]);
        let (early, done_early) = t.eval(0.05);
        assert!(early > 0.5 && early < 1.0, "{early}");
        assert!(!done_early);
        let (late, done_late) = t.eval(2.0);
        assert!(late.abs() < 1e-3, "{late}");
        assert!(done_late, "a settled setTargetAtTime must retire its timeline");
    }

    #[test]
    fn a_future_event_holds_the_current_value() {
        let t = tl(0.25, vec![Tl::Set { t: 10.0, v: 1.0 }]);
        assert_eq!(t.eval(0.0), (0.25, false));
    }
}
