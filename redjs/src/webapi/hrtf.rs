//! Native HRTF panning.
//!
//! `WebPannerNode` is the Firewheel node behind the JS `PannerNode`: one node,
//! two panning modes switchable live (no graph rewiring):
//!
//! - `equalpower` — spec equal-power panning, including the spec's separate
//!   STEREO-source law (left/right inputs re-balanced, not downmixed).
//! - `HRTF` — true binaural rendering: direct FIR convolution with the SAME
//!   head-related impulse responses Chrome ships (`Composite.flac`, BSD, from
//!   chromium/third_party/blink/renderer/platform/audio/resources — 10
//!   elevations × 24 azimuths × 256 taps @ 44.1 kHz, IRCAM-derived). Kernels
//!   are BILINEARLY interpolated (azimuth × elevation) and crossfaded over
//!   128 frames on direction changes. Stereo sources follow Chromium: the
//!   left input feeds the left-ear convolution, the right input the right-ear
//!   one; mono sources are dual-mono through the same path.
//!
//! Distance/cone gain arrives pre-computed from the pump side (audio.rs
//! `update_spatial_with`) as the `gain` param; the node smooths gain and
//! azimuth with a ~15 ms one-pole to keep moving sources artifact-free.
//!
//! The convolution runs over a CONTIGUOUS scratch window with reversed
//! kernels — a plain `zip().map().sum()` dot product over two linear slices,
//! which LLVM auto-vectorizes (the previous ring-masked indexing didn't).
//!
//! The DSP core (`PannerDsp`) is deliberately independent from the Firewheel
//! traits so the binaural math is unit-testable without an audio stream.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use firewheel::{
    channel_config::{ChannelConfig, ChannelCount},
    diff::{Diff, Patch},
    event::ProcEvents,
    node::{
        AudioNode, AudioNodeInfo, AudioNodeProcessor, ConstructProcessorContext, ProcBuffers,
        ProcExtra, ProcInfo, ProcessStatus,
    },
};

// ---------------------------------------------------------------------------
// HRIR database (Chromium Composite.flac)
// ---------------------------------------------------------------------------

pub const KERNEL_LEN: usize = 256;
/// Max frames rendered per inner chunk (bounds the scratch buffers).
const CHUNK: usize = 256;
const NUM_AZIMUTHS: usize = 24; // 0..345 in 15° steps (IRCAM convention)
const NUM_ELEVATIONS: usize = 10;
const NUM_RESPONSES: usize = NUM_AZIMUTHS * NUM_ELEVATIONS;
const SOURCE_RATE: u32 = 44100;
const ELEVATION_STEP: f32 = 15.0;
/// Crossfade length (frames) when the direction kernels change.
const XFADE_LEN: u32 = 128;

static COMPOSITE_FLAC: &[u8] = include_bytes!("../../assets/hrtf/Composite.flac");

/// Composite elevation slot for a grid elevation in degrees (−45..90 step 15).
/// The file stores negatives as 315/330/345 after the positives
/// (Chromium `kElevationIndexTable`: 0,15,…,90,315,330,345).
fn elevation_index(el_deg: f32) -> usize {
    if el_deg < 0.0 {
        (7.0 + (el_deg + 45.0) / ELEVATION_STEP) as usize // −45→7, −30→8, −15→9
    } else {
        (el_deg / ELEVATION_STEP) as usize // 0..90 → 0..6
    }
}

pub struct HrtfDatabase {
    /// Flattened `[response][ear][tap]`, response = azimuth_index * 10 + elevation_index,
    /// resampled to the stream rate and truncated to KERNEL_LEN taps.
    kernels: Vec<f32>,
}

impl HrtfDatabase {
    /// The database resampled for `rate`, cached per rate. None when the
    /// embedded asset fails to decode (HRTF then falls back to equal-power).
    pub fn for_rate(rate: u32) -> Option<Arc<HrtfDatabase>> {
        static CACHE: OnceLock<Mutex<HashMap<u32, Option<Arc<HrtfDatabase>>>>> = OnceLock::new();
        let cache = CACHE.get_or_init(Default::default);
        let mut map = cache.lock().unwrap();
        map.entry(rate)
            .or_insert_with(|| Self::load(rate).map(Arc::new))
            .clone()
    }

    fn load(rate: u32) -> Option<HrtfDatabase> {
        let decoded = super::audio::decode_bytes(COMPOSITE_FLAC.to_vec(), None).ok()?;
        if decoded.data.len() != 2
            || decoded.sample_rate != SOURCE_RATE
            || decoded.data[0].len() != NUM_RESPONSES * KERNEL_LEN
        {
            return None;
        }

        // Resample each 256-frame response INDEPENDENTLY — resampling the
        // concatenation would put fractional frame offsets between responses
        // and corrupt the interaural time differences baked into them.
        let ratio = rate as f64 / SOURCE_RATE as f64;
        let mut kernels = vec![0.0f32; NUM_RESPONSES * 2 * KERNEL_LEN];
        for resp in 0..NUM_RESPONSES {
            for ear in 0..2 {
                let src = &decoded.data[ear][resp * KERNEL_LEN..(resp + 1) * KERNEL_LEN];
                let dst = &mut kernels[(resp * 2 + ear) * KERNEL_LEN..][..KERNEL_LEN];
                resample_linear(src, dst, ratio);
            }
        }
        Some(HrtfDatabase { kernels })
    }

    fn response(&self, az_index: usize, el_index: usize, ear: usize) -> &[f32] {
        let resp = az_index * NUM_ELEVATIONS + el_index;
        &self.kernels[(resp * 2 + ear) * KERNEL_LEN..][..KERNEL_LEN]
    }

    /// Kernels for a Web-Audio direction (azimuth 0 = front, +90 = right;
    /// elevation −90..+90), bilinearly interpolated between the four
    /// surrounding grid responses (azimuth × elevation).
    pub fn kernels_for(&self, azimuth_deg: f32, elevation_deg: f32, out_l: &mut [f32], out_r: &mut [f32]) {
        // The IRCAM responses are reversed relative to Web Audio's azimuth
        let az = (-azimuth_deg).rem_euclid(360.0);
        let azf = az / (360.0 / NUM_AZIMUTHS as f32);
        let a1 = (azf as usize) % NUM_AZIMUTHS;
        let a2 = (a1 + 1) % NUM_AZIMUTHS;
        let ab = azf.fract();

        let el = elevation_deg.clamp(-45.0, 90.0);
        let e_low = (el / ELEVATION_STEP).floor() * ELEVATION_STEP;
        let e_high = (e_low + ELEVATION_STEP).min(90.0);
        let eb = if e_high > e_low { (el - e_low) / ELEVATION_STEP } else { 0.0 };
        let e1 = elevation_index(e_low);
        let e2 = elevation_index(e_high);

        let w11 = (1.0 - ab) * (1.0 - eb);
        let w12 = (1.0 - ab) * eb;
        let w21 = ab * (1.0 - eb);
        let w22 = ab * eb;

        for (ear, out) in [out_l, out_r].into_iter().enumerate() {
            let k11 = self.response(a1, e1, ear);
            let k12 = self.response(a1, e2, ear);
            let k21 = self.response(a2, e1, ear);
            let k22 = self.response(a2, e2, ear);
            for i in 0..KERNEL_LEN {
                out[i] = k11[i] * w11 + k12[i] * w12 + k21[i] * w21 + k22[i] * w22;
            }
        }
    }
}

fn resample_linear(src: &[f32], dst: &mut [f32], ratio: f64) {
    for (i, d) in dst.iter_mut().enumerate() {
        let pos = i as f64 / ratio;
        let i0 = pos as usize;
        let frac = (pos - i0 as f64) as f32;
        let a = src.get(i0).copied().unwrap_or(0.0);
        let b = src.get(i0 + 1).copied().unwrap_or(0.0);
        *d = a + (b - a) * frac;
    }
}

// ---------------------------------------------------------------------------
// DSP core (Firewheel-independent, unit-testable)
// ---------------------------------------------------------------------------

/// One ear's convolution state: reversed kernels + a contiguous scratch that
/// carries the last KERNEL_LEN−1 input samples across blocks.
struct EarConv {
    /// REVERSED kernel — lets the per-sample op be a dot product of two
    /// contiguous ascending slices (auto-vectorizable).
    cur: [f32; KERNEL_LEN],
    new: [f32; KERNEL_LEN],
    /// `[history tail (KERNEL_LEN−1)] [current block (≤CHUNK)]`
    scratch: [f32; KERNEL_LEN - 1 + CHUNK],
}

impl EarConv {
    fn new() -> Self {
        Self {
            cur: [0.0; KERNEL_LEN],
            new: [0.0; KERNEL_LEN],
            scratch: [0.0; KERNEL_LEN - 1 + CHUNK],
        }
    }

    /// Load this block's input after the carried history tail.
    fn begin_block(&mut self, input: &[f32]) {
        self.scratch[KERNEL_LEN - 1..KERNEL_LEN - 1 + input.len()].copy_from_slice(input);
    }

    /// Carry the last KERNEL_LEN−1 samples into the next block's history.
    fn end_block(&mut self, n: usize) {
        self.scratch.copy_within(n..n + KERNEL_LEN - 1, 0);
    }

    #[inline]
    fn convolve(&self, i: usize, kernel: &[f32; KERNEL_LEN]) -> f32 {
        // window = x[i-255..=i] ascending; kernel is reversed → plain dot
        self.scratch[i..i + KERNEL_LEN]
            .iter()
            .zip(kernel.iter())
            .map(|(x, k)| x * k)
            .sum()
    }
}

pub struct PannerDsp {
    db: Option<Arc<HrtfDatabase>>,
    left: EarConv,
    right: EarConv,
    xfade_left: u32,
    have_kernels: bool,
    smoothed_gain: f32,
    smoothed_az: f32,
    smooth_coef: f32,
}

impl PannerDsp {
    pub fn new(db: Option<Arc<HrtfDatabase>>, sample_rate: f32) -> Self {
        Self {
            db,
            left: EarConv::new(),
            right: EarConv::new(),
            xfade_left: 0,
            have_kernels: false,
            smoothed_gain: 1.0,
            smoothed_az: 0.0,
            // ~15 ms one-pole, matching the smoothing the rest of the graph uses
            smooth_coef: 1.0 - (-1.0 / (0.015 * sample_rate.max(1.0))).exp(),
        }
    }

    pub fn has_hrtf(&self) -> bool {
        self.db.is_some()
    }

    #[cfg(test)]
    fn kill_smoothing(&mut self) {
        self.smooth_coef = 1.0;
    }

    /// Retarget the binaural kernels (crossfaded in over XFADE_LEN frames).
    pub fn set_direction(&mut self, azimuth_deg: f32, elevation_deg: f32) {
        let Some(db) = &self.db else { return };
        let mut kl = [0.0f32; KERNEL_LEN];
        let mut kr = [0.0f32; KERNEL_LEN];
        db.kernels_for(azimuth_deg, elevation_deg, &mut kl, &mut kr);
        kl.reverse();
        kr.reverse();
        if self.have_kernels {
            self.left.new = kl;
            self.right.new = kr;
            self.xfade_left = XFADE_LEN;
        } else {
            self.left.cur = kl;
            self.right.cur = kr;
            self.have_kernels = true;
        }
    }

    /// Render one block (≤CHUNK frames). `in_r` is the right channel of a
    /// STEREO source; pass the left slice again for mono/dual-mono sources.
    /// `hrtf` falls back to equal-power when the database is unavailable;
    /// `azimuth` steers the equal-power path (the HRTF path is steered by
    /// `set_direction`); `gain` is the pre-computed distance × cone value.
    pub fn render(
        &mut self,
        hrtf: bool,
        azimuth_deg: f32,
        gain: f32,
        in_l: &[f32],
        in_r: &[f32],
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) {
        let n = in_l.len();
        debug_assert!(n <= CHUNK && in_r.len() == n);
        let stereo = !core::ptr::eq(in_l.as_ptr(), in_r.as_ptr()) && in_l != in_r;

        if hrtf && self.have_kernels {
            // Chromium's stereo HRTF: left input → left-ear convolution,
            // right input → right-ear convolution (mono = dual-mono).
            self.left.begin_block(in_l);
            self.right.begin_block(in_r);
            for i in 0..n {
                self.smoothed_gain += (gain - self.smoothed_gain) * self.smooth_coef;
                let mut l = self.left.convolve(i, &self.left.cur);
                let mut r = self.right.convolve(i, &self.right.cur);
                if self.xfade_left > 0 {
                    let t = 1.0 - self.xfade_left as f32 / XFADE_LEN as f32;
                    l += (self.left.convolve(i, &self.left.new) - l) * t;
                    r += (self.right.convolve(i, &self.right.new) - r) * t;
                    self.xfade_left -= 1;
                    if self.xfade_left == 0 {
                        self.left.cur = self.left.new;
                        self.right.cur = self.right.new;
                    }
                }
                out_l[i] = l * self.smoothed_gain;
                out_r[i] = r * self.smoothed_gain;
            }
            self.left.end_block(n);
            self.right.end_block(n);
        } else {
            for i in 0..n {
                self.smoothed_gain += (gain - self.smoothed_gain) * self.smooth_coef;
                self.smoothed_az += (azimuth_deg - self.smoothed_az) * self.smooth_coef;
                let g = self.smoothed_gain;

                // Spec equal-power panning with the rear fold
                let mut az = self.smoothed_az.clamp(-180.0, 180.0);
                if az < -90.0 {
                    az = -180.0 - az;
                } else if az > 90.0 {
                    az = 180.0 - az;
                }

                if stereo {
                    // Spec STEREO source law: the leading channel passes
                    // through, the trailing one is re-panned across.
                    let x = if az <= 0.0 { (az + 90.0) / 90.0 } else { az / 90.0 };
                    let phase = x * core::f32::consts::FRAC_PI_2;
                    let (gl, gr) = (phase.cos(), phase.sin());
                    if az <= 0.0 {
                        out_l[i] = (in_l[i] + in_r[i] * gl) * g;
                        out_r[i] = in_r[i] * gr * g;
                    } else {
                        out_l[i] = in_l[i] * gl * g;
                        out_r[i] = (in_r[i] + in_l[i] * gr) * g;
                    }
                } else {
                    let phase = (az + 90.0) / 180.0 * core::f32::consts::FRAC_PI_2;
                    out_l[i] = in_l[i] * phase.cos() * g;
                    out_r[i] = in_l[i] * phase.sin() * g;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Firewheel node
// ---------------------------------------------------------------------------

/// The JS `PannerNode`'s params — pump-side derives push these
/// (see audio.rs `update_spatial_with`).
#[derive(Diff, Patch, Debug, Clone, Copy, PartialEq)]
pub struct WebPannerNode {
    /// Web Audio azimuth in degrees: 0 = front, +90 = right.
    pub azimuth: f32,
    /// Elevation in degrees, −90..+90.
    pub elevation: f32,
    /// Pre-computed distance × cone gain.
    pub gain: f32,
    /// True = binaural HRTF convolution, false = equal-power panning.
    pub hrtf: bool,
}

impl Default for WebPannerNode {
    fn default() -> Self {
        Self { azimuth: 0.0, elevation: 0.0, gain: 1.0, hrtf: false }
    }
}

#[derive(Default)]
pub struct WebPannerConfig {
    pub db: Option<Arc<HrtfDatabase>>,
}

impl AudioNode for WebPannerNode {
    type Configuration = WebPannerConfig;

    fn info(&self, _config: &Self::Configuration) -> AudioNodeInfo {
        AudioNodeInfo::new()
            .debug_name("webaudio_panner")
            .channel_config(ChannelConfig {
                num_inputs: ChannelCount::new(2).unwrap(),
                num_outputs: ChannelCount::new(2).unwrap(),
            })
    }

    fn construct_processor(
        &self,
        config: &Self::Configuration,
        cx: ConstructProcessorContext,
    ) -> impl AudioNodeProcessor {
        let mut dsp = PannerDsp::new(config.db.clone(), cx.stream_info.sample_rate.get() as f32);
        if self.hrtf {
            dsp.set_direction(self.azimuth, self.elevation);
        }
        WebPannerProcessor { params: *self, dsp }
    }
}

struct WebPannerProcessor {
    params: WebPannerNode,
    dsp: PannerDsp,
}

impl AudioNodeProcessor for WebPannerProcessor {
    fn process(
        &mut self,
        info: &ProcInfo,
        buffers: ProcBuffers,
        events: &mut ProcEvents,
        _extra: &mut ProcExtra,
    ) -> ProcessStatus {
        let before = self.params;
        for patch in events.drain_patches::<WebPannerNode>() {
            self.params.apply(patch);
        }
        if self.params.hrtf
            && (!before.hrtf
                || before.azimuth != self.params.azimuth
                || before.elevation != self.params.elevation)
        {
            self.dsp.set_direction(self.params.azimuth, self.params.elevation);
        }

        // Treat the input as stereo only when the right channel carries its
        // own signal; a silent right channel means a mono routing (dual-mono
        // keeps the browser's mono pan law and loudness).
        let l_silent = info.in_silence_mask.is_channel_silent(0);
        let r_silent = buffers.inputs.len() < 2 || info.in_silence_mask.is_channel_silent(1);

        let (out_l_slot, rest) = buffers.outputs.split_at_mut(1);
        let out_l = &mut *out_l_slot[0];
        let out_r = &mut *rest[0];

        let silent = [0.0f32; CHUNK];
        let mut off = 0usize;
        while off < info.frames {
            let n = (info.frames - off).min(CHUNK);
            let in_l: &[f32] = if l_silent { &silent[..n] } else { &buffers.inputs[0][off..off + n] };
            let in_r: &[f32] = if r_silent { in_l } else { &buffers.inputs[1][off..off + n] };
            self.dsp.render(
                self.params.hrtf,
                self.params.azimuth,
                self.params.gain,
                in_l,
                in_r,
                &mut out_l[off..off + n],
                &mut out_r[off..off + n],
            );
            off += n;
        }

        ProcessStatus::OutputsModified
    }
}

// ---------------------------------------------------------------------------
// Tests — the binaural math, no audio stream needed
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn energy(samples: &[f32]) -> f32 {
        samples.iter().map(|s| s * s).sum()
    }

    /// Peak-position = crude arrival time (ITD check).
    fn peak_index(samples: &[f32]) -> usize {
        let mut best = 0;
        for (i, s) in samples.iter().enumerate() {
            if s.abs() > samples[best].abs() {
                best = i;
            }
        }
        best
    }

    fn db() -> Arc<HrtfDatabase> {
        HrtfDatabase::for_rate(48000).expect("embedded HRTF database must decode")
    }

    /// Impulse through the HRTF path in ≤CHUNK blocks.
    fn render_impulse(azimuth: f32, elevation: f32) -> (Vec<f32>, Vec<f32>) {
        let mut dsp = PannerDsp::new(Some(db()), 48000.0);
        dsp.kill_smoothing();
        dsp.set_direction(azimuth, elevation);
        let mut input = vec![0.0f32; KERNEL_LEN * 2];
        input[0] = 1.0;
        let mut l = vec![0.0f32; input.len()];
        let mut r = vec![0.0f32; input.len()];
        for off in (0..input.len()).step_by(CHUNK) {
            let n = (input.len() - off).min(CHUNK);
            let (in_chunk, _) = input[off..].split_at(n);
            // mono: same slice for both channels
            let (l_chunk, r_chunk) = (&mut l[off..off + n], &mut r[off..off + n]);
            dsp.render(true, azimuth, 1.0, in_chunk, in_chunk, l_chunk, r_chunk);
        }
        (l, r)
    }

    #[test]
    fn database_loads_and_is_energetic() {
        let db = db();
        let mut l = [0.0f32; KERNEL_LEN];
        let mut r = [0.0f32; KERNEL_LEN];
        db.kernels_for(0.0, 0.0, &mut l, &mut r);
        assert!(energy(&l) > 0.01 && energy(&r) > 0.01, "front kernels silent");
        let ratio = energy(&l) / energy(&r);
        assert!((0.5..2.0).contains(&ratio), "front L/R asymmetry: {ratio}");
    }

    #[test]
    fn elevation_blend_is_bilinear() {
        let db = db();
        let mut mid_l = [0.0f32; KERNEL_LEN];
        let mut mid_r = [0.0f32; KERNEL_LEN];
        db.kernels_for(0.0, 7.5, &mut mid_l, &mut mid_r);

        let mut lo_l = [0.0f32; KERNEL_LEN];
        let mut lo_r = [0.0f32; KERNEL_LEN];
        db.kernels_for(0.0, 0.0, &mut lo_l, &mut lo_r);
        let mut hi_l = [0.0f32; KERNEL_LEN];
        let mut hi_r = [0.0f32; KERNEL_LEN];
        db.kernels_for(0.0, 15.0, &mut hi_l, &mut hi_r);

        for i in 0..KERNEL_LEN {
            let expect = (lo_l[i] + hi_l[i]) * 0.5;
            assert!((mid_l[i] - expect).abs() < 1e-6, "tap {i}: {} vs {expect}", mid_l[i]);
        }
        // And negative elevations resolve through the wrapped table slots
        db.kernels_for(0.0, -45.0, &mut lo_l, &mut lo_r);
        assert!(energy(&lo_l) > 0.01, "-45° kernels silent");
    }

    #[test]
    fn hrtf_right_source_favors_right_ear_and_leads() {
        let (l, r) = render_impulse(90.0, 0.0);
        let (el, er) = (energy(&l), energy(&r));
        assert!(er > el * 2.0, "right source should be louder right: L={el} R={er}");
        assert!(peak_index(&r) < peak_index(&l), "right ear should lead: L@{} R@{}", peak_index(&l), peak_index(&r));
    }

    #[test]
    fn hrtf_left_source_mirrors() {
        let (l, r) = render_impulse(-90.0, 0.0);
        assert!(energy(&l) > energy(&r) * 2.0, "left source should be louder left");
        assert!(peak_index(&l) < peak_index(&r), "left ear should lead");
    }

    #[test]
    fn hrtf_stereo_keeps_channels_on_their_ears() {
        // Distinct L/R inputs: impulse only in the LEFT channel must reach the
        // left ear at full kernel energy and the right ear not at all
        // (Chromium: left source → left HRTF, right source → right HRTF).
        let mut dsp = PannerDsp::new(Some(db()), 48000.0);
        dsp.kill_smoothing();
        dsp.set_direction(0.0, 0.0);
        let mut in_l = vec![0.0f32; KERNEL_LEN];
        let in_r = vec![0.0f32; KERNEL_LEN];
        in_l[0] = 1.0;
        let mut l = vec![0.0f32; KERNEL_LEN];
        let mut r = vec![0.0f32; KERNEL_LEN];
        dsp.render(true, 0.0, 1.0, &in_l, &in_r, &mut l, &mut r);
        assert!(energy(&l) > 0.01, "left ear should render the left channel");
        assert!(energy(&r) < 1e-9, "right ear must stay silent for a silent right channel");
    }

    #[test]
    fn equalpower_gains_match_spec() {
        let mut dsp = PannerDsp::new(None, 48000.0);
        dsp.kill_smoothing();
        let input = [1.0f32; 4];
        let mut l = [0.0f32; 4];
        let mut r = [0.0f32; 4];
        let inv_sqrt2 = core::f32::consts::FRAC_1_SQRT_2;

        dsp.render(false, 0.0, 1.0, &input, &input, &mut l, &mut r); // mono center
        assert!((l[3] - r[3]).abs() < 1e-3, "center should be symmetric");
        assert!((l[3] - inv_sqrt2).abs() < 1e-3, "center gain should be cos(45°)");

        dsp.render(false, 90.0, 1.0, &input, &input, &mut l, &mut r); // hard right
        assert!(l[3].abs() < 1e-3 && (r[3] - 1.0).abs() < 1e-3, "hard right: L={} R={}", l[3], r[3]);

        // Rear fold: 180° behaves like 0° (front)
        dsp.render(false, 180.0, 1.0, &input, &input, &mut l, &mut r);
        assert!((l[3] - inv_sqrt2).abs() < 1e-3, "rear fold should re-center");
    }

    #[test]
    fn equalpower_stereo_law() {
        let mut dsp = PannerDsp::new(None, 48000.0);
        dsp.kill_smoothing();
        let in_l = [1.0f32; 4];
        let in_r = [0.5f32; 4];
        let mut l = [0.0f32; 4];
        let mut r = [0.0f32; 4];

        // Center (az 0, stereo): x = 1 → gainL = 0, gainR = 1 → passthrough
        dsp.render(false, 0.0, 1.0, &in_l, &in_r, &mut l, &mut r);
        assert!((l[3] - 1.0).abs() < 1e-3 && (r[3] - 0.5).abs() < 1e-3, "stereo center should pass through: L={} R={}", l[3], r[3]);

        // Hard right (az 90): both channels collapse to the right
        dsp.render(false, 90.0, 1.0, &in_l, &in_r, &mut l, &mut r);
        assert!(l[3].abs() < 1e-3, "hard-right stereo: left should be empty");
        assert!((r[3] - 1.5).abs() < 1e-3, "hard-right stereo: R = inR + inL, got {}", r[3]);

        // Hard left (az −90): x = 0 → gainL = 1, gainR = 0 → both collapse left
        dsp.render(false, -90.0, 1.0, &in_l, &in_r, &mut l, &mut r);
        assert!((l[3] - 1.5).abs() < 1e-3, "hard-left stereo: L = inL + inR, got {}", l[3]);
        assert!(r[3].abs() < 1e-3, "hard-left stereo: right should be empty");
    }

    #[test]
    fn hrtf_mode_survives_missing_database() {
        let mut dsp = PannerDsp::new(None, 48000.0);
        dsp.kill_smoothing();
        dsp.set_direction(90.0, 0.0);
        let input = [1.0f32; 4];
        let mut l = [0.0f32; 4];
        let mut r = [0.0f32; 4];
        dsp.render(true, 90.0, 1.0, &input, &input, &mut l, &mut r);
        assert!((r[3] - 1.0).abs() < 1e-3, "fallback should pan equal-power");
    }
}
