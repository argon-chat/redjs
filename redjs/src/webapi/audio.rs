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
//! | `decodeAudioData`        | symphonium (resampled to the stream rate)    |
//! | `currentTime`            | the audio-stream sample clock (seconds)      |
//!
//! `AudioParam` automation (`setValueAtTime`/`linearRampToValueAtTime`/
//! `setTargetAtTime`) is evaluated on every pump tick (once per frame) and
//! forwarded as Firewheel param events; the nodes' built-in ~15 ms smoothers
//! turn the per-frame steps into artifact-free ramps. Ends of playback are
//! detected from the sampler's shared playback state and fire `onended`.
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
//! - `exponentialRampToValueAtTime` falls back to a linear ramp.

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

use super::hrtf::{HrtfDatabase, WebPannerConfig, WebPannerNode};
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
    Target { t: f64, v: f64, tau: f64 },
}

impl Tl {
    fn time(&self) -> f64 {
        match *self {
            Tl::Set { t, .. } | Tl::Lin { t, .. } | Tl::Target { t, .. } => t,
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
        Self {
            engine,
            id,
            params,
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
}

impl Engine {
    fn now(&self) -> f64 {
        self.fw.audio_clock().seconds.0
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

/// Push the sampler's full state and start playback.
fn do_start(eng: &mut Engine, rec: &mut NodeRec, offset: f64) {
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

/// Allocates a node in the graph + registry. Returns the new handle.
fn alloc_node(gl: &mut Glob, engine: u64, params: Params) -> Option<u64> {
    let eng = gl.engines.get_mut(&engine)?;
    if eng.state == ST_CLOSED {
        return None;
    }
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
        Params::Passthrough => eng.fw.add_node(VolumeNode::default(), None),
        Params::Dest => eng.master,
    };
    let handle = gl.next;
    gl.next += 1;
    gl.nodes.insert(handle, NodeRec::new(engine, id, params));
    Some(handle)
}

// ---------------------------------------------------------------------------
// AudioNode base: connect / disconnect
// ---------------------------------------------------------------------------

const STEREO_PORTS: &[(u32, u32)] = &[(0, 0), (1, 1)];

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
    if let Some(eng) = gl.engines.get_mut(&engine) {
        let _ = eng.fw.connect(sid, did, STEREO_PORTS, true);
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
                let _ = eng.fw.disconnect(sid, dr.id, STEREO_PORTS);
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

/// exponentialRampToValueAtTime — approximated linearly (see module docs).
unsafe extern "sysv64" fn param_exp_ramp(g: *mut c_void, cf: *mut c_void) -> u64 {
    param_schedule(g, cf, |v, t| Tl::Lin { t, v })
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

unsafe extern "sysv64" fn src_start(g: *mut c_void, cf: *mut c_void) -> u64 {
    let when = dec_f64(cf_arg(cf, 0)).unwrap_or(0.0);
    let offset = dec_f64(cf_arg(cf, 1)).unwrap_or(0.0);
    let this = cf_this(cf);
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, "AudioBufferSourceNode.start on wrong this");
    };
    let mut gl = glob().lock().unwrap();
    let gl = &mut *gl;
    let Some(rec) = gl.nodes.get_mut(&handle) else { return UNDEF };
    if rec.started {
        return throw_error(g, 0, "AudioBufferSourceNode.start: already started");
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
        return throw_type_error(g, "AudioBufferSourceNode.stop on wrong this");
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

/// suspend() mutes the master bus / resume() restores it (the stream keeps
/// running so `currentTime` keeps advancing — see module docs).
unsafe fn ctx_set_suspended(g: *mut c_void, cf: *mut c_void, suspended: bool) -> u64 {
    let Some(h) = this_engine_handle(g, cf) else {
        return rejected_promise(g, 1, "AudioContext method on wrong this");
    };
    let mut gl = glob().lock().unwrap();
    if let Some(eng) = gl.engines.get_mut(&h) {
        if eng.state == ST_CLOSED {
            return rejected_promise(g, 0, "AudioContext is closed");
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

/// createMediaElementSource: no media elements in the player — a silent
/// pass-through keeps callers alive instead of crashing.
unsafe extern "sysv64" fn ctx_create_media_element_source(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    ctx_create_node(g, cf, Params::Passthrough, c.node_base.proto, &[])
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
        .map_err(|e| format!("decodeAudioData: {e}"))?;
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
        return rejected_promise(g, 0, "decodeAudioData: empty buffer");
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
                let m: Vec<u16> = msg.encode_utf16().collect();
                let mut err = UNDEF;
                glue::rjs_error_create(g, 0, m.as_ptr(), m.len(), &mut err);
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
                gl.nodes.retain(|_, r| r.engine != h);
                gl.timelines.retain(|(n, _), _| *n != h);
                continue;
            }
            if let Some(rec) = gl.nodes.remove(&h) {
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
            ("createBuffer", 3, ctx_create_buffer),
            ("createMediaElementSource", 1, ctx_create_media_element_source),
            ("decodeAudioData", 1, ctx_decode_audio_data),
            ("resume", 0, ctx_resume),
            ("suspend", 0, ctx_suspend),
            ("close", 0, ctx_close),
        ],
        getters: &[
            ("currentTime", ctx_get_current_time),
            ("sampleRate", ctx_get_sample_rate),
            ("state", ctx_get_state),
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
        dest,
        listener,
        param,
        buffer,
    }));
    class_registry().lock().unwrap().insert(g as usize, boxed);
}
