//! Native Web Audio API (a subset) over the Firewheel
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
//! | `AudioBufferSourceNode`  | `SamplerNode` reading a `SourceSample` view (offset, duration, loop region) |
//! | `AudioBuffer`            | planar `f32` `SampleResource`                |
//! | `OscillatorNode`         | `OscNode` (webapi/osc.rs): PolyBLEP-limited  |
//! | `ChannelSplitterNode`    | `SplitterNode` (ours): output k = port k     |
//! | `ChannelMergerNode`      | `MergerNode` (ours): input j = ports 2j, 2j+1 |
//! | `RedStreamSourceNode`    | `StreamWriterNode` (webapi/pcm.rs) — ours     |
//! | `MediaStreamAudioSourceNode` | `StreamWriterNode`, fed by a track (webapi/mediastream.rs) |
//! | `MediaStreamAudioDestinationNode` | `TapNode` (ours): 2 inputs, no outputs, hands each block to its track |
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
//! - `AnalyserNode` is REAL: a bypass monitor node captures the
//!   last `MONITOR_WINDOW` mono-mixed samples into a lock-free ring; the
//!   getters serve true time-domain data and a Blackman-windowed radix-2 FFT
//!   with spec smoothing. Deviations: `fftSize` caps at 4096 (spec: 32768) and
//!   reads may tear across a processing-block boundary (fine for meters).
//! - Scheduling (`start`/`stop`, automation) lands on the pump tick at or after
//!   the requested time, not on the exact sample.
//! - The graph mixes in stereo. An output wider than two channels (a
//!   `ChannelMergerNode` of 3+ inputs, a wide `RedStreamSourceNode`) keeps
//!   channels 0 and 1 when it feeds anything but a splitter, where a browser
//!   would carry every channel or down-mix them (5.1 → stereo). A one-time
//!   advisory says so on the first such `connect`.
//! - `channelCount` / `channelCountMode` / `channelInterpretation` exist on
//!   `ChannelSplitterNode` and `ChannelMergerNode` only — the two nodes whose
//!   values the spec pins (see [`SplitterNode`] / [`MergerNode`]). The other
//!   nodes do not have the three attributes.
//! - `loadNativeAudioWorklet` selects precompiled Rust DSP; it does not execute
//!   arbitrary JavaScript worklet modules (see `audio/native_worklet.rs`).
//! - No `OfflineAudioContext`, JavaScript `AudioWorklet`, `ConvolverNode`, `DelayNode`,
//!   `WaveShaperNode`, `DynamicsCompressorNode`, or `PeriodicWave`. Each is
//!   absent rather than stubbed, so `typeof ctx.createDelay === 'function'`
//!   tells the truth.
//! - `createMediaElementSource` is a silent pass-through that says so — see
//!   `ctx_create_media_element_source`.

use core::ffi::c_void;
use std::collections::HashMap;
use std::num::{NonZeroU32, NonZeroUsize};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
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
use super::audio_worklet::{NativeKind, NativeMessage, NativeNode, NativeShared};

mod native_worklet;

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
const P_NATIVE_ENABLED: u8 = 20;
const P_NATIVE_THRESHOLD: u8 = 21;
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

impl PlanarBuffer {
    fn len_frames(&self) -> u64 {
        self.data.first().map(|c| c.len() as u64).unwrap_or(0)
    }
}

/// Seconds of buffer content → whole frames at `rate`.
fn secs_to_frames(s: f64, rate: f64) -> u64 {
    if s.is_finite() && s > 0.0 {
        (s * rate).round() as u64
    } else {
        0
    }
}

/// The `loop` / `loopStart` / `loopEnd` attributes of one
/// `AudioBufferSourceNode`, shared with the audio thread so a change made
/// while the source plays takes effect from the current playhead, as it does
/// in a browser.
#[derive(Default)]
struct SourceCtl {
    looping: AtomicBool,
    /// `loopStart`, seconds (f64 bits).
    loop_start: AtomicU64,
    /// `loopEnd`, seconds (f64 bits).
    loop_end: AtomicU64,
    /// Raised by the audio thread when the walk is over: a non-looping walk
    /// went past the end of the buffer, or `duration` worth of content has
    /// been played. The pump ends the node on it.
    done: AtomicBool,
}

impl SourceCtl {
    fn loop_start(&self) -> f64 {
        f64::from_bits(self.loop_start.load(Ordering::Relaxed))
    }

    fn loop_end(&self) -> f64 {
        f64::from_bits(self.loop_end.load(Ordering::Relaxed))
    }

    /// The spec's `actualLoopStart`..`actualLoopEnd` in frames of a
    /// `len`-frame buffer at `rate`, or `None` when the source is not looping.
    /// A `loopStart`/`loopEnd` pair that is not a forward range inside the
    /// buffer loops the whole buffer, as the spec's defaults (0, 0) do.
    fn region(&self, len: u64, rate: f64) -> Option<(u64, u64)> {
        if !self.looping.load(Ordering::Relaxed) || len == 0 {
            return None;
        }
        let (ls, le) = (self.loop_start(), self.loop_end());
        if ls >= 0.0 && le > 0.0 && ls < le {
            let (a, b) = (secs_to_frames(ls, rate).min(len), secs_to_frames(le, rate).min(len));
            if a < b {
                return Some((a, b));
            }
        }
        Some((0, len))
    }
}

/// What a started `AudioBufferSourceNode` hands the sampler: its buffer as the
/// spec's playback algorithm walks it — from `offset`, for `duration` seconds of
/// content (loop iterations included), wrapping `loopEnd` → `loopStart` while
/// `loop` is set.
///
/// The sampler sees a plain sample of `len` frames, played once from frame 0
/// (`len` = `duration`, or unbounded); this view maps each of those frames onto
/// the buffer. So `start(when, offset, duration)` plays exactly that slice —
/// an audio sprite out of an atlas — and a loop region loops only itself.
struct SourceSample {
    buf: Arc<PlanarBuffer>,
    ctl: Arc<SourceCtl>,
    /// The buffer frame `start(when, offset)` begins at.
    start: u64,
    /// Frames to output in all: `duration`, or [`SourceSample::UNBOUNDED`].
    len: u64,
    /// Audio-thread cursor: the next sampler frame expected, and the buffer
    /// frame that plays there. A sequential read continues the walk (so a loop
    /// change mid-play applies from the current playhead); any other read
    /// re-derives the position from `start` with the current attributes.
    next_v: AtomicU64,
    pos: AtomicU64,
}

impl SourceSample {
    /// "Until stopped": a looping source without a `duration`, or one whose
    /// end is the end of the buffer (found by the walk, so that `loop` set
    /// during playback still loops).
    const UNBOUNDED: u64 = u64::MAX / 4;

    fn new(buf: Arc<PlanarBuffer>, ctl: Arc<SourceCtl>, start: u64, len: u64) -> Self {
        Self { buf, ctl, start, len, next_v: AtomicU64::new(0), pos: AtomicU64::new(start) }
    }

    fn rate(&self) -> f64 {
        self.buf.sample_rate.max(1) as f64
    }

    /// The buffer frame `v` sampler frames after the start, the attributes
    /// assumed constant.
    fn walk(&self, v: u64) -> u64 {
        let blen = self.buf.len_frames();
        match self.ctl.region(blen, self.rate()) {
            None => self.start.saturating_add(v).min(blen),
            Some((a, b)) => {
                // Starting at or past `loopEnd` begins at `loopStart`.
                let p = if self.start >= b { a } else { self.start }.saturating_add(v);
                if p < b { p } else { a + (p - b) % (b - a) }
            }
        }
    }
}

impl SampleResourceInfo for SourceSample {
    fn num_channels(&self) -> NonZeroUsize {
        NonZeroUsize::new(self.buf.data.len().max(1)).unwrap()
    }

    fn len_frames(&self) -> u64 {
        self.len
    }

    fn sample_rate(&self) -> Option<NonZeroU32> {
        NonZeroU32::new(self.buf.sample_rate)
    }
}

impl SampleResource for SourceSample {
    fn fill_buffers(&self, buffers: &mut [&mut [f32]], buffer_range: Range<usize>, start_frame: u64) {
        let frames = buffer_range.len();
        let blen = self.buf.len_frames();
        let rate = self.rate();
        let mut pos = if start_frame == self.next_v.load(Ordering::Relaxed) {
            self.pos.load(Ordering::Relaxed)
        } else {
            self.walk(start_frame)
        };
        let mut filled = 0usize;
        while filled < frames {
            let region = self.ctl.region(blen, rate);
            let end = match region {
                Some((a, b)) => {
                    if pos >= b {
                        pos = a;
                    }
                    b
                }
                None => blen,
            };
            let at = buffer_range.start + filled;
            if pos >= end {
                // Not looping and past the end of the buffer: silence, and the
                // node has ended.
                for out in buffers.iter_mut() {
                    out[at..buffer_range.end].fill(0.0);
                }
                self.ctl.done.store(true, Ordering::Relaxed);
                break;
            }
            let n = ((end - pos) as usize).min(frames - filled);
            let p = pos as usize;
            for (ch, out) in self.buf.data.iter().zip(buffers.iter_mut()) {
                out[at..at + n].copy_from_slice(&ch[p..p + n]);
            }
            pos += n as u64;
            filled += n;
        }
        let next = start_frame + frames as u64;
        if next >= self.len || (pos >= blen && self.ctl.region(blen, rate).is_none()) {
            self.ctl.done.store(true, Ordering::Relaxed);
        }
        self.next_v.store(next, Ordering::Relaxed);
        self.pos.store(pos, Ordering::Relaxed);
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
    /// Context time at that moment. A ramp with no event before it starts
    /// here, from `start_v`: the spec treats it as if `setValueAtTime(value,
    /// currentTime)` had been called first.
    start_t: f64,
}

impl Timeline {
    /// (value at `now`, finished)
    fn eval(&self, now: f64) -> (f64, bool) {
        let mut cur = self.start_v;
        let mut cur_t = self.start_t;
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
// AnalyserNode backing: a bypass monitor node captures the last
// MONITOR_WINDOW mono-mixed samples into a lock-free ring shared with the main
// thread; the analysis getters (time-domain + FFT) read it on demand.
// ---------------------------------------------------------------------------

/// Native max `fftSize` (power of two). The spec allows up to 32768; typical use is
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

// ---------------------------------------------------------------------------
// The render reference: what an AudioContext plays, for an echo canceller.
// ---------------------------------------------------------------------------
//
// An echo canceller on the microphone (mediastream.rs, "Processing") has to
// know what the speakers play. Here that is the destination of every
// AudioContext: when the embedder installs a reference sink, each context
// built after that gets a sink node beside its graph output, fed by the same
// master volume, which hands every block it renders to the embedder with the
// context's key. A context built before then has no tap.

/// Receives `(context key, interleaved block, channels, sample rate)` on the
/// context's audio thread, once per rendered block. It must not block.
pub type RenderReference = fn(u64, &[f32], usize, u32);

static RENDER_REFERENCE: OnceLock<RenderReference> = OnceLock::new();
static NEXT_REFERENCE_KEY: AtomicU64 = AtomicU64::new(1);

/// Install the embedder's render reference, once per process, before the
/// page builds its `AudioContext`s.
pub fn set_render_reference(f: RenderReference) {
    let _ = RENDER_REFERENCE.set(f);
}

#[derive(Clone)]
struct RefTapNode {
    key: u64,
    sink: RenderReference,
}

impl firewheel::node::AudioNode for RefTapNode {
    type Configuration = firewheel::node::EmptyConfig;

    fn info(&self, _config: &Self::Configuration) -> firewheel::node::AudioNodeInfo {
        firewheel::node::AudioNodeInfo::new().debug_name("render_reference").channel_config(
            firewheel::channel_config::ChannelConfig {
                num_inputs: firewheel::channel_config::ChannelCount::STEREO,
                num_outputs: firewheel::channel_config::ChannelCount::ZERO,
            },
        )
    }

    fn construct_processor(
        &self,
        _config: &Self::Configuration,
        cx: firewheel::node::ConstructProcessorContext,
    ) -> impl firewheel::node::AudioNodeProcessor {
        RefTapProcessor {
            key: self.key,
            sink: self.sink,
            scratch: vec![0.0; cx.stream_info.max_block_frames.get() as usize * 2],
            rate: cx.stream_info.sample_rate.get(),
        }
    }
}

struct RefTapProcessor {
    key: u64,
    sink: RenderReference,
    scratch: Vec<f32>,
    rate: u32,
}

impl firewheel::node::AudioNodeProcessor for RefTapProcessor {
    fn process(
        &mut self,
        info: &firewheel::node::ProcInfo,
        buffers: firewheel::node::ProcBuffers,
        _events: &mut firewheel::event::ProcEvents,
        _extra: &mut firewheel::node::ProcExtra,
    ) -> firewheel::node::ProcessStatus {
        let frames = info.frames.min(self.scratch.len() / 2);
        for ch in 0..2 {
            let silent = info.in_silence_mask.is_channel_silent(ch);
            let input = buffers.inputs.get(ch);
            for i in 0..frames {
                self.scratch[2 * i + ch] = match input {
                    Some(buf) if !silent => buf[i],
                    _ => 0.0,
                };
            }
        }
        (self.sink)(self.key, &self.scratch[..frames * 2], 2, self.rate);
        firewheel::node::ProcessStatus::ClearAllOutputs
    }

    fn new_stream(&mut self, stream_info: &firewheel::StreamInfo, _context: &mut firewheel::node::ProcStreamCtx) {
        self.rate = stream_info.sample_rate.get();
        self.scratch.resize(stream_info.max_block_frames.get() as usize * 2, 0.0);
    }
}

// ---------------------------------------------------------------------------
// MediaStreamAudioDestinationNode backing: a sink that hands every block to a
// track (webapi/mediastream.rs).
// ---------------------------------------------------------------------------
//
// Two inputs (the node's stereo input), no outputs. Firewheel schedules every
// node of the graph, a sink included, so the processor runs once per block
// whether or not anything downstream exists. It interleaves into a buffer
// sized off the audio thread and calls `Source::deliver`, which never blocks.

#[derive(Clone)]
struct TapNode {
    source: Arc<super::mediastream::Source>,
}

impl firewheel::node::AudioNode for TapNode {
    type Configuration = firewheel::node::EmptyConfig;

    fn info(&self, _config: &Self::Configuration) -> firewheel::node::AudioNodeInfo {
        firewheel::node::AudioNodeInfo::new()
            .debug_name("webaudio_media_stream_destination")
            .channel_config(firewheel::channel_config::ChannelConfig {
                num_inputs: firewheel::channel_config::ChannelCount::STEREO,
                num_outputs: firewheel::channel_config::ChannelCount::ZERO,
            })
    }

    fn construct_processor(
        &self,
        _config: &Self::Configuration,
        cx: firewheel::node::ConstructProcessorContext,
    ) -> impl firewheel::node::AudioNodeProcessor {
        let rate = cx.stream_info.sample_rate.get();
        self.source.set_format(2, rate);
        TapProcessor {
            source: Arc::clone(&self.source),
            scratch: vec![0.0; cx.stream_info.max_block_frames.get() as usize * 2],
            rate,
        }
    }
}

struct TapProcessor {
    source: Arc<super::mediastream::Source>,
    scratch: Vec<f32>,
    rate: u32,
}

impl firewheel::node::AudioNodeProcessor for TapProcessor {
    fn process(
        &mut self,
        info: &firewheel::node::ProcInfo,
        buffers: firewheel::node::ProcBuffers,
        _events: &mut firewheel::event::ProcEvents,
        _extra: &mut firewheel::node::ProcExtra,
    ) -> firewheel::node::ProcessStatus {
        let frames = info.frames.min(self.scratch.len() / 2);
        for ch in 0..2 {
            let silent = info.in_silence_mask.is_channel_silent(ch);
            let input = buffers.inputs.get(ch);
            for i in 0..frames {
                self.scratch[2 * i + ch] = match input {
                    Some(buf) if !silent => buf[i],
                    _ => 0.0,
                };
            }
        }
        self.source.deliver(&self.scratch[..frames * 2], 2, self.rate);
        firewheel::node::ProcessStatus::ClearAllOutputs
    }

    fn new_stream(&mut self, stream_info: &firewheel::StreamInfo, _context: &mut firewheel::node::ProcStreamCtx) {
        // Off the audio thread: allocating here is fine.
        self.rate = stream_info.sample_rate.get();
        self.scratch.resize(stream_info.max_block_frames.get() as usize * 2, 0.0);
        self.source.set_format(2, self.rate);
    }
}

// ---------------------------------------------------------------------------
// ChannelSplitterNode / ChannelMergerNode backing: two custom Firewheel nodes.
// ---------------------------------------------------------------------------
//
// Firewheel has channels (ports) but no node with several OUTPUTS, which is
// what a Web Audio splitter is (n mono outputs) — nor, mirrored, with several
// INPUTS (the merger). Both are written as ports:
//
// - a splitter is n ports in and n ports out, and its processor is the
//   engine's own copy (`Bypass`). Web Audio output k IS Firewheel output port
//   k, so `splitter.connect(dest, k)` wires port k alone (`conn_ports`). Its
//   input is "discrete": a stereo source fills ports 0 and 1, the rest stay
//   silent, exactly the spec's discrete up-mix;
// - a merger's Web Audio input j is the PAIR of Firewheel input ports
//   (2j, 2j+1). A stereo source lands on the pair, a mono one is fed into both
//   ports, and the processor down-mixes the pair to the mono channel j of its
//   one output: 0.5·(L+R) under "speakers" (the spec's stereo → mono), L under
//   "discrete". Mixing is linear, so several connections that Firewheel sums
//   on one pair come out exactly as the spec's per-connection mix-then-sum.
//
// Neither needs a Firewheel event: the merger's one live attribute
// (`channelInterpretation`) is an atomic shared with its processor.

/// The most `numberOfOutputs` / `numberOfInputs` a splitter / merger takes:
/// Chromium's `BaseAudioContext::MaxNumberOfChannels()` (and `createBuffer`'s
/// channel cap here). A merger of 32 is 64 Firewheel input ports — exactly
/// Firewheel's `MAX_CHANNELS`.
const MAX_SPLIT_CHANNELS: u32 = 32;

/// `ChannelSplitterNode`: n ports in, n ports out, copied through.
#[derive(Clone)]
struct SplitterNode {
    n: u32,
}

impl firewheel::node::AudioNode for SplitterNode {
    type Configuration = firewheel::node::EmptyConfig;

    fn info(&self, _config: &Self::Configuration) -> firewheel::node::AudioNodeInfo {
        let n = firewheel::channel_config::ChannelCount::new(self.n).unwrap();
        firewheel::node::AudioNodeInfo::new()
            .debug_name("webaudio_channel_splitter")
            .channel_config(firewheel::channel_config::ChannelConfig { num_inputs: n, num_outputs: n })
    }

    fn construct_processor(
        &self,
        _config: &Self::Configuration,
        _cx: firewheel::node::ConstructProcessorContext,
    ) -> impl firewheel::node::AudioNodeProcessor {
        SplitterProcessor
    }
}

struct SplitterProcessor;

impl firewheel::node::AudioNodeProcessor for SplitterProcessor {
    fn process(
        &mut self,
        _info: &firewheel::node::ProcInfo,
        _buffers: firewheel::node::ProcBuffers,
        _events: &mut firewheel::event::ProcEvents,
        _extra: &mut firewheel::node::ProcExtra,
    ) -> firewheel::node::ProcessStatus {
        // Input channel k → output port k is the whole node, and the engine's
        // own copy does it, silence mask included.
        firewheel::node::ProcessStatus::Bypass
    }
}

/// `ChannelMergerNode`: 2n ports in (a pair per Web Audio input), n out.
#[derive(Clone)]
struct MergerNode {
    n: u32,
    /// `channelInterpretation === "discrete"`, written by the JS setter.
    discrete: ArcGc<AtomicBool>,
}

impl firewheel::node::AudioNode for MergerNode {
    type Configuration = firewheel::node::EmptyConfig;

    fn info(&self, _config: &Self::Configuration) -> firewheel::node::AudioNodeInfo {
        firewheel::node::AudioNodeInfo::new()
            .debug_name("webaudio_channel_merger")
            .channel_config(firewheel::channel_config::ChannelConfig {
                num_inputs: firewheel::channel_config::ChannelCount::new(2 * self.n).unwrap(),
                num_outputs: firewheel::channel_config::ChannelCount::new(self.n).unwrap(),
            })
    }

    fn construct_processor(
        &self,
        _config: &Self::Configuration,
        _cx: firewheel::node::ConstructProcessorContext,
    ) -> impl firewheel::node::AudioNodeProcessor {
        MergerProcessor { discrete: ArcGc::clone(&self.discrete) }
    }
}

struct MergerProcessor {
    discrete: ArcGc<AtomicBool>,
}

impl firewheel::node::AudioNodeProcessor for MergerProcessor {
    fn process(
        &mut self,
        info: &firewheel::node::ProcInfo,
        buffers: firewheel::node::ProcBuffers,
        _events: &mut firewheel::event::ProcEvents,
        _extra: &mut firewheel::node::ProcExtra,
    ) -> firewheel::node::ProcessStatus {
        let discrete = self.discrete.load(Ordering::Relaxed);
        let mask = merge_block(
            buffers.inputs,
            buffers.outputs,
            info.frames,
            |port| info.in_silence_mask.is_channel_silent(port),
            discrete,
        );
        firewheel::node::ProcessStatus::outputs_modified_with_silence_mask(mask)
    }
}

/// The merger's mix: output channel j is input j, down-mixed to mono from its
/// port pair (2j, 2j+1) — 0.5·(L+R) for "speakers", L for "discrete" (spec
/// "Channel up-mixing and down-mixing", stereo → mono). A port flagged silent
/// reads as zeros. Returns the output silence mask.
fn merge_block(
    inputs: &[&[f32]],
    outputs: &mut [&mut [f32]],
    frames: usize,
    silent: impl Fn(usize) -> bool,
    discrete: bool,
) -> firewheel::mask::SilenceMask {
    let mut mask = firewheel::mask::SilenceMask::NONE_SILENT;
    for (j, out) in outputs.iter_mut().enumerate() {
        let out = &mut out[..frames];
        let (l, r) = (2 * j, 2 * j + 1);
        let l_silent = silent(l);
        let r_silent = discrete || silent(r);
        match (l_silent, r_silent) {
            (true, true) => {
                out.fill(0.0);
                mask.set_channel(j, true);
            }
            (false, true) if discrete => out.copy_from_slice(&inputs[l][..frames]),
            (false, true) => {
                for (o, &a) in out.iter_mut().zip(&inputs[l][..frames]) {
                    *o = 0.5 * a;
                }
            }
            (true, false) => {
                for (o, &b) in out.iter_mut().zip(&inputs[r][..frames]) {
                    *o = 0.5 * b;
                }
            }
            (false, false) => {
                for ((o, &a), &b) in out.iter_mut().zip(&inputs[l][..frames]).zip(&inputs[r][..frames]) {
                    *o = 0.5 * (a + b);
                }
            }
        }
    }
    mask
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
    /// Precompiled native DSP, with a bounded audio-thread -> VM mailbox.
    Native { kind: NativeKind, shared: ArcGc<NativeShared>, discrete: bool },
    /// RedStreamSourceNode: the graph end of a streaming-PCM push (webapi/pcm.rs).
    /// The producer end lives in `pcm`'s own registry, keyed by the node handle.
    PcmStream(PcmStreamConfig),
    /// ChannelSplitterNode with `n` outputs (see [`SplitterNode`]).
    Splitter(u32),
    /// ChannelMergerNode with `n` inputs (see [`MergerNode`]); `discrete`
    /// is the processor's copy of `channelInterpretation`.
    Merger { n: u32, discrete: ArcGc<AtomicBool> },
    /// The media-element source's silent pass-through
    /// (`ctx_create_media_element_source`): no inputs, audio passes untouched.
    Passthrough,
    /// MediaStreamAudioDestinationNode: a sink feeding its track (see [`TapNode`]).
    MediaDest(Arc<super::mediastream::Source>),
}

/// One Web Audio connection, kept so `disconnect(…)` can find it again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Conn {
    dst: u64,
    output: u32,
    input: u32,
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
    /// Port counts of the Firewheel node, mirrored here so `connect` can
    /// build a port list Firewheel will accept without reaching into the graph.
    /// Everything is stereo except `RedStreamSourceNode` and the splitter /
    /// merger.
    out_ch: u32,
    in_ch: u32,
    /// Outgoing connections, for `disconnect(…)`.
    outs: Vec<Conn>,
    /// PannerNode world position.
    pos: [f32; 3],
    /// PannerNode orientation (cone direction; Web Audio default +x).
    orient: [f32; 3],
    /// PannerNode cone: [innerAngle°, outerAngle°, outerGain].
    cone: [f32; 3],
    /// PannerNode distance attenuation: model + [refDistance, maxDistance, rolloffFactor].
    dist_model: DistModel,
    dist: [f32; 3],
    /// GainNode `gain` as the page set it. Its VolumeNode carries the
    /// magnitude through [`gain_volume`].
    gain: f32,
    // -- sampler bookkeeping --
    buffer: Option<Arc<PlanarBuffer>>,
    /// `loop` / `loopStart` / `loopEnd`, shared with the playing [`SourceSample`].
    src: Arc<SourceCtl>,
    started: bool,
    saw_playing: bool,
    ended: bool,
    /// The JS node object, protected while playing (spec keeps it alive).
    js_obj: u64,
    /// Weak wrapper reference: its finalizer is drained before notification collection.
    /// The wrapper/port own each other through GC slots; no permanent JS root.
    worklet_obj: u64,
    pending_start: Option<(f64, f64, Option<f64>)>, // (when, offset, duration)
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
            Params::Splitter(n) => (*n, *n),
            Params::Merger { n, .. } => (2 * n, *n),
            // A sink: its track is its output, not a port.
            Params::MediaDest(_) => (2, 0),
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
            gain: 1.0,
            buffer: None,
            src: Arc::new(SourceCtl::default()),
            started: false,
            saw_playing: false,
            ended: false,
            js_obj: 0,
            worklet_obj: 0,
            pending_start: None,
            pending_stop: None,
        }
    }

    /// Web Audio `numberOfInputs`.
    fn web_inputs(&self) -> u32 {
        match &self.params {
            Params::Merger { n, .. } => *n,
            // The sources: AudioScheduledSourceNode, the stream source and the
            // media-element source (MediaElementAudioSourceNode) take none.
            Params::Osc(_) | Params::Sampler(_) | Params::PcmStream(_) | Params::Passthrough => 0,
            _ => 1,
        }
    }

    /// Web Audio `numberOfOutputs` (the destination's is 1, as in the spec;
    /// a MediaStreamAudioDestinationNode's is 0).
    fn web_outputs(&self) -> u32 {
        match &self.params {
            Params::Splitter(n) => *n,
            Params::MediaDest(_) => 0,
            _ => 1,
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
    /// The system default output's generation this engine last followed
    /// (`mediadevices::default_generation`).
    default_seen: u64,
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

/// An engine on the default sink follows the system default output when it
/// moves (webapi/mediadevices.rs, "Following the system default"), as
/// Chrome's `AudioContext` does: its stream reopens on the new default and
/// the graph and the clock carry over (`restart_stream`). One with a chosen
/// sink stays on it. An engine whose stream could not open (no device at
/// all) retries on each move. Returns whether it reopened.
fn follow_default_output(eng: &mut Engine, generation: u64) -> bool {
    if eng.default_seen == generation {
        return false;
    }
    eng.default_seen = generation;
    if eng.state == ST_CLOSED || !(eng.sink_id.is_empty() || eng.sink_id == mediadevices::DEFAULT_SINK) {
        return false;
    }
    match restart_stream(eng, None) {
        Ok(()) => {
            eprintln!("[audio] the system default output moved: the context follows it ({} Hz)", eng.sample_rate);
            true
        }
        Err(e) => {
            eprintln!("[audio] the system default output moved, and it could not be opened: {e}");
            false
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
    worklet: Class,
    worklet_port: Class,
    worklet_port_cls: crate::capi::JSClassRef,
    splitter: Class,
    merger: Class,
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

/// Whether the page in `g` has audio work that advances on pump ticks: a
/// running context with a param timeline, a scheduled start or stop, or a
/// source still playing. Scheduling lands on the first tick at or after the
/// requested time, so an embedder that slows its pump while idle keeps the
/// display rate while this holds.
pub fn busy(g: *mut c_void) -> bool {
    let gl = glob().lock().unwrap();
    let running: Vec<u64> = gl
        .engines
        .iter()
        .filter(|(_, e)| e.g == g as usize && e.state != ST_CLOSED && e.running)
        .map(|(&h, _)| h)
        .collect();
    if running.is_empty() {
        return false;
    }
    let owned = |node: u64, kind: u8| {
        if kind >= P_L_POS_X {
            running.contains(&node)
        } else {
            gl.nodes.get(&node).is_some_and(|r| running.contains(&r.engine))
        }
    };
    gl.timelines.keys().any(|&(node, kind)| owned(node, kind))
        || gl.nodes.values().any(|r| {
            running.contains(&r.engine)
                && (r.pending_start.is_some() || r.pending_stop.is_some() || (r.started && !r.ended))
        })
}

/// Close every `AudioContext` the page in `g` has open, as unloading a
/// document does: for an embedder that replaces the page without replacing
/// the context (`location.reload()` in one realm). Each one's output stream
/// stops and its `state` reads `"closed"`, exactly as after `close()`. A
/// looping source of the old page — a ringtone — stops with it. Returns how
/// many were open.
pub fn close_all(g: *mut c_void) -> usize {
    let mut gl = glob().lock().unwrap();
    let mut n = 0;
    for eng in gl.engines.values_mut().filter(|e| e.g == g as usize && e.state != ST_CLOSED) {
        if eng.fw.is_audio_stream_running() {
            eng.fw.stop_stream();
        }
        eng.running = false;
        eng.state = ST_CLOSED;
        n += 1;
    }
    n
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
/// GainNode `gain`, an amplitude as in the spec, as firewheel's `Volume`.
/// `Volume::Linear` is a slider position that firewheel squares into an
/// amplitude, so it is given the square root. Before this, every gain other
/// than 0 and 1 played squared: 0.5 at 0.25 (−12 dB for −6), the Argon
/// dialpad's 0.075 at 0.0056 (−45 dB for −22.5). A negative gain plays at its
/// magnitude, since a VolumeNode cannot invert the phase (advisory, once).
fn gain_volume(v: f32) -> Volume {
    if v < 0.0 {
        static ONCE: OnceLock<()> = OnceLock::new();
        ONCE.get_or_init(|| {
            eprintln!(
                "[audio] advisory: a GainNode was given a negative gain. It plays at the gain's \
                 magnitude here, without the phase inversion a browser applies."
            );
        });
    }
    Volume::Linear(v.abs().sqrt())
}

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
                rec.gain = v as f32;
                p.volume = gain_volume(rec.gain);
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
            (Params::Native { shared, .. }, P_NATIVE_ENABLED) => shared.set_enabled(v.clamp(0.0, 1.0) as f32),
            (Params::Native { shared, .. }, P_NATIVE_THRESHOLD) => shared.set_threshold(v.clamp(-100.0, 0.0) as f32),
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
fn do_start(eng: &mut Engine, rec: &mut NodeRec, offset: f64, duration: Option<f64>) {
    if let Params::Osc(sh) = &rec.params {
        // `offset` is meaningless for an oscillator (the spec's `start(when)`
        // takes no offset), so it is ignored rather than misinterpreted.
        alog!("osc start node={:?} freq={}", rec.id, sh.frequency());
        sh.start();
        return;
    }
    let Params::Sampler(s) = &mut rec.params else { return };
    alog!(
        "do_start node={:?} buffer_frames={:?} loop={} speed={} offset={offset} duration={duration:?}",
        rec.id,
        rec.buffer.as_ref().map(|b| b.len_frames()),
        rec.src.looping.load(Ordering::Relaxed),
        s.speed
    );
    // Param changes go through Diff (NOT the sync_*_event helpers): in
    // firewheel 0.10 `sync_play_event()` emits `ParamData::Bool`, but the
    // derived Patch for `play: Notify<bool>` only accepts
    // `ParamData::any(Notify<bool>)` — the event fails to downcast and is
    // silently dropped, so the sampler never starts. Diff emits the correct
    // payloads for every field (Notify's DerefMut bumps its counter).
    let baseline = (**s).clone();
    arm_source(s, rec.buffer.as_ref(), &rec.src, offset, duration);
    unsafe { queue_diff(&mut eng.fw, rec.id, &baseline, &**s) };
}

/// Point a sampler at `start(when, offset, duration)`'s walk of `buffer`.
fn arm_source(
    s: &mut SamplerNode,
    buffer: Option<&Arc<PlanarBuffer>>,
    ctl: &Arc<SourceCtl>,
    offset: f64,
    duration: Option<f64>,
) {
    if let Some(buf) = buffer {
        // `offset` is clamped to the buffer; `duration` counts content
        // frames, loop iterations included (the spec's wording), which is
        // exactly what the sampler counts.
        let rate = buf.sample_rate.max(1) as f64;
        let start = secs_to_frames(offset, rate).min(buf.len_frames());
        let len = duration.map(|d| secs_to_frames(d, rate)).unwrap_or(SourceSample::UNBOUNDED);
        let looping = ctl.region(buf.len_frames(), rate).is_some();
        if len == 0 || (!looping && start >= buf.len_frames()) {
            // Nothing to play: the sampler would finish without ever being
            // seen playing, so the end is raised here.
            ctl.done.store(true, Ordering::Relaxed);
        }
        let view = SourceSample::new(buf.clone(), ctl.clone(), start, len);
        s.set_sample(ArcGc::new_unsized(|| Arc::new(view) as Arc<dyn SampleResource>));
    }
    // The view does the looping; the sampler plays it once from its start.
    s.repeat_mode = RepeatMode::PlayOnce;
    s.start_from(PlayFrom::BEGINNING);
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

/// The JSClass a node with prototype `proto` is made with: a child of
/// `node_cls` named after the node's interface (the tag its prototype chain
/// carries: `GainNode`, or mediastream.js's `MediaStreamAudioSourceNode`).
///
/// JSC answers an API object's `Symbol.toStringTag` with its JSClass's name
/// BEFORE it looks at the prototype chain (JSCallbackObject's
/// `getOwnPropertySlot`), so with the one `AudioNode` class every node said
/// `[object AudioNode]` where a browser says `[object GainNode]`. A child
/// class still passes the `node_cls` brand check (JSC follows `parentClass`)
/// and has no finalizer of its own: JSC runs every class's in the chain, and
/// the parent's frees the handle.
unsafe fn node_class_for(g: *mut c_void, proto: u64) -> crate::capi::JSClassRef {
    let base = classes(g).node_cls;
    if !is_cell(proto) {
        return base;
    }
    let a = glue::rjs_atom_wellknown(g, 1); // Symbol.toStringTag
    if a.is_null() {
        return base;
    }
    let tag = read_string(g, get_a(g, proto, a));
    glue::rjs_atom_free(g, a);
    let Some(name) = tag.filter(|t| !t.is_empty() && t != "AudioNode") else {
        return base;
    };
    static SUBCLASSES: OnceLock<Mutex<HashMap<(usize, String), usize>>> = OnceLock::new();
    let mut map = SUBCLASSES.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
    let key = (base as usize, name);
    if let Some(&cls) = map.get(&key) {
        return cls as crate::capi::JSClassRef;
    }
    let Ok(cname) = std::ffi::CString::new(key.1.as_str()) else {
        return base;
    };
    let mut def = crate::capi::JSClassDefinition::empty();
    def.class_name = cname.as_ptr();
    def.parent_class = base;
    let cls = crate::capi::JSClassCreate(&def);
    map.insert(key, cls as usize);
    cls
}

/// Creates the JS wrapper for a node: host object + private slot bag with
/// its eagerly created AudioParam objects (stable identity, GC-tied).
unsafe fn wrap_node(g: *mut c_void, proto: u64, handle: u64, params: &[(&str, u8, f64)]) -> u64 {
    let c = classes(g);
    let obj = host_create(g, node_class_for(g, proto), proto, NodeHandle(handle));
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

/// Where a node's wrapper keeps the wrappers its outputs are connected to.
const OUTS_SLOT: &str = "~outs";

/// The spec's connection reference: a node that a live node is connected to
/// is itself live, with or without a script reference. The Argon dialpad keeps
/// only its oscillators. They are live while they play, but the panners, gain
/// and filter after them were left to the collector. A collection mid-tone
/// took them out of the graph and cut the tone short. Under fast presses that
/// sounded like stutter, worse the busier the page was.
///
/// So `src`'s wrapper holds `dst`'s, in an array on its private slots, until
/// the last connection between them is gone ([`release_downstream`]). A
/// source's own playing reference (`redjs_protect` while it plays) then holds
/// the whole chain after it. A chain whose source has ended and that nothing
/// else holds is collected as one.
unsafe fn hold_downstream(g: *mut c_void, src_obj: u64, dst_obj: u64) {
    let slots = slots_of(g, src_obj);
    if !is_cell(slots) {
        return;
    }
    let mut held = get(g, slots, OUTS_SLOT);
    if !is_cell(held) {
        held = new_array(g);
        define(g, slots, OUTS_SLOT, held, 0);
    }
    let n = array_len(g, held);
    let mut free = None;
    for i in 0..n {
        let v = get_i(g, held, i);
        if v == dst_obj {
            return;
        }
        if free.is_none() && v == UNDEF {
            free = Some(i);
        }
    }
    set_i(g, held, free.unwrap_or(n), dst_obj);
}

/// Let go of every wrapper `src` holds that it is no longer connected to.
/// A released slot is set to `undefined` and reused by the next connect, so
/// the array never grows past the most destinations held at once.
unsafe fn release_downstream(g: *mut c_void, src_obj: u64, src: u64) {
    let slots = slots_of(g, src_obj);
    if !is_cell(slots) {
        return;
    }
    let held = get(g, slots, OUTS_SLOT);
    if !is_cell(held) {
        return;
    }
    let still: Vec<u64> = {
        let gl = glob().lock().unwrap();
        gl.nodes.get(&src).map(|r| r.outs.iter().map(|k| k.dst).collect()).unwrap_or_default()
    };
    let cls = classes(g).node_cls;
    for i in 0..array_len(g, held) {
        let v = get_i(g, held, i);
        if v == UNDEF {
            continue;
        }
        let kept = host_data::<NodeHandle>(g, cls, v).is_some_and(|h| still.contains(&h.0));
        if !kept {
            set_i(g, held, i, UNDEF);
        }
    }
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
        Params::Native { kind, shared, .. } => eng.fw.add_node(NativeNode { kind: *kind, shared: ArcGc::clone(shared) }, None),
        Params::PcmStream(cfg) => {
            let ch = firewheel::channel_config::NonZeroChannelCount::new(cfg.channels as u32)
                .unwrap_or(firewheel::channel_config::NonZeroChannelCount::STEREO);
            eng.fw.add_node(
                StreamWriterNode,
                Some(StreamWriterConfig { channels: ch, check_for_silence: true }),
            )
        }
        Params::Splitter(n) => eng.fw.add_node(SplitterNode { n: *n }, None),
        Params::Merger { n, discrete } => {
            eng.fw.add_node(MergerNode { n: *n, discrete: ArcGc::clone(discrete) }, None)
        }
        Params::Passthrough => eng.fw.add_node(VolumeNode::default(), None),
        Params::MediaDest(source) => eng.fw.add_node(TapNode { source: Arc::clone(source) }, None),
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

/// The channels Web Audio output `output` of `src` carries, as Firewheel
/// output ports: one port per splitter output, every port otherwise.
fn out_channels(src: &NodeRec, output: u32) -> Vec<u32> {
    match src.params {
        Params::Splitter(_) => vec![output],
        _ => (0..src.out_ch.max(1)).collect(),
    }
}

/// The Firewheel port pairs one Web Audio connection — output `output` of
/// `src` into input `input` of `dst` — occupies. `connect` and `disconnect`
/// both go through here, so the edges they name always match.
///
/// Firewheel's `connect` refuses the WHOLE call if any port index is out of
/// range, so a fixed `[(0,0),(1,1)]` would make a mono source connect to
/// nothing at all — silently, since the result is only logged. Deriving the
/// pairs also gets the spec's mixing right:
/// - into a stereo input ("speakers"): a mono output feeds BOTH ports (the
///   centre up-mix), a wider one gives ports 0 and 1 (see the module header
///   for what that drops);
/// - into a merger input j: the same rule onto its pair (2j, 2j+1), which the
///   merger's processor then down-mixes to mono ([`merge_block`]);
/// - into a splitter ("discrete"): channel k into port k, as far as both go.
fn conn_ports(src: &NodeRec, output: u32, dst: &NodeRec, input: u32) -> Vec<(u32, u32)> {
    let from = out_channels(src, output);
    let pair = |l: u32, r: u32| -> Vec<(u32, u32)> {
        match from.as_slice() {
            [mono] => vec![(*mono, l), (*mono, r)],
            [a, b, ..] => vec![(*a, l), (*b, r)],
            [] => Vec::new(),
        }
    };
    match &dst.params {
        Params::Merger { .. } => pair(2 * input, 2 * input + 1),
        Params::Splitter(n) => from.iter().copied().zip(0..*n).collect(),
        Params::Native { discrete: true, .. } => from.iter().copied().zip(0..2).collect(),
        _ => match dst.in_ch {
            0 => Vec::new(),
            1 => vec![(from[0], 0)],
            _ => pair(0, 1),
        },
    }
}

/// WebIDL `unsigned long` (no `[EnforceRange]`): ToNumber, rethrowing, then
/// ToUint32 — NaN and ±Infinity are 0, the rest wraps modulo 2^32. `undefined`
/// is `default` (an omitted optional argument).
unsafe fn idl_u32(g: *mut c_void, v: u64, default: u32) -> Result<u32, ()> {
    if v == UNDEF {
        return Ok(default);
    }
    let d = to_number(g, v)?;
    if !d.is_finite() {
        return Ok(0);
    }
    Ok(d.trunc().rem_euclid(4_294_967_296.0) as u32)
}

/// A `connect` or `disconnect` refusal, made under the graph lock and thrown
/// after it is released.
enum Refusal {
    Type(String),
    Dom(&'static str, String),
}

unsafe fn throw_refusal(g: *mut c_void, r: Refusal) -> u64 {
    match r {
        Refusal::Type(m) => throw_type_error(g, &m),
        Refusal::Dom(name, m) => super::domexception::throw(g, name, &m),
    }
}

/// Said once: the graph mixes in stereo, so a wider output loses channels 2+.
fn advise_wide_output(channels: usize) {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        eprintln!(
            "[audio] advisory: a {channels}-channel output was connected to a stereo input. This \
             graph mixes in stereo, so channels 0 and 1 are kept and the rest are dropped; a \
             browser would carry every channel or down-mix them (5.1 → stereo). A \
             ChannelSplitterNode takes every channel."
        );
    });
}

/// `connect(destinationNode, output = 0, input = 0)` — the AudioNode overload
/// (an AudioParam destination is still a TypeError here).
///
/// The spec's checks, in Chromium's order: another context's node is an
/// `InvalidAccessError`, an output index past `numberOfOutputs` or an input
/// index past the destination's `numberOfInputs` an `IndexSizeError`. A
/// connection whose termini already exist is ignored, as the spec says.
#[crate::host_fn]
unsafe fn node_connect(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let Some(src) = this_node_handle(g, cf) else {
        return throw_type_error(g, "AudioNode.connect called on wrong this");
    };
    let dst_v = cf_arg(cf, 0);
    let Some(dst) = host_data::<NodeHandle>(g, c.node_cls, dst_v).map(|h| h.0) else {
        return throw_type_error(g, "AudioNode.connect: destination is not an AudioNode");
    };
    let Ok(output) = idl_u32(g, cf_arg(cf, 1), 0) else { return UNDEF };
    let Ok(input) = idl_u32(g, cf_arg(cf, 2), 0) else { return UNDEF };

    let step: Result<Option<usize>, Refusal> = 'step: {
        let mut guard = glob().lock().unwrap();
        let gl = &mut *guard;
        let (Some(sr), Some(dr)) = (gl.nodes.get(&src), gl.nodes.get(&dst)) else {
            break 'step Err(Refusal::Type("AudioNode.connect: node is gone".into()));
        };
        let refusal = if sr.engine != dr.engine {
            Some(Refusal::Dom(
                "InvalidAccessError",
                "Failed to execute 'connect' on 'AudioNode': cannot connect to an AudioNode belonging \
                 to a different audio context."
                    .into(),
            ))
        } else if output >= sr.web_outputs() {
            Some(Refusal::Dom(
                "IndexSizeError",
                format!(
                    "Failed to execute 'connect' on 'AudioNode': output index ({output}) exceeds \
                     number of outputs ({}).",
                    sr.web_outputs()
                ),
            ))
        } else if input >= dr.web_inputs() {
            Some(Refusal::Dom(
                "IndexSizeError",
                format!(
                    "Failed to execute 'connect' on 'AudioNode': input index ({input}) exceeds \
                     number of inputs ({}).",
                    dr.web_inputs()
                ),
            ))
        } else {
            None
        };
        if let Some(r) = refusal {
            break 'step Err(r);
        }
        let conn = Conn { dst, output, input };
        if sr.outs.contains(&conn) {
            break 'step Ok(None);
        }
        let ports = conn_ports(sr, output, dr, input);
        let carried = out_channels(sr, output).len();
        let wide = (carried > 2 && !matches!(dr.params, Params::Splitter(_))).then_some(carried);
        let (sid, did, engine) = (sr.id, dr.id, sr.engine);
        if let Some(eng) = gl.engines.get_mut(&engine) {
            if let Err(e) = eng.fw.connect(sid, did, &ports, true) {
                // Loud, because the symptom of a dropped edge is silence with a
                // graph that looks correctly wired from JS.
                eprintln!("[audio] connect failed ({e:?}) — {sid:?} → {did:?} on ports {ports:?}");
            }
        }
        gl.nodes.get_mut(&src).unwrap().outs.push(conn);
        Ok(wide)
    };
    match step {
        Err(r) => throw_refusal(g, r),
        Ok(wide) => {
            if let Some(channels) = wide {
                advise_wide_output(channels);
            }
            hold_downstream(g, cf_this(cf), dst_v);
            dst_v
        }
    }
}

/// `disconnect()`, `disconnect(output)`, `disconnect(destinationNode)`,
/// `disconnect(destinationNode, output)` and
/// `disconnect(destinationNode, output, input)`, picked the way WebIDL's
/// overload resolution picks them: an AudioNode first argument is a
/// destination; anything else alone is an output index.
///
/// Refusals as in the spec: an index past `numberOfOutputs` / the
/// destination's `numberOfInputs` is an `IndexSizeError`, and naming a
/// connection that does not exist is an `InvalidAccessError`.
/// `disconnect(audioParam)` is always the latter: `connect` refuses a param, so
/// nothing can be connected to one.
#[crate::host_fn]
unsafe fn node_disconnect(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let Some(src) = this_node_handle(g, cf) else {
        return throw_type_error(g, "AudioNode.disconnect called on wrong this");
    };
    let argc = cf_argc(cf);
    let a0 = cf_arg(cf, 0);
    let dest = host_data::<NodeHandle>(g, c.node_cls, a0).map(|h| h.0);
    let is_param = dest.is_none() && host_data::<ParamRef>(g, c.param_cls, a0).is_some();

    enum Sel {
        All,
        Output(u32),
        To { dst: u64, output: Option<u32>, input: Option<u32> },
        Param,
    }
    let sel = if argc == 0 {
        Sel::All
    } else if let Some(dst) = dest {
        let output = if argc >= 2 {
            let Ok(o) = idl_u32(g, cf_arg(cf, 1), 0) else { return UNDEF };
            Some(o)
        } else {
            None
        };
        let input = if argc >= 3 {
            let Ok(i) = idl_u32(g, cf_arg(cf, 2), 0) else { return UNDEF };
            Some(i)
        } else {
            None
        };
        Sel::To { dst, output, input }
    } else if is_param {
        Sel::Param
    } else if argc >= 2 {
        return throw_type_error(
            g,
            "Failed to execute 'disconnect' on 'AudioNode': parameter 1 is not of type 'AudioNode'.",
        );
    } else {
        let Ok(o) = idl_u32(g, a0, 0) else { return UNDEF };
        Sel::Output(o)
    };

    let refusal = {
        let mut gl = glob().lock().unwrap();
        let gl = &mut *gl;
        let Some(sr) = gl.nodes.get(&src) else { return UNDEF };
        let outputs = sr.web_outputs();
        let past_outputs = |o: u32| {
            Refusal::Dom(
                "IndexSizeError",
                format!(
                    "Failed to execute 'disconnect' on 'AudioNode': output index ({o}) exceeds \
                     number of outputs ({outputs})."
                ),
            )
        };
        let picked: Result<Vec<Conn>, Refusal> = match sel {
            Sel::All => Ok(sr.outs.clone()),
            Sel::Output(o) if o >= outputs => Err(past_outputs(o)),
            Sel::Output(o) => Ok(sr.outs.iter().copied().filter(|k| k.output == o).collect()),
            Sel::To { output: Some(o), .. } if o >= outputs => Err(past_outputs(o)),
            Sel::To { dst, output, input } => {
                let inputs = gl.nodes.get(&dst).map(|d| d.web_inputs()).unwrap_or(0);
                match input {
                    Some(i) if i >= inputs => Err(Refusal::Dom(
                        "IndexSizeError",
                        format!(
                            "Failed to execute 'disconnect' on 'AudioNode': input index ({i}) \
                             exceeds number of inputs ({inputs})."
                        ),
                    )),
                    _ => {
                        let hit: Vec<Conn> = sr
                            .outs
                            .iter()
                            .copied()
                            .filter(|k| {
                                k.dst == dst
                                    && output.is_none_or(|o| k.output == o)
                                    && input.is_none_or(|i| k.input == i)
                            })
                            .collect();
                        if hit.is_empty() {
                            let what = match (output, input) {
                                (None, _) => "the given destination is not connected.".to_string(),
                                (Some(o), None) => format!("output ({o}) is not connected to the given destination."),
                                (Some(o), Some(i)) => {
                                    format!("output ({o}) is not connected to input ({i}) of the destination.")
                                }
                            };
                            Err(Refusal::Dom(
                                "InvalidAccessError",
                                format!("Failed to execute 'disconnect' on 'AudioNode': {what}"),
                            ))
                        } else {
                            Ok(hit)
                        }
                    }
                }
            }
            Sel::Param => Err(Refusal::Dom(
                "InvalidAccessError",
                "Failed to execute 'disconnect' on 'AudioNode': the given AudioParam is not connected."
                    .into(),
            )),
        };
        match picked {
            Err(r) => Some(r),
            Ok(gone) => {
                let (sid, engine) = (sr.id, sr.engine);
                let pairs: Vec<(NodeID, Vec<(u32, u32)>)> = gone
                    .iter()
                    .filter_map(|k| gl.nodes.get(&k.dst).map(|dr| (dr.id, conn_ports(sr, k.output, dr, k.input))))
                    .collect();
                if let Some(eng) = gl.engines.get_mut(&engine) {
                    for (did, ports) in pairs {
                        // The pairs `connect` used, so the edges match.
                        let _ = eng.fw.disconnect(sid, did, &ports);
                    }
                }
                if let Some(sr) = gl.nodes.get_mut(&src) {
                    sr.outs.retain(|k| !gone.contains(k));
                }
                None
            }
        }
    };
    match refusal {
        Some(r) => throw_refusal(g, r),
        None => {
            release_downstream(g, cf_this(cf), src);
            UNDEF
        }
    }
}

#[crate::host_fn]
unsafe fn node_get_number_of_inputs(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(h) = this_node_handle(g, cf) else {
        return throw_type_error(g, "AudioNode.numberOfInputs on wrong this");
    };
    let gl = glob().lock().unwrap();
    enc_i32(gl.nodes.get(&h).map(|r| r.web_inputs()).unwrap_or(0) as i32)
}

#[crate::host_fn]
unsafe fn node_get_number_of_outputs(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(h) = this_node_handle(g, cf) else {
        return throw_type_error(g, "AudioNode.numberOfOutputs on wrong this");
    };
    let gl = glob().lock().unwrap();
    enc_i32(gl.nodes.get(&h).map(|r| r.web_outputs()).unwrap_or(0) as i32)
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
        (Params::Gain(_), P_GAIN) => rec.gain as f64,
        (Params::Pan(p), P_PAN) => p.pan as f64,
        (Params::Svf(p), P_FREQ) => p.cutoff_hz as f64,
        (Params::Svf(p), P_Q) => p.q_factor as f64,
        (Params::Sampler(s), P_RATE) => s.speed,
        (Params::Native { shared, .. }, P_NATIVE_ENABLED) => shared.enabled() as f64,
        (Params::Native { shared, .. }, P_NATIVE_THRESHOLD) => shared.threshold() as f64,
        (Params::Osc(sh), P_FREQ) => sh.frequency() as f64,
        (Params::Osc(sh), P_DETUNE) => sh.detune() as f64,
        (Params::Spatial(_), P_POS_X..=P_POS_Z) => rec.pos[(kind - P_POS_X) as usize] as f64,
        (Params::Spatial(_), P_ORIENT_X..=P_ORIENT_Z) => rec.orient[(kind - P_ORIENT_X) as usize] as f64,
        _ => fallback,
    }
}

#[crate::host_fn]
unsafe fn param_get_value(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_param(g, cf) {
        Some(p) => {
            let gl = glob().lock().unwrap();
            enc_f64(param_current(&gl, p.node, p.kind, p.last))
        }
        None => throw_type_error(g, "AudioParam.value on wrong this"),
    }
}

#[crate::host_fn]
unsafe fn param_set_value(g: *mut c_void, cf: *mut c_void) -> u64 {
    let native = this_param(g, cf).is_some_and(|p| matches!(p.kind, P_NATIVE_ENABLED | P_NATIVE_THRESHOLD));
    let v = if native {
        let Ok(v) = to_number(g, cf_arg(cf, 0)) else { return UNDEF; };
        if !v.is_finite() { return throw_type_error(g, "Native audio parameters must be finite"); }
        v
    } else { dec_f64(cf_arg(cf, 0)).unwrap_or(0.0) };
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
    let native = this_param(g, cf).is_some_and(|p| matches!(p.kind, P_NATIVE_ENABLED | P_NATIVE_THRESHOLD));
    let (v, t) = if native {
        let Ok(v) = to_number(g, cf_arg(cf, 0)) else { return UNDEF; };
        let Ok(t) = to_number(g, cf_arg(cf, 1)) else { return UNDEF; };
        if !v.is_finite() || !t.is_finite() { return throw_type_error(g, "Native audio automation values/times must be finite"); }
        (v, t)
    } else { (dec_f64(cf_arg(cf, 0)).unwrap_or(0.0), dec_f64(cf_arg(cf, 1)).unwrap_or(0.0)) };
    let this = cf_this(cf);
    let Some(p) = this_param(g, cf) else {
        return throw_type_error(g, "AudioParam method on wrong this");
    };
    p.last = v;
    let (node, kind) = (p.node, p.kind);
    let mut gl = glob().lock().unwrap();
    let gl = &mut *gl;
    // The context's time: listener params key by engine handle.
    let engine = if kind >= P_L_POS_X { Some(node) } else { gl.nodes.get(&node).map(|r| r.engine) };
    let now = engine.and_then(|e| gl.engines.get(&e)).map(|e| e.now()).unwrap_or(0.0);
    let cur = param_current(gl, node, kind, v);
    let tl = gl.timelines.entry((node, kind)).or_insert_with(|| Timeline { evs: Vec::new(), start_v: cur, start_t: now });
    let ev = mk(v, t);
    // keep sorted by time (stable for equal timestamps)
    let at = tl.evs.partition_point(|e| e.time() <= ev.time());
    tl.evs.insert(at, ev);
    // The value the timeline gives now, applied at once rather than at the
    // next pump. Pages build a node, schedule its envelope and start its
    // source in one task (`gain.setValueAtTime(0, now)`, a ramp up,
    // `osc.start(now)`); applied a pump later, the node would play its first
    // blocks at the parameter's default, which for a GainNode is a full-scale
    // burst. Set now, the value ships with the node in the next graph update
    // and its first block already has it.
    let (value, _) = tl.eval(now);
    set_param(&mut gl.engines, &mut gl.nodes, node, kind, value);
    this
}

#[crate::host_fn]
unsafe fn param_set_value_at(g: *mut c_void, cf: *mut c_void) -> u64 {
    param_schedule(g, cf, |v, t| Tl::Set { t, v })
}

#[crate::host_fn]
unsafe fn param_linear_ramp(g: *mut c_void, cf: *mut c_void) -> u64 {
    param_schedule(g, cf, |v, t| Tl::Lin { t, v })
}

#[crate::host_fn]
unsafe fn param_exp_ramp(g: *mut c_void, cf: *mut c_void) -> u64 {
    param_schedule(g, cf, |v, t| Tl::Exp { t, v })
}

#[crate::host_fn]
unsafe fn param_set_target(g: *mut c_void, cf: *mut c_void) -> u64 {
    let tau = dec_f64(cf_arg(cf, 2)).unwrap_or(0.01);
    param_schedule(g, cf, move |v, t| Tl::Target { t, v, tau })
}

#[crate::host_fn]
unsafe fn param_cancel(g: *mut c_void, cf: *mut c_void) -> u64 {
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

#[crate::host_fn]
unsafe fn buf_get_sample_rate(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_buf(g, cf) {
        Some(b) => enc_f64(b.buf.sample_rate as f64),
        None => throw_type_error(g, "AudioBuffer.sampleRate on wrong this"),
    }
}

#[crate::host_fn]
unsafe fn buf_get_length(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_buf(g, cf) {
        Some(b) => enc_f64(b.buf.data.first().map(|c| c.len()).unwrap_or(0) as f64),
        None => throw_type_error(g, "AudioBuffer.length on wrong this"),
    }
}

#[crate::host_fn]
unsafe fn buf_get_duration(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_buf(g, cf) {
        Some(b) => {
            let frames = b.buf.data.first().map(|c| c.len()).unwrap_or(0) as f64;
            enc_f64(frames / (b.buf.sample_rate.max(1) as f64))
        }
        None => throw_type_error(g, "AudioBuffer.duration on wrong this"),
    }
}

#[crate::host_fn]
unsafe fn buf_get_channels(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_buf(g, cf) {
        Some(b) => enc_i32(b.buf.data.len() as i32),
        None => throw_type_error(g, "AudioBuffer.numberOfChannels on wrong this"),
    }
}

#[crate::host_fn]
unsafe fn buf_get_channel_data(g: *mut c_void, cf: *mut c_void) -> u64 {
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

#[crate::host_fn]
unsafe fn buf_copy_to_channel(g: *mut c_void, cf: *mut c_void) -> u64 {
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

#[crate::host_fn]
unsafe fn src_get_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    let slots = slots_of(g, cf_this(cf));
    get(g, slots, "buffer")
}

#[crate::host_fn]
unsafe fn src_set_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
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

#[crate::host_fn]
unsafe fn src_get_loop(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, "AudioBufferSourceNode.loop on wrong this");
    };
    let gl = glob().lock().unwrap();
    enc_bool(gl.nodes.get(&handle).map(|r| r.src.looping.load(Ordering::Relaxed)).unwrap_or(false))
}

/// `loop` — read by the playing [`SourceSample`] on every block, so setting it
/// during playback loops (or stops looping) from the current playhead.
#[crate::host_fn]
unsafe fn src_set_loop(g: *mut c_void, cf: *mut c_void) -> u64 {
    let v = cf_arg(cf, 0) == 0x7; // true
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, "AudioBufferSourceNode.loop on wrong this");
    };
    let gl = glob().lock().unwrap();
    if let Some(rec) = gl.nodes.get(&handle) {
        rec.src.looping.store(v, Ordering::Relaxed);
    }
    UNDEF
}

/// `loopStart` / `loopEnd` (seconds; both default 0, which loops the whole
/// buffer). Live, like `loop`.
unsafe fn src_loop_point(g: *mut c_void, cf: *mut c_void, end: bool) -> u64 {
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, "AudioBufferSourceNode.loopStart/loopEnd on wrong this");
    };
    let gl = glob().lock().unwrap();
    let ctl = gl.nodes.get(&handle).map(|r| r.src.clone());
    drop(gl);
    let v = ctl.map(|c| if end { c.loop_end() } else { c.loop_start() }).unwrap_or(0.0);
    enc_f64(v)
}

unsafe fn src_set_loop_point(g: *mut c_void, cf: *mut c_void, end: bool) -> u64 {
    let name = if end { "loopEnd" } else { "loopStart" };
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, &format!("AudioBufferSourceNode.{name} on wrong this"));
    };
    // WebIDL `double`: ToNumber, then a non-finite value is a TypeError.
    let Ok(v) = to_number(g, cf_arg(cf, 0)) else { return UNDEF };
    if !v.is_finite() {
        return throw_type_error(
            g,
            &format!("Failed to set the '{name}' property on 'AudioBufferSourceNode': The provided double value is non-finite."),
        );
    }
    let gl = glob().lock().unwrap();
    if let Some(rec) = gl.nodes.get(&handle) {
        let slot = if end { &rec.src.loop_end } else { &rec.src.loop_start };
        slot.store(v.to_bits(), Ordering::Relaxed);
    }
    UNDEF
}

#[crate::host_fn]
unsafe fn src_get_loop_start(g: *mut c_void, cf: *mut c_void) -> u64 {
    src_loop_point(g, cf, false)
}

#[crate::host_fn]
unsafe fn src_set_loop_start(g: *mut c_void, cf: *mut c_void) -> u64 {
    src_set_loop_point(g, cf, false)
}

#[crate::host_fn]
unsafe fn src_get_loop_end(g: *mut c_void, cf: *mut c_void) -> u64 {
    src_loop_point(g, cf, true)
}

#[crate::host_fn]
unsafe fn src_set_loop_end(g: *mut c_void, cf: *mut c_void) -> u64 {
    src_set_loop_point(g, cf, true)
}

#[crate::host_fn]
unsafe fn src_get_onended(g: *mut c_void, cf: *mut c_void) -> u64 {
    let slots = slots_of(g, cf_this(cf));
    get(g, slots, "onended")
}

#[crate::host_fn]
unsafe fn src_set_onended(g: *mut c_void, cf: *mut c_void) -> u64 {
    let slots = slots_of(g, cf_this(cf));
    define(g, slots, "onended", cf_arg(cf, 0), 0);
    UNDEF
}

/// `AudioScheduledSourceNode.start(when[, offset[, duration]])` — shared by
/// `AudioBufferSourceNode` and `OscillatorNode`; `do_start` dispatches on the
/// node's `Params` (`offset` and `duration` are ignored for an oscillator,
/// whose `start(when)` has neither).
#[crate::host_fn]
unsafe fn src_start(g: *mut c_void, cf: *mut c_void) -> u64 {
    let when = dec_f64(cf_arg(cf, 0)).unwrap_or(0.0);
    let offset = dec_f64(cf_arg(cf, 1)).unwrap_or(0.0);
    // Absent = play to the end (or loop until stopped).
    let duration = dec_f64(cf_arg(cf, 2));
    let this = cf_this(cf);
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, "AudioScheduledSourceNode.start on wrong this");
    };
    let mut gl = glob().lock().unwrap();
    let gl = &mut *gl;
    let Some(rec) = gl.nodes.get_mut(&handle) else { return UNDEF };
    // Spec: a negative `when` is a RangeError, and so is a negative `offset`
    // or `duration` on a buffer source (an oscillator's `start` has neither).
    let (iface, checks) = match rec.params {
        Params::Sampler(_) => ("AudioBufferSourceNode", [("when", when), ("offset", offset), ("duration", duration.unwrap_or(0.0))]),
        _ => ("OscillatorNode", [("when", when), ("offset", 0.0), ("duration", 0.0)]),
    };
    if let Some((name, v)) = checks.into_iter().find(|&(_, v)| v < 0.0) {
        return throw_error(
            g,
            2,
            &format!("Failed to execute 'start' on '{iface}': The {name} provided ({v}) is less than the minimum bound (0)."),
        );
    }
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
        do_start(eng, rec, offset, duration);
    } else {
        rec.pending_start = Some((when, offset, duration));
    }
    UNDEF
}

#[crate::host_fn]
unsafe fn src_stop(g: *mut c_void, cf: *mut c_void) -> u64 {
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

#[crate::host_fn]
unsafe fn osc_get_type(g: *mut c_void, cf: *mut c_void) -> u64 {
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

#[crate::host_fn]
unsafe fn osc_set_type(g: *mut c_void, cf: *mut c_void) -> u64 {
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
#[crate::host_fn]
unsafe fn stream_push(g: *mut c_void, cf: *mut c_void) -> u64 {
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
#[crate::host_fn]
unsafe fn stream_close(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Some(h) = this_node_handle(g, cf) {
        pcm::unregister(h);
    }
    UNDEF
}

/// Seconds of audio buffered ahead of the device; `null` when not streaming.
#[crate::host_fn]
unsafe fn stream_get_queued(g: *mut c_void, cf: *mut c_void) -> u64 {
    match this_sink(g, cf).and_then(|s| s.occupied_seconds()) {
        Some(v) => enc_f64(v),
        None => NULL,
    }
}

#[crate::host_fn]
unsafe fn stream_get_available(g: *mut c_void, cf: *mut c_void) -> u64 {
    enc_f64(this_sink(g, cf).map(|s| s.available_frames()).unwrap_or(0) as f64)
}

/// Read-AND-CLEAR, exactly like the native side: reading tells you whether it
/// happened since your last read, so a meter cannot latch on forever.
#[crate::host_fn]
unsafe fn stream_get_underflowed(g: *mut c_void, cf: *mut c_void) -> u64 {
    enc_bool(this_sink(g, cf).map(|s| s.underflowed()).unwrap_or(false))
}

#[crate::host_fn]
unsafe fn stream_get_overflowed(g: *mut c_void, cf: *mut c_void) -> u64 {
    enc_bool(this_sink(g, cf).map(|s| s.overflowed()).unwrap_or(false))
}

/// Is the graph actually pulling frames? False while the context has no output
/// stream, or before the processor's first block.
#[crate::host_fn]
unsafe fn stream_get_ready(g: *mut c_void, cf: *mut c_void) -> u64 {
    enc_bool(this_sink(g, cf).map(|s| s.is_ready()).unwrap_or(false))
}

// ---------------------------------------------------------------------------
// BiquadFilterNode.type / AnalyserNode extras
// ---------------------------------------------------------------------------

#[crate::host_fn]
unsafe fn biquad_get_type(g: *mut c_void, cf: *mut c_void) -> u64 {
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

#[crate::host_fn]
unsafe fn biquad_set_type(g: *mut c_void, cf: *mut c_void) -> u64 {
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
// PannerNode attributes: distance model + cone + panning model
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
        #[crate::host_fn]
        unsafe fn $get(g: *mut c_void, cf: *mut c_void) -> u64 {
            panner_read(g, cf, |rec| rec.$field[$idx] as f64)
        }
        #[crate::host_fn]
        unsafe fn $set(g: *mut c_void, cf: *mut c_void) -> u64 {
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

#[crate::host_fn]
unsafe fn panner_get_distance_model(g: *mut c_void, cf: *mut c_void) -> u64 {
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

#[crate::host_fn]
unsafe fn panner_set_distance_model(g: *mut c_void, cf: *mut c_void) -> u64 {
    let ty = read_string(g, cf_arg(cf, 0)).unwrap_or_default();
    let model = match ty.as_str() {
        "linear" => DistModel::Linear,
        "exponential" => DistModel::Exponential,
        _ => DistModel::Inverse,
    };
    panner_write(g, cf, |rec| rec.dist_model = model)
}

/// Real HRTF: switches the node's panning mode live.
#[crate::host_fn]
unsafe fn panner_get_panning_model(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(handle) = this_node_handle(g, cf) else {
        return throw_type_error(g, "PannerNode.panningModel on wrong this");
    };
    let gl = glob().lock().unwrap();
    let hrtf = matches!(gl.nodes.get(&handle).map(|r| &r.params), Some(Params::Spatial(p)) if p.hrtf);
    js_string(g, if hrtf { "HRTF" } else { "equalpower" })
}

#[crate::host_fn]
unsafe fn panner_set_panning_model(g: *mut c_void, cf: *mut c_void) -> u64 {
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

#[crate::host_fn]
unsafe fn analyser_float_time(g: *mut c_void, cf: *mut c_void) -> u64 {
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

#[crate::host_fn]
unsafe fn analyser_byte_time(g: *mut c_void, cf: *mut c_void) -> u64 {
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

#[crate::host_fn]
unsafe fn analyser_float_freq(g: *mut c_void, cf: *mut c_void) -> u64 {
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

#[crate::host_fn]
unsafe fn analyser_byte_freq(g: *mut c_void, cf: *mut c_void) -> u64 {
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

#[crate::host_fn]
unsafe fn analyser_get_fft(g: *mut c_void, cf: *mut c_void) -> u64 {
    enc_i32(analyser_fft_size(g, cf) as i32)
}

#[crate::host_fn]
unsafe fn analyser_set_fft(g: *mut c_void, cf: *mut c_void) -> u64 {
    let v = dec_f64(cf_arg(cf, 0)).unwrap_or(f64::NAN);
    if valid_fft_size(v).is_none() {
        return throw_type_error(g, "AnalyserNode.fftSize must be a power of two in [32, 4096]");
    }
    let slots = slots_of(g, cf_this(cf));
    define(g, slots, "fftSize", cf_arg(cf, 0), 0);
    UNDEF
}

#[crate::host_fn]
unsafe fn analyser_bin_count(g: *mut c_void, cf: *mut c_void) -> u64 {
    enc_i32((analyser_fft_size(g, cf) / 2) as i32)
}

// smoothingTimeConstant / minDecibels / maxDecibels — plain slot-backed accessors
macro_rules! analyser_slot_accessor {
    ($get:ident, $set:ident, $name:literal, $default:expr) => {
        #[crate::host_fn]
        unsafe fn $get(g: *mut c_void, cf: *mut c_void) -> u64 {
            enc_f64(analyser_slot_f64(g, cf, $name, $default))
        }
        #[crate::host_fn]
        unsafe fn $set(g: *mut c_void, cf: *mut c_void) -> u64 {
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
        #[crate::host_fn]
        unsafe fn $fn_name(g: *mut c_void, cf: *mut c_void) -> u64 {
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

#[crate::host_fn]
unsafe fn ctx_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);

    let mut fw = FirewheelContext::new(firewheel::FirewheelConfig::default());
    let master = fw.add_node(VolumeNode::default(), None);
    let out = fw.graph_out_node_id();
    let _ = fw.connect(master, out, STEREO_PORTS, true);
    if let Some(&sink) = RENDER_REFERENCE.get() {
        // What the speakers play, for the microphone's echo canceller.
        let key = NEXT_REFERENCE_KEY.fetch_add(1, Ordering::Relaxed);
        let tap = fw.add_node(RefTapNode { key, sink }, None);
        let _ = fw.connect(master, tap, STEREO_PORTS, true);
    }
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
                default_seen: mediadevices::default_generation(mediadevices::Flow::Output),
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

#[crate::host_fn]
unsafe fn ctx_get_current_time(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(h) = this_engine_handle(g, cf) else {
        return throw_type_error(g, "AudioContext.currentTime on wrong this");
    };
    let gl = glob().lock().unwrap();
    enc_f64(gl.engines.get(&h).map(|e| e.now()).unwrap_or(0.0))
}

#[crate::host_fn]
unsafe fn ctx_get_sample_rate(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(h) = this_engine_handle(g, cf) else {
        return throw_type_error(g, "AudioContext.sampleRate on wrong this");
    };
    let gl = glob().lock().unwrap();
    enc_f64(gl.engines.get(&h).map(|e| e.sample_rate).unwrap_or(48_000) as f64)
}

#[crate::host_fn]
unsafe fn ctx_get_state(g: *mut c_void, cf: *mut c_void) -> u64 {
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

#[crate::host_fn]
unsafe fn ctx_get_sink_id(g: *mut c_void, cf: *mut c_void) -> u64 {
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
#[crate::host_fn]
unsafe fn ctx_set_sink_id(g: *mut c_void, cf: *mut c_void) -> u64 {
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
                if previous == device {
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

#[crate::host_fn]
unsafe fn ctx_resume(g: *mut c_void, cf: *mut c_void) -> u64 {
    ctx_set_suspended(g, cf, false)
}

#[crate::host_fn]
unsafe fn ctx_suspend(g: *mut c_void, cf: *mut c_void) -> u64 {
    ctx_set_suspended(g, cf, true)
}

#[crate::host_fn]
unsafe fn ctx_close(g: *mut c_void, cf: *mut c_void) -> u64 {
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
    create_node_on(g, h, params, proto, jsparams)
}

/// A node on context `engine`, wrapped — the factories' and the node
/// constructors' common end.
unsafe fn create_node_on(g: *mut c_void, engine: u64, params: Params, proto: u64, jsparams: &[(&str, u8, f64)]) -> u64 {
    let handle = {
        let mut gl = glob().lock().unwrap();
        match alloc_node(&mut gl, engine, params) {
            Some(x) => x,
            None => return throw_error(g, 0, "AudioContext is closed"),
        }
    };
    wrap_node(g, proto, handle, jsparams)
}

#[crate::host_fn]
unsafe fn ctx_create_gain(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    ctx_create_node(g, cf, Params::Gain(VolumeNode::default()), c.gain.proto, &[("gain", P_GAIN, 1.0)])
}

#[crate::host_fn]
unsafe fn ctx_create_stereo_panner(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    ctx_create_node(g, cf, Params::Pan(VolumePanNode::default()), c.stereo.proto, &[("pan", P_PAN, 0.0)])
}

#[crate::host_fn]
unsafe fn ctx_create_panner(g: *mut c_void, cf: *mut c_void) -> u64 {
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

#[crate::host_fn]
unsafe fn ctx_create_biquad(g: *mut c_void, cf: *mut c_void) -> u64 {
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

#[crate::host_fn]
unsafe fn ctx_create_analyser(g: *mut c_void, cf: *mut c_void) -> u64 {
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
#[crate::host_fn]
unsafe fn ctx_create_media_element_source(g: *mut c_void, cf: *mut c_void) -> u64 {
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
#[crate::host_fn]
unsafe fn ctx_create_oscillator(g: *mut c_void, cf: *mut c_void) -> u64 {
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
/// takes a `MediaStream` the engine pulls from (webapi/mediastream.rs has
/// those now, and that node is built on this one). The producers this is for
/// — a WebRTC track, a native decoder — already hold decoded `f32` frames and
/// want to push without a stream object in between. Calling that
/// `createMediaStreamSource` would be a different API wearing a standard name;
/// see webapi/pcm.rs for the contract.
///
/// `options`: `{ sampleRate?, channelCount?, latencySeconds?, capacitySeconds? }`.
#[crate::host_fn]
unsafe fn ctx_create_red_stream_source(g: *mut c_void, cf: *mut c_void) -> u64 {
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

/// The engine behind an `AudioContext` passed as an argument (the
/// MediaStream node constructors take the context first, as in the spec).
unsafe fn engine_of(g: *mut c_void, v: u64) -> Option<u64> {
    host_data::<CtxHandle>(g, classes(g).ctx_cls, v).map(|h| h.0)
}

/// mediastream.js: `sourceNode(context, trackHandle, proto)` → the native
/// node of a `MediaStreamAudioSourceNode`: a streaming-PCM node at the track's
/// rate, at most stereo, fed by the track (`mediastream::pipe`). The pcm
/// channel resamples to this graph's rate, so a 44.1 kHz microphone plays into
/// a 48 kHz context as a browser's does.
#[crate::host_fn]
pub(crate) unsafe fn ms_source_node(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(engine) = engine_of(g, cf_arg(cf, 0)) else {
        return throw_type_error(g, "MediaStreamAudioSourceNode: parameter 1 is not of type 'AudioContext'.");
    };
    let Some(track) = super::mediastream::handle_id(g, cf_arg(cf, 1)) else {
        return throw_type_error(g, "MediaStreamAudioSourceNode: the track is not a MediaStreamTrack.");
    };
    let Some(info) = super::mediastream::info(track) else {
        return super::domexception::throw(g, "InvalidStateError", "MediaStreamAudioSourceNode: the track is gone.");
    };
    // A live device: a short queue keeps the monitor latency near a
    // browser's. A network track arrives in the network's bursts and needs
    // the jitter room the room's own playout has (skathi's livekit.ts).
    let remote = info.kind == super::mediastream::SourceKind::Remote;
    let cfg = PcmStreamConfig {
        sample_rate: info.sample_rate,
        channels: info.channels.min(2),
        latency_seconds: if remote { 0.08 } else { 0.03 },
        capacity_seconds: if remote { 0.4 } else { 0.2 },
    }
    .sanitized();
    let proto = cf_arg(cf, 2);
    let obj = create_node_on(g, engine, Params::PcmStream(cfg), proto, &[]);
    if is_cell(obj) {
        if let Some(h) = host_data::<NodeHandle>(g, classes(g).node_cls, obj).map(|x| x.0) {
            // An ended track pipes nothing: the node is silent, as the spec's
            // source of an ended track is.
            if !super::mediastream::pipe(h, track) {
                alog!("MediaStreamAudioSourceNode {h}: track {track} is not live");
            }
        }
    }
    obj
}

/// Whether node `start` (a node handle) feeds its context's destination along
/// node connections, whatever the gains on the way. For
/// [`super::mediastream::page_plays`]. Called on the context's thread.
pub(crate) fn reaches_destination(start: u64) -> bool {
    let gl = glob().lock().unwrap();
    let mut seen = std::collections::HashSet::new();
    let mut todo = vec![start];
    while let Some(n) = todo.pop() {
        if !seen.insert(n) {
            continue;
        }
        let Some(rec) = gl.nodes.get(&n) else { continue };
        if matches!(rec.params, Params::Dest) {
            return true;
        }
        todo.extend(rec.outs.iter().map(|c| c.dst));
    }
    false
}

/// mediastream.js: `destNode(context, proto, channelCount)` → `[node,
/// trackHandle]`: a `MediaStreamAudioDestinationNode` and the track it feeds.
#[crate::host_fn]
pub(crate) unsafe fn ms_dest_node(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(engine) = engine_of(g, cf_arg(cf, 0)) else {
        return throw_type_error(g, "MediaStreamAudioDestinationNode: parameter 1 is not of type 'AudioContext'.");
    };
    let rate = glob().lock().unwrap().engines.get(&engine).map(|e| e.sample_rate).unwrap_or(48_000);
    let source = super::mediastream::Source::new(
        super::mediastream::SourceKind::Graph,
        "MediaStreamAudioDestinationNode".into(),
        2,
        rate,
        super::mediastream::Settings::default(),
    );
    let obj = create_node_on(g, engine, Params::MediaDest(Arc::clone(&source)), cf_arg(cf, 1), &[]);
    if !is_cell(obj) {
        return obj; // the context is closed: already thrown
    }
    let track = super::mediastream::new_track(&source, super::mediastream::owner_of(g));
    let handle = super::mediastream::new_handle(g, track);
    let out = new_array(g);
    set_i(g, out, 0, obj);
    set_i(g, out, 1, handle);
    out
}

#[crate::host_fn]
unsafe fn ctx_create_buffer_source(g: *mut c_void, cf: *mut c_void) -> u64 {
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

#[crate::host_fn]
unsafe fn ctx_create_buffer(g: *mut c_void, cf: *mut c_void) -> u64 {
    let channels = (dec_f64(cf_arg(cf, 0)).unwrap_or(1.0) as usize).clamp(1, 32);
    let length = dec_f64(cf_arg(cf, 1)).unwrap_or(0.0).max(0.0) as usize;
    let rate = dec_f64(cf_arg(cf, 2)).unwrap_or(48_000.0).max(1.0) as u32;
    if this_engine_handle(g, cf).is_none() {
        return throw_type_error(g, "AudioContext.createBuffer on wrong this");
    }
    wrap_buffer(g, Arc::new(PlanarBuffer { data: vec![vec![0.0; length]; channels], sample_rate: rate }))
}

// ---------------------------------------------------------------------------
// ChannelSplitterNode / ChannelMergerNode: factories, constructors, and the
// three attributes the spec pins on them
// ---------------------------------------------------------------------------
//
// The values (spec §ChannelSplitterNode, §ChannelMergerNode; Chromium's
// handlers are the same):
//
// |                         | splitter                | merger                    |
// |-------------------------|-------------------------|---------------------------|
// | `numberOfInputs`        | 1                       | n                         |
// | `numberOfOutputs`       | n                       | 1                         |
// | `channelCount`          | n, fixed                | 1 (per input), fixed      |
// | `channelCountMode`      | "explicit", fixed       | "explicit", fixed         |
// | `channelInterpretation` | "discrete", fixed       | "speakers", changeable    |
//
// Changing a fixed one to another value is an InvalidStateError; setting it
// to the value it has is allowed. An enum assignment that is not one of the
// enum's strings is ignored, as WebIDL says for every enum attribute.

const COUNT_MODES: &[&str] = &["max", "clamped-max", "explicit"];
const INTERPRETATIONS: &[&str] = &["speakers", "discrete"];

/// The IndexSizeError text for a `numberOfOutputs` / `numberOfInputs` outside
/// 1..=32 (Chromium's wording); `None` when it is in range.
fn count_out_of_range(n: u32, what: &str) -> Option<String> {
    (!(1..=MAX_SPLIT_CHANNELS).contains(&n))
        .then(|| format!("The number of {what} provided ({n}) is outside the range [1, {MAX_SPLIT_CHANNELS}]."))
}

/// What a splitter / merger `this` is, for the pinned attributes.
#[derive(Clone, Copy)]
enum Pinned {
    Splitter(u32),
    Merger { discrete: bool },
}

impl Pinned {
    fn who(self) -> &'static str {
        match self {
            Pinned::Splitter(_) => "ChannelSplitter",
            Pinned::Merger { .. } => "ChannelMerger",
        }
    }

    fn count(self) -> u32 {
        match self {
            Pinned::Splitter(n) => n,
            Pinned::Merger { .. } => 1,
        }
    }

    /// The spec's rule per attribute, shared by the setters and the
    /// constructors' `AudioNodeOptions`. `Err` is the InvalidStateError text.
    fn check_count(self, v: u32) -> Result<(), String> {
        if v == self.count() {
            Ok(())
        } else {
            Err(format!("{}: channelCount cannot be changed from {}", self.who(), self.count()))
        }
    }

    fn check_mode(self, mode: &str) -> Result<(), String> {
        if mode == "explicit" {
            Ok(())
        } else {
            Err(format!("{}: channelCountMode cannot be changed from 'explicit'", self.who()))
        }
    }

    fn check_interpretation(self, interp: &str) -> Result<(), String> {
        match self {
            Pinned::Splitter(_) if interp != "discrete" => {
                Err("ChannelSplitter: channelInterpretation cannot be changed from 'discrete'".into())
            }
            _ => Ok(()),
        }
    }
}

unsafe fn pinned_of(g: *mut c_void, cf: *mut c_void) -> Option<(u64, Pinned)> {
    let h = this_node_handle(g, cf)?;
    let gl = glob().lock().unwrap();
    match gl.nodes.get(&h).map(|r| &r.params) {
        Some(Params::Splitter(n)) => Some((h, Pinned::Splitter(*n))),
        Some(Params::Merger { discrete, .. }) => Some((h, Pinned::Merger { discrete: discrete.load(Ordering::Relaxed) })),
        _ => None,
    }
}

unsafe fn set_refused(g: *mut c_void, attr: &str, msg: &str) -> u64 {
    super::domexception::throw(
        g,
        "InvalidStateError",
        &format!("Failed to set the '{attr}' property on 'AudioNode': {msg}"),
    )
}

/// An enum attribute's new value. `Ok(None)` when it is not one of `allowed`
/// (WebIDL ignores that assignment); `Err` when ToString threw.
unsafe fn enum_value(g: *mut c_void, v: u64, allowed: &[&str]) -> Result<Option<String>, ()> {
    let s = to_js_string(g, v)?;
    let s = read_string(g, s).unwrap_or_default();
    Ok(allowed.contains(&s.as_str()).then_some(s))
}

#[crate::host_fn]
unsafe fn pinned_get_count(g: *mut c_void, cf: *mut c_void) -> u64 {
    match pinned_of(g, cf) {
        Some((_, p)) => enc_i32(p.count() as i32),
        None => throw_type_error(g, "AudioNode.channelCount on wrong this"),
    }
}

#[crate::host_fn]
unsafe fn pinned_set_count(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some((_, p)) = pinned_of(g, cf) else {
        return throw_type_error(g, "AudioNode.channelCount on wrong this");
    };
    let Ok(v) = idl_u32(g, cf_arg(cf, 0), 0) else { return UNDEF };
    match p.check_count(v) {
        Ok(()) => UNDEF,
        Err(m) => set_refused(g, "channelCount", &m),
    }
}

#[crate::host_fn]
unsafe fn pinned_get_mode(g: *mut c_void, cf: *mut c_void) -> u64 {
    match pinned_of(g, cf) {
        Some(_) => js_string(g, "explicit"),
        None => throw_type_error(g, "AudioNode.channelCountMode on wrong this"),
    }
}

#[crate::host_fn]
unsafe fn pinned_set_mode(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some((_, p)) = pinned_of(g, cf) else {
        return throw_type_error(g, "AudioNode.channelCountMode on wrong this");
    };
    match enum_value(g, cf_arg(cf, 0), COUNT_MODES) {
        Err(()) | Ok(None) => UNDEF,
        Ok(Some(mode)) => match p.check_mode(&mode) {
            Ok(()) => UNDEF,
            Err(m) => set_refused(g, "channelCountMode", &m),
        },
    }
}

#[crate::host_fn]
unsafe fn pinned_get_interpretation(g: *mut c_void, cf: *mut c_void) -> u64 {
    match pinned_of(g, cf) {
        Some((_, Pinned::Merger { discrete: false })) => js_string(g, "speakers"),
        Some(_) => js_string(g, "discrete"),
        None => throw_type_error(g, "AudioNode.channelInterpretation on wrong this"),
    }
}

#[crate::host_fn]
unsafe fn pinned_set_interpretation(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some((h, p)) = pinned_of(g, cf) else {
        return throw_type_error(g, "AudioNode.channelInterpretation on wrong this");
    };
    let interp = match enum_value(g, cf_arg(cf, 0), INTERPRETATIONS) {
        Err(()) | Ok(None) => return UNDEF,
        Ok(Some(s)) => s,
    };
    if let Err(m) = p.check_interpretation(&interp) {
        return set_refused(g, "channelInterpretation", &m);
    }
    let gl = glob().lock().unwrap();
    if let Some(Params::Merger { discrete, .. }) = gl.nodes.get(&h).map(|r| &r.params) {
        // Live: the merger's processor reads it every block.
        discrete.store(interp == "discrete", Ordering::Relaxed);
    }
    UNDEF
}

#[crate::host_fn]
unsafe fn ctx_create_channel_splitter(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    if this_engine_handle(g, cf).is_none() {
        return throw_type_error(g, "AudioContext.createChannelSplitter on wrong this");
    }
    let Ok(n) = idl_u32(g, cf_arg(cf, 0), 6) else { return UNDEF };
    if let Some(m) = count_out_of_range(n, "outputs") {
        return super::domexception::throw(
            g,
            "IndexSizeError",
            &format!("Failed to execute 'createChannelSplitter' on 'BaseAudioContext': {m}"),
        );
    }
    ctx_create_node(g, cf, Params::Splitter(n), c.splitter.proto, &[])
}

#[crate::host_fn]
unsafe fn ctx_create_channel_merger(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    if this_engine_handle(g, cf).is_none() {
        return throw_type_error(g, "AudioContext.createChannelMerger on wrong this");
    }
    let Ok(n) = idl_u32(g, cf_arg(cf, 0), 6) else { return UNDEF };
    if let Some(m) = count_out_of_range(n, "inputs") {
        return super::domexception::throw(
            g,
            "IndexSizeError",
            &format!("Failed to execute 'createChannelMerger' on 'BaseAudioContext': {m}"),
        );
    }
    let params = Params::Merger { n, discrete: ArcGc::new(AtomicBool::new(false)) };
    ctx_create_node(g, cf, params, c.merger.proto, &[])
}

/// `AudioNodeOptions` plus the count member, as the constructors read them.
struct PinnedOptions {
    count: Option<u32>,
    mode: Option<String>,
    interpretation: Option<String>,
    n: u32,
}

/// An enum dictionary member: absent → `None`; a value outside the enum is a
/// TypeError (a dictionary, unlike an attribute, does not ignore it).
unsafe fn enum_member(g: *mut c_void, dict: u64, member: &str, allowed: &[&str], ty: &str, iface: &str) -> Result<Option<String>, ()> {
    let v = try_get(g, dict, member)?;
    if v == UNDEF {
        return Ok(None);
    }
    let s = to_js_string(g, v)?;
    let s = read_string(g, s).unwrap_or_default();
    if !allowed.contains(&s.as_str()) {
        throw_type_error(
            g,
            &format!(
                "Failed to construct '{iface}': Failed to read the '{member}' property from \
                 'AudioNodeOptions': The provided value '{s}' is not a valid enum value of type {ty}."
            ),
        );
        return Err(());
    }
    Ok(Some(s))
}

/// The options dictionary, converted in WebIDL order: the inherited
/// `AudioNodeOptions` members first, then `numberOfOutputs` / `numberOfInputs`,
/// each dictionary's members in lexicographic order. `undefined` / `null` are
/// the empty dictionary.
unsafe fn pinned_options(g: *mut c_void, v: u64, n_member: &str, iface: &str, dict: &str) -> Result<PinnedOptions, ()> {
    let mut o = PinnedOptions { count: None, mode: None, interpretation: None, n: 6 };
    if v == UNDEF || v == NULL {
        return Ok(o);
    }
    if !super::blob::is_object(g, v) {
        throw_type_error(g, &format!("Failed to construct '{iface}': The provided value is not of type '{dict}'."));
        return Err(());
    }
    let count = try_get(g, v, "channelCount")?;
    if count != UNDEF {
        o.count = Some(idl_u32(g, count, 0)?);
    }
    o.mode = enum_member(g, v, "channelCountMode", COUNT_MODES, "ChannelCountMode", iface)?;
    o.interpretation = enum_member(g, v, "channelInterpretation", INTERPRETATIONS, "ChannelInterpretation", iface)?;
    o.n = idl_u32(g, try_get(g, v, n_member)?, 6)?;
    Ok(o)
}

/// `new ChannelSplitterNode(context, options)` / `new ChannelMergerNode(…)`.
/// Chromium's order: the dictionary converts (TypeError), the count is checked
/// (IndexSizeError), then the `AudioNodeOptions` go through the attributes'
/// rules (InvalidStateError) — the spec's "initialize the AudioNode".
unsafe fn construct_pinned(g: *mut c_void, cf: *mut c_void, splitter: bool) -> u64 {
    let c = classes(g);
    let (iface, dict, n_member, what) = if splitter {
        ("ChannelSplitterNode", "ChannelSplitterOptions", "numberOfOutputs", "outputs")
    } else {
        ("ChannelMergerNode", "ChannelMergerOptions", "numberOfInputs", "inputs")
    };
    let Some(engine) = host_data::<CtxHandle>(g, c.ctx_cls, cf_arg(cf, 0)).map(|h| h.0) else {
        return throw_type_error(g, &format!("Failed to construct '{iface}': parameter 1 is not of type 'BaseAudioContext'."));
    };
    let Ok(o) = pinned_options(g, cf_arg(cf, 1), n_member, iface, dict) else { return UNDEF };
    if let Some(m) = count_out_of_range(o.n, what) {
        return super::domexception::throw(g, "IndexSizeError", &format!("Failed to construct '{iface}': {m}"));
    }
    let p = if splitter { Pinned::Splitter(o.n) } else { Pinned::Merger { discrete: false } };
    let checks = [
        o.count.map(|v| p.check_count(v)),
        o.mode.as_deref().map(|m| p.check_mode(m)),
        o.interpretation.as_deref().map(|i| p.check_interpretation(i)),
    ];
    if let Some(refused) = checks.into_iter().flatten().find_map(Result::err) {
        return super::domexception::throw(g, "InvalidStateError", &format!("Failed to construct '{iface}': {refused}"));
    }
    let nt = cf_new_target(cf);
    let mut proto = if is_cell(nt) { get(g, nt, "prototype") } else { UNDEF };
    if !is_cell(proto) {
        proto = if splitter { c.splitter.proto } else { c.merger.proto };
    }
    let params = if splitter {
        Params::Splitter(o.n)
    } else {
        let discrete = o.interpretation.as_deref() == Some("discrete");
        Params::Merger { n: o.n, discrete: ArcGc::new(AtomicBool::new(discrete)) }
    };
    create_node_on(g, engine, params, proto, &[])
}

#[crate::host_fn]
unsafe fn splitter_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    construct_pinned(g, cf, true)
}

#[crate::host_fn]
unsafe fn merger_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    construct_pinned(g, cf, false)
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

#[crate::host_fn]
unsafe fn ctx_decode_audio_data(g: *mut c_void, cf: *mut c_void) -> u64 {
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
    // (native task), which for short sound effects is a couple of milliseconds.
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
    let mut worklet_messages: Vec<(u64, NativeMessage)> = Vec::new();
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
                        super::mediastream::unpipe(*nh);
                        pcm::unregister(*nh);
                    }
                }
                gl.nodes.retain(|_, r| r.engine != h);
                gl.timelines.retain(|(n, _), _| *n != h);
                continue;
            }
            if let Some(rec) = gl.nodes.remove(&h) {
                if matches!(rec.params, Params::PcmStream(_)) {
                    // A MediaStreamAudioSourceNode stops reading its track.
                    super::mediastream::unpipe(h);
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

        let default_output = mediadevices::default_generation(mediadevices::Flow::Output);
        for eh in handles {
            // The graph update that ships this frame's work comes LAST (below),
            // after the timelines and the scheduled starts and stops. Values
            // set here then reach the audio thread in this frame, together
            // with any node created since the last one, rather than a frame
            // after it. A node that is created, automated and started in one
            // task must never play a block at its parameters' defaults.
            let now = {
                let eng = engines.get_mut(&eh).unwrap();
                follow_default_output(eng, default_output);
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
                if let Params::Native { shared, .. } = &rec.params {
                    if rec.worklet_obj != 0 {
                        if let Some(message) = shared.take_message() {
                            crate::ffi::redjs_protect(g, rec.worklet_obj);
                            worklet_messages.push((rec.worklet_obj, message));
                        }
                    }
                }
                if let Some((when, offset, duration)) = rec.pending_start {
                    if when <= now {
                        rec.pending_start = None;
                        do_start(eng, rec, offset, duration);
                    }
                }
                if let Some(when) = rec.pending_stop {
                    if when <= now {
                        rec.pending_stop = None;
                        do_stop(eng, rec);
                    }
                }
                if rec.started && !rec.ended && rec.src.done.load(Ordering::Relaxed) {
                    // The walk is over (`SourceSample`): past the end of the
                    // buffer without `loop`, or `duration` played. The sampler
                    // itself only ends at its view's length, so it is stopped
                    // here — and the node ends whether or not a tick ever saw
                    // it playing (a sprite shorter than a frame).
                    do_stop(eng, rec);
                    rec.ended = true;
                    alog!("source walk done node={:?}", rec.id);
                    if rec.js_obj != 0 {
                        ended.push(rec.js_obj);
                        rec.js_obj = 0;
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

            // Ship: graph changes since the last frame, and every value set
            // above, in one go (see the top of this loop).
            if let Err(e) = eng.fw.update() {
                // Always loud: a swallowed update error is silent audio.
                eprintln!("[audio] firewheel update error (engine {eh}): {e:?}");
            }
        }
    }

    // JS callbacks outside the lock. A listener may close a context or force GC.
    for (obj, message) in worklet_messages {
        native_worklet::dispatch_message(g, obj, message);
        crate::ffi::redjs_unprotect(g, obj);
    }
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
        methods: &[("connect", 1, node_connect), ("disconnect", 0, node_disconnect)],
        getters: &[("numberOfInputs", node_get_number_of_inputs), ("numberOfOutputs", node_get_number_of_outputs)],
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
        methods: &[("start", 0, src_start), ("stop", 0, src_stop)],
        getters: &[("playbackRate", get_slot_playback_rate), ("detune", get_slot_detune)],
        accessors: &[
            ("buffer", src_get_buffer, src_set_buffer),
            ("loop", src_get_loop, src_set_loop),
            ("loopStart", src_get_loop_start, src_set_loop_start),
            ("loopEnd", src_get_loop_end, src_set_loop_end),
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

    // The three attributes the spec pins on these two (see `Pinned`).
    let pinned: &[(&str, HostFn, HostFn)] = &[
        ("channelCount", pinned_get_count, pinned_set_count),
        ("channelCountMode", pinned_get_mode, pinned_set_mode),
        ("channelInterpretation", pinned_get_interpretation, pinned_set_interpretation),
    ];
    let splitter = define_class(g, &ClassSpec {
        name: "ChannelSplitterNode",
        arity: 1,
        construct: Some(splitter_construct),
        parent_proto: node_base.proto,
        methods: &[],
        getters: &[],
        accessors: pinned,
    });
    let merger = define_class(g, &ClassSpec {
        name: "ChannelMergerNode",
        arity: 1,
        construct: Some(merger_construct),
        parent_proto: node_base.proto,
        methods: &[],
        getters: &[],
        accessors: pinned,
    });

    let (worklet, worklet_port, worklet_port_cls) = native_worklet::install(g, node_base.proto);

    let ctx = define_class(g, &ClassSpec {
        name: "AudioContext",
        arity: 0,
        construct: Some(ctx_construct),
        parent_proto: 0,
        methods: &[
            ("loadNativeAudioWorklet", 1, native_worklet::load),
            ("createGain", 0, ctx_create_gain),
            ("createStereoPanner", 0, ctx_create_stereo_panner),
            ("createPanner", 0, ctx_create_panner),
            ("createBiquadFilter", 0, ctx_create_biquad),
            ("createAnalyser", 0, ctx_create_analyser),
            ("createBufferSource", 0, ctx_create_buffer_source),
            ("createOscillator", 0, ctx_create_oscillator),
            ("createChannelSplitter", 0, ctx_create_channel_splitter),
            ("createChannelMerger", 0, ctx_create_channel_merger),
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
        worklet,
        worklet_port,
        worklet_port_cls,
        splitter,
        merger,
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
               .filter(d => d.kind === 'audiooutput')
               .map(d => d.deviceId).filter(id => id !== '' && id !== 'default');
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

    #[test]
    fn an_envelope_scheduled_with_its_node_ships_with_it() {
        // The Argon dialpad's tone, in one task: a gain built, its envelope
        // scheduled from 0 at `now`, its sources started. The next graph
        // update sends the node with the value stored here, so that value
        // must already be the envelope's. It used to stay at the default 1
        // until a pump evaluated the timeline, and the update after that
        // delivered it: a frame or two of a full-scale dual tone per key.
        let c = ctx();
        s(
            &c,
            "globalThis.__d = new AudioContext(); const g = __d.createGain(); globalThis.__g = g; \
             const now = __d.currentTime; g.gain.setValueAtTime(0, now); \
             g.gain.linearRampToValueAtTime(0.075, now + 0.008); 'ok'",
        );
        // The amplitude the queued VolumeNode will apply.
        let gains = || -> Vec<f32> {
            let gl = super::glob().lock().unwrap();
            gl.nodes
                .values()
                .filter(|r| gl.engines.get(&r.engine).is_some_and(|e| e.g == c.raw() as usize))
                .filter_map(|r| match &r.params {
                    super::Params::Gain(p) => Some(p.volume.amp()),
                    _ => None,
                })
                .collect()
        };
        let now = gains();
        assert_eq!(now.len(), 1);
        // 0 at `now`, at most the ramp's 0.075 if the clock ticked meanwhile.
        assert!(now[0] <= 0.075 + 1e-6, "the gain ships at {}, not the envelope's value", now[0]);

        // A value scheduled for later holds the current one until then.
        s(&c, "__g.gain.setValueAtTime(0.5, __d.currentTime + 60); 'ok'");
        assert!(gains()[0] <= 0.075 + 1e-6);
        s(&c, "__d.close(); 'ok'");
    }

    #[test]
    fn a_gain_is_an_amplitude() {
        // firewheel squares `Volume::Linear`, so a gain of 0.5 used to play at
        // 0.25. The page reads back exactly what it set, a negative gain
        // included; the node applies the magnitude.
        let c = ctx();
        let got = s(
            &c,
            "globalThis.__d = new AudioContext(); \
             [0.075, 0.5, 1, 2, -0.5].map(v => { const g = __d.createGain(); g.gain.value = v; \
               return g.gain.value === Math.fround(v); }).join()",
        );
        assert_eq!(got, "true,true,true,true,true");
        let mut amps: Vec<(f32, f32)> = {
            let gl = super::glob().lock().unwrap();
            gl.nodes
                .values()
                .filter(|r| gl.engines.get(&r.engine).is_some_and(|e| e.g == c.raw() as usize))
                .filter_map(|r| match &r.params {
                    super::Params::Gain(p) => Some((r.gain, p.volume.amp())),
                    _ => None,
                })
                .collect()
        };
        amps.sort_by(|a, b| a.0.total_cmp(&b.0));
        assert_eq!(amps.len(), 5);
        for (set, amp) in amps {
            assert!((amp - set.abs()).abs() <= 1e-6 * set.abs().max(1.0), "gain {set} plays at {amp}");
        }
        s(&c, "__d.close(); 'ok'");
    }

    #[test]
    fn a_playing_source_keeps_the_nodes_after_it() {
        // The dialpad's chain, kept by nothing but its playing oscillator:
        // osc → panner → gain → filter → destination. Beside it, nodes no
        // one holds or connects to, which a collection must take (they show
        // the collection really ran), and a gain the oscillator was connected
        // to and then disconnected from, which runs the release path (it is
        // let go of, but a conservative collector may still keep it).
        let c = ctx();
        s(
            &c,
            "globalThis.__d = new AudioContext(); (() => { \
               const o = __d.createOscillator(), p = __d.createStereoPanner(), \
                     g = __d.createGain(), f = __d.createBiquadFilter(), gone = __d.createGain(); \
               g.gain.value = 0; \
               o.connect(p).connect(g).connect(f).connect(__d.destination); \
               o.connect(gone); o.disconnect(gone); \
               o.start(); \
               for (let i = 0; i < 64; i++) __d.createGain(); \
             })(); 'ok'",
        );
        // Nodes of this context, by kind: (panners, gains, filters).
        let census = || -> (usize, usize, usize) {
            let gl = super::glob().lock().unwrap();
            let mine = gl.nodes.values().filter(|r| gl.engines.get(&r.engine).is_some_and(|e| e.g == c.raw() as usize));
            let (mut p, mut g, mut f) = (0, 0, 0);
            for r in mine {
                match &r.params {
                    super::Params::Pan(_) => p += 1,
                    super::Params::Gain(_) => g += 1,
                    super::Params::Svf(_) => f += 1,
                    _ => {}
                }
            }
            (p, g, f)
        };
        assert_eq!(census(), (1, 66, 1));
        for _ in 0..4 {
            c.gc();
            unsafe { super::pump(c.raw()) };
        }
        let (p, g, f) = census();
        // The collector is conservative, so a stray stack word may keep one
        // of the loose gains; most of them must go.
        assert!(g < 33, "the unreferenced gains were not collected ({g} left), so this proves nothing");
        assert_eq!((p, f), (1, 1), "the panner or the filter after a playing oscillator was collected");
        assert!(g >= 1, "the gain after a playing oscillator was collected");
        let chain_gain = {
            let gl = super::glob().lock().unwrap();
            gl.nodes
                .values()
                .filter(|r| gl.engines.get(&r.engine).is_some_and(|e| e.g == c.raw() as usize))
                .any(|r| matches!(&r.params, super::Params::Gain(_)) && r.gain == 0.0)
        };
        assert!(chain_gain, "the chain's own gain (set to 0) was collected");
        s(&c, "__d.close(); 'ok'");
    }

    #[test]
    fn a_context_on_the_default_sink_follows_the_default_and_a_chosen_sink_stays() {
        // Driven per engine, not through the process-wide generation, so the
        // other tests' contexts are not reopened under them. The reopen itself
        // is `restart_stream`, which the setSinkId tests above cover.
        let c = ctx();
        s(&c, "globalThis.__a = new AudioContext(); globalThis.__b = new AudioContext(); 'ok'");
        let mut gl = super::glob().lock().unwrap();
        let mut mine: Vec<u64> = gl.engines.iter().filter(|(_, e)| e.g == c.raw() as usize).map(|(&h, _)| h).collect();
        mine.sort_unstable();
        assert_eq!(mine.len(), 2);
        let (a, b) = (mine[0], mine[1]);
        // `b` was given a sink of its own (the id only has to be one that is
        // not the default; nothing reopens it).
        gl.engines.get_mut(&b).unwrap().sink_id = "chosen-endpoint".into();

        let next = gl.engines[&a].default_seen + 1;
        let ea = gl.engines.get_mut(&a).unwrap();
        let had_device = ea.running;
        let reopened = super::follow_default_output(ea, next);
        // Hardware-conditional: with no output device, the reopen fails and
        // the context keeps running silent, as it was created.
        assert_eq!(reopened, had_device, "a default-sink context reopens on the new default");
        assert_eq!(ea.default_seen, next);
        assert!(!super::follow_default_output(ea, next), "the same move is followed once");

        let eb = gl.engines.get_mut(&b).unwrap();
        let next_b = eb.default_seen + 1;
        assert!(!super::follow_default_output(eb, next_b), "a chosen sink stays where it is");
        assert_eq!(eb.default_seen, next_b);

        eb.sink_id = super::mediadevices::DEFAULT_SINK.into();
        eb.state = super::ST_CLOSED;
        assert!(!super::follow_default_output(eb, next_b + 1), "a closed context is not reopened");
        drop(gl);
        s(&c, "__a.close(); __b.close(); 'ok'");
    }

    // -----------------------------------------------------------------------
    // OscillatorNode
    // -----------------------------------------------------------------------

    /// Run the audio pump for roughly `ms`, so the audio thread gets to render
    /// some blocks. Returns whether the context's clock actually advanced —
    /// i.e. whether there is a real output stream. Every signal assertion below
    /// is conditional on that, so the suite is green on a machine with no sound
    /// card AND meaningful on one with.
    pub(super) fn pump_for(c: &Context, ms: u64) -> bool {
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

    /// Pump until the audio thread has rendered everything up to `t` seconds
    /// of `__ctx`'s time, and what it posted on the way has been dispatched.
    /// Gives up after `cap_ms` of wall time, returning false.
    ///
    /// For assertions about DSP timing, which counts samples: a CI VM's
    /// virtual output renders in bursts and can fall behind real time, so a
    /// wall-clock window says nothing about how much audio was processed.
    /// Firewheel publishes its clock at the START of a callback, as the end of
    /// the frames that callback is about to render, so a clock past `t` does
    /// not yet mean those frames are done; the clock moving once more does
    /// (callbacks run one after another).
    pub(super) fn pump_until_rendered(c: &Context, t: f64, cap_ms: u64) -> bool {
        let now = || s(c, "globalThis.__ctx.currentTime").parse::<f64>().unwrap_or(0.0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(cap_ms);
        let mut reached = None;
        loop {
            unsafe { super::pump(c.raw()) };
            c.drain_microtasks();
            let clock = now();
            match reached {
                None if clock >= t => reached = Some(clock),
                Some(r) if clock > r => {
                    // The clock may have moved after that pump drained the
                    // mailboxes; one more delivers what those frames posted.
                    unsafe { super::pump(c.raw()) };
                    c.drain_microtasks();
                    return true;
                }
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// RMS of an analyser's time-domain window, as JS sees it.
    pub(super) fn analyser_rms(c: &Context, which: &str) -> f64 {
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

        // Prime the channel, then keep it fed across the pump ticks. A slow
        // machine (a CI VM's virtual output) can take longer than the first
        // 400 ms to carry the signal through, so a live context gets up to 3 s.
        let mut accepted_total = 0f64;
        let mut rms = 0.0;
        for tick in 0..300 {
            accepted_total += s(&c, "__fill()").parse::<f64>().unwrap_or(0.0);
            unsafe { super::pump(c.raw()) };
            c.drain_microtasks();
            std::thread::sleep(std::time::Duration::from_millis(10));
            if tick >= 40 {
                rms = analyser_rms(&c, "__an");
                if rms > 0.1 || s(&c, "__ctx.currentTime") == "0" {
                    break;
                }
            }
        }
        let live = s(&c, "__ctx.currentTime") != "0";

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
        if got.contains(r#""rate":48000"#) {
            assert!(got.contains(r#""peak":0.732"#), "decoded amplitude is wrong: {got}");
            assert!(got.contains(r#""rms":0.732"#), "a decoded square must have RMS == peak: {got}");
        } else {
            // The buffer comes at the context's rate (the output device's, e.g.
            // 44.1 kHz on a CI Mac), and band-limited resampling of a square
            // overshoots at the edges (Gibbs), so only the energy is exact.
            let field = |k: &str| -> f64 {
                let at = got.find(&format!("\"{k}\":")).unwrap() + k.len() + 3;
                got[at..].split([',', '}']).next().unwrap().parse().unwrap()
            };
            assert!((field("rms") - 0.732).abs() < 0.01, "decoded energy is wrong: {got}");
            assert!(field("peak") >= 0.72, "decoded amplitude is wrong: {got}");
        }
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

    #[test]
    fn a_buffer_source_has_loop_points_and_start_checks_its_range() {
        let c = ctx();
        ok(
            &c,
            "{ const a = new AudioContext(); const s = a.createBufferSource();
               const fresh = s.loopStart === 0 && s.loopEnd === 0;
               s.loopStart = 1.5; s.loopEnd = '2.25';
               const r = fresh && s.loopStart === 1.5 && s.loopEnd === 2.25
                      && 'loopStart' in AudioBufferSourceNode.prototype
                      && 'loopEnd' in AudioBufferSourceNode.prototype
                      && AudioBufferSourceNode.prototype.start.length === 0;
               a.close(); r }",
        );
        let msg =
            throws(&c, "{ const a = new AudioContext(); try { a.createBufferSource().loopEnd = NaN; } finally { a.close(); } }");
        assert!(msg.contains("non-finite"), "{msg}");
        for call in ["s.start(-1)", "s.start(0, -0.5)", "s.start(0, 0, -1)"] {
            ok(
                &c,
                &format!(
                    "(() => {{ const a = new AudioContext(); const s = a.createBufferSource();
                       try {{ {call}; return false; }} catch (e) {{ return e instanceof RangeError; }}
                       finally {{ a.close(); }} }})()"
                ),
            );
        }
        // An oscillator's `start(when)` has no offset: a second argument is ignored.
        ok(
            &c,
            "{ const a = new AudioContext(); const o = a.createOscillator();
               o.start(0, -1); o.stop(); a.close(); true }",
        );
    }

    #[test]
    fn unloading_the_page_closes_its_contexts() {
        // `location.reload()` in one realm: the old page's AudioContexts
        // close (a looping ringtone stops with them), already-closed ones are
        // not counted, and the page that comes next starts its own.
        let c = ctx();
        s(&c, "globalThis.__a = new AudioContext(); globalThis.__b = new AudioContext();
               globalThis.__z = new AudioContext(); __z.close(); 0");
        assert_eq!(super::close_all(c.raw()), 2);
        ok(&c, "__a.state === 'closed' && __b.state === 'closed'");
        assert_eq!(super::close_all(c.raw()), 0);
        ok(&c, "{ const n = new AudioContext(); const r = n.state !== 'closed'; n.close(); r }");
    }

    #[test]
    fn a_sprite_ends_after_its_duration() {
        // `start(0, offset, duration)` on a 1 s buffer: `onended` fires after
        // the 20 ms slice, not after the rest of the buffer. Output muted.
        let c = ctx();
        let wav = wav_fixture(48_000, 48_000);
        s(&c, &format!("globalThis.__wav = new Uint8Array([{}]).buffer; 'ok'", as_js_bytes(&wav)));
        let started = run(
            &c,
            "globalThis.__ctx = new AudioContext();
             const a = globalThis.__ctx;
             const mute = a.createGain(); mute.gain.value = 0; mute.connect(a.destination);
             const buf = await a.decodeAudioData(__wav);
             globalThis.__ended = 0;
             const src = a.createBufferSource();
             src.buffer = buf; src.connect(mute);
             src.onended = () => { globalThis.__ended = a.currentTime; };
             globalThis.__t0 = a.currentTime;
             src.start(0, 0.5, 0.02);
             return 'started';",
        );
        assert_eq!(started, "started", "{started}");
        let live = pump_for(&c, 300);
        if !live {
            eprintln!("[sprite] no output stream — the end is not asserted");
            s(&c, "__ctx.close(); 'closed'");
            return;
        }
        ok(&c, "__ended > 0 && __ended - __t0 < 0.25");
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

    // -----------------------------------------------------------------------
    // ChannelSplitterNode / ChannelMergerNode (the signal itself is checked
    // sample by sample in `split_merge_graph_tests`, with no device)
    // -----------------------------------------------------------------------

    /// `stmt` must throw a DOMException named `name`.
    fn dom_throw(c: &Context, stmt: &str, name: &str) {
        ok(
            c,
            &format!(
                "(() => {{ try {{ {stmt}; return false; }} \
                   catch (e) {{ return e instanceof DOMException && e.name === '{name}'; }} }})()"
            ),
        );
    }

    #[test]
    fn splitter_and_merger_have_the_spec_shape() {
        let c = ctx();
        ok(&c, "AudioContext.prototype.createChannelSplitter.length === 0 && AudioContext.prototype.createChannelMerger.length === 0");
        assert_eq!(
            s(
                &c,
                "{ const a = new AudioContext();
                   const d = a.createChannelSplitter(), m = a.createChannelMerger(), d2 = a.createChannelSplitter(2);
                   const r = JSON.stringify([
                     d instanceof ChannelSplitterNode && d instanceof AudioNode,
                     d.numberOfInputs, d.numberOfOutputs, d.channelCount, d.channelCountMode, d.channelInterpretation,
                     m instanceof ChannelMergerNode && m instanceof AudioNode,
                     m.numberOfInputs, m.numberOfOutputs, m.channelCount, m.channelCountMode, m.channelInterpretation,
                     d2.numberOfOutputs, d2.channelCount]);
                   a.close(); r }"
            ),
            r#"[true,1,6,6,"explicit","discrete",true,6,1,1,"explicit","speakers",2,2]"#
        );
        // Every node answers the two counts now, and they are the spec's.
        ok(
            &c,
            "{ const a = new AudioContext();
               const g = a.createGain(), o = a.createOscillator(), an = a.createAnalyser(), b = a.createBufferSource();
               const r = g.numberOfInputs === 1 && g.numberOfOutputs === 1
                      && o.numberOfInputs === 0 && o.numberOfOutputs === 1
                      && b.numberOfInputs === 0 && an.numberOfInputs === 1
                      && a.destination.numberOfInputs === 1
                      && AudioNode.prototype.connect.length === 1 && AudioNode.prototype.disconnect.length === 0;
               a.close(); r }",
        );
    }

    #[test]
    fn the_constructors_take_a_context_and_options() {
        let c = ctx();
        ok(
            &c,
            "{ const a = new AudioContext();
               const d = new ChannelSplitterNode(a, { numberOfOutputs: 2 });
               const m = new ChannelMergerNode(a, { numberOfInputs: 3, channelInterpretation: 'discrete' });
               const e = new ChannelSplitterNode(a);
               const r = d instanceof ChannelSplitterNode && d.numberOfOutputs === 2
                      && m.numberOfInputs === 3 && m.channelInterpretation === 'discrete'
                      && e.numberOfOutputs === 6
                      && ChannelSplitterNode.length === 1 && ChannelMergerNode.length === 1;
               a.close(); r }",
        );
        ok(
            &c,
            "{ const a = new AudioContext(); class Mine extends ChannelMergerNode {}
               const m = new Mine(a, { numberOfInputs: 2 });
               const r = m instanceof Mine && m instanceof ChannelMergerNode && m.numberOfInputs === 2; a.close(); r }",
        );
        assert!(throws(&c, "new ChannelSplitterNode()").contains("BaseAudioContext"));
        assert!(throws(&c, "new ChannelMergerNode({})").contains("BaseAudioContext"));
        assert!(throws(&c, "ChannelSplitterNode({})").contains("new"));
        // A non-dictionary options is a TypeError, and so is a member outside its enum.
        ok(
            &c,
            "(() => { const a = new AudioContext();
               try { new ChannelSplitterNode(a, 2); return false; } catch (e) { return e instanceof TypeError; }
               finally { a.close(); } })()",
        );
        ok(
            &c,
            "(() => { const a = new AudioContext();
               try { new ChannelMergerNode(a, { channelCountMode: 'bogus' }); return false; }
               catch (e) { return e instanceof TypeError && /ChannelCountMode/.test(e.message); }
               finally { a.close(); } })()",
        );
    }

    #[test]
    fn a_count_outside_1_to_32_is_an_index_size_error() {
        let c = ctx();
        s(&c, "globalThis.__a = new AudioContext(); 0");
        for call in [
            "__a.createChannelSplitter(0)",
            "__a.createChannelSplitter(33)",
            "__a.createChannelMerger(0)",
            "__a.createChannelMerger(33)",
            "__a.createChannelSplitter(-1)",
            "__a.createChannelMerger(NaN)",
            "new ChannelSplitterNode(__a, { numberOfOutputs: 0 })",
            "new ChannelMergerNode(__a, { numberOfInputs: 40 })",
        ] {
            dom_throw(&c, call, "IndexSizeError");
        }
        let msg = throws(&c, "__a.createChannelSplitter(33)");
        assert!(msg.contains("outside the range [1, 32]"), "{msg}");
        ok(
            &c,
            "__a.createChannelSplitter(32).numberOfOutputs === 32 && __a.createChannelMerger(32).numberOfInputs === 32
             && __a.createChannelSplitter(1).numberOfOutputs === 1",
        );
        // The IDL `unsigned long`: 2.9 is 2 and '3' is 3.
        ok(&c, "__a.createChannelSplitter(2.9).numberOfOutputs === 2 && __a.createChannelMerger('3').numberOfInputs === 3");
        s(&c, "__a.close(); 0");
    }

    #[test]
    fn the_pinned_attributes_refuse_a_change_and_accept_their_own_value() {
        let c = ctx();
        s(&c, "globalThis.__a = new AudioContext(); globalThis.__d = __a.createChannelSplitter(4); globalThis.__m = __a.createChannelMerger(2); 0");
        for stmt in [
            "__d.channelCount = 2",
            "__d.channelCountMode = 'max'",
            "__d.channelInterpretation = 'speakers'",
            "__m.channelCount = 2",
            "__m.channelCountMode = 'clamped-max'",
            "new ChannelSplitterNode(__a, { numberOfOutputs: 2, channelCount: 3 })",
            "new ChannelSplitterNode(__a, { channelCountMode: 'max' })",
            "new ChannelSplitterNode(__a, { channelInterpretation: 'speakers' })",
            "new ChannelMergerNode(__a, { channelCount: 2 })",
            "new ChannelMergerNode(__a, { channelCountMode: 'max' })",
        ] {
            dom_throw(&c, stmt, "InvalidStateError");
        }
        let msg = throws(&c, "__d.channelCount = 6");
        assert!(msg.contains("cannot be changed from 4"), "{msg}");
        // Their own value is allowed.
        ok(
            &c,
            "{ __d.channelCount = 4; __d.channelCountMode = 'explicit'; __d.channelInterpretation = 'discrete';
               __m.channelCount = 1; __m.channelCountMode = 'explicit';
               new ChannelSplitterNode(__a, { numberOfOutputs: 3, channelCount: 3, channelCountMode: 'explicit',
                                              channelInterpretation: 'discrete' }).channelCount === 3 }",
        );
        // Not one of the enum's strings: ignored, as WebIDL says for an attribute.
        ok(
            &c,
            "{ __d.channelCountMode = 'bogus'; __d.channelInterpretation = 'nope';
               __d.channelCountMode === 'explicit' && __d.channelInterpretation === 'discrete' }",
        );
        // The merger's interpretation is the one that may change.
        ok(
            &c,
            "{ __m.channelInterpretation = 'discrete'; const was = __m.channelInterpretation;
               __m.channelInterpretation = 'speakers'; was === 'discrete' && __m.channelInterpretation === 'speakers' }",
        );
        // The accessors are brand-checked.
        assert!(throws(&c, "Object.getOwnPropertyDescriptor(ChannelSplitterNode.prototype, 'channelCount').get.call(__a.createGain())")
            .contains("wrong this"));
        s(&c, "__a.close(); 0");
    }

    #[test]
    fn connect_and_disconnect_use_the_output_and_input_indices() {
        let c = ctx();
        s(&c, "globalThis.__a = new AudioContext(); 0");
        // The client's AudioManager.initVirtualOutputStream, verbatim in shape:
        // the call that used to be a TypeError at module scope.
        ok(
            &c,
            "{ const ctx = __a;
               const outputAnalyserNode = ctx.createAnalyser();
               outputAnalyserNode.fftSize = 256; outputAnalyserNode.smoothingTimeConstant = 0.3;
               outputAnalyserNode.connect(ctx.destination);
               const outputSplitter = ctx.createChannelSplitter(2);
               const left = ctx.createAnalyser(); left.fftSize = 256; left.smoothingTimeConstant = 0.3;
               const right = ctx.createAnalyser(); right.fftSize = 256; right.smoothingTimeConstant = 0.3;
               const l = outputSplitter.connect(left, 0);
               const r = outputSplitter.connect(right, 1);
               const masterGainNode = ctx.createGain();
               masterGainNode.connect(outputAnalyserNode);
               masterGainNode.connect(outputSplitter);
               const voiceBusNode = ctx.createGain();
               voiceBusNode.connect(masterGainNode);
               l === left && r === right }",
        );
        s(
            &c,
            "globalThis.__d = __a.createChannelSplitter(2); globalThis.__m = __a.createChannelMerger(2);
             globalThis.__g = __a.createGain(); globalThis.__o = __a.createOscillator(); 0",
        );
        for stmt in [
            "__d.connect(__g, 2)",
            "__g.connect(__a.destination, 1)",
            "__g.connect(__m, 0, 2)",
            "__g.connect(__o)",
            "__d.disconnect(2)",
            "__g.disconnect(__m, 1)",
            "__d.disconnect(__m, 0, 2)",
        ] {
            dom_throw(&c, stmt, "IndexSizeError");
        }
        ok(&c, "__d.connect(__m, 0, 1) === __m && __d.connect(__m, 1, 0) === __m && __m.connect(__g) === __g");
        // The same termini twice are one connection: the second disconnect finds nothing.
        s(&c, "__d.connect(__m, 0, 1); __d.disconnect(__m, 0, 1); 0");
        dom_throw(&c, "__d.disconnect(__m, 0, 1)", "InvalidAccessError");
        // disconnect(output) takes only that output.
        s(&c, "__d.connect(__g, 0); __d.connect(__g, 1); __d.disconnect(1); 0");
        dom_throw(&c, "__d.disconnect(__g, 1)", "InvalidAccessError");
        s(&c, "__d.disconnect(__g, 0); 0");
        dom_throw(&c, "__g.disconnect(__o)", "InvalidAccessError");
        dom_throw(&c, "__g.disconnect(__g.gain)", "InvalidAccessError");
        dom_throw(&c, "{ const b = new AudioContext(); try { __g.connect(b.destination); } finally { b.close(); } }", "InvalidAccessError");
        // disconnect() still takes everything, and is then a no-op.
        ok(&c, "{ __m.disconnect(); __m.disconnect(); __d.disconnect(); true }");
        s(&c, "__a.close(); 0");
    }

    #[test]
    fn a_split_stereo_signal_reaches_each_channel_s_analyser_and_merges_back() {
        // The same graph `split_merge_graph_tests` renders by hand, through the
        // JS surface and a live device: L a ±0.5 square, R a ±0.25 one, each
        // read back off its own analyser (RMS = amplitude), then merged back
        // swapped and split again. Output muted.
        let c = ctx();
        s(
            &c,
            "globalThis.__ctx = new AudioContext();
             const a = __ctx, rate = a.sampleRate, n = rate;
             const buf = a.createBuffer(2, n, rate);
             const L = new Float32Array(n), R = new Float32Array(n);
             for (let i = 0; i < n; i++) { L[i] = (i % 100) < 50 ? 0.5 : -0.5; R[i] = (i % 60) < 30 ? 0.25 : -0.25; }
             buf.copyToChannel(L, 0); buf.copyToChannel(R, 1);
             const src = a.createBufferSource(); src.buffer = buf; src.loop = true;
             const sp = a.createChannelSplitter(2);
             globalThis.__l = a.createAnalyser(); globalThis.__r = a.createAnalyser();
             src.connect(sp); sp.connect(__l, 0); sp.connect(__r, 1);
             const m = a.createChannelMerger(2); sp.connect(m, 0, 1); sp.connect(m, 1, 0);
             const sp2 = a.createChannelSplitter(2); m.connect(sp2);
             globalThis.__l2 = a.createAnalyser(); globalThis.__r2 = a.createAnalyser();
             sp2.connect(__l2, 0); sp2.connect(__r2, 1);
             const mute = a.createGain(); mute.gain.value = 0; m.connect(mute); mute.connect(a.destination);
             src.start(); 'wired'",
        );
        let live = pump_for(&c, 400);
        let (l, r) = (analyser_rms(&c, "__l"), analyser_rms(&c, "__r"));
        let (l2, r2) = (analyser_rms(&c, "__l2"), analyser_rms(&c, "__r2"));
        if !live {
            eprintln!("[splitter] no output stream on this machine — signal assertions skipped");
            s(&c, "__ctx.close(); 'closed'");
            return;
        }
        assert!((l - 0.5).abs() < 0.02, "left channel's analyser: RMS {l}, want 0.5");
        assert!((r - 0.25).abs() < 0.02, "right channel's analyser: RMS {r}, want 0.25");
        assert!((l2 - 0.25).abs() < 0.02, "merged-swapped left: RMS {l2}, want 0.25");
        assert!((r2 - 0.5).abs() < 0.02, "merged-swapped right: RMS {r2}, want 0.5");
        s(&c, "__ctx.close(); 'closed'");
    }
}

#[cfg(test)]
mod split_merge_graph_tests {
    //! The splitter and merger through a REAL Firewheel graph, rendered by
    //! hand: an offline backend hands the test the processor, so every sample
    //! is known and the test means the same on a machine with no sound card.
    //! The port maps are `conn_ports` itself — exactly what `connect` wires.
    use super::*;
    use core::time::Duration;
    use firewheel::backend::{AudioBackend, BackendProcessInfo};
    use firewheel::node::StreamStatus;
    use firewheel::processor::FirewheelProcessor;
    use firewheel::{FirewheelConfig, FirewheelCtx, StreamInfo};

    const RATE: u32 = 48_000;
    const BLOCK: usize = 128;

    #[derive(Debug)]
    struct Never;

    impl core::fmt::Display for Never {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("never")
        }
    }

    impl std::error::Error for Never {}

    /// No device: `set_processor` keeps the processor for the test to drive.
    struct Offline {
        processor: Option<FirewheelProcessor<Offline>>,
    }

    impl AudioBackend for Offline {
        type Enumerator = ();
        type Config = ();
        type StartStreamError = Never;
        type StreamError = Never;
        type Instant = ();

        fn enumerator() -> Self::Enumerator {}

        fn start_stream(_config: ()) -> Result<(Self, StreamInfo), Never> {
            let info = StreamInfo {
                sample_rate: NonZeroU32::new(RATE).unwrap(),
                prev_sample_rate: NonZeroU32::new(RATE).unwrap(),
                max_block_frames: NonZeroU32::new(BLOCK as u32).unwrap(),
                num_stream_out_channels: 2,
                ..Default::default()
            };
            Ok((Offline { processor: None }, info))
        }

        fn set_processor(&mut self, processor: FirewheelProcessor<Self>) {
            self.processor = Some(processor);
        }

        fn poll_status(&mut self) -> Result<(), Never> {
            Ok(())
        }

        fn delay_from_last_process(&self, _process_timestamp: ()) -> Option<Duration> {
            None
        }
    }

    /// The known stereo signal, by sample clock: L a ±0.5 square of period 64,
    /// R a sawtooth over [-0.25, 0.24].
    fn left(t: u64) -> f32 {
        if (t / 32) % 2 == 0 { 0.5 } else { -0.5 }
    }

    fn right(t: u64) -> f32 {
        (t % 50) as f32 / 100.0 - 0.25
    }

    #[derive(Clone)]
    struct Known;

    impl firewheel::node::AudioNode for Known {
        type Configuration = firewheel::node::EmptyConfig;

        fn info(&self, _config: &Self::Configuration) -> firewheel::node::AudioNodeInfo {
            firewheel::node::AudioNodeInfo::new().debug_name("test_known_stereo").channel_config(
                firewheel::channel_config::ChannelConfig {
                    num_inputs: firewheel::channel_config::ChannelCount::ZERO,
                    num_outputs: firewheel::channel_config::ChannelCount::STEREO,
                },
            )
        }

        fn construct_processor(
            &self,
            _config: &Self::Configuration,
            _cx: firewheel::node::ConstructProcessorContext,
        ) -> impl firewheel::node::AudioNodeProcessor {
            KnownProcessor { t: 0 }
        }
    }

    struct KnownProcessor {
        t: u64,
    }

    impl firewheel::node::AudioNodeProcessor for KnownProcessor {
        fn process(
            &mut self,
            info: &firewheel::node::ProcInfo,
            buffers: firewheel::node::ProcBuffers,
            _events: &mut firewheel::event::ProcEvents,
            _extra: &mut firewheel::node::ProcExtra,
        ) -> firewheel::node::ProcessStatus {
            for i in 0..info.frames {
                let t = self.t + i as u64;
                buffers.outputs[0][i] = left(t);
                buffers.outputs[1][i] = right(t);
            }
            self.t += info.frames as u64;
            firewheel::node::ProcessStatus::OutputsModified
        }
    }

    type Fw = FirewheelCtx<Offline>;

    fn rec(id: NodeID, params: Params) -> NodeRec {
        NodeRec::new(0, id, params)
    }

    fn splitter(fw: &mut Fw, n: u32) -> NodeRec {
        rec(fw.add_node(SplitterNode { n }, None), Params::Splitter(n))
    }

    fn merger(fw: &mut Fw, n: u32, discrete: bool) -> NodeRec {
        let d = ArcGc::new(AtomicBool::new(discrete));
        rec(fw.add_node(MergerNode { n, discrete: ArcGc::clone(&d) }, None), Params::Merger { n, discrete: d })
    }

    /// An analyser's monitor: it records the mono mix of its stereo input, so
    /// a mono output fed into both ports reads back as itself, exactly.
    fn monitor(fw: &mut Fw) -> (NodeRec, ArcGc<MonitorRing>) {
        let ring = ArcGc::new(MonitorRing::new());
        let id = fw.add_node(MonitorNode { ring: ArcGc::clone(&ring) }, None);
        (rec(id, Params::Analyser { ring: ArcGc::clone(&ring), smooth: Vec::new() }), ring)
    }

    fn wire(fw: &mut Fw, src: &NodeRec, output: u32, dst: &NodeRec, input: u32) {
        let ports = conn_ports(src, output, dst, input);
        fw.connect(src.id, dst.id, &ports, true).unwrap_or_else(|e| panic!("connect {ports:?}: {e:?}"));
    }

    fn tap(fw: &mut Fw, src: &NodeRec, output: u32) -> ArcGc<MonitorRing> {
        let (m, ring) = monitor(fw);
        wire(fw, src, output, &m, 0);
        ring
    }

    fn render(fw: &mut Fw, blocks: usize) {
        let mut out = vec![0.0f32; BLOCK * 2];
        for b in 0..blocks {
            fw.update().expect("firewheel update");
            let p = fw.active_backend_mut().unwrap().processor.as_mut().expect("the processor was handed over");
            p.process_interleaved(
                &[],
                &mut out,
                BackendProcessInfo {
                    num_in_channels: 0,
                    num_out_channels: 2,
                    frames: BLOCK,
                    process_timestamp: (),
                    duration_since_stream_start: Duration::from_secs_f64((b * BLOCK) as f64 / RATE as f64),
                    input_stream_status: StreamStatus::empty(),
                    output_stream_status: StreamStatus::empty(),
                    dropped_frames: 0,
                },
            );
        }
    }

    /// The last 1024 samples `ring` saw, against `want(t)` on the sample clock.
    fn assert_signal(ring: &MonitorRing, total: u64, what: &str, want: impl Fn(u64) -> f32) {
        assert_eq!(ring.cursor.load(Ordering::Acquire) as u64, total, "{what}: the monitor missed frames");
        let n = 1024usize;
        let mut got = vec![0.0f32; n];
        ring.read_latest(&mut got);
        for (i, v) in got.iter().enumerate() {
            let t = total - n as u64 + i as u64;
            let w = want(t);
            assert!((v - w).abs() < 1e-6, "{what}: sample {t} is {v}, want {w}");
        }
    }

    #[test]
    fn a_known_stereo_signal_splits_into_its_channels_and_merges_back() {
        let mut fw = Fw::new(FirewheelConfig::default());
        // A stand-in record for the source: `conn_ports` reads only its shape
        // (two output ports, not a splitter).
        let src = rec(fw.add_node(Known, None), Params::Passthrough);

        // Split: each output is one channel.
        let split = splitter(&mut fw, 2);
        wire(&mut fw, &src, 0, &split, 0);
        let split_l = tap(&mut fw, &split, 0);
        let split_r = tap(&mut fw, &split, 1);

        // Merged back with the two swapped, then split again.
        let swap = merger(&mut fw, 2, false);
        wire(&mut fw, &split, 0, &swap, 1);
        wire(&mut fw, &split, 1, &swap, 0);
        let split2 = splitter(&mut fw, 2);
        wire(&mut fw, &swap, 0, &split2, 0);
        let swapped_l = tap(&mut fw, &split2, 0);
        let swapped_r = tap(&mut fw, &split2, 1);

        // A stereo connection into one merger input is down-mixed to mono:
        // "speakers" 0.5·(L+R), "discrete" L.
        let speakers = merger(&mut fw, 1, false);
        wire(&mut fw, &src, 0, &speakers, 0);
        let speakers_mix = tap(&mut fw, &speakers, 0);
        let discrete = merger(&mut fw, 1, true);
        wire(&mut fw, &src, 0, &discrete, 0);
        let discrete_mix = tap(&mut fw, &discrete, 0);

        // Two connections into one input sum.
        let summed = merger(&mut fw, 1, false);
        wire(&mut fw, &split, 0, &summed, 0);
        wire(&mut fw, &split, 1, &summed, 0);
        let sum = tap(&mut fw, &summed, 0);

        // A one-output splitter keeps channel 0 of a stereo input (discrete).
        let narrow = splitter(&mut fw, 1);
        wire(&mut fw, &src, 0, &narrow, 0);
        let narrow_0 = tap(&mut fw, &narrow, 0);

        // An unconnected merger input is silence beside a connected one.
        let half = merger(&mut fw, 2, false);
        wire(&mut fw, &split, 1, &half, 1);
        let split3 = splitter(&mut fw, 2);
        wire(&mut fw, &half, 0, &split3, 0);
        let half_0 = tap(&mut fw, &split3, 0);
        let half_1 = tap(&mut fw, &split3, 1);

        fw.start_stream(()).expect("the offline stream starts");
        let blocks = 40;
        render(&mut fw, blocks);
        let total = (blocks * BLOCK) as u64;

        assert_signal(&split_l, total, "splitter output 0", left);
        assert_signal(&split_r, total, "splitter output 1", right);
        assert_signal(&swapped_l, total, "merged back swapped, channel 0", right);
        assert_signal(&swapped_r, total, "merged back swapped, channel 1", left);
        assert_signal(&speakers_mix, total, "speakers down-mix", |t| 0.5 * (left(t) + right(t)));
        assert_signal(&discrete_mix, total, "discrete down-mix", left);
        assert_signal(&sum, total, "two connections into one input", |t| left(t) + right(t));
        assert_signal(&narrow_0, total, "one-output splitter", left);
        assert_signal(&half_0, total, "unconnected merger input", |_| 0.0);
        assert_signal(&half_1, total, "connected merger input", right);
    }

    #[test]
    fn merge_block_mixes_each_pair_and_reports_silence() {
        // The processor's arithmetic on its own, including the silence mask
        // the engine optimises on.
        let (a, b) = ([0.5f32, -0.5, 0.25], [0.25f32, 0.25, -0.75]);
        let z = [0.0f32; 3];
        let inputs: [&[f32]; 4] = [&a, &b, &z, &z];
        let (mut o0, mut o1) = ([9.0f32; 3], [9.0f32; 3]);
        let mut outputs: [&mut [f32]; 2] = [&mut o0, &mut o1];
        let mask = merge_block(&inputs, &mut outputs, 3, |p| p >= 2, false);
        assert_eq!(o0, [0.375, -0.125, -0.25]);
        assert_eq!(o1, [0.0; 3]);
        assert!(!mask.is_channel_silent(0) && mask.is_channel_silent(1));

        let (mut d0, mut d1) = ([9.0f32; 3], [9.0f32; 3]);
        let mut outputs: [&mut [f32]; 2] = [&mut d0, &mut d1];
        merge_block(&inputs, &mut outputs, 3, |p| p >= 2, true);
        assert_eq!(d0, a, "discrete keeps the left port");
    }

    // -----------------------------------------------------------------------
    // AudioBufferSourceNode: start(when, offset, duration) and the loop region
    // -----------------------------------------------------------------------

    /// A mono ramp: frame `i` holds `(i + 1) / 8192` (exact in f32), so every
    /// rendered sample names the buffer frame it came from, and frame 0 is not
    /// silence.
    fn ramp(frames: u64) -> Arc<PlanarBuffer> {
        Arc::new(PlanarBuffer { data: vec![(0..frames).map(ramp_at).collect()], sample_rate: RATE })
    }

    fn ramp_at(frame: u64) -> f32 {
        (frame + 1) as f32 / 8192.0
    }

    fn secs(frames: u64) -> f64 {
        frames as f64 / RATE as f64
    }

    /// A sampler armed exactly as `src_start` arms one, tapped by a monitor.
    fn source(
        fw: &mut Fw,
        buffer: &Arc<PlanarBuffer>,
        ctl: &Arc<SourceCtl>,
        offset: f64,
        duration: Option<f64>,
        speed: f64,
    ) -> ArcGc<MonitorRing> {
        let mut s = SamplerNode { speed, ..SamplerNode::default() };
        let id = fw.add_node(s.clone(), Some(SamplerConfig::default()));
        let baseline = s.clone();
        arm_source(&mut s, Some(buffer), ctl, offset, duration);
        let mut evs: Vec<NodeEventType> = Vec::new();
        s.diff(&baseline, PathBuilder::default(), &mut evs);
        for e in evs {
            fw.queue_event_for(id, e);
        }
        tap(fw, &rec(id, Params::Sampler(Box::new(s))), 0)
    }

    /// Everything `ring` recorded from the start of the stream (`total` < the
    /// monitor window).
    fn recorded(ring: &MonitorRing, total: usize) -> Vec<f32> {
        assert!(total <= MONITOR_WINDOW);
        assert_eq!(ring.cursor.load(Ordering::Acquire), total, "the monitor missed frames");
        let mut got = vec![0.0f32; total];
        ring.read_latest(&mut got);
        got
    }

    #[test]
    fn a_sprite_renders_only_its_slice_of_the_buffer() {
        // The audio-atlas case (@argon/soundfx): `start(0, offset, duration)`
        // on a buffer holding many sounds must play that one slice and stop —
        // not run on through the rest of the atlas.
        let mut fw = Fw::new(FirewheelConfig::default());
        let atlas = ramp(4_000);
        let ctl = Arc::new(SourceCtl::default());
        let ring = source(&mut fw, &atlas, &ctl, secs(1_000), Some(secs(500)), 1.0);
        fw.start_stream(()).expect("the offline stream starts");
        let blocks = 16;
        render(&mut fw, blocks);
        let got = recorded(&ring, blocks * BLOCK);
        for (t, &v) in got.iter().enumerate() {
            let want = if t < 500 { ramp_at(1_000 + t as u64) } else { 0.0 };
            assert!((v - want).abs() < 1e-6, "frame {t}: {v}, want {want} (buffer frame {})", v * 8192.0 - 1.0);
        }
        assert!(ctl.done.load(Ordering::Relaxed), "the walk did not report its end");
    }

    #[test]
    fn a_sprite_at_another_playback_rate_still_stays_inside_its_slice() {
        // `duration` is buffer content, not wall time: at rate 2 the 500-frame
        // slice lasts 250 output frames, and nothing outside it is ever read.
        let mut fw = Fw::new(FirewheelConfig::default());
        let atlas = ramp(4_000);
        let ctl = Arc::new(SourceCtl::default());
        let ring = source(&mut fw, &atlas, &ctl, secs(1_000), Some(secs(500)), 2.0);
        fw.start_stream(()).expect("the offline stream starts");
        let blocks = 16;
        render(&mut fw, blocks);
        let got = recorded(&ring, blocks * BLOCK);
        let (lo, hi) = (ramp_at(1_000), ramp_at(1_499));
        let mut last = 0;
        for (t, &v) in got.iter().enumerate() {
            if v != 0.0 {
                // Linear interpolation may blend toward the silence after the
                // slice on its very last frame, never past the slice's end.
                assert!(v <= hi + 1e-6 && (v >= lo - 1e-6 || t >= 248), "frame {t}: {v} is outside the slice");
                last = t;
            }
        }
        assert!((245..=252).contains(&last), "the slice should last ~250 output frames, last sound at {last}");
    }

    #[test]
    fn a_loop_region_loops_only_itself_after_the_lead_in() {
        // `loop` with `loopStart`/`loopEnd`: playback from `offset` (before the
        // region) runs into it, then wraps loopEnd → loopStart forever.
        let mut fw = Fw::new(FirewheelConfig::default());
        let atlas = ramp(4_000);
        let ctl = Arc::new(SourceCtl::default());
        ctl.looping.store(true, Ordering::Relaxed);
        ctl.loop_start.store(secs(1_000).to_bits(), Ordering::Relaxed);
        ctl.loop_end.store(secs(1_200).to_bits(), Ordering::Relaxed);
        let ring = source(&mut fw, &atlas, &ctl, secs(900), None, 1.0);
        fw.start_stream(()).expect("the offline stream starts");
        let blocks = 24;
        render(&mut fw, blocks);
        let got = recorded(&ring, blocks * BLOCK);
        for (t, &v) in got.iter().enumerate() {
            let t = t as u64;
            let frame = if t < 300 { 900 + t } else { 1_000 + (t - 300) % 200 };
            let want = ramp_at(frame);
            assert!((v - want).abs() < 1e-6, "frame {t}: {v}, want buffer frame {frame}");
        }
        assert!(!ctl.done.load(Ordering::Relaxed), "a loop without a duration never ends by itself");
    }
}

#[cfg(test)]
mod source_walk_tests {
    //! [`SourceSample`] read directly, block by block, as the sampler reads
    //! it: the cases the graph tests do not reach.
    use super::*;

    const RATE: u32 = 48_000;

    /// Frame `i` holds `i + 1`, so silence (0.0) reads back as frame `-1`.
    fn ramp(frames: usize) -> Arc<PlanarBuffer> {
        Arc::new(PlanarBuffer { data: vec![(0..frames).map(|i| (i + 1) as f32).collect()], sample_rate: RATE })
    }

    fn secs(frames: u64) -> f64 {
        frames as f64 / RATE as f64
    }

    /// Reads `frames` frames from sampler frame `v`; returns the buffer
    /// frames they came from, `-1` for silence.
    fn read(view: &SourceSample, v: u64, frames: usize) -> Vec<i64> {
        let mut out = vec![f32::NAN; frames];
        {
            let mut bufs: [&mut [f32]; 1] = [&mut out];
            view.fill_buffers(&mut bufs, 0..frames, v);
        }
        assert!(out.iter().all(|x| !x.is_nan()), "frames left unwritten: {out:?}");
        out.iter().map(|&x| x as i64 - 1).collect()
    }

    fn looping(ctl: &SourceCtl, start: u64, end: u64) {
        ctl.loop_start.store(secs(start).to_bits(), Ordering::Relaxed);
        ctl.loop_end.store(secs(end).to_bits(), Ordering::Relaxed);
        ctl.looping.store(true, Ordering::Relaxed);
    }

    #[test]
    fn a_duration_counts_loop_iterations() {
        let ctl = Arc::new(SourceCtl::default());
        looping(&ctl, 10, 14);
        // From 8, 10 frames of content: 8 9 | 10 11 12 13 | 10 11 12 13.
        let view = SourceSample::new(ramp(100), ctl.clone(), 8, 10);
        assert_eq!(read(&view, 0, 10), [8, 9, 10, 11, 12, 13, 10, 11, 12, 13]);
        assert!(ctl.done.load(Ordering::Relaxed));
    }

    #[test]
    fn turning_loop_off_mid_play_runs_on_to_the_end_of_the_buffer() {
        let ctl = Arc::new(SourceCtl::default());
        looping(&ctl, 2, 4);
        let view = SourceSample::new(ramp(8), ctl.clone(), 0, SourceSample::UNBOUNDED);
        assert_eq!(read(&view, 0, 6), [0, 1, 2, 3, 2, 3]);
        ctl.looping.store(false, Ordering::Relaxed);
        // The playhead is at loopEnd, not yet wrapped (the spec wraps when the
        // next frame is read): with `loop` off it runs on to the end.
        assert_eq!(read(&view, 6, 8), [4, 5, 6, 7, -1, -1, -1, -1]);
        assert!(ctl.done.load(Ordering::Relaxed));
    }

    #[test]
    fn turning_loop_on_mid_play_loops_the_region_from_the_playhead() {
        let ctl = Arc::new(SourceCtl::default());
        let view = SourceSample::new(ramp(8), ctl.clone(), 0, SourceSample::UNBOUNDED);
        assert_eq!(read(&view, 0, 3), [0, 1, 2]);
        looping(&ctl, 2, 5);
        assert_eq!(read(&view, 3, 6), [3, 4, 2, 3, 4, 2]);
        assert!(!ctl.done.load(Ordering::Relaxed));
    }

    #[test]
    fn an_offset_past_loop_end_begins_at_loop_start() {
        let ctl = Arc::new(SourceCtl::default());
        looping(&ctl, 2, 4);
        let view = SourceSample::new(ramp(8), ctl.clone(), 6, SourceSample::UNBOUNDED);
        assert_eq!(read(&view, 0, 5), [2, 3, 2, 3, 2]);
    }

    #[test]
    fn a_region_that_is_not_a_forward_range_loops_the_whole_buffer() {
        // The spec's defaults (0, 0), and loopStart ≥ loopEnd, both mean "all".
        for (a, b) in [(0, 0), (5, 3)] {
            let ctl = Arc::new(SourceCtl::default());
            looping(&ctl, a, b);
            let view = SourceSample::new(ramp(4), ctl.clone(), 1, SourceSample::UNBOUNDED);
            assert_eq!(read(&view, 0, 7), [1, 2, 3, 0, 1, 2, 3], "loopStart {a}, loopEnd {b}");
        }
    }

    #[test]
    fn a_non_sequential_read_rederives_the_position() {
        // The sampler re-reads from 0 after a stop; the view must not carry
        // the old playhead into it.
        let ctl = Arc::new(SourceCtl::default());
        let view = SourceSample::new(ramp(10), ctl, 3, SourceSample::UNBOUNDED);
        assert_eq!(read(&view, 0, 4), [3, 4, 5, 6]);
        assert_eq!(read(&view, 0, 2), [3, 4]);
        assert_eq!(read(&view, 5, 2), [8, 9]);
    }

    #[test]
    fn a_slice_ending_inside_a_block_is_silence_after_it() {
        let ctl = Arc::new(SourceCtl::default());
        let view = SourceSample::new(ramp(6), ctl.clone(), 3, SourceSample::UNBOUNDED);
        assert_eq!(read(&view, 0, 5), [3, 4, 5, -1, -1]);
        assert!(ctl.done.load(Ordering::Relaxed), "past the end without loop is the end");
    }
}

#[cfg(test)]
mod timeline_tests {
    use super::{Timeline, Tl};

    /// `Timeline::eval` is the whole of `AudioParam` automation, and it is pure
    /// arithmetic — so it is testable without a JS context or an audio device.
    fn tl(start: f64, evs: Vec<Tl>) -> Timeline {
        Timeline { evs, start_v: start, start_t: 0.0 }
    }

    #[test]
    fn a_ramp_with_nothing_before_it_starts_when_it_was_scheduled() {
        // Scheduled at t = 100 with the value at 1: a ramp to 0 over 1 s is
        // halfway at 100.5. Measured from t = 0 instead, it was already at 0.005
        // the moment it was scheduled, which is a jump, not a fade.
        let t = Timeline { evs: vec![Tl::Lin { t: 101.0, v: 0.0 }], start_v: 1.0, start_t: 100.0 };
        assert_eq!(t.eval(100.0), (1.0, false));
        assert!((t.eval(100.5).0 - 0.5).abs() < 1e-9);
        assert_eq!(t.eval(101.0), (0.0, true));
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
