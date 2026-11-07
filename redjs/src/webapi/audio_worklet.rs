//! Fixed native audio processors: no JavaScript runtime on the audio thread.
//!
//! These are deliberately named DSP primitives, not a JavaScript AudioWorklet
//! interpreter. The callback performs no allocation, locking, VM calls or message
//! queue growth. Parameters are atomics and telemetry is a single coherent,
//! latest-value mailbox. Intermediate meter samples/gate transitions may coalesce
//! while the UI thread is busy; the newest state is retained.
//!
//! Meter envelopes use exact 128-frame quanta across arbitrary backend callback
//! boundaries. Gates inspect chunks of at most 128 frames inside each callback,
//! with a sample-counted 150 ms hold and no added buffering/latency. A callback
//! boundary inside a quantum can move a gate decision by at most 127 frames
//! compared with the browser's fixed-128 lookahead. Adding a whole quantum of
//! audio delay just to match that boundary would be worse for live voice.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use firewheel::{
    channel_config::{ChannelConfig, ChannelCount},
    collector::ArcGc,
    node::{
        AudioNode, AudioNodeInfo, AudioNodeProcessor, ConstructProcessorContext, ProcBuffers,
        ProcExtra, ProcInfo, ProcessStatus,
    },
};

const QUANTUM: usize = 128;
const MESSAGE_PENDING: u64 = 1 << 63;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeKind {
    PeakMeterStereo,
    PeakMeterMono,
    StereoToMono,
    InputGate,
}

impl NativeKind {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "peak-meter-stereo" => Some(Self::PeakMeterStereo),
            "peak-meter-mono" => Some(Self::PeakMeterMono),
            "stereo-to-mono" => Some(Self::StereoToMono),
            "input-gate" => Some(Self::InputGate),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::PeakMeterStereo => "peak-meter-stereo",
            Self::PeakMeterMono => "peak-meter-mono",
            Self::StereoToMono => "stereo-to-mono",
            Self::InputGate => "input-gate",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NativeMessage {
    Stereo([f32; 2]),
    Mono(f32),
    Gate(bool),
}

/// Shared control/telemetry. AtomicU64 is lock-free on the supported 64-bit
/// desktop targets. A meter's levels are finite and nonnegative, so the second
/// float's unused sign bit stores the mailbox's pending flag. A single swap
/// retrieves a coherent pair; the audio writer never retries or waits.
pub struct NativeShared {
    kind: NativeKind,
    enabled: AtomicU32,
    /// Low word: dB; high word: its linear amplitude. Published together.
    threshold: AtomicU64,
    mailbox: AtomicU64,
}

impl NativeShared {
    pub fn new(kind: NativeKind) -> Self {
        let threshold = -40.0f32;
        Self {
            kind,
            enabled: AtomicU32::new(if kind == NativeKind::StereoToMono {
                1.0f32.to_bits()
            } else {
                0.0f32.to_bits()
            }),
            threshold: AtomicU64::new(
                threshold.to_bits() as u64 | ((0.01f32.to_bits() as u64) << 32),
            ),
            mailbox: AtomicU64::new(0),
        }
    }

    pub fn enabled(&self) -> f32 {
        f32::from_bits(self.enabled.load(Ordering::Relaxed))
    }

    pub fn set_enabled(&self, value: f32) {
        if value.is_finite() {
            self.enabled
                .store(value.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        }
    }

    pub fn threshold(&self) -> f32 {
        f32::from_bits(self.threshold.load(Ordering::Relaxed) as u32)
    }

    pub fn set_threshold(&self, value: f32) {
        if value.is_finite() {
            let db = value.clamp(-100.0, 0.0);
            let linear = 10.0f32.powf(db / 20.0);
            self.threshold.store(
                db.to_bits() as u64 | ((linear.to_bits() as u64) << 32),
                Ordering::Relaxed,
            );
        }
    }

    fn linear_threshold(&self) -> f32 {
        f32::from_bits((self.threshold.load(Ordering::Relaxed) >> 32) as u32)
    }

    fn post_stereo(&self, levels: [f32; 2]) {
        debug_assert!(levels.iter().all(|v| v.is_finite() && *v >= 0.0));
        let payload = levels[0].to_bits() as u64 | ((levels[1].to_bits() as u64) << 32);
        self.mailbox
            .store(payload | MESSAGE_PENDING, Ordering::Release);
    }

    fn post_mono(&self, level: f32) {
        debug_assert!(level.is_finite() && level >= 0.0);
        self.mailbox
            .store(level.to_bits() as u64 | MESSAGE_PENDING, Ordering::Release);
    }

    fn post_gate(&self, open: bool) {
        self.mailbox
            .store(u64::from(open) | MESSAGE_PENDING, Ordering::Release);
    }

    /// UI-thread drain. At most one message, even if audio ran for hours while
    /// the UI was asleep. A concurrent publication is either returned now or
    /// stays pending for the next drain; components cannot tear.
    pub fn take_message(&self) -> Option<NativeMessage> {
        let value = self.mailbox.swap(0, Ordering::AcqRel);
        if value & MESSAGE_PENDING == 0 {
            return None;
        }
        let payload = value & !MESSAGE_PENDING;
        match self.kind {
            NativeKind::PeakMeterStereo => Some(NativeMessage::Stereo([
                f32::from_bits(payload as u32),
                f32::from_bits((payload >> 32) as u32),
            ])),
            NativeKind::PeakMeterMono => Some(NativeMessage::Mono(f32::from_bits(payload as u32))),
            NativeKind::InputGate => Some(NativeMessage::Gate(payload != 0)),
            NativeKind::StereoToMono => None,
        }
    }
}

#[derive(Clone)]
pub struct NativeNode {
    pub kind: NativeKind,
    pub shared: ArcGc<NativeShared>,
}

impl AudioNode for NativeNode {
    type Configuration = firewheel::node::EmptyConfig;

    fn info(&self, _config: &Self::Configuration) -> AudioNodeInfo {
        AudioNodeInfo::new()
            .debug_name(self.kind.name())
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
        NativeProcessor {
            shared: ArcGc::clone(&self.shared),
            dsp: NativeDsp::new(self.kind, cx.stream_info.sample_rate.get()),
        }
    }
}

struct NativeProcessor {
    shared: ArcGc<NativeShared>,
    dsp: NativeDsp,
}

impl AudioNodeProcessor for NativeProcessor {
    fn process(
        &mut self,
        info: &ProcInfo,
        buffers: ProcBuffers,
        _events: &mut firewheel::event::ProcEvents,
        _extra: &mut ProcExtra,
    ) -> ProcessStatus {
        self.dsp.render(
            &self.shared,
            buffers.inputs,
            buffers.outputs,
            info.frames,
            [
                info.in_silence_mask.is_channel_silent(0),
                info.in_silence_mask.is_channel_silent(1),
            ],
        );
        ProcessStatus::OutputsModified
    }

    fn new_stream(
        &mut self,
        info: &firewheel::StreamInfo,
        _context: &mut firewheel::node::ProcStreamCtx,
    ) {
        self.dsp.set_sample_rate(info.sample_rate.get());
    }
}

/// State is a handful of scalars; neither a per-block buffer nor an event
/// queue is needed. Kept separate from Firewheel for deterministic DSP tests.
struct NativeDsp {
    kind: NativeKind,
    sample_rate: u32,
    quantum_frames: usize,
    quantum_peaks: [f64; 2],
    envelope: [f64; 2],
    samples_since_post: f64,
    last_levels: [i32; 2],
    hold_samples: usize,
    hold_counter: usize,
    gate_open: bool,
    last_gate: Option<bool>,
}

impl NativeDsp {
    fn new(kind: NativeKind, sample_rate: u32) -> Self {
        let sample_rate = sample_rate.max(1);
        Self {
            kind,
            sample_rate,
            quantum_frames: 0,
            quantum_peaks: [0.0; 2],
            envelope: [0.0; 2],
            samples_since_post: 0.0,
            last_levels: [-1; 2],
            hold_samples: (sample_rate as f64 * 0.15).round() as usize,
            hold_counter: 0,
            gate_open: false,
            last_gate: None,
        }
    }

    fn set_sample_rate(&mut self, sample_rate: u32) {
        let rate = sample_rate.max(1);
        self.hold_counter =
            (self.hold_counter as f64 * rate as f64 / self.sample_rate as f64).round() as usize;
        self.sample_rate = rate;
        self.hold_samples = (rate as f64 * 0.15).round() as usize;
        self.samples_since_post = 0.0;
        self.quantum_frames = 0;
        self.quantum_peaks = [0.0; 2];
    }

    fn render(
        &mut self,
        shared: &NativeShared,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        frames: usize,
        silent: [bool; 2],
    ) {
        // Always initialize outputs, including missing or silence-marked input
        // channels: Firewheel explicitly permits stale values in its buffers.
        for out in outputs.iter_mut() {
            out[..frames].fill(0.0);
        }
        match self.kind {
            NativeKind::PeakMeterMono | NativeKind::PeakMeterStereo => {
                for i in 0..frames {
                    let left = input_sample(inputs, silent, 0, i).abs() as f64;
                    let right = input_sample(inputs, silent, 1, i).abs() as f64;
                    self.quantum_peaks[0] = self.quantum_peaks[0].max(left);
                    self.quantum_peaks[1] = self.quantum_peaks[1].max(right);
                    self.quantum_frames += 1;
                    if self.quantum_frames == QUANTUM {
                        self.finish_meter_quantum(shared);
                    }
                }
            }
            NativeKind::StereoToMono => {
                let enabled = shared.enabled() >= 0.5;
                for (ch, out) in outputs.iter_mut().enumerate() {
                    let source = if enabled { 0 } else { ch };
                    for (i, sample) in out[..frames].iter_mut().enumerate() {
                        *sample = input_sample(inputs, silent, source, i);
                    }
                }
            }
            NativeKind::InputGate => {
                let enabled = shared.enabled() >= 0.5;
                let threshold = shared.linear_threshold();
                for start in (0..frames).step_by(QUANTUM) {
                    let end = (start + QUANTUM).min(frames);
                    let open = if !enabled {
                        true
                    } else {
                        let mut peak = 0.0f32;
                        for i in start..end {
                            for ch in 0..2 {
                                peak = peak.max(input_sample(inputs, silent, ch, i).abs());
                            }
                        }
                        if peak >= threshold {
                            self.gate_open = true;
                            self.hold_counter = self.hold_samples;
                        } else if self.hold_counter > 0 {
                            self.hold_counter = self.hold_counter.saturating_sub(end - start);
                        } else {
                            self.gate_open = false;
                        }
                        self.gate_open
                    };
                    if open {
                        for (ch, out) in outputs.iter_mut().enumerate() {
                            for i in start..end {
                                out[i] = input_sample(inputs, silent, ch, i);
                            }
                        }
                    }
                    if self.last_gate != Some(open) {
                        self.last_gate = Some(open);
                        shared.post_gate(open);
                    }
                }
            }
        }
    }

    fn finish_meter_quantum(&mut self, shared: &NativeShared) {
        let mono = self.kind == NativeKind::PeakMeterMono;
        let (attack, release, hz) = if mono {
            (0.2, 0.8, 30.0)
        } else {
            (0.15, 0.85, 60.0)
        };
        if mono {
            self.quantum_peaks[0] = self.quantum_peaks[0].max(self.quantum_peaks[1]);
        }
        for ch in 0..if mono { 1 } else { 2 } {
            self.envelope[ch] = self.envelope[ch] * release + self.quantum_peaks[ch] * attack;
        }
        self.quantum_peaks = [0.0; 2];
        self.quantum_frames = 0;
        self.samples_since_post += QUANTUM as f64;
        let samples_per_post = self.sample_rate as f64 / hz;
        if self.samples_since_post < samples_per_post {
            return;
        }
        self.samples_since_post %= samples_per_post;
        let levels = self
            .envelope
            .map(|value| (value.min(1.0) * 100.0).round() as i32);
        if levels[0] == self.last_levels[0] && (mono || levels[1] == self.last_levels[1]) {
            return;
        }
        self.last_levels = levels;
        if mono {
            shared.post_mono(self.envelope[0] as f32);
        } else {
            shared.post_stereo([self.envelope[0] as f32, self.envelope[1] as f32]);
        }
    }
}

fn input_sample(inputs: &[&[f32]], silent: [bool; 2], channel: usize, frame: usize) -> f32 {
    if channel >= 2 || silent[channel] {
        return 0.0;
    }
    let value = inputs
        .get(channel)
        .and_then(|input| input.get(frame))
        .copied()
        .unwrap_or(0.0);
    // Invalid PCM must not poison the persistent envelope/threshold state.
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(
        dsp: &mut NativeDsp,
        shared: &NativeShared,
        left: &[f32],
        right: &[f32],
        silent: [bool; 2],
    ) -> [Vec<f32>; 2] {
        let mut l = vec![1234.0; left.len()];
        let mut r = vec![1234.0; left.len()];
        dsp.render(
            shared,
            &[left, right],
            &mut [&mut l, &mut r],
            left.len(),
            silent,
        );
        [l, r]
    }

    #[test]
    fn names_and_parameter_defaults_are_generic_finite_and_clamped() {
        for kind in [
            NativeKind::PeakMeterStereo,
            NativeKind::PeakMeterMono,
            NativeKind::StereoToMono,
            NativeKind::InputGate,
        ] {
            assert_eq!(NativeKind::from_name(kind.name()), Some(kind));
            let shared = NativeShared::new(kind);
            assert_eq!(
                shared.enabled(),
                if kind == NativeKind::StereoToMono {
                    1.0
                } else {
                    0.0
                }
            );
            assert_eq!(shared.threshold(), -40.0);
            shared.set_threshold(-1000.0);
            assert_eq!(shared.threshold(), -100.0);
            shared.set_threshold(f32::NAN);
            assert_eq!(shared.threshold(), -100.0);
            shared.set_enabled(10.0);
            shared.set_enabled(f32::INFINITY);
            assert_eq!(shared.enabled(), 1.0);
            shared.set_enabled(-10.0);
            assert_eq!(shared.enabled(), 0.0);
        }
        assert_eq!(NativeKind::from_name("vu-meter-processor"), None);
    }

    #[test]
    fn stereo_to_mono_uses_left_not_average_and_disabled_passes_stereo() {
        let kind = NativeKind::StereoToMono;
        let shared = NativeShared::new(kind);
        let mut dsp = NativeDsp::new(kind, 48000);
        let left = [0.25, -0.75, 1.0];
        let right = [-1.0, 0.5, -0.2];
        let output = render(&mut dsp, &shared, &left, &right, [false; 2]);
        assert_eq!(output[0], left);
        assert_eq!(output[1], left);
        shared.set_enabled(0.0);
        let output = render(&mut dsp, &shared, &left, &right, [false; 2]);
        assert_eq!(output[0], left);
        assert_eq!(output[1], right);
        assert_eq!(shared.take_message(), None);
    }

    #[test]
    fn silence_masks_missing_channels_and_invalid_pcm_never_replay_stale_values() {
        let kind = NativeKind::StereoToMono;
        let shared = NativeShared::new(kind);
        shared.set_enabled(0.0);
        let mut dsp = NativeDsp::new(kind, 48000);
        let output = render(&mut dsp, &shared, &[0.5; 3], &[0.9; 3], [false, true]);
        assert_eq!(output[0], [0.5; 3]);
        assert_eq!(output[1], [0.0; 3]);
        let output = render(
            &mut dsp,
            &shared,
            &[f32::NAN, f32::INFINITY, -0.3],
            &[],
            [false; 2],
        );
        assert_eq!(output[0], [0.0, 0.0, -0.3]);
        assert_eq!(output[1], [0.0; 3]);
    }

    #[test]
    fn meter_envelope_rate_and_silent_output_match_128_frame_reference() {
        for kind in [NativeKind::PeakMeterStereo, NativeKind::PeakMeterMono] {
            let shared = NativeShared::new(kind);
            let mut dsp = NativeDsp::new(kind, 48000);
            let mono = kind == NativeKind::PeakMeterMono;
            let (attack, release, first_post) = if mono {
                (0.2f64, 0.8f64, 13)
            } else {
                (0.15, 0.85, 7)
            };
            for quantum in 1..=first_post {
                let out = render(&mut dsp, &shared, &[0.25; 128], &[-0.75; 128], [false; 2]);
                assert!(out.iter().flatten().all(|v| *v == 0.0));
                if quantum < first_post {
                    assert_eq!(shared.take_message(), None);
                }
            }
            let expected_left =
                (if mono { 0.75 } else { 0.25 }) * attack * (1.0 - release.powi(first_post))
                    / (1.0 - release);
            let expected_right = 0.75 * attack * (1.0 - release.powi(first_post)) / (1.0 - release);
            match shared.take_message().unwrap() {
                NativeMessage::Mono(level) => assert!((level as f64 - expected_left).abs() < 1e-7),
                NativeMessage::Stereo([l, r]) => {
                    assert!((l as f64 - expected_left).abs() < 1e-7);
                    assert!((r as f64 - expected_right).abs() < 1e-7);
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }
    }

    #[test]
    fn meter_callback_splits_preserve_all_quanta_and_final_telemetry() {
        for kind in [NativeKind::PeakMeterStereo, NativeKind::PeakMeterMono] {
            let reference = NativeShared::new(kind);
            let split = NativeShared::new(kind);
            let mut a = NativeDsp::new(kind, 44100);
            let mut b = NativeDsp::new(kind, 44100);
            let samples = 128 * 100;
            let l: Vec<f32> = (0..samples)
                .map(|i| ((i * 17) % 101) as f32 / 100.0 - 0.5)
                .collect();
            let r: Vec<f32> = (0..samples)
                .map(|i| ((i * 31) % 103) as f32 / 104.0 - 0.5)
                .collect();
            render(&mut a, &reference, &l, &r, [false; 2]);
            let pattern = [1, 127, 37, 480, 63, 2048, 7];
            let mut offset = 0;
            for block in pattern.iter().cycle() {
                if offset == samples {
                    break;
                }
                let end = (offset + block).min(samples);
                render(&mut b, &split, &l[offset..end], &r[offset..end], [false; 2]);
                offset = end;
            }
            assert_eq!(a.envelope, b.envelope);
            assert_eq!(a.last_levels, b.last_levels);
            assert_eq!(reference.take_message(), split.take_message());
        }
    }

    #[test]
    fn meters_publish_baseline_then_suppress_unchanged_integer_levels() {
        for kind in [NativeKind::PeakMeterStereo, NativeKind::PeakMeterMono] {
            let shared = NativeShared::new(kind);
            let mut dsp = NativeDsp::new(kind, 48000);
            render(&mut dsp, &shared, &[0.0; 4096], &[0.0; 4096], [false; 2]);
            assert!(shared.take_message().is_some());
            render(&mut dsp, &shared, &[1.0; 4096], &[1.0; 4096], [true; 2]);
            assert_eq!(shared.take_message(), None);
        }
    }

    #[test]
    fn mailbox_is_latest_coherent_value_and_does_not_grow() {
        let shared = std::sync::Arc::new(NativeShared::new(NativeKind::PeakMeterStereo));
        let writer = std::sync::Arc::clone(&shared);
        let thread = std::thread::spawn(move || {
            for i in 1..=100000 {
                let value = i as f32;
                writer.post_stereo([value, value * 2.0]);
            }
        });
        while !thread.is_finished() {
            if let Some(NativeMessage::Stereo([left, right])) = shared.take_message() {
                assert_eq!(right, left * 2.0);
            }
        }
        thread.join().unwrap();
        if let Some(NativeMessage::Stereo([left, right])) = shared.take_message() {
            assert_eq!(right, left * 2.0);
        }
        assert_eq!(shared.take_message(), None);
        shared.post_stereo([3.0, 9.0]);
        shared.post_stereo([7.0, 11.0]);
        assert_eq!(
            shared.take_message(),
            Some(NativeMessage::Stereo([7.0, 11.0]))
        );
    }

    #[test]
    fn gate_threshold_stereo_hold_and_transition_messages_match_reference() {
        let kind = NativeKind::InputGate;
        let shared = NativeShared::new(kind);
        shared.set_enabled(1.0);
        shared.set_threshold(-20.0);
        let mut dsp = NativeDsp::new(kind, 48000);
        let quiet = [0.01; 128];
        let loud = [-0.25; 128];
        let out = render(&mut dsp, &shared, &quiet, &quiet, [false; 2]);
        assert!(out.iter().flatten().all(|v| *v == 0.0));
        assert_eq!(shared.take_message(), Some(NativeMessage::Gate(false)));
        render(&mut dsp, &shared, &quiet, &quiet, [false; 2]);
        assert_eq!(shared.take_message(), None);
        let out = render(&mut dsp, &shared, &quiet, &loud, [false; 2]);
        assert_eq!(out[0], quiet);
        assert_eq!(out[1], loud);
        assert_eq!(shared.take_message(), Some(NativeMessage::Gate(true)));
        // ceil(7200/128) quiet quanta stay open, just as the browser's pre-decrement test.
        for _ in 0..57 {
            let out = render(&mut dsp, &shared, &quiet, &quiet, [false; 2]);
            assert_eq!(out[0], quiet);
        }
        assert_eq!(shared.take_message(), None);
        let out = render(&mut dsp, &shared, &quiet, &quiet, [false; 2]);
        assert!(out.iter().flatten().all(|v| *v == 0.0));
        assert_eq!(shared.take_message(), Some(NativeMessage::Gate(false)));
        shared.set_enabled(0.0);
        let out = render(&mut dsp, &shared, &quiet, &loud, [false; 2]);
        assert_eq!(out[0], quiet);
        assert_eq!(out[1], loud);
        assert_eq!(shared.take_message(), Some(NativeMessage::Gate(true)));
    }

    #[test]
    fn gate_hold_counts_samples_across_small_callbacks_and_new_sample_rates() {
        let kind = NativeKind::InputGate;
        let shared = NativeShared::new(kind);
        shared.set_enabled(1.0);
        let mut dsp = NativeDsp::new(kind, 48000);
        render(&mut dsp, &shared, &[0.5], &[0.5], [false; 2]);
        for _ in 0..7200 {
            let out = render(&mut dsp, &shared, &[0.001], &[0.001], [false; 2]);
            assert_eq!(out[0], [0.001]);
        }
        let out = render(&mut dsp, &shared, &[0.001], &[0.001], [false; 2]);
        assert_eq!(out[0], [0.0]);
        render(&mut dsp, &shared, &[0.5], &[0.5], [false; 2]);
        dsp.set_sample_rate(96000);
        assert_eq!(dsp.hold_samples, 14400);
        assert_eq!(dsp.hold_counter, 14400);
    }
}
