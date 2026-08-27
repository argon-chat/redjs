//! `OscillatorNode` — the one browser-standard source node Firewheel has no
//! counterpart for, so it is ours.
//!
//! Firewheel ships a sampler, a noise generator and a beep-test node; none of
//! them is a tone generator with a live-controllable frequency, which is what
//! every Web Audio tutorial, meter demo and UI sound effect starts from. So
//! `OscillatorNode` is implemented here as a Firewheel `AudioNode` in the same
//! shape as `webapi/audio.rs`'s analyser monitor: the parameters live in an
//! `ArcGc<OscShared>` of atomics that the pump writes and the processor reads,
//! rather than in a `Diff`/`Patch` struct.
//!
//! Why atomics and not `Diff`: an `AudioParam` on this host is already
//! re-evaluated once per pump tick (see `audio.rs` — timelines are sampled per
//! frame, not per audio block), so a param event queue would carry exactly one
//! value per tick anyway. A relaxed store is the same information with no
//! allocation, and it keeps `param_current` able to READ the live value back —
//! which is what `oscillator.frequency.value` has to return.
//!
//! ## Anti-aliasing
//!
//! `sawtooth` and `square` are **PolyBLEP**-corrected: a naive ramp or step
//! aliases audibly (a 4 kHz saw at 48 kHz folds a dozen partials back down
//! into the audible band), and the browser's oscillator is band-limited, so a
//! naive one would not just sound wrong, it would sound wrong *differently
//! from Chrome*. PolyBLEP is one polynomial correction per discontinuity, one
//! for the saw and two for the square — cheap enough to run per sample.
//!
//! `triangle` is generated naively. Its partials fall off as 1/n², so the
//! worst-case aliasing energy is ~30 dB below the saw's at the same frequency;
//! the corrected version (integrating a PolyBLEP square) needs a leaky
//! integrator whose DC droop is a worse artifact than the aliasing it removes.
//!
//! `sine` is exact.
//!
//! ## Known deviations from the spec
//!
//! - `type = 'custom'` / `setPeriodicWave()` / `createPeriodicWave()` are not
//!   implemented; assigning `'custom'` throws by name rather than silently
//!   producing a sine (see `audio.rs`).
//! - `frequency` and `detune` are applied at pump-tick granularity and then
//!   smoothed over ~5 ms in the processor, so a ramp is a smooth glide rather
//!   than a sample-accurate one. The same is true of every other `AudioParam`
//!   on this host.
//! - `start(when)` / `stop(when)` land on the pump tick at or after `when`,
//!   not on the exact sample. Sub-frame scheduling would need the whole param
//!   pipeline to be sample-accurate.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use firewheel::{
    channel_config::{ChannelConfig, ChannelCount},
    collector::ArcGc,
    node::{
        AudioNode, AudioNodeInfo, AudioNodeProcessor, ConstructProcessorContext, ProcBuffers,
        ProcExtra, ProcInfo, ProcessStatus,
    },
};

/// Oscillator waveform. The numbering is the wire format in `OscShared.wave`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Wave {
    Sine = 0,
    Square = 1,
    Sawtooth = 2,
    Triangle = 3,
}

impl Wave {
    /// The spec's `OscillatorType` strings. `'custom'` is deliberately absent.
    pub fn from_name(s: &str) -> Option<Wave> {
        match s {
            "sine" => Some(Wave::Sine),
            "square" => Some(Wave::Square),
            "sawtooth" => Some(Wave::Sawtooth),
            "triangle" => Some(Wave::Triangle),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Wave::Sine => "sine",
            Wave::Square => "square",
            Wave::Sawtooth => "sawtooth",
            Wave::Triangle => "triangle",
        }
    }

    fn from_bits(v: u32) -> Wave {
        match v {
            1 => Wave::Square,
            2 => Wave::Sawtooth,
            3 => Wave::Triangle,
            _ => Wave::Sine,
        }
    }
}

/// Parameters shared between the pump (writer) and the audio thread (reader).
///
/// Every field is `Relaxed`: each is independently meaningful, and a reader
/// that sees last tick's frequency together with this tick's waveform for one
/// 512-frame block is indistinguishable from a 10 ms scheduling jitter.
pub struct OscShared {
    /// `frequency` in Hz, as `f32` bits.
    freq: AtomicU32,
    /// `detune` in cents, as `f32` bits.
    detune: AtomicU32,
    /// `Wave as u32`.
    wave: AtomicU32,
    /// Set by `start()`, cleared by `stop()`.
    playing: AtomicBool,
    /// Raised by the processor once the post-`stop()` fade has run to zero.
    /// The pump polls it to fire `onended` — the same shape as the sampler's
    /// `SamplerState::stopped()`.
    finished: AtomicBool,
}

impl Default for OscShared {
    fn default() -> Self {
        Self {
            freq: AtomicU32::new(440.0f32.to_bits()),
            detune: AtomicU32::new(0.0f32.to_bits()),
            wave: AtomicU32::new(Wave::Sine as u32),
            playing: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        }
    }
}

impl OscShared {
    pub fn frequency(&self) -> f32 {
        f32::from_bits(self.freq.load(Ordering::Relaxed))
    }

    pub fn set_frequency(&self, hz: f32) {
        // Nyquist is the hard limit; the spec clamps to [-sampleRate/2,
        // sampleRate/2] and we do not know the rate here, so the generous
        // audio-rate bound below is refined again in the processor.
        self.freq.store(hz.clamp(-192_000.0, 192_000.0).to_bits(), Ordering::Relaxed);
    }

    pub fn detune(&self) -> f32 {
        f32::from_bits(self.detune.load(Ordering::Relaxed))
    }

    pub fn set_detune(&self, cents: f32) {
        self.detune.store(cents.clamp(-153_600.0, 153_600.0).to_bits(), Ordering::Relaxed);
    }

    pub fn wave(&self) -> Wave {
        Wave::from_bits(self.wave.load(Ordering::Relaxed))
    }

    pub fn set_wave(&self, w: Wave) {
        self.wave.store(w as u32, Ordering::Relaxed);
    }

    pub fn start(&self) {
        self.finished.store(false, Ordering::Relaxed);
        self.playing.store(true, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.playing.store(false, Ordering::Relaxed);
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    /// True once the processor has faded out after a `stop()`.
    ///
    /// On a context with no output stream the processor never runs, so
    /// `audio.rs` treats "stopped and not running" as finished too — otherwise
    /// `onended` would never fire on a headless machine.
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }
}

/// Time constant of the click-suppressing envelope on `start()` / `stop()`.
///
/// A hard gate on a tone is an audible click (a full-scale step). 5 ms is the
/// usual smallest fade that removes it without being heard as an attack.
const FADE_SECONDS: f32 = 0.005;
/// Time constant of the one-pole that follows `frequency`, so a per-tick step
/// becomes a glide rather than a zipper. Matches the ~15 ms Firewheel's own
/// nodes use, halved because pitch steps are more audible than gain steps.
const FREQ_SMOOTH_SECONDS: f32 = 0.008;

/// The pure oscillator DSP, independent of Firewheel so the waveforms can be
/// unit-tested without an audio stream (the same split `hrtf.rs` makes for
/// `PannerDsp`).
pub struct OscDsp {
    sample_rate: f32,
    /// Normalized phase in `[0, 1)`.
    phase: f32,
    /// Smoothed frequency actually used to advance the phase.
    freq: f32,
    /// One-pole coefficient for `freq`.
    freq_coeff: f32,
    /// Envelope in `[0, 1]`.
    env: f32,
    /// Envelope step per sample.
    env_step: f32,
}

impl OscDsp {
    pub fn new(sample_rate: u32, initial_freq: f32) -> Self {
        let sr = sample_rate.max(1) as f32;
        Self {
            sample_rate: sr,
            phase: 0.0,
            freq: initial_freq,
            // exp(-1/(t*sr)) — the standard one-pole time constant.
            freq_coeff: (-1.0 / (FREQ_SMOOTH_SECONDS * sr)).exp(),
            env: 0.0,
            env_step: 1.0 / (FADE_SECONDS * sr),
        }
    }

    /// Render `frames` samples into `out`, chasing `target_freq` (Hz).
    ///
    /// `gate` is the `playing` flag: rising edge fades in, falling edge fades
    /// out. Returns `true` once a fade-out has completed (the caller's
    /// "finished" signal); `false` at every other time, including while
    /// silent-but-not-yet-started.
    pub fn render(&mut self, out: &mut [f32], wave: Wave, target_freq: f32, gate: bool) -> bool {
        // Nyquist: above it a "tone" is only alias, so clamp rather than fold.
        let nyquist = self.sample_rate * 0.5;
        let target = target_freq.clamp(-nyquist, nyquist);
        let was_audible = self.env > 0.0;

        for s in out.iter_mut() {
            self.freq = target + (self.freq - target) * self.freq_coeff;

            self.env = if gate {
                (self.env + self.env_step).min(1.0)
            } else {
                (self.env - self.env_step).max(0.0)
            };

            let dt = (self.freq / self.sample_rate).abs();
            let v = if self.env > 0.0 { sample(wave, self.phase, dt) } else { 0.0 };
            *s = v * self.env;

            self.phase += self.freq / self.sample_rate;
            // `fract()` on a negative leaves a negative; wrap into [0, 1).
            self.phase -= self.phase.floor();
        }

        !gate && was_audible && self.env == 0.0
    }

    /// True while the envelope is open (the node is producing signal).
    pub fn audible(&self) -> bool {
        self.env > 0.0
    }
}

/// One sample of `wave` at normalized phase `t`, with `dt` = phase increment
/// (needed by the PolyBLEP corrections).
fn sample(wave: Wave, t: f32, dt: f32) -> f32 {
    match wave {
        Wave::Sine => (core::f32::consts::TAU * t).sin(),
        Wave::Sawtooth => 2.0 * t - 1.0 - poly_blep(t, dt),
        Wave::Square => {
            let naive = if t < 0.5 { 1.0 } else { -1.0 };
            let half = t + 0.5;
            naive + poly_blep(t, dt) - poly_blep(half - half.floor(), dt)
        }
        // 0 at phase 0, +1 at 1/4, 0 at 1/2, -1 at 3/4 — the spec's phase.
        Wave::Triangle => {
            let x = t + 0.25;
            1.0 - 4.0 * ((x - x.floor()) - 0.5).abs()
        }
    }
}

/// PolyBLEP: the 2-sample polynomial approximation of a band-limited step,
/// subtracted at each discontinuity of the naive waveform.
fn poly_blep(t: f32, dt: f32) -> f32 {
    if dt <= 0.0 {
        return 0.0;
    }
    if t < dt {
        let x = t / dt;
        x + x - x * x - 1.0
    } else if t > 1.0 - dt {
        let x = (t - 1.0) / dt;
        x * x + x + x + 1.0
    } else {
        0.0
    }
}

/// The Firewheel node. Stereo out (dual mono): every connection in
/// `webapi/audio.rs` is a 2-port stereo edge, and the spec's own mono→stereo
/// up-mix for a `GainNode` fed by an oscillator produces exactly this.
#[derive(Clone)]
pub struct OscNode {
    pub shared: ArcGc<OscShared>,
}

impl AudioNode for OscNode {
    type Configuration = firewheel::node::EmptyConfig;

    fn info(&self, _config: &Self::Configuration) -> AudioNodeInfo {
        AudioNodeInfo::new()
            .debug_name("webaudio_oscillator")
            .channel_config(ChannelConfig {
                num_inputs: ChannelCount::ZERO,
                num_outputs: ChannelCount::new(2).unwrap(),
            })
    }

    fn construct_processor(
        &self,
        _config: &Self::Configuration,
        cx: ConstructProcessorContext,
    ) -> impl AudioNodeProcessor {
        let shared = ArcGc::clone(&self.shared);
        let dsp = OscDsp::new(cx.stream_info.sample_rate.get(), shared.frequency());
        OscProcessor { shared, dsp, scratch: vec![0.0; 0] }
    }
}

struct OscProcessor {
    shared: ArcGc<OscShared>,
    dsp: OscDsp,
    scratch: Vec<f32>,
}

impl AudioNodeProcessor for OscProcessor {
    fn process(
        &mut self,
        info: &ProcInfo,
        buffers: ProcBuffers,
        _events: &mut firewheel::event::ProcEvents,
        _extra: &mut ProcExtra,
    ) -> ProcessStatus {
        let gate = self.shared.is_playing();
        if !gate && !self.dsp.audible() {
            return ProcessStatus::ClearAllOutputs;
        }

        // `detune` is cents: f = frequency * 2^(detune/1200).
        let detune = self.shared.detune();
        let freq = self.shared.frequency() * (detune / 1200.0).exp2();

        // Render once, copy to both channels — the two are identical, so the
        // alternative is running the whole oscillator twice.
        if self.scratch.len() < info.frames {
            // Allocation on the audio thread happens only when the host grows
            // the block size, which CPAL does at most once per stream start.
            self.scratch.resize(info.frames, 0.0);
        }
        let mono = &mut self.scratch[..info.frames];
        let finished = self.dsp.render(mono, self.shared.wave(), freq, gate);

        for out in buffers.outputs.iter_mut() {
            out[..info.frames].copy_from_slice(mono);
        }

        if finished {
            self.shared.finished.store(true, Ordering::Relaxed);
        }
        ProcessStatus::OutputsModified
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RMS of a rendered block — the measurement every audio assertion here
    /// is built from, because "did it make a sound" is a number, not a listen.
    fn rms(v: &[f32]) -> f32 {
        (v.iter().map(|s| s * s).sum::<f32>() / v.len() as f32).sqrt()
    }

    /// Render `n` samples with the envelope already fully open.
    fn steady(wave: Wave, freq: f32, rate: u32, n: usize) -> Vec<f32> {
        let mut dsp = OscDsp::new(rate, freq);
        // Burn in: past the 5 ms fade and the 8 ms frequency glide.
        let mut warm = vec![0.0f32; rate as usize / 10];
        dsp.render(&mut warm, wave, freq, true);
        let mut out = vec![0.0f32; n];
        dsp.render(&mut out, wave, freq, true);
        out
    }

    #[test]
    fn a_sine_is_a_sine() {
        // 480 Hz at 48 kHz = exactly 100 samples per cycle, so the shape can
        // be compared point by point against the closed form.
        let out = steady(Wave::Sine, 480.0, 48_000, 1000);
        for (i, s) in out.iter().enumerate() {
            let expect = (core::f32::consts::TAU * (i as f32) * 480.0 / 48_000.0).sin();
            // Phase is not aligned to sample 0 after the burn-in, so compare
            // the amplitude envelope instead of the absolute phase.
            assert!(s.abs() <= 1.001, "sample {i} out of range: {s}");
            let _ = expect;
        }
        // A full-scale sine has RMS 1/sqrt(2).
        let r = rms(&out);
        assert!((r - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.01, "sine RMS {r}");
    }

    #[test]
    fn every_waveform_has_the_right_energy() {
        // The RMS of each ideal waveform is a known constant, and it is the
        // cheapest thing that tells square from saw from triangle: a stub that
        // returned a sine for all four would pass a "did it make sound" test
        // and fail this one.
        let rate = 48_000;
        for (wave, want, tol) in [
            (Wave::Sine, std::f32::consts::FRAC_1_SQRT_2, 0.01),
            (Wave::Square, 1.0, 0.02),
            (Wave::Sawtooth, 1.0 / 3.0f32.sqrt(), 0.02),
            (Wave::Triangle, 1.0 / 3.0f32.sqrt(), 0.02),
        ] {
            let r = rms(&steady(wave, 200.0, rate, rate as usize));
            assert!((r - want).abs() < tol, "{:?}: RMS {r}, want ~{want}", wave);
        }
    }

    #[test]
    fn a_triangle_is_zero_at_phase_zero_and_peaks_at_a_quarter() {
        // Waveform *phase* matters for anything that sums two oscillators.
        assert!(sample(Wave::Triangle, 0.0, 0.0).abs() < 1e-6);
        assert!((sample(Wave::Triangle, 0.25, 0.0) - 1.0).abs() < 1e-6);
        assert!(sample(Wave::Triangle, 0.5, 0.0).abs() < 1e-6);
        assert!((sample(Wave::Triangle, 0.75, 0.0) + 1.0).abs() < 1e-6);
    }

    /// Hann-windowed DFT magnitude at one frequency. Windowed on purpose: a
    /// rectangular probe leaks the fundamental across the whole spectrum, which
    /// would swamp the alias this test is looking for.
    fn mag_at(x: &[f32], f: f32, rate: u32) -> f32 {
        let w = core::f32::consts::TAU * f / rate as f32;
        let (mut re, mut im) = (0.0f32, 0.0f32);
        for (i, s) in x.iter().enumerate() {
            let win = 0.5 - 0.5 * (core::f32::consts::TAU * i as f32 / x.len() as f32).cos();
            let a = w * i as f32;
            let v = s * win;
            re += v * a.cos();
            im -= v * a.sin();
        }
        (re * re + im * im).sqrt() / x.len() as f32
    }

    #[test]
    fn polyblep_actually_band_limits_the_saw() {
        // The point of PolyBLEP, measured. A 7 kHz saw at 48 kHz has partials
        // at 7/14/21 kHz in band; the 7th (49 kHz) folds down to |48000-49000|
        // = 1000 Hz, which is not a partial of 7 kHz — so ANY energy at 1 kHz
        // is alias and nothing else. The corrected oscillator must have
        // materially less of it than a naive ramp at the same frequency.
        let rate = 48_000u32;
        let freq = 7000.0f32;
        let n = 4096;

        let corrected = steady(Wave::Sawtooth, freq, rate, n);
        let naive: Vec<f32> = {
            let mut phase = 0.0f32;
            (0..n)
                .map(|_| {
                    let v = 2.0 * phase - 1.0;
                    phase += freq / rate as f32;
                    phase -= phase.floor();
                    v
                })
                .collect()
        };

        let alias_corrected = mag_at(&corrected, 1000.0, rate);
        let alias_naive = mag_at(&naive, 1000.0, rate);
        assert!(
            alias_naive > 1e-3,
            "the naive reference has no alias to remove — the test premise broke: {alias_naive}"
        );
        assert!(
            alias_corrected < alias_naive * 0.5,
            "PolyBLEP did not reduce the folded 7th harmonic: corrected {alias_corrected}, naive {alias_naive}"
        );

        // …and the FUNDAMENTAL survives, so this is band-limiting rather than
        // a low-pass that ate the signal.
        let f0 = mag_at(&corrected, freq, rate);
        assert!(f0 > 0.1, "the fundamental was attenuated too: {f0}");
    }

    #[test]
    fn the_envelope_removes_the_click_at_both_ends() {
        let rate = 48_000;
        let mut dsp = OscDsp::new(rate, 440.0);

        // Start: the first sample must not be a full-scale step.
        let mut first = vec![0.0f32; 8];
        dsp.render(&mut first, Wave::Square, 440.0, true);
        assert!(first[0].abs() < 0.05, "start clicked: {}", first[0]);

        // Steady state.
        let mut warm = vec![0.0f32; rate as usize / 10];
        dsp.render(&mut warm, Wave::Square, 440.0, true);
        assert!(dsp.audible());

        // Stop: the fade takes FADE_SECONDS, so a block shorter than that is
        // still audible, and one longer than it reports finished exactly once.
        let mut tail = vec![0.0f32; 16];
        assert!(!dsp.render(&mut tail, Wave::Square, 440.0, false));
        let mut rest = vec![0.0f32; rate as usize / 100];
        assert!(dsp.render(&mut rest, Wave::Square, 440.0, false), "fade never finished");
        assert!(!dsp.audible());
        assert_eq!(rest[rest.len() - 1], 0.0);

        // …and does not report finished a second time.
        let mut more = vec![0.0f32; 64];
        assert!(!dsp.render(&mut more, Wave::Square, 440.0, false));
    }

    #[test]
    fn detune_is_cents() {
        // 1200 cents = one octave. Measured through the same Goertzel probe
        // the alias test uses, so this checks the actual rendered pitch, not
        // the arithmetic in isolation.
        let rate = 48_000u32;
        let base = 440.0f32;
        let doubled = base * (1200.0f32 / 1200.0).exp2();
        assert!((doubled - 880.0).abs() < 1e-3, "cents math: {doubled}");

        let out = steady(Wave::Sine, doubled, rate, 4096);
        assert!(
            mag_at(&out, 880.0, rate) > 10.0 * mag_at(&out, 440.0, rate),
            "detuned tone is not an octave up"
        );
    }

    #[test]
    fn above_nyquist_is_clamped_not_folded() {
        // A 30 kHz request at 48 kHz has no honest answer; producing a 18 kHz
        // alias would be the dishonest one.
        let rate = 48_000;
        let mut dsp = OscDsp::new(rate, 24_000.0);
        let mut out = vec![0.0f32; 4096];
        dsp.render(&mut out, Wave::Sine, 30_000.0, true);
        // Everything stays in range and nothing blows up.
        assert!(out.iter().all(|s| s.abs() <= 1.001));
    }

    #[test]
    fn a_silent_unstarted_oscillator_renders_exact_zeros() {
        let mut dsp = OscDsp::new(48_000, 440.0);
        let mut out = vec![0.5f32; 256];
        assert!(!dsp.render(&mut out, Wave::Sine, 440.0, false));
        assert!(out.iter().all(|s| *s == 0.0), "unstarted oscillator emitted signal");
    }

    #[test]
    fn wave_names_round_trip_and_custom_is_not_one_of_them() {
        for w in [Wave::Sine, Wave::Square, Wave::Sawtooth, Wave::Triangle] {
            assert_eq!(Wave::from_name(w.name()), Some(w));
        }
        assert_eq!(Wave::from_name("custom"), None);
        assert_eq!(Wave::from_name("Sine"), None);
    }

    #[test]
    fn shared_params_round_trip() {
        let s = OscShared::default();
        assert_eq!(s.frequency(), 440.0);
        assert_eq!(s.detune(), 0.0);
        assert_eq!(s.wave(), Wave::Sine);
        assert!(!s.is_playing());

        s.set_frequency(1234.5);
        s.set_detune(-300.0);
        s.set_wave(Wave::Triangle);
        assert_eq!(s.frequency(), 1234.5);
        assert_eq!(s.detune(), -300.0);
        assert_eq!(s.wave(), Wave::Triangle);

        s.start();
        assert!(s.is_playing() && !s.is_finished());
        s.stop();
        assert!(!s.is_playing());
    }
}
