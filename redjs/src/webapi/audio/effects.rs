//! `DelayNode`, `WaveShaperNode`, `DynamicsCompressorNode` and `ConvolverNode`: the DSP and the
//! Firewheel nodes that run it. Their parameters are atomics the pump writes, as the
//! oscillator's are (`webapi/osc.rs`).

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use firewheel::channel_config::{ChannelConfig, ChannelCount};
use firewheel::collector::ArcGc;
use firewheel::event::ProcEvents;
use firewheel::node::{
    AudioNode, AudioNodeInfo, AudioNodeProcessor, ConstructProcessorContext, EmptyConfig,
    ProcBuffers, ProcExtra, ProcInfo, ProcStreamCtx, ProcessStatus,
};
use firewheel::StreamInfo;

use super::super::osc::Latest;
use super::fft::Fft;
use super::PlanarBuffer;

pub(super) struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }

    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
}

fn channels(n: u32) -> ChannelCount {
    ChannelCount::new(n).unwrap_or(ChannelCount::STEREO)
}

// ---------------------------------------------------------------------------
// DelayNode
// ---------------------------------------------------------------------------
//
// Two Firewheel nodes around one delay line: a sink that writes the input and a source that
// reads the output. Firewheel schedules an acyclic graph, and with the node split a Web Audio
// cycle through a delay is not a cycle in it. Outside a cycle an edge from the writer to the
// reader makes the writer run first, and any delay down to zero is exact. Inside one the reader
// can run before the writer has this block, so the delay is at least a block.

/// The longest block the graph renders (`output::GRAPH_BLOCK_FRAMES`): the shortest delay of a
/// `DelayNode` in a cycle.
pub(super) const CYCLE_DELAY_FRAMES: usize = 1024;

const DELAY_SMOOTH_SECONDS: f64 = 0.02;

struct DelayLine {
    ch: Box<[Box<[f32]>]>,
    mask: usize,
}

pub(super) struct DelayShared {
    seconds: AtomicF32,
    pub max_seconds: f64,
    /// Frames written so far, and the clock of the block written last.
    written: AtomicU64,
    stamp: AtomicI64,
    line: UnsafeCell<DelayLine>,
}

// SAFETY: `line` is touched only on the thread rendering the stream, by the node's two processors
// one after the other.
unsafe impl Sync for DelayShared {}

impl DelayShared {
    pub fn new(width: u32, max_seconds: f64, seconds: f32, rate: u32) -> Self {
        let frames =
            (max_seconds * rate.max(1) as f64).ceil() as usize + 2 * CYCLE_DELAY_FRAMES + 2;
        let len = frames.next_power_of_two();
        Self {
            seconds: AtomicF32::new(seconds),
            max_seconds,
            written: AtomicU64::new(0),
            stamp: AtomicI64::new(i64::MIN),
            line: UnsafeCell::new(DelayLine {
                ch: (0..width.max(1))
                    .map(|_| vec![0.0f32; len].into_boxed_slice())
                    .collect(),
                mask: len - 1,
            }),
        }
    }

    pub fn seconds(&self) -> f32 {
        self.seconds.get()
    }

    pub fn set_seconds(&self, v: f32) {
        self.seconds.set(v.clamp(0.0, self.max_seconds as f32));
    }
}

#[derive(Clone)]
pub(super) struct DelayIn {
    pub shared: ArcGc<DelayShared>,
    pub channels: u32,
}

impl AudioNode for DelayIn {
    type Configuration = EmptyConfig;

    fn info(&self, _config: &Self::Configuration) -> AudioNodeInfo {
        AudioNodeInfo::new()
            .debug_name("webaudio_delay_in")
            .channel_config(ChannelConfig {
                num_inputs: channels(self.channels),
                num_outputs: ChannelCount::MONO,
            })
    }

    fn construct_processor(
        &self,
        _config: &Self::Configuration,
        _cx: ConstructProcessorContext,
    ) -> impl AudioNodeProcessor {
        DelayWriter {
            shared: ArcGc::clone(&self.shared),
        }
    }
}

struct DelayWriter {
    shared: ArcGc<DelayShared>,
}

impl AudioNodeProcessor for DelayWriter {
    fn process(
        &mut self,
        info: &ProcInfo,
        buffers: ProcBuffers,
        _events: &mut ProcEvents,
        _extra: &mut ProcExtra,
    ) -> ProcessStatus {
        // SAFETY: this is the rendering thread (see `DelayShared`).
        let line = unsafe { &mut *self.shared.line.get() };
        let at = self.shared.written.load(Ordering::Relaxed);
        let mask = line.mask;
        for (c, ring) in line.ch.iter_mut().enumerate() {
            let src = buffers
                .inputs
                .get(c)
                .filter(|_| !info.in_silence_mask.is_channel_silent(c));
            for i in 0..info.frames {
                ring[(at as usize).wrapping_add(i) & mask] = src.map_or(0.0, |s| s[i]);
            }
        }
        self.shared
            .written
            .store(at + info.frames as u64, Ordering::Relaxed);
        self.shared
            .stamp
            .store(info.clock_samples.0, Ordering::Relaxed);
        ProcessStatus::ClearAllOutputs
    }
}

#[derive(Clone)]
pub(super) struct DelayOut {
    pub shared: ArcGc<DelayShared>,
    pub channels: u32,
}

impl AudioNode for DelayOut {
    type Configuration = EmptyConfig;

    fn info(&self, _config: &Self::Configuration) -> AudioNodeInfo {
        AudioNodeInfo::new()
            .debug_name("webaudio_delay_out")
            .channel_config(ChannelConfig {
                num_inputs: ChannelCount::MONO,
                num_outputs: channels(self.channels),
            })
    }

    fn construct_processor(
        &self,
        _config: &Self::Configuration,
        _cx: ConstructProcessorContext,
    ) -> impl AudioNodeProcessor {
        DelayReader {
            shared: ArcGc::clone(&self.shared),
            frames: None,
        }
    }
}

struct DelayReader {
    shared: ArcGc<DelayShared>,
    /// The smoothed delay in frames; `None` before the first block.
    frames: Option<f64>,
}

impl AudioNodeProcessor for DelayReader {
    fn process(
        &mut self,
        info: &ProcInfo,
        buffers: ProcBuffers,
        _events: &mut ProcEvents,
        _extra: &mut ProcExtra,
    ) -> ProcessStatus {
        // SAFETY: this is the rendering thread (see `DelayShared`).
        let line = unsafe { &*self.shared.line.get() };
        let n = info.frames;
        let rate = info.sample_rate.get() as f64;
        let written = self.shared.written.load(Ordering::Relaxed);
        let ordered = self.shared.stamp.load(Ordering::Relaxed) == info.clock_samples.0;
        let (base, min) = if ordered {
            (written.wrapping_sub(n as u64), 0.0)
        } else {
            (written, CYCLE_DELAY_FRAMES as f64)
        };
        let max = (line.mask + 1 - CYCLE_DELAY_FRAMES - 2) as f64;
        let target = (self.shared.seconds() as f64 * rate).clamp(min, max);
        let coeff = (-1.0 / (DELAY_SMOOTH_SECONDS * rate)).exp();
        let start = self.frames.unwrap_or(target);
        let mut cur = start;
        for (c, out) in buffers.outputs.iter_mut().enumerate() {
            let Some(ring) = line.ch.get(c) else {
                out[..n].fill(0.0);
                continue;
            };
            cur = start;
            for (i, o) in out[..n].iter_mut().enumerate() {
                cur = target + (cur - target) * coeff;
                if (cur - target).abs() < 1e-6 {
                    cur = target;
                }
                let d = cur.clamp(min, max);
                let whole = d.floor();
                let frac = (d - whole) as f32;
                let at = (base.wrapping_add(i as u64) as usize).wrapping_sub(whole as usize);
                let a = ring[at & line.mask];
                let b = ring[at.wrapping_sub(1) & line.mask];
                *o = a + (b - a) * frac;
            }
        }
        self.frames = Some(cur);
        ProcessStatus::OutputsModified
    }

    fn new_stream(&mut self, _stream_info: &StreamInfo, _context: &mut ProcStreamCtx) {
        self.frames = None;
    }
}

// ---------------------------------------------------------------------------
// WaveShaperNode
// ---------------------------------------------------------------------------

pub(super) const OVERSAMPLE: &[&str] = &["none", "2x", "4x"];

pub(super) struct ShaperShared {
    pub curve: Latest<Vec<f32>>,
    /// Index into [`OVERSAMPLE`].
    pub oversample: AtomicU32,
}

impl Default for ShaperShared {
    fn default() -> Self {
        Self {
            curve: Latest::new(None),
            oversample: AtomicU32::new(0),
        }
    }
}

/// The spec's curve lookup: `x` in [-1, 1] across the curve, linear between its points.
fn shape(curve: &[f32], x: f32) -> f32 {
    let last = curve.len() - 1;
    let v = 0.5 * last as f32 * (x + 1.0);
    if !(v > 0.0) {
        curve[0]
    } else if v >= last as f32 {
        curve[last]
    } else {
        let k = v as usize;
        let f = v - k as f32;
        curve[k] + (curve[k + 1] - curve[k]) * f
    }
}

/// Half of the 47 taps of a half-band low-pass: the even ones. The odd ones are zero but for
/// the centre, which is 0.5.
const HB: usize = 24;
const HB_CENTRE: usize = 11;

fn halfband() -> [f32; HB] {
    let mut taps = [0.0f32; HB];
    for (j, t) in taps.iter_mut().enumerate() {
        let n = 2.0 * j as f64;
        let x = core::f64::consts::PI * (n - 23.0) / 2.0;
        let w = core::f64::consts::TAU * n / 46.0;
        let window = 0.42 - 0.5 * w.cos() + 0.08 * (2.0 * w).cos();
        *t = (0.5 * x.sin() / x * window) as f32;
    }
    let sum: f32 = taps.iter().sum();
    for t in &mut taps {
        *t *= 0.5 / sum;
    }
    taps
}

/// The last `HB` samples, newest first, as one slice.
#[derive(Clone)]
struct History {
    buf: [f32; 2 * HB],
    pos: usize,
}

impl History {
    fn new() -> Self {
        Self {
            buf: [0.0; 2 * HB],
            pos: 0,
        }
    }

    fn push(&mut self, x: f32) {
        self.pos = if self.pos == 0 { HB - 1 } else { self.pos - 1 };
        self.buf[self.pos] = x;
        self.buf[self.pos + HB] = x;
    }

    fn dot(&self, taps: &[f32; HB]) -> f32 {
        self.buf[self.pos..self.pos + HB]
            .iter()
            .zip(taps)
            .map(|(a, b)| a * b)
            .sum()
    }

    fn centre(&self) -> f32 {
        self.buf[self.pos + HB_CENTRE]
    }
}

/// One doubling of the sample rate and the halving back.
#[derive(Clone)]
struct Stage {
    up: History,
    down_odd: History,
    down_even: History,
}

impl Stage {
    fn new() -> Self {
        Self {
            up: History::new(),
            down_odd: History::new(),
            down_even: History::new(),
        }
    }

    fn up(&mut self, taps: &[f32; HB], x: f32) -> [f32; 2] {
        self.up.push(x);
        [2.0 * self.up.dot(taps), self.up.centre()]
    }

    fn down(&mut self, taps: &[f32; HB], pair: [f32; 2]) -> f32 {
        self.down_even.push(pair[0]);
        self.down_odd.push(pair[1]);
        self.down_odd.dot(taps) + 0.5 * self.down_even.centre()
    }
}

pub(super) struct ShaperDsp {
    taps: [f32; HB],
    stages: Vec<[Stage; 2]>,
}

impl ShaperDsp {
    pub fn new(width: usize) -> Self {
        Self {
            taps: halfband(),
            stages: vec![[Stage::new(), Stage::new()]; width],
        }
    }

    /// Shape channel `c`. `factor` is 1, 2 or 4.
    pub fn process(
        &mut self,
        c: usize,
        curve: &[f32],
        factor: u32,
        input: &[f32],
        out: &mut [f32],
    ) {
        let taps = &self.taps;
        let [outer, inner] = &mut self.stages[c];
        for (o, &x) in out.iter_mut().zip(input) {
            *o = match factor {
                2 => {
                    let u = outer.up(taps, x);
                    outer.down(taps, u.map(|v| shape(curve, v)))
                }
                4 => {
                    let u = outer.up(taps, x).map(|v| {
                        let w = inner.up(taps, v);
                        inner.down(taps, w.map(|v| shape(curve, v)))
                    });
                    outer.down(taps, u)
                }
                _ => shape(curve, x),
            };
        }
    }
}

#[derive(Clone)]
pub(super) struct ShaperNode {
    pub shared: ArcGc<ShaperShared>,
    pub channels: u32,
}

impl AudioNode for ShaperNode {
    type Configuration = EmptyConfig;

    fn info(&self, _config: &Self::Configuration) -> AudioNodeInfo {
        let n = channels(self.channels);
        AudioNodeInfo::new()
            .debug_name("webaudio_waveshaper")
            .channel_config(ChannelConfig {
                num_inputs: n,
                num_outputs: n,
            })
    }

    fn construct_processor(
        &self,
        _config: &Self::Configuration,
        _cx: ConstructProcessorContext,
    ) -> impl AudioNodeProcessor {
        ShaperProcessor {
            curve: self.shared.curve.get(),
            shared: ArcGc::clone(&self.shared),
            dsp: ShaperDsp::new(self.channels as usize),
        }
    }
}

struct ShaperProcessor {
    shared: ArcGc<ShaperShared>,
    curve: Option<ArcGc<Vec<f32>>>,
    dsp: ShaperDsp,
}

impl AudioNodeProcessor for ShaperProcessor {
    fn process(
        &mut self,
        info: &ProcInfo,
        buffers: ProcBuffers,
        _events: &mut ProcEvents,
        _extra: &mut ProcExtra,
    ) -> ProcessStatus {
        self.shared.curve.follow(&mut self.curve);
        let Some(curve) = self.curve.as_ref().filter(|c| c.len() >= 2) else {
            return ProcessStatus::Bypass;
        };
        let factor = 1 << self.shared.oversample.load(Ordering::Relaxed).min(2);
        let n = info.frames;
        for (c, (out, input)) in buffers.outputs.iter_mut().zip(buffers.inputs).enumerate() {
            self.dsp
                .process(c, curve, factor, &input[..n], &mut out[..n]);
        }
        ProcessStatus::OutputsModified
    }
}

// ---------------------------------------------------------------------------
// DynamicsCompressorNode
// ---------------------------------------------------------------------------

/// The node's fixed look-ahead, as in the spec.
const COMPRESSOR_DELAY_SECONDS: f32 = 0.006;

pub(super) struct CompShared {
    pub threshold: AtomicF32,
    pub knee: AtomicF32,
    pub ratio: AtomicF32,
    pub attack: AtomicF32,
    pub release: AtomicF32,
    /// The gain reduction applied now, in dB (0 or negative).
    pub reduction: AtomicF32,
}

impl Default for CompShared {
    fn default() -> Self {
        Self {
            threshold: AtomicF32::new(-24.0),
            knee: AtomicF32::new(30.0),
            ratio: AtomicF32::new(12.0),
            attack: AtomicF32::new(0.003),
            release: AtomicF32::new(0.25),
            reduction: AtomicF32::new(0.0),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct CompSettings {
    pub threshold: f32,
    pub knee: f32,
    pub ratio: f32,
    pub attack: f32,
    pub release: f32,
}

impl CompSettings {
    /// The output level for an input level, both in dB: unchanged below the threshold, a
    /// quadratic knee above it, then one dB out for every `ratio` dB in.
    fn curve(&self, x: f32) -> f32 {
        let over = x - self.threshold;
        let slope = 1.0 / self.ratio.max(1.0) - 1.0;
        if over <= 0.0 {
            x
        } else if over < self.knee {
            x + slope * over * over / (2.0 * self.knee)
        } else {
            x + slope * (over - 0.5 * self.knee)
        }
    }

    /// The spec's make-up gain, in dB: the gain at full scale, to the power 0.6, undone.
    fn makeup(&self) -> f32 {
        -0.6 * self.curve(0.0)
    }
}

pub(super) struct CompDsp {
    rate: f32,
    /// The smoothed gain reduction, dB.
    gain: f32,
    delay: Vec<Vec<f32>>,
    at: usize,
}

impl CompDsp {
    pub fn new(rate: u32, width: usize) -> Self {
        let len = ((COMPRESSOR_DELAY_SECONDS * rate as f32) as usize).max(1);
        Self {
            rate: rate.max(1) as f32,
            gain: 0.0,
            delay: vec![vec![0.0; len]; width],
            at: 0,
        }
    }

    /// Compress `frames` frames; the channels share one gain. Returns the reduction in dB.
    pub fn process(
        &mut self,
        s: CompSettings,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        frames: usize,
    ) -> f32 {
        let pole = |seconds: f32| {
            if seconds > 0.0 {
                (-1.0 / (seconds * self.rate)).exp()
            } else {
                0.0
            }
        };
        let (attack, release) = (pole(s.attack), pole(s.release));
        let makeup = s.makeup();
        let len = self.delay[0].len();
        for i in 0..frames {
            let peak = inputs.iter().fold(0.0f32, |m, ch| m.max(ch[i].abs()));
            let level = 20.0 * peak.max(1e-6).log10();
            let want = s.curve(level) - level;
            let k = if want < self.gain { attack } else { release };
            self.gain = want + (self.gain - want) * k;
            let g = 10.0f32.powf((self.gain + makeup) / 20.0);
            for ((line, input), out) in self.delay.iter_mut().zip(inputs).zip(outputs.iter_mut()) {
                out[i] = line[self.at] * g;
                line[self.at] = input[i];
            }
            self.at = (self.at + 1) % len;
        }
        self.gain
    }
}

#[derive(Clone)]
pub(super) struct CompNode {
    pub shared: ArcGc<CompShared>,
}

impl AudioNode for CompNode {
    type Configuration = EmptyConfig;

    fn info(&self, _config: &Self::Configuration) -> AudioNodeInfo {
        AudioNodeInfo::new()
            .debug_name("webaudio_compressor")
            .channel_config(ChannelConfig {
                num_inputs: ChannelCount::STEREO,
                num_outputs: ChannelCount::STEREO,
            })
    }

    fn construct_processor(
        &self,
        _config: &Self::Configuration,
        cx: ConstructProcessorContext,
    ) -> impl AudioNodeProcessor {
        CompProcessor {
            shared: ArcGc::clone(&self.shared),
            dsp: CompDsp::new(cx.stream_info.sample_rate.get(), 2),
        }
    }
}

struct CompProcessor {
    shared: ArcGc<CompShared>,
    dsp: CompDsp,
}

impl AudioNodeProcessor for CompProcessor {
    fn process(
        &mut self,
        info: &ProcInfo,
        buffers: ProcBuffers,
        _events: &mut ProcEvents,
        _extra: &mut ProcExtra,
    ) -> ProcessStatus {
        let sh = &self.shared;
        let settings = CompSettings {
            threshold: sh.threshold.get(),
            knee: sh.knee.get(),
            ratio: sh.ratio.get(),
            attack: sh.attack.get(),
            release: sh.release.get(),
        };
        let reduction = self
            .dsp
            .process(settings, buffers.inputs, buffers.outputs, info.frames);
        sh.reduction.set(reduction);
        ProcessStatus::OutputsModified
    }

    fn new_stream(&mut self, stream_info: &StreamInfo, _context: &mut ProcStreamCtx) {
        self.dsp = CompDsp::new(stream_info.sample_rate.get(), 2);
    }
}

// ---------------------------------------------------------------------------
// ConvolverNode
// ---------------------------------------------------------------------------
//
// The first `HEAD` taps of the impulse response are convolved directly, so the node adds no
// latency. The rest goes through FFTs in uniform partitions: taps 128..1024 in partitions of
// 128, the taps after those in partitions of 1024. A partition's output is due one partition
// after its input block ends, which is when it is computed.

const HEAD: usize = 128;
/// (partition size, where the next tier takes over).
const TIERS: [(usize, usize); 2] = [(128, 1024), (1024, usize::MAX)];

struct TierKernel {
    p: usize,
    fft: Arc<Fft>,
    /// Per partition, the `p + 1` bins of its spectrum.
    re: Vec<f32>,
    im: Vec<f32>,
}

impl TierKernel {
    fn parts(&self) -> usize {
        self.re.len() / (self.p + 1)
    }
}

struct Kernel {
    head: Vec<f32>,
    tiers: Vec<TierKernel>,
}

impl Kernel {
    fn new(ir: &[f32], scale: f32, ffts: &[Arc<Fft>; 2]) -> Self {
        let head = ir[..ir.len().min(HEAD)].iter().map(|v| v * scale).collect();
        let mut tiers = Vec::new();
        for (&(p, end), fft) in TIERS.iter().zip(ffts) {
            if ir.len() <= p {
                break;
            }
            let seg = &ir[p..ir.len().min(end)];
            let bins = p + 1;
            let (mut re, mut im) = (Vec::new(), Vec::new());
            for part in seg.chunks(p) {
                let mut r = vec![0.0f32; 2 * p];
                let mut i = vec![0.0f32; 2 * p];
                for (d, s) in r.iter_mut().zip(part) {
                    *d = s * scale;
                }
                fft.forward(&mut r, &mut i);
                re.extend_from_slice(&r[..bins]);
                im.extend_from_slice(&i[..bins]);
            }
            tiers.push(TierKernel {
                p,
                fft: Arc::clone(fft),
                re,
                im,
            });
        }
        Self { head, tiers }
    }
}

struct TierState {
    /// The previous input block, then the one being filled.
    input: Vec<f32>,
    /// The spectra of the last blocks, one per partition of the kernel, as a ring.
    fdl_re: Vec<f32>,
    fdl_im: Vec<f32>,
    newest: usize,
    /// What this tier adds to the block being rendered.
    out: Vec<f32>,
    pos: usize,
    re: Vec<f32>,
    im: Vec<f32>,
}

impl TierState {
    fn new(k: &TierKernel) -> Self {
        let size = k.parts() * (k.p + 1);
        Self {
            input: vec![0.0; 2 * k.p],
            fdl_re: vec![0.0; size],
            fdl_im: vec![0.0; size],
            newest: 0,
            out: vec![0.0; k.p],
            pos: 0,
            re: vec![0.0; 2 * k.p],
            im: vec![0.0; 2 * k.p],
        }
    }

    fn block(&mut self, k: &TierKernel) {
        let (p, bins, parts) = (k.p, k.p + 1, k.parts());
        self.re.copy_from_slice(&self.input);
        self.im.fill(0.0);
        k.fft.forward(&mut self.re, &mut self.im);
        self.newest = (self.newest + 1) % parts;
        let slot = self.newest * bins;
        self.fdl_re[slot..slot + bins].copy_from_slice(&self.re[..bins]);
        self.fdl_im[slot..slot + bins].copy_from_slice(&self.im[..bins]);

        self.re[..bins].fill(0.0);
        self.im[..bins].fill(0.0);
        for part in 0..parts {
            let x = ((self.newest + parts - part) % parts) * bins;
            let h = part * bins;
            let (xr, xi) = (&self.fdl_re[x..x + bins], &self.fdl_im[x..x + bins]);
            let (hr, hi) = (&k.re[h..h + bins], &k.im[h..h + bins]);
            for b in 0..bins {
                self.re[b] += xr[b] * hr[b] - xi[b] * hi[b];
                self.im[b] += xr[b] * hi[b] + xi[b] * hr[b];
            }
        }
        for b in 1..p {
            self.re[2 * p - b] = self.re[b];
            self.im[2 * p - b] = -self.im[b];
        }
        k.fft.inverse(&mut self.re, &mut self.im);
        self.out.copy_from_slice(&self.re[p..]);
        self.input.copy_within(p.., 0);
    }
}

/// One input channel through one channel of the impulse response.
struct Path {
    input: usize,
    output: usize,
    kernel: Arc<Kernel>,
    /// The last `HEAD` inputs, newest first, written twice so they are always one slice.
    hist: Vec<f32>,
    at: usize,
    tiers: Vec<TierState>,
}

impl Path {
    fn new(input: usize, output: usize, kernel: &Arc<Kernel>) -> Self {
        Self {
            input,
            output,
            kernel: Arc::clone(kernel),
            hist: vec![0.0; 2 * HEAD],
            at: 0,
            tiers: kernel.tiers.iter().map(TierState::new).collect(),
        }
    }

    fn step(&mut self, x: f32) -> f32 {
        self.at = if self.at == 0 { HEAD - 1 } else { self.at - 1 };
        self.hist[self.at] = x;
        self.hist[self.at + HEAD] = x;
        let head = &self.kernel.head;
        let mut y: f32 = head
            .iter()
            .zip(&self.hist[self.at..self.at + head.len()])
            .map(|(h, x)| h * x)
            .sum();
        for (t, k) in self.tiers.iter_mut().zip(&self.kernel.tiers) {
            y += t.out[t.pos];
            t.input[k.p + t.pos] = x;
            t.pos += 1;
            if t.pos == k.p {
                t.block(k);
                t.pos = 0;
            }
        }
        y
    }
}

/// A convolver for one impulse response: mono, stereo, or four channels ("true stereo": left
/// to left and right, then right to left and right).
pub(super) struct ConvState {
    paths: Vec<Path>,
    ir_channels: usize,
    /// Frames of silent input after which the output is silent too.
    tail: usize,
}

impl ConvState {
    pub fn new(buf: &PlanarBuffer, normalize: bool) -> Option<Box<Self>> {
        let n = buf.data.len();
        let len = buf.data.first().map_or(0, |c| c.len());
        if len == 0 || !matches!(n, 1 | 2 | 4) {
            return None;
        }
        let scale = if normalize { normalization(buf) } else { 1.0 };
        let ffts = TIERS.map(|(p, _)| Arc::new(Fft::new(2 * p)));
        let kernels: Vec<Arc<Kernel>> = buf
            .data
            .iter()
            .map(|c| Arc::new(Kernel::new(c, scale, &ffts)))
            .collect();
        let routes: &[(usize, usize, usize)] = match n {
            1 => &[(0, 0, 0), (1, 0, 1)],
            2 => &[(0, 0, 0), (1, 1, 1)],
            _ => &[(0, 0, 0), (0, 1, 1), (1, 2, 0), (1, 3, 1)],
        };
        Some(Box::new(Self {
            paths: routes
                .iter()
                .map(|&(input, k, output)| Path::new(input, output, &kernels[k]))
                .collect(),
            ir_channels: n,
            tail: len + 2 * TIERS[1].0,
        }))
    }

    /// `mono` plays the left input through both sides of the response.
    pub fn process(&mut self, inputs: [&[f32]; 2], outputs: [&mut [f32]; 2], mono: bool) {
        let [left, right] = outputs;
        let live = if mono && self.ir_channels == 1 {
            1
        } else {
            self.paths.len()
        };
        for i in 0..left.len() {
            let x = [inputs[0][i], if mono { inputs[0][i] } else { inputs[1][i] }];
            let mut y = [0.0f32; 2];
            for path in &mut self.paths[..live] {
                y[path.output] += path.step(x[path.input]);
            }
            left[i] = y[0];
            right[i] = y[1];
        }
    }
}

/// The spec's normalization scale: the response's RMS power brought to a fixed level.
fn normalization(buf: &PlanarBuffer) -> f32 {
    const GAIN_CALIBRATION: f32 = 0.00125;
    const GAIN_CALIBRATION_RATE: f32 = 44_100.0;
    const MIN_POWER: f32 = 0.000125;
    let frames = buf.data.first().map_or(0, |c| c.len());
    let sum: f64 = buf
        .data
        .iter()
        .flat_map(|c| c.iter())
        .map(|&v| v as f64 * v as f64)
        .sum();
    let mut power = (sum / (buf.data.len() * frames).max(1) as f64).sqrt() as f32;
    if !power.is_finite() || power < MIN_POWER {
        power = MIN_POWER;
    }
    let mut scale = GAIN_CALIBRATION / power;
    if buf.sample_rate != 0 {
        scale *= GAIN_CALIBRATION_RATE / buf.sample_rate as f32;
    }
    if buf.data.len() == 4 {
        scale *= 0.5;
    }
    scale
}

pub(super) struct ConvShared {
    /// `buffer` and `normalize` as the page set them.
    spec: Mutex<(Option<Arc<PlanarBuffer>>, bool)>,
    /// The state built for the last `buffer`, until the processor takes it; then the state it
    /// replaced, until the page's thread drops it.
    pending: Mutex<Option<Box<ConvState>>>,
    fresh: AtomicBool,
    /// The input's computed channel count is 1.
    pub mono: AtomicBool,
}

impl Default for ConvShared {
    fn default() -> Self {
        Self {
            spec: Mutex::new((None, true)),
            pending: Mutex::new(None),
            fresh: AtomicBool::new(false),
            mono: AtomicBool::new(true),
        }
    }
}

impl ConvShared {
    pub fn normalize(&self) -> bool {
        self.spec.lock().unwrap().1
    }

    /// Takes effect at the next `set_buffer`, as in the spec.
    pub fn set_normalize(&self, on: bool) {
        self.spec.lock().unwrap().1 = on;
    }

    pub fn set_buffer(&self, buf: Option<Arc<PlanarBuffer>>) {
        let normalize = {
            let mut spec = self.spec.lock().unwrap();
            spec.0 = buf.clone();
            spec.1
        };
        let state = buf.and_then(|b| ConvState::new(&b, normalize));
        *self.pending.lock().unwrap() = state;
        self.fresh.store(true, Ordering::Release);
    }
}

#[derive(Clone)]
pub(super) struct ConvNode {
    pub shared: ArcGc<ConvShared>,
}

impl AudioNode for ConvNode {
    type Configuration = EmptyConfig;

    fn info(&self, _config: &Self::Configuration) -> AudioNodeInfo {
        AudioNodeInfo::new()
            .debug_name("webaudio_convolver")
            .channel_config(ChannelConfig {
                num_inputs: ChannelCount::STEREO,
                num_outputs: ChannelCount::STEREO,
            })
    }

    fn construct_processor(
        &self,
        _config: &Self::Configuration,
        _cx: ConstructProcessorContext,
    ) -> impl AudioNodeProcessor {
        let (buf, normalize) = self.shared.spec.lock().unwrap().clone();
        ConvProcessor {
            shared: ArcGc::clone(&self.shared),
            state: buf.and_then(|b| ConvState::new(&b, normalize)),
            quiet: 0,
        }
    }
}

struct ConvProcessor {
    shared: ArcGc<ConvShared>,
    state: Option<Box<ConvState>>,
    /// Frames of silent input in a row.
    quiet: usize,
}

impl AudioNodeProcessor for ConvProcessor {
    fn process(
        &mut self,
        info: &ProcInfo,
        buffers: ProcBuffers,
        _events: &mut ProcEvents,
        _extra: &mut ProcExtra,
    ) -> ProcessStatus {
        if self.shared.fresh.swap(false, Ordering::AcqRel) {
            match self.shared.pending.try_lock() {
                Ok(mut next) => {
                    core::mem::swap(&mut self.state, &mut *next);
                    self.quiet = 0;
                }
                Err(_) => self.shared.fresh.store(true, Ordering::Release),
            }
        }
        let Some(state) = self.state.as_mut() else {
            return ProcessStatus::ClearAllOutputs;
        };
        let n = info.frames;
        let mono = self.shared.mono.load(Ordering::Relaxed);
        let silent = info.in_silence_mask.is_channel_silent(0)
            && (mono || info.in_silence_mask.is_channel_silent(1));
        self.quiet = if silent {
            self.quiet.saturating_add(n)
        } else {
            0
        };
        if self.quiet > state.tail + n {
            return ProcessStatus::ClearAllOutputs;
        }
        let [left, right, ..] = buffers.outputs else {
            return ProcessStatus::ClearAllOutputs;
        };
        state.process(
            [&buffers.inputs[0][..n], &buffers.inputs[1][..n]],
            [&mut left[..n], &mut right[..n]],
            mono,
        );
        ProcessStatus::OutputsModified
    }
}

// ---------------------------------------------------------------------------
// IIRFilterNode
// ---------------------------------------------------------------------------

/// The longest coefficient list an `IIRFilterNode` takes.
pub(super) const IIR_MAX: usize = 20;
const IIR_HISTORY: usize = 32;

/// The coefficients, divided through by the first feedback one. They never change.
pub(super) struct IirShared {
    pub feedforward: Vec<f64>,
    pub feedback: Vec<f64>,
}

impl IirShared {
    pub fn new(feedforward: &[f64], feedback: &[f64]) -> Self {
        let a0 = feedback[0];
        Self {
            feedforward: feedforward.iter().map(|b| b / a0).collect(),
            feedback: feedback.iter().map(|a| a / a0).collect(),
        }
    }

    /// The magnitude and phase of the response at `omega` radians a sample.
    pub fn response(&self, omega: f64) -> (f32, f32) {
        let sum = |coefficients: &[f64]| {
            coefficients
                .iter()
                .enumerate()
                .fold((0.0, 0.0), |(re, im), (k, c)| {
                    let angle = -omega * k as f64;
                    (re + c * angle.cos(), im + c * angle.sin())
                })
        };
        let (nr, ni) = sum(&self.feedforward);
        let (dr, di) = sum(&self.feedback);
        let d = dr * dr + di * di;
        let (re, im) = ((nr * dr + ni * di) / d, (ni * dr - nr * di) / d);
        (re.hypot(im) as f32, im.atan2(re) as f32)
    }
}

/// One channel's past input and output, newest at `at`.
#[derive(Clone, Copy)]
pub(super) struct IirState {
    x: [f64; IIR_HISTORY],
    y: [f64; IIR_HISTORY],
    at: usize,
}

impl Default for IirState {
    fn default() -> Self {
        Self {
            x: [0.0; IIR_HISTORY],
            y: [0.0; IIR_HISTORY],
            at: 0,
        }
    }
}

impl IirState {
    pub fn process(&mut self, sh: &IirShared, input: &[f32], out: &mut [f32]) {
        const MASK: usize = IIR_HISTORY - 1;
        for (o, &i) in out.iter_mut().zip(input) {
            self.at = (self.at + 1) & MASK;
            self.x[self.at] = i as f64;
            let mut y = 0.0;
            for (k, b) in sh.feedforward.iter().enumerate() {
                y += b * self.x[(self.at + IIR_HISTORY - k) & MASK];
            }
            for (k, a) in sh.feedback.iter().enumerate().skip(1) {
                y -= a * self.y[(self.at + IIR_HISTORY - k) & MASK];
            }
            // A filter the page made unstable must not carry infinities into the mix.
            if !y.is_finite() {
                y = 0.0;
            }
            self.y[self.at] = y;
            *o = y as f32;
        }
    }
}

#[derive(Clone)]
pub(super) struct IirNode {
    pub shared: ArcGc<IirShared>,
}

impl AudioNode for IirNode {
    type Configuration = EmptyConfig;

    fn info(&self, _config: &Self::Configuration) -> AudioNodeInfo {
        AudioNodeInfo::new()
            .debug_name("webaudio_iir")
            .channel_config(ChannelConfig {
                num_inputs: ChannelCount::STEREO,
                num_outputs: ChannelCount::STEREO,
            })
    }

    fn construct_processor(
        &self,
        _config: &Self::Configuration,
        _cx: ConstructProcessorContext,
    ) -> impl AudioNodeProcessor {
        IirProcessor {
            shared: ArcGc::clone(&self.shared),
            state: [IirState::default(); 2],
        }
    }
}

struct IirProcessor {
    shared: ArcGc<IirShared>,
    state: [IirState; 2],
}

impl AudioNodeProcessor for IirProcessor {
    fn process(
        &mut self,
        info: &ProcInfo,
        buffers: ProcBuffers,
        _events: &mut ProcEvents,
        _extra: &mut ProcExtra,
    ) -> ProcessStatus {
        let n = info.frames;
        for ((out, input), state) in buffers
            .outputs
            .iter_mut()
            .zip(buffers.inputs)
            .zip(&mut self.state)
        {
            state.process(&self.shared, &input[..n], &mut out[..n]);
        }
        ProcessStatus::OutputsModified
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(v: &[f32]) -> f32 {
        (v.iter().map(|s| s * s).sum::<f32>() / v.len() as f32).sqrt()
    }

    fn sine(freq: f32, amp: f32, rate: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (core::f32::consts::TAU * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    fn noise(n: usize, mut seed: u32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / (1 << 23) as f32 - 1.0
            })
            .collect()
    }

    fn mono_ir(ir: Vec<f32>) -> PlanarBuffer {
        PlanarBuffer {
            data: vec![ir],
            sample_rate: 48_000,
        }
    }

    fn convolve(state: &mut ConvState, left: &[f32], right: &[f32], mono: bool) -> [Vec<f32>; 2] {
        let mut out = [vec![0.0f32; left.len()], vec![0.0f32; left.len()]];
        let [l, r] = &mut out;
        // Uneven blocks, as a device asks for them.
        let mut at = 0;
        for size in [1usize, 127, 128, 500, 1024, 77].iter().cycle() {
            let end = (at + size).min(left.len());
            state.process(
                [&left[at..end], &right[at..end]],
                [&mut l[at..end], &mut r[at..end]],
                mono,
            );
            at = end;
            if at == left.len() {
                break;
            }
        }
        out
    }

    #[test]
    fn a_unit_impulse_response_is_the_identity() {
        let x = noise(4000, 1);
        let mut state = ConvState::new(&mono_ir(vec![1.0]), false).unwrap();
        let [l, _] = convolve(&mut state, &x, &x, true);
        assert_eq!(l, x);
    }

    #[test]
    fn a_long_response_matches_direct_convolution_across_the_partition_tiers() {
        let ir: Vec<f32> = noise(3000, 7)
            .iter()
            .enumerate()
            .map(|(i, v)| v * (-(i as f32) / 800.0).exp())
            .collect();
        let x = noise(6000, 3);
        let mut state = ConvState::new(&mono_ir(ir.clone()), false).unwrap();
        let [got, _] = convolve(&mut state, &x, &x, true);
        for (n, g) in got.iter().enumerate() {
            let want: f32 = ir
                .iter()
                .take(n + 1)
                .enumerate()
                .map(|(k, h)| h * x[n - k])
                .sum();
            assert!((g - want).abs() < 2e-3, "sample {n}: {g} for {want}");
        }
    }

    #[test]
    fn a_stereo_response_keeps_its_sides_and_spreads_a_mono_input() {
        let mut left = vec![0.0f32; 300];
        let mut right = vec![0.0f32; 300];
        left[0] = 1.0;
        right[200] = 0.5;
        let buf = PlanarBuffer {
            data: vec![left, right],
            sample_rate: 48_000,
        };
        let mut x = vec![0.0f32; 600];
        x[10] = 1.0;
        let silent = vec![0.0f32; 600];

        let mut state = ConvState::new(&buf, false).unwrap();
        let [l, r] = convolve(&mut state, &x, &silent, true);
        assert!((l[10] - 1.0).abs() < 1e-4 && (r[210] - 0.5).abs() < 1e-4);

        let mut state = ConvState::new(&buf, false).unwrap();
        let [l, r] = convolve(&mut state, &silent, &x, false);
        assert!(
            rms(&l) < 1e-5,
            "the right input leaked into the left output"
        );
        assert!((r[210] - 0.5).abs() < 1e-4);
    }

    #[test]
    fn a_two_second_response_renders_faster_than_real_time() {
        let ir = noise(96_000, 5);
        let buf = PlanarBuffer {
            data: vec![ir.clone(), ir],
            sample_rate: 48_000,
        };
        let mut state = ConvState::new(&buf, true).unwrap();
        let x = noise(48_000, 9);
        let started = std::time::Instant::now();
        let out = convolve(&mut state, &x, &x, false);
        let took = started.elapsed();
        assert!(rms(&out[0]) > 0.0);
        // A debug build is several times slower than the release build this must hold for.
        if !cfg!(debug_assertions) {
            assert!(took.as_secs_f32() < 0.5, "one second took {took:?}");
        }
    }

    fn compress(input: &[f32]) -> (Vec<f32>, f32) {
        let s = CompSettings {
            threshold: -24.0,
            knee: 30.0,
            ratio: 12.0,
            attack: 0.003,
            release: 0.25,
        };
        let mut dsp = CompDsp::new(48_000, 1);
        let mut out = vec![0.0f32; input.len()];
        let reduction = dsp.process(s, &[input], &mut [&mut out], input.len());
        (out, reduction)
    }

    #[test]
    fn a_compressor_reduces_a_loud_sine_and_leaves_a_quiet_one() {
        let n = 48_000;
        let quiet = sine(440.0, 0.01, 48_000, n);
        let loud = sine(440.0, 1.0, 48_000, n);
        let (q, q_red) = compress(&quiet);
        let (l, l_red) = compress(&loud);
        let half = n / 2;
        let q_gain = rms(&q[half..]) / rms(&quiet[half..]);
        let l_gain = rms(&l[half..]) / rms(&loud[half..]);
        assert!(
            q_red.abs() < 0.01,
            "a -40 dB sine was reduced by {q_red} dB"
        );
        assert!(l_red < -6.0, "a full-scale sine was reduced by {l_red} dB");
        assert!(
            l_gain < 0.6 * q_gain,
            "loud gain {l_gain}, quiet gain {q_gain}"
        );
        assert!(
            (1.0..2.5).contains(&q_gain),
            "below the threshold only the make-up gain applies: {q_gain}"
        );
    }

    fn mag_at(x: &[f32], f: f32, rate: u32) -> f32 {
        let w = core::f32::consts::TAU * f / rate as f32;
        let (mut re, mut im) = (0.0f32, 0.0f32);
        for (i, s) in x.iter().enumerate() {
            let win = 0.5 - 0.5 * (core::f32::consts::TAU * i as f32 / x.len() as f32).cos();
            re += s * win * (w * i as f32).cos();
            im -= s * win * (w * i as f32).sin();
        }
        (re * re + im * im).sqrt() / x.len() as f32
    }

    #[test]
    fn a_curve_is_looked_up_across_minus_one_to_one() {
        let curve = [-0.5f32, 0.0, 1.0];
        for (x, want) in [
            (-2.0, -0.5),
            (-1.0, -0.5),
            (-0.5, -0.25),
            (0.0, 0.0),
            (0.5, 0.5),
            (1.0, 1.0),
            (3.0, 1.0),
        ] {
            assert!((shape(&curve, x) - want).abs() < 1e-6, "f({x})");
        }
    }

    #[test]
    fn oversampling_keeps_the_signal_and_folds_less_of_the_clipping_back() {
        // A 5 kHz sine clipped hard: its 9th harmonic (45 kHz) folds to 3 kHz at 48 kHz.
        let clip = [-1.0f32, -1.0, -1.0, 1.0, 1.0, 1.0];
        let x = sine(5000.0, 0.9, 48_000, 8192);
        let render = |factor: u32| {
            let mut dsp = ShaperDsp::new(1);
            let mut out = vec![0.0f32; x.len()];
            dsp.process(0, &clip, factor, &x, &mut out);
            out[1024..].to_vec()
        };
        let plain = render(1);
        let alias = mag_at(&plain, 3000.0, 48_000);
        assert!(alias > 1e-3, "the test's premise: {alias}");
        for factor in [2, 4] {
            let out = render(factor);
            assert!(
                mag_at(&out, 5000.0, 48_000) > 0.8 * mag_at(&plain, 5000.0, 48_000),
                "{factor}x lost the fundamental"
            );
            assert!(
                mag_at(&out, 3000.0, 48_000) < alias,
                "{factor}x aliases more than no oversampling"
            );
        }
        assert!(mag_at(&render(4), 3000.0, 48_000) < 0.1 * alias);
    }

    #[test]
    fn an_iir_filter_runs_its_difference_equation() {
        // y[n] = x[n] + 0.5 y[n-1], fed an impulse.
        let shared = IirShared::new(&[2.0], &[2.0, -1.0]);
        let mut state = IirState::default();
        let mut out = [0.0f32; 4];
        state.process(&shared, &[1.0, 0.0, 0.0, 0.0], &mut out);
        assert_eq!(out, [1.0, 0.5, 0.25, 0.125]);
    }
}
