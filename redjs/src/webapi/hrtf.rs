//! Native HRTF panning (W-MIX MIX-20).
//!
//! `WebPannerNode` is the Firewheel node behind the JS `PannerNode`: one node,
//! two panning modes switchable live (no graph rewiring):
//!
//! - `equalpower` — spec equal-power stereo panning with the rear fold.
//! - `HRTF` — true binaural rendering: direct FIR convolution with the SAME
//!   head-related impulse responses Chrome ships (`Composite.flac`, BSD, from
//!   chromium/third_party/blink/renderer/platform/audio/resources — 10
//!   elevations × 24 azimuths × 256 taps @ 44.1 kHz, IRCAM-derived). Kernels
//!   are linearly interpolated between the two nearest azimuths and
//!   crossfaded over 128 frames on direction changes (Chromium's approach).
//!
//! Distance/cone gain arrives pre-computed from the pump side (audio.rs
//! `update_spatial_with`) as the `gain` param; the node smooths gain and
//! azimuth with a ~15 ms one-pole to keep moving sources artifact-free.
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
const HISTORY_LEN: usize = 512; // power of two ≥ KERNEL_LEN
const NUM_AZIMUTHS: usize = 24; // 0..345 in 15° steps (IRCAM convention)
const NUM_ELEVATIONS: usize = 10;
const NUM_RESPONSES: usize = NUM_AZIMUTHS * NUM_ELEVATIONS;
const SOURCE_RATE: u32 = 44100;
/// Elevation (degrees) for index 0..9 — the composite stores negative
/// elevations as 315/330/345 (Chromium `kElevationIndexTable`).
const ELEVATIONS: [f32; NUM_ELEVATIONS] =
    [0.0, 15.0, 30.0, 45.0, 60.0, 75.0, 90.0, -45.0, -30.0, -15.0];
/// Crossfade length (frames) when the direction kernels change.
const XFADE_LEN: u32 = 128;

static COMPOSITE_FLAC: &[u8] = include_bytes!("../../assets/hrtf/Composite.flac");

pub struct HrtfDatabase {
    /// Flattened `[response][ear][tap]`, response = azimuth_index * 10 + elevation_index,
    /// resampled to the stream rate and truncated/zero-padded to KERNEL_LEN taps.
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
    /// elevation −90..+90). Azimuth-blended between the two nearest responses,
    /// elevation snapped to the nearest 15° (Chromium blends elevations too —
    /// acceptable v1 simplification).
    pub fn kernels_for(&self, azimuth_deg: f32, elevation_deg: f32, out_l: &mut [f32], out_r: &mut [f32]) {
        // The IRCAM responses are reversed relative to Web Audio's azimuth
        let az = (-azimuth_deg).rem_euclid(360.0);
        let azf = az / (360.0 / NUM_AZIMUTHS as f32);
        let i1 = (azf as usize) % NUM_AZIMUTHS;
        let i2 = (i1 + 1) % NUM_AZIMUTHS;
        let blend = azf.fract();

        let el_snapped = (elevation_deg.clamp(-45.0, 90.0) / 15.0).round() * 15.0;
        let el_index = ELEVATIONS.iter().position(|&e| e == el_snapped).unwrap_or(0);

        for (ear, out) in [out_l, out_r].into_iter().enumerate() {
            let k1 = self.response(i1, el_index, ear);
            let k2 = self.response(i2, el_index, ear);
            for i in 0..KERNEL_LEN {
                out[i] = k1[i] * (1.0 - blend) + k2[i] * blend;
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

pub struct PannerDsp {
    db: Option<Arc<HrtfDatabase>>,
    hist: [f32; HISTORY_LEN],
    pos: usize,
    cur_l: [f32; KERNEL_LEN],
    cur_r: [f32; KERNEL_LEN],
    new_l: [f32; KERNEL_LEN],
    new_r: [f32; KERNEL_LEN],
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
            hist: [0.0; HISTORY_LEN],
            pos: 0,
            cur_l: [0.0; KERNEL_LEN],
            cur_r: [0.0; KERNEL_LEN],
            new_l: [0.0; KERNEL_LEN],
            new_r: [0.0; KERNEL_LEN],
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

    /// Retarget the binaural kernels (crossfaded in over XFADE_LEN frames).
    pub fn set_direction(&mut self, azimuth_deg: f32, elevation_deg: f32) {
        let Some(db) = &self.db else { return };
        if self.have_kernels {
            db.kernels_for(azimuth_deg, elevation_deg, &mut self.new_l, &mut self.new_r);
            self.xfade_left = XFADE_LEN;
        } else {
            db.kernels_for(azimuth_deg, elevation_deg, &mut self.cur_l, &mut self.cur_r);
            self.have_kernels = true;
        }
    }

    /// Render one block. `hrtf` falls back to equal-power when the database is
    /// unavailable. `azimuth` only steers the equal-power path (the HRTF path
    /// is steered by `set_direction`); `gain` is the pre-computed distance ×
    /// cone attenuation.
    pub fn render(
        &mut self,
        hrtf: bool,
        azimuth_deg: f32,
        gain: f32,
        mono_in: &[f32],
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) {
        let use_hrtf = hrtf && self.have_kernels;
        for i in 0..mono_in.len() {
            self.smoothed_gain += (gain - self.smoothed_gain) * self.smooth_coef;
            self.smoothed_az += (azimuth_deg - self.smoothed_az) * self.smooth_coef;
            let x = mono_in[i];

            if use_hrtf {
                self.hist[self.pos & (HISTORY_LEN - 1)] = x;
                let (mut l, mut r) = self.convolve(&self.cur_l, &self.cur_r);
                if self.xfade_left > 0 {
                    let (nl, nr) = self.convolve(&self.new_l, &self.new_r);
                    let t = 1.0 - self.xfade_left as f32 / XFADE_LEN as f32;
                    l += (nl - l) * t;
                    r += (nr - r) * t;
                    self.xfade_left -= 1;
                    if self.xfade_left == 0 {
                        self.cur_l = self.new_l;
                        self.cur_r = self.new_r;
                    }
                }
                self.pos = self.pos.wrapping_add(1);
                out_l[i] = l * self.smoothed_gain;
                out_r[i] = r * self.smoothed_gain;
            } else {
                // Spec equal-power panning with the rear fold
                let mut az = self.smoothed_az.clamp(-180.0, 180.0);
                if az < -90.0 {
                    az = -180.0 - az;
                } else if az > 90.0 {
                    az = 180.0 - az;
                }
                let phase = (az + 90.0) / 180.0 * core::f32::consts::FRAC_PI_2;
                out_l[i] = x * phase.cos() * self.smoothed_gain;
                out_r[i] = x * phase.sin() * self.smoothed_gain;
            }
        }
    }

    #[inline]
    fn convolve(&self, kl: &[f32; KERNEL_LEN], kr: &[f32; KERNEL_LEN]) -> (f32, f32) {
        let mut l = 0.0f32;
        let mut r = 0.0f32;
        for t in 0..KERNEL_LEN {
            let s = self.hist[self.pos.wrapping_sub(t) & (HISTORY_LEN - 1)];
            l += s * kl[t];
            r += s * kr[t];
        }
        (l, r)
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

        let n_ch = buffers.inputs.len().max(1) as f32;
        let (out_l_slot, rest) = buffers.outputs.split_at_mut(1);
        let out_l = &mut *out_l_slot[0];
        let out_r = &mut *rest[0];

        let mut off = 0usize;
        let mut mono = [0.0f32; 256];
        while off < info.frames {
            let n = (info.frames - off).min(256);
            for j in 0..n {
                let mut s = 0.0f32;
                for (ch, buf) in buffers.inputs.iter().enumerate() {
                    if !info.in_silence_mask.is_channel_silent(ch) {
                        s += buf[off + j];
                    }
                }
                mono[j] = s / n_ch;
            }
            self.dsp.render(
                self.params.hrtf,
                self.params.azimuth,
                self.params.gain,
                &mono[..n],
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

    fn render_impulse(azimuth: f32, hrtf: bool) -> (Vec<f32>, Vec<f32>) {
        let db = HrtfDatabase::for_rate(48000).expect("embedded HRTF database must decode");
        let mut dsp = PannerDsp::new(Some(db), 48000.0);
        // Kill the smoothing ramp for deterministic gains
        dsp.smooth_coef = 1.0;
        dsp.set_direction(azimuth, 0.0);
        let mut input = vec![0.0f32; KERNEL_LEN * 2];
        input[0] = 1.0;
        let mut l = vec![0.0f32; input.len()];
        let mut r = vec![0.0f32; input.len()];
        dsp.render(hrtf, azimuth, 1.0, &input, &mut l, &mut r);
        (l, r)
    }

    #[test]
    fn database_loads_and_is_energetic() {
        let db = HrtfDatabase::for_rate(48000).expect("decode");
        let mut l = [0.0f32; KERNEL_LEN];
        let mut r = [0.0f32; KERNEL_LEN];
        db.kernels_for(0.0, 0.0, &mut l, &mut r);
        assert!(energy(&l) > 0.01 && energy(&r) > 0.01, "front kernels silent");
        // Frontal incidence is roughly symmetric
        let ratio = energy(&l) / energy(&r);
        assert!((0.5..2.0).contains(&ratio), "front L/R asymmetry: {ratio}");
    }

    #[test]
    fn hrtf_right_source_favors_right_ear_and_leads() {
        let (l, r) = render_impulse(90.0, true);
        let (el, er) = (energy(&l), energy(&r));
        assert!(er > el * 2.0, "right source should be louder right: L={el} R={er}");
        // Interaural time difference: the right (near) ear receives it earlier
        assert!(peak_index(&r) < peak_index(&l), "right ear should lead: L@{} R@{}", peak_index(&l), peak_index(&r));
    }

    #[test]
    fn hrtf_left_source_mirrors() {
        let (l, r) = render_impulse(-90.0, true);
        assert!(energy(&l) > energy(&r) * 2.0, "left source should be louder left");
        assert!(peak_index(&l) < peak_index(&r), "left ear should lead");
    }

    #[test]
    fn equalpower_gains_match_spec() {
        let db = HrtfDatabase::for_rate(48000);
        let mut dsp = PannerDsp::new(db, 48000.0);
        dsp.smooth_coef = 1.0;
        let input = [1.0f32; 4];
        let mut l = [0.0f32; 4];
        let mut r = [0.0f32; 4];

        dsp.render(false, 0.0, 1.0, &input, &mut l, &mut r); // center
        assert!((l[3] - r[3]).abs() < 1e-3, "center should be symmetric");
        let inv_sqrt2 = core::f32::consts::FRAC_1_SQRT_2;
        assert!((l[3] - inv_sqrt2).abs() < 1e-3, "center gain should be cos(45°)");

        dsp.render(false, 90.0, 1.0, &input, &mut l, &mut r); // hard right
        assert!(l[3].abs() < 1e-3 && (r[3] - 1.0).abs() < 1e-3, "hard right: L={} R={}", l[3], r[3]);

        // Rear fold: 180° behaves like 0° (front)
        dsp.render(false, 180.0, 1.0, &input, &mut l, &mut r);
        assert!((l[3] - inv_sqrt2).abs() < 1e-3, "rear fold should re-center");
    }

    #[test]
    fn hrtf_mode_survives_missing_database() {
        let mut dsp = PannerDsp::new(None, 48000.0);
        dsp.smooth_coef = 1.0;
        dsp.set_direction(90.0, 0.0);
        let input = [1.0f32; 4];
        let mut l = [0.0f32; 4];
        let mut r = [0.0f32; 4];
        dsp.render(true, 90.0, 1.0, &input, &mut l, &mut r);
        // Falls back to equal-power — still audible
        assert!((r[3] - 1.0).abs() < 1e-3, "fallback should pan equal-power");
    }
}
