//! Channel up-mixing and down-mixing (Web Audio, "Channel up-mixing and down-mixing") as sparse gain
//! matrices, the routes a connection takes through Firewheel ports, and the node that applies a
//! matrix whose gains are not all 1.

use firewheel::channel_config::{ChannelConfig, ChannelCount};
use firewheel::node::{
    AudioNode, AudioNodeInfo, AudioNodeProcessor, ConstructProcessorContext, EmptyConfig,
    ProcBuffers, ProcExtra, ProcInfo, ProcessStatus,
};

/// Chromium's `BaseAudioContext::MaxNumberOfChannels()`.
pub const MAX_CHANNELS: u32 = 32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CountMode {
    Max,
    ClampedMax,
    Explicit,
}

impl CountMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "max" => Some(Self::Max),
            "clamped-max" => Some(Self::ClampedMax),
            "explicit" => Some(Self::Explicit),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Max => "max",
            Self::ClampedMax => "clamped-max",
            Self::Explicit => "explicit",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Interp {
    Speakers,
    Discrete,
}

impl Interp {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "speakers" => Some(Self::Speakers),
            "discrete" => Some(Self::Discrete),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Speakers => "speakers",
            Self::Discrete => "discrete",
        }
    }
}

/// The spec's `computedNumberOfChannels`; `widest` is the widest connection, 0 with none.
pub fn computed_channels(mode: CountMode, count: u32, widest: u32) -> u32 {
    let widest = widest.max(1);
    match mode {
        CountMode::Max => widest,
        CountMode::ClampedMax => widest.min(count),
        CountMode::Explicit => count,
    }
}

/// WAVEFORMATEXTENSIBLE speaker positions, the layout vocabulary output devices share.
pub mod speaker {
    pub const FL: u32 = 0x1;
    pub const FR: u32 = 0x2;
    pub const FC: u32 = 0x4;
    pub const LFE: u32 = 0x8;
    pub const BL: u32 = 0x10;
    pub const BR: u32 = 0x20;
    pub const SL: u32 = 0x200;
    pub const SR: u32 = 0x400;
    pub const TFL: u32 = 0x1000;
    pub const TFR: u32 = 0x4000;
    pub const TBL: u32 = 0x8000;
    pub const TBR: u32 = 0x20000;

    pub const STEREO: u32 = FL | FR;
    pub const QUAD: u32 = FL | FR | BL | BR;
    pub const FIVE_ONE: u32 = FL | FR | FC | LFE | SL | SR;
    pub const SEVEN_ONE: u32 = FL | FR | FC | LFE | BL | BR | SL | SR;
    pub const SEVEN_ONE_FOUR: u32 = SEVEN_ONE | TFL | TFR | TBL | TBR;
}

/// The usual layout of a device that reports only its channel count.
pub fn standard_mask(channels: u32) -> u32 {
    match channels {
        1 => speaker::FC,
        2 => speaker::STEREO,
        4 => speaker::QUAD,
        6 => speaker::FIVE_ONE,
        8 => speaker::SEVEN_ONE,
        _ => 0,
    }
}

/// `(input channel, output channel, gain)` triples; absent pairs are 0.
pub type Matrix = Vec<(u32, u32, f32)>;

const H: f32 = core::f32::consts::FRAC_1_SQRT_2;

fn discrete(from: u32, to: u32) -> Matrix {
    (0..from.min(to)).map(|c| (c, c, 1.0)).collect()
}

/// The mix of a `from`-channel connection into a `to`-channel input, between the spec's own
/// layouts: mono, stereo, quad (L R SL SR) and 5.1 (L R C LFE SL SR). Anything else is discrete.
pub fn web_mix(from: u32, to: u32, interp: Interp) -> Matrix {
    if from == to || interp == Interp::Discrete {
        return discrete(from, to);
    }
    match (from, to) {
        (1, 2) | (1, 4) => vec![(0, 0, 1.0), (0, 1, 1.0)],
        (1, 6) => vec![(0, 2, 1.0)],
        (2, 4) | (2, 6) => vec![(0, 0, 1.0), (1, 1, 1.0)],
        (4, 6) => vec![(0, 0, 1.0), (1, 1, 1.0), (2, 4, 1.0), (3, 5, 1.0)],
        (2, 1) => vec![(0, 0, 0.5), (1, 0, 0.5)],
        (4, 1) => (0..4).map(|c| (c, 0, 0.25)).collect(),
        (6, 1) => vec![(0, 0, H), (1, 0, H), (2, 0, 1.0), (4, 0, 0.5), (5, 0, 0.5)],
        (4, 2) => vec![(0, 0, 0.5), (2, 0, 0.5), (1, 1, 0.5), (3, 1, 0.5)],
        (6, 2) => vec![
            (0, 0, 1.0),
            (2, 0, H),
            (4, 0, H),
            (1, 1, 1.0),
            (2, 1, H),
            (5, 1, H),
        ],
        (6, 4) => vec![
            (0, 0, 1.0),
            (2, 0, H),
            (1, 1, 1.0),
            (2, 1, H),
            (4, 2, 1.0),
            (5, 3, 1.0),
        ],
        _ => discrete(from, to),
    }
}

/// The speaker each channel of a spec layout plays from, with the position to use instead when
/// a device lacks it.
fn web_speakers(channels: u32) -> Option<&'static [(u32, u32)]> {
    use speaker::*;
    match channels {
        1 => Some(&[(FC, 0)]),
        2 => Some(&[(FL, 0), (FR, 0)]),
        4 => Some(&[(FL, 0), (FR, 0), (BL, SL), (BR, SR)]),
        6 => Some(&[(FL, 0), (FR, 0), (FC, 0), (LFE, 0), (SL, BL), (SR, BR)]),
        _ => None,
    }
}

fn index_in(mask: u32, spk: u32) -> Option<u32> {
    (mask & spk != 0).then(|| (mask & (spk - 1)).count_ones())
}

/// Each channel of a spec layout placed on a device speaker, or None when one has no home.
fn place(from: u32, mask: u32) -> Option<Matrix> {
    let speakers = web_speakers(from)?;
    let mut m = Matrix::new();
    for (c, &(spk, alt)) in speakers.iter().enumerate() {
        let c = c as u32;
        if let Some(i) =
            index_in(mask, spk).or_else(|| (alt != 0).then(|| index_in(mask, alt)).flatten())
        {
            m.push((c, i, 1.0));
        } else if from == 1 && mask & speaker::STEREO == speaker::STEREO {
            m.push((0, index_in(mask, speaker::FL)?, 1.0));
            m.push((0, index_in(mask, speaker::FR)?, 1.0));
        } else {
            return None;
        }
    }
    Some(m)
}

pub fn compose(first: &Matrix, then: &Matrix) -> Matrix {
    let mut out = Matrix::new();
    for &(i, mid, g1) in first {
        for &(m2, o, g2) in then {
            if mid != m2 {
                continue;
            }
            match out.iter_mut().find(|e| e.0 == i && e.1 == o) {
                Some(e) => e.2 += g1 * g2,
                None => out.push((i, o, g1 * g2)),
            }
        }
    }
    out
}

/// The destination's `from` channels onto a device of `to` channels with speaker `mask`
/// (0 when the device does not say: the usual layout for its count is assumed).
pub fn device_mix(from: u32, interp: Interp, to: u32, mask: u32) -> Matrix {
    if from == to || interp == Interp::Discrete {
        return discrete(from, to);
    }
    let mask = if mask.count_ones() == to {
        mask
    } else {
        standard_mask(to)
    };
    if mask == 0 || web_speakers(from).is_none() {
        return discrete(from, to);
    }
    if let Some(m) = place(from, mask) {
        return m;
    }
    for narrower in [6, 4, 2, 1] {
        if narrower >= from {
            continue;
        }
        if let Some(m) = place(narrower, mask) {
            return compose(&web_mix(from, narrower, Interp::Speakers), &m);
        }
    }
    discrete(from, to)
}

pub fn is_port_map(m: &Matrix) -> bool {
    m.iter().all(|&(_, _, g)| g == 1.0)
}

/// How one connection lands on Firewheel ports: straight edges, or through a [`MixNode`].
#[derive(Clone, PartialEq, Debug)]
pub enum Route {
    Ports(Vec<(u32, u32)>),
    Mix {
        n_in: u32,
        n_out: u32,
        matrix: Matrix,
        /// Mixer input k is fed from source port `from[k]`.
        from: Vec<u32>,
        /// `(mixer output, destination port)` edges.
        to: Vec<(u32, u32)>,
    },
}

/// `src` maps a source channel to its port; `dst` a destination channel to the ports it feeds.
pub fn route(
    m: Matrix,
    n_in: u32,
    n_out: u32,
    src: impl Fn(u32) -> u32,
    dst: impl Fn(u32) -> Vec<u32>,
) -> Route {
    if is_port_map(&m) {
        let mut ports = Vec::new();
        for &(i, o, _) in &m {
            for p in dst(o) {
                if !ports.contains(&(src(i), p)) {
                    ports.push((src(i), p));
                }
            }
        }
        return Route::Ports(ports);
    }
    let to = (0..n_out)
        .flat_map(|o| dst(o).into_iter().map(move |p| (o, p)))
        .collect();
    Route::Mix {
        n_in,
        n_out,
        from: (0..n_in).map(src).collect(),
        to,
        matrix: m,
    }
}

/// Applies a mixing matrix. Inserted on a connection only when the gains are not all 1, so a
/// stereo graph never has one.
#[derive(Clone)]
pub struct MixNode {
    pub n_in: u32,
    pub n_out: u32,
    pub matrix: Matrix,
}

impl AudioNode for MixNode {
    type Configuration = EmptyConfig;

    fn info(&self, _config: &Self::Configuration) -> AudioNodeInfo {
        AudioNodeInfo::new()
            .debug_name("webaudio_channel_mix")
            .channel_config(ChannelConfig {
                num_inputs: ChannelCount::new(self.n_in).unwrap(),
                num_outputs: ChannelCount::new(self.n_out).unwrap(),
            })
    }

    fn construct_processor(
        &self,
        _config: &Self::Configuration,
        _cx: ConstructProcessorContext,
    ) -> impl AudioNodeProcessor {
        MixProcessor {
            matrix: self.matrix.clone(),
        }
    }
}

struct MixProcessor {
    matrix: Matrix,
}

impl AudioNodeProcessor for MixProcessor {
    fn process(
        &mut self,
        info: &ProcInfo,
        buffers: ProcBuffers,
        _events: &mut firewheel::event::ProcEvents,
        _extra: &mut ProcExtra,
    ) -> ProcessStatus {
        let mask = mix_block(
            &self.matrix,
            buffers.inputs,
            buffers.outputs,
            info.frames,
            |c| info.in_silence_mask.is_channel_silent(c),
        );
        ProcessStatus::outputs_modified_with_silence_mask(mask)
    }
}

pub fn mix_block(
    matrix: &Matrix,
    inputs: &[&[f32]],
    outputs: &mut [&mut [f32]],
    frames: usize,
    silent: impl Fn(usize) -> bool,
) -> firewheel::mask::SilenceMask {
    let mut mask = firewheel::mask::SilenceMask::NONE_SILENT;
    for (o, out) in outputs.iter_mut().enumerate() {
        out[..frames].fill(0.0);
        let mut touched = false;
        for &(i, to, g) in matrix {
            let i = i as usize;
            if to as usize != o || silent(i) {
                continue;
            }
            touched = true;
            for (y, &x) in out[..frames].iter_mut().zip(&inputs[i][..frames]) {
                *y += g * x;
            }
        }
        if !touched {
            mask.set_channel(o, true);
        }
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;
    use speaker::*;

    fn apply(m: &Matrix, input: &[f32], n_out: usize) -> Vec<f32> {
        let mut out = vec![0.0; n_out];
        for &(i, o, g) in m {
            out[o as usize] += g * input[i as usize];
        }
        out
    }

    #[test]
    fn the_spec_down_mixes() {
        let s = Interp::Speakers;
        let five_one = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let mono = apply(&web_mix(6, 1, s), &five_one, 1)[0];
        assert!((mono - (H * (1.0 + 2.0) + 3.0 + 0.5 * (5.0 + 6.0))).abs() < 1e-6);
        let st = apply(&web_mix(6, 2, s), &five_one, 2);
        assert!((st[0] - (1.0 + H * (3.0 + 5.0))).abs() < 1e-6);
        assert!((st[1] - (2.0 + H * (3.0 + 6.0))).abs() < 1e-6);
        assert_eq!(
            apply(&web_mix(4, 2, s), &[1.0, 2.0, 3.0, 4.0], 2),
            vec![2.0, 3.0]
        );
        assert_eq!(apply(&web_mix(2, 1, s), &[1.0, 3.0], 1), vec![2.0]);
        let q = apply(&web_mix(6, 4, s), &five_one, 4);
        assert!((q[0] - (1.0 + H * 3.0)).abs() < 1e-6 && q[2] == 5.0 && q[3] == 6.0);
    }

    #[test]
    fn up_mixes_are_port_maps_and_discrete_keeps_the_first_channels() {
        let s = Interp::Speakers;
        for (from, to) in [(1, 2), (1, 4), (1, 6), (2, 4), (2, 6), (4, 6)] {
            assert!(is_port_map(&web_mix(from, to, s)), "{from} -> {to}");
        }
        assert_eq!(web_mix(1, 6, s), vec![(0, 2, 1.0)]);
        assert_eq!(web_mix(4, 6, s)[2..], [(2, 4, 1.0), (3, 5, 1.0)]);
        assert_eq!(
            web_mix(6, 2, Interp::Discrete),
            vec![(0, 0, 1.0), (1, 1, 1.0)]
        );
        assert_eq!(web_mix(3, 2, s), vec![(0, 0, 1.0), (1, 1, 1.0)]);
        assert_eq!(web_mix(2, 3, s), vec![(0, 0, 1.0), (1, 1, 1.0)]);
    }

    #[test]
    fn a_device_gets_each_speaker_where_its_mask_puts_it() {
        let s = Interp::Speakers;
        assert_eq!(
            device_mix(6, s, 8, 0),
            vec![
                (0, 0, 1.0),
                (1, 1, 1.0),
                (2, 2, 1.0),
                (3, 3, 1.0),
                (4, 6, 1.0),
                (5, 7, 1.0)
            ]
        );
        let back_five_one = FL | FR | FC | LFE | BL | BR;
        assert_eq!(device_mix(6, s, 6, back_five_one), web_mix(6, 6, s));
        assert_eq!(
            device_mix(4, s, 6, back_five_one)[2..],
            [(2, 4, 1.0), (3, 5, 1.0)]
        );
        assert_eq!(
            device_mix(4, s, 6, FIVE_ONE)[2..],
            [(2, 4, 1.0), (3, 5, 1.0)]
        );
        assert_eq!(device_mix(1, s, 2, 0), vec![(0, 0, 1.0), (0, 1, 1.0)]);
        assert_eq!(device_mix(1, s, 12, SEVEN_ONE_FOUR), vec![(0, 2, 1.0)]);
        assert_eq!(
            device_mix(6, s, 12, SEVEN_ONE_FOUR)[4..],
            [(4, 6, 1.0), (5, 7, 1.0)]
        );
        assert_eq!(
            device_mix(2, s, 12, SEVEN_ONE_FOUR),
            vec![(0, 0, 1.0), (1, 1, 1.0)]
        );
    }

    #[test]
    fn a_device_without_a_speaker_gets_the_spec_down_mix() {
        let s = Interp::Speakers;
        assert_eq!(device_mix(6, s, 2, 0), web_mix(6, 2, s));
        assert_eq!(device_mix(6, s, 4, 0), web_mix(6, 4, s));
        let out = apply(
            &device_mix(6, s, 3, FL | FR | FC),
            &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
            3,
        );
        assert!(
            (out[0] - (1.0 + 2.0 * H)).abs() < 1e-6 && out[2] == 0.0,
            "{out:?}"
        );
        assert_eq!(device_mix(3, s, 6, 0), discrete(3, 6));
    }

    #[test]
    fn a_route_is_ports_for_unit_gains_and_a_mixer_otherwise() {
        let r = route(
            web_mix(1, 2, Interp::Speakers),
            1,
            2,
            |c| c,
            |o| vec![10 + 2 * o, 11 + 2 * o],
        );
        assert_eq!(r, Route::Ports(vec![(0, 10), (0, 11), (0, 12), (0, 13)]));
        match route(
            web_mix(2, 1, Interp::Speakers),
            2,
            1,
            |c| c + 4,
            |o| vec![o],
        ) {
            Route::Mix {
                from,
                to,
                n_in,
                n_out,
                ..
            } => {
                assert_eq!((from, to, n_in, n_out), (vec![4, 5], vec![(0, 0)], 2, 1));
            }
            r => panic!("{r:?}"),
        }
    }

    #[test]
    fn mix_block_marks_untouched_outputs_silent() {
        let m = web_mix(2, 1, Interp::Speakers);
        let (a, b) = ([1.0f32; 4], [3.0f32; 4]);
        let mut o0 = [9.0f32; 4];
        let mut o1 = [9.0f32; 4];
        let mut outs: [&mut [f32]; 2] = [&mut o0, &mut o1];
        let mask = mix_block(&m, &[&a, &b], &mut outs, 4, |c| c == 1);
        assert_eq!(o0, [0.5; 4]);
        assert_eq!(o1, [0.0; 4]);
        assert!(!mask.is_channel_silent(0) && mask.is_channel_silent(1));
    }
}
