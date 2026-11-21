//! Between the audio callback and a JavaScript `AudioWorkletProcessor`. The processor runs on
//! the worklet's own thread, never in the callback: the callback leaves its input in one ring,
//! says how many frames it has asked for, and takes what the processor has rendered from the
//! other ring. A processor that falls behind costs its own node silence, and those frames stay
//! in the ring as a cushion, so the delay settles at what this machine's callback needs.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::thread::Thread;

use firewheel::{
    channel_config::{ChannelConfig, ChannelCount},
    collector::ArcGc,
    event::ProcEvents,
    node::{
        AudioNode, AudioNodeInfo, AudioNodeProcessor, ConstructProcessorContext, EmptyConfig,
        ProcBuffers, ProcExtra, ProcInfo, ProcessStatus,
    },
};

pub(super) const QUANTUM: usize = 128;
pub(super) const MAX_PARAMS: usize = 32;
pub(super) const CHANNELS: usize = 2;
/// How far ahead of the callback a processor without an input renders, and the silence a
/// processor with one starts behind.
pub(super) const LEAD_FRAMES: usize = 2 * QUANTUM;
const RING_FRAMES: usize = 16_384;

/// Interleaved stereo samples, one writer and one reader.
pub(super) struct Ring {
    samples: Box<[AtomicU32]>,
    written: AtomicUsize,
    read: AtomicUsize,
}

impl Ring {
    fn new() -> Self {
        Self {
            samples: (0..RING_FRAMES * CHANNELS)
                .map(|_| AtomicU32::new(0))
                .collect(),
            written: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
        }
    }

    pub fn frames(&self) -> usize {
        self.written
            .load(Ordering::Acquire)
            .wrapping_sub(self.read.load(Ordering::Acquire))
            / CHANNELS
    }

    /// Writes whole frames from two channels; frames the ring has no room for are dropped.
    pub fn push(&self, left: &[f32], right: &[f32]) {
        let written = self.written.load(Ordering::Relaxed);
        let free = self.samples.len() - written.wrapping_sub(self.read.load(Ordering::Acquire));
        let frames = left.len().min(right.len()).min(free / CHANNELS);
        for i in 0..frames {
            let at = written.wrapping_add(i * CHANNELS);
            self.samples[at % self.samples.len()].store(left[i].to_bits(), Ordering::Relaxed);
            self.samples[(at + 1) % self.samples.len()]
                .store(right[i].to_bits(), Ordering::Relaxed);
        }
        self.written
            .store(written.wrapping_add(frames * CHANNELS), Ordering::Release);
    }

    /// Reads up to `left.len()` frames and returns how many there were.
    pub fn pop(&self, left: &mut [f32], right: &mut [f32]) -> usize {
        let read = self.read.load(Ordering::Relaxed);
        let ready = self.written.load(Ordering::Acquire).wrapping_sub(read) / CHANNELS;
        let frames = left.len().min(right.len()).min(ready);
        for i in 0..frames {
            let at = read.wrapping_add(i * CHANNELS);
            left[i] = f32::from_bits(self.samples[at % self.samples.len()].load(Ordering::Relaxed));
            right[i] =
                f32::from_bits(self.samples[(at + 1) % self.samples.len()].load(Ordering::Relaxed));
        }
        self.read
            .store(read.wrapping_add(frames * CHANNELS), Ordering::Release);
        frames
    }
}

pub(super) struct Bridge {
    pub input: Ring,
    pub output: Ring,
    /// Frames the callback has asked for since the node was made.
    pub demand: AtomicU64,
    params: [AtomicU32; MAX_PARAMS],
    pub has_input: bool,
    pub has_output: bool,
    /// One output channel, played on both.
    pub mono: bool,
    /// Cleared when the processor is done: it returned false with no input, or it threw.
    pub active: AtomicBool,
    worklet: OnceLock<Thread>,
}

impl Bridge {
    pub fn new(has_input: bool, has_output: bool, mono: bool) -> Self {
        let bridge = Self {
            input: Ring::new(),
            output: Ring::new(),
            demand: AtomicU64::new(0),
            params: core::array::from_fn(|_| AtomicU32::new(0)),
            has_input,
            has_output,
            mono,
            active: AtomicBool::new(true),
            worklet: OnceLock::new(),
        };
        if has_input && has_output {
            let silence = [0.0; LEAD_FRAMES];
            bridge.output.push(&silence, &silence);
        }
        bridge
    }

    /// The channels of the output a processor writes.
    pub fn channels(&self) -> usize {
        if self.mono {
            1
        } else {
            CHANNELS
        }
    }

    pub fn param(&self, index: usize) -> f32 {
        f32::from_bits(self.params[index].load(Ordering::Relaxed))
    }

    pub fn set_param(&self, index: usize, value: f32) {
        if let Some(cell) = self.params.get(index) {
            cell.store(value.to_bits(), Ordering::Relaxed);
        }
    }

    /// The thread to wake when the callback has asked for more.
    pub fn attach(&self, worklet: Thread) {
        let _ = self.worklet.set(worklet);
    }
}

#[derive(Clone)]
pub(super) struct WorkletNode {
    pub bridge: ArcGc<Bridge>,
}

impl AudioNode for WorkletNode {
    type Configuration = EmptyConfig;

    fn info(&self, _config: &Self::Configuration) -> AudioNodeInfo {
        AudioNodeInfo::new()
            .debug_name("webaudio_worklet")
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
        WorkletProcessor {
            bridge: ArcGc::clone(&self.bridge),
        }
    }
}

struct WorkletProcessor {
    bridge: ArcGc<Bridge>,
}

impl AudioNodeProcessor for WorkletProcessor {
    fn process(
        &mut self,
        info: &ProcInfo,
        buffers: ProcBuffers,
        _events: &mut ProcEvents,
        _extra: &mut ProcExtra,
    ) -> ProcessStatus {
        let n = info.frames;
        let bridge = &*self.bridge;
        let [left, right] = buffers.outputs else {
            return ProcessStatus::ClearAllOutputs;
        };
        if !bridge.active.load(Ordering::Relaxed) && bridge.output.frames() == 0 {
            return ProcessStatus::ClearAllOutputs;
        }
        if bridge.has_input {
            if let [in_left, in_right] = buffers.inputs {
                bridge.input.push(&in_left[..n], &in_right[..n]);
            }
        }
        bridge.demand.fetch_add(n as u64, Ordering::Release);
        if let Some(worklet) = bridge.worklet.get() {
            worklet.unpark();
        }
        let got = bridge.output.pop(&mut left[..n], &mut right[..n]);
        left[got..n].fill(0.0);
        right[got..n].fill(0.0);
        ProcessStatus::OutputsModified
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ring_hands_frames_over_in_order_and_drops_what_does_not_fit() {
        let ring = Ring::new();
        let ramp: Vec<f32> = (0..RING_FRAMES + 10).map(|i| i as f32).collect();
        let minus: Vec<f32> = ramp.iter().map(|v| -v).collect();
        ring.push(&ramp[..300], &minus[..300]);
        let (mut left, mut right) = ([0.0; 200], [0.0; 200]);
        assert_eq!(ring.pop(&mut left, &mut right), 200);
        assert_eq!((left[199], right[199]), (199.0, -199.0));
        ring.push(&ramp, &minus);
        assert_eq!(ring.frames(), RING_FRAMES);
        assert_eq!(ring.pop(&mut left, &mut right), 200);
        assert_eq!((left[0], left[100]), (200.0, 0.0));
    }
}
