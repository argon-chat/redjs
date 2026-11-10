//! Streaming PCM input — the seam a native producer (a WebRTC/LiveKit remote
//! track, an app's own voice mixer, a capture bridge) pushes decoded frames
//! into so they come out of the Web Audio graph like any other source.
//!
//! # Why this is not `createMediaElementSource` or `MediaStreamAudioSourceNode`
//!
//! Both of those browser APIs assume the *engine* owns the decode: you hand it
//! a `<video>` or a `MediaStream` and it pulls. Here the producer owns the
//! decode and pushes — it already has `f32` frames, at its own rate, arriving
//! on its own thread, and it needs somewhere to put them. That is a different
//! shape, so it gets a different, visibly non-standard name
//! (`RedStreamSourceNode` / `createRedStreamSource`) rather than a
//! browser-looking one that behaves differently from the browser.
//!
//! # The contract
//!
//! ```ignore
//! // JS (or the host) creates the node and connects it into the graph:
//! //   const node = ctx.createRedStreamSource({ sampleRate: 48000, channelCount: 1 });
//! //   node.connect(ctx.destination);
//! //   sendToNative(node.streamId);
//!
//! // Native, on ANY thread, for the life of the track:
//! let sink = redjs::webapi::pcm::sink(stream_id).unwrap();
//! loop {
//!     let frames: &[f32] = decode_next();      // INTERLEAVED, `channels` wide
//!     match sink.push_interleaved(frames) {
//!         PushOutcome::Ok => {}
//!         PushOutcome::Overflow { frames_pushed } => { /* dropped the rest */ }
//!         PushOutcome::UnderflowCorrected { .. } => { /* zeros were inserted */ }
//!         PushOutcome::NotReady => { /* graph not consuming yet; drop */ }
//!     }
//! }
//! ```
//!
//! ## Threading
//!
//! `sink()` returns a `PcmSink` that is `Send + Sync` and **does not touch the
//! JS context or the Firewheel graph** — pushes go straight into a
//! lock-free-ish resampling channel shared with the audio thread. This is the
//! whole reason the sink is a separate registry from `audio.rs`'s `Glob`:
//! `FirewheelContext` is `!Send` and may only be used from the thread that owns
//! the JS context, while a network track's frames arrive wherever the network
//! stack put them.
//!
//! Pushing after the node has been closed or garbage-collected is safe and
//! returns `PushOutcome::NotReady` — a producer that outlives its node does not
//! need a shutdown handshake.
//!
//! ## Rate and channel conversion
//!
//! The push rate is declared once, at node creation, and `fixed_resample`
//! converts to the output stream's rate continuously (including slow drift
//! between two nominally-48 kHz clocks — the thing that makes naive
//! network-audio playout click every few minutes). The channel count is fixed
//! at creation and pushes MUST be interleaved at exactly that width; a producer
//! whose channel count changes mid-track closes the node and opens a new one.
//!
//! ## Backpressure
//!
//! The channel holds `capacity_seconds` (default 400 ms) and targets
//! `latency_seconds` (default 150 ms) of buffered audio:
//!
//! - **Overflow** (producer faster than the device): frames past the capacity
//!   are DROPPED, and `PushOutcome::Overflow` names how many made it. There is
//!   no blocking — blocking a network thread on an audio device is how you get
//!   a stall instead of a glitch. `fixed_resample` also pre-emptively discards
//!   down to the latency target past 75 % of capacity, which turns a rare
//!   audible dropout into a continuous small correction.
//! - **Underflow** (producer slower, or a late packet): the audio thread reads
//!   zeros, `underflow_occurred()` latches, and the channel inserts silence to
//!   re-establish the latency target. Nothing crashes and the graph keeps
//!   running.
//!
//! Both conditions are observable (`overflowed()` / `underflowed()`, each a
//! read-and-clear) so a caller can log or adapt instead of guessing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

pub use firewheel::nodes::stream::writer::StreamWriterState;
use firewheel::nodes::stream::writer::PushStatus;

/// What a push did. A flattened `fixed_resample::PushStatus` so callers do not
/// need that crate in their dependency graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// Every frame was accepted.
    Ok,
    /// The graph is not consuming from this node (yet, or any more): no node,
    /// no output stream, the node was closed, or the processor has not started
    /// reading. Nothing was pushed. Not an error — drop the frames.
    NotReady,
    /// The channel filled up; `frames_pushed` of the submitted frames made it
    /// and the rest were dropped.
    Overflow { frames_pushed: usize },
    /// Everything was pushed, plus `zero_frames` of silence to re-establish
    /// the latency target after the producer fell behind.
    UnderflowCorrected { zero_frames: usize },
}

/// Options for a streaming-PCM node. Defaults match `fixed_resample`'s, which
/// are tuned for exactly this case (network audio into a device clock).
#[derive(Debug, Clone, Copy)]
pub struct PcmStreamConfig {
    /// The rate the producer will push at.
    pub sample_rate: u32,
    /// Interleave width of every push. Fixed for the node's lifetime.
    pub channels: u32,
    /// Target buffered audio. Lower = less mouth-to-ear delay, more underruns.
    pub latency_seconds: f64,
    /// Hard ceiling on buffered audio; must be > `latency_seconds`.
    pub capacity_seconds: f64,
}

impl Default for PcmStreamConfig {
    fn default() -> Self {
        Self { sample_rate: 48_000, channels: 2, latency_seconds: 0.15, capacity_seconds: 0.4 }
    }
}

impl PcmStreamConfig {
    /// Clamp to values the channel can actually be built from.
    pub fn sanitized(mut self) -> Self {
        self.sample_rate = self.sample_rate.clamp(4_000, 384_000);
        self.channels = self.channels.clamp(1, 16);
        self.latency_seconds = self.latency_seconds.clamp(0.005, 30.0);
        // The crate's own guidance: capacity should be at least twice latency.
        self.capacity_seconds = self.capacity_seconds.clamp(self.latency_seconds * 2.0, 60.0);
        self
    }
}

struct Entry {
    state: Arc<Mutex<StreamWriterState>>,
    cfg: PcmStreamConfig,
}

/// Producer-side registry, deliberately separate from `audio.rs`'s `Glob`.
///
/// `Glob` owns `FirewheelContext`, which is `!Send`; this map owns only the
/// resampling channel producers, which are. Keeping them apart is what lets a
/// network thread push without ever being able to reach the graph.
fn sinks() -> &'static Mutex<HashMap<u64, Entry>> {
    static S: OnceLock<Mutex<HashMap<u64, Entry>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Publish a started `StreamWriterState` under `id`. Called by `audio.rs` on
/// the JS thread when the node is created.
pub(crate) fn register(id: u64, state: StreamWriterState, cfg: PcmStreamConfig) {
    sinks().lock().unwrap().insert(id, Entry { state: Arc::new(Mutex::new(state)), cfg });
}

/// Drop the producer half. Later pushes return `NotReady`.
pub(crate) fn unregister(id: u64) {
    sinks().lock().unwrap().remove(&id);
}

/// A push handle for one streaming-PCM node.
///
/// Cheap to clone, safe to keep past the node's death, and safe to use from any
/// thread. Holding one does NOT keep the JS node alive — the node's lifetime is
/// the graph's business; this is only the pipe into it.
#[derive(Clone)]
pub struct PcmSink {
    id: u64,
    state: Arc<Mutex<StreamWriterState>>,
    cfg: PcmStreamConfig,
}

/// Look up the sink for a `streamId` handed out by `createRedStreamSource`.
///
/// Returns `None` if the id was never issued, or if the node has been closed.
pub fn sink(stream_id: u64) -> Option<PcmSink> {
    let map = sinks().lock().unwrap();
    let e = map.get(&stream_id)?;
    Some(PcmSink { id: stream_id, state: Arc::clone(&e.state), cfg: e.cfg })
}

/// Every live streaming-PCM node id, newest last. Diagnostics.
pub fn live_streams() -> Vec<u64> {
    let mut v: Vec<u64> = sinks().lock().unwrap().keys().copied().collect();
    v.sort_unstable();
    v
}

impl PcmSink {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The rate this sink's frames are interpreted at (NOT the device rate;
    /// conversion to the device happens downstream).
    pub fn sample_rate(&self) -> u32 {
        self.cfg.sample_rate
    }

    /// The interleave width every push must use.
    pub fn channels(&self) -> u32 {
        self.cfg.channels
    }

    /// Push interleaved frames. `data.len()` should be a multiple of
    /// `channels()`; a trailing partial frame is ignored.
    ///
    /// Realtime-safe (no allocation), non-blocking, and safe to call from any
    /// thread — including an audio callback belonging to somebody else.
    pub fn push_interleaved(&self, data: &[f32]) -> PushOutcome {
        let ch = self.cfg.channels as usize;
        if ch == 0 || data.len() < ch {
            return PushOutcome::Ok; // nothing to do
        }
        let usable = data.len() - (data.len() % ch);
        let mut st = self.state.lock().unwrap();
        match st.push_interleaved(&data[..usable]) {
            PushStatus::Ok => PushOutcome::Ok,
            PushStatus::OutputNotReady => PushOutcome::NotReady,
            PushStatus::OverflowOccurred { num_frames_pushed } => {
                PushOutcome::Overflow { frames_pushed: num_frames_pushed }
            }
            PushStatus::UnderflowCorrected { num_zero_frames_pushed } => {
                PushOutcome::UnderflowCorrected { zero_frames: num_zero_frames_pushed }
            }
        }
    }

    /// Frames that can be pushed right now without overflowing. `0` when the
    /// graph is not consuming.
    pub fn available_frames(&self) -> usize {
        self.state.lock().unwrap().available_frames()
    }

    /// Seconds of audio currently buffered ahead of the device, or `None` when
    /// there is no active stream.
    pub fn occupied_seconds(&self) -> Option<f64> {
        self.state.lock().unwrap().occupied_seconds()
    }

    /// Read-and-clear: did the device run dry since the last check?
    pub fn underflowed(&self) -> bool {
        self.state.lock().unwrap().underflow_occurred()
    }

    /// Read-and-clear: did the producer outrun the device since the last check?
    pub fn overflowed(&self) -> bool {
        self.state.lock().unwrap().overflow_occurred()
    }

    /// Is the graph actually pulling from this node?
    pub fn is_ready(&self) -> bool {
        self.state.lock().unwrap().is_ready()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_is_clamped_into_something_buildable() {
        let c = PcmStreamConfig {
            sample_rate: 0,
            channels: 0,
            latency_seconds: 0.0,
            capacity_seconds: 0.0,
        }
        .sanitized();
        assert!(c.sample_rate >= 4_000);
        assert!(c.channels >= 1);
        assert!(c.latency_seconds > 0.0);
        // The invariant `fixed_resample` asks for.
        assert!(c.capacity_seconds >= c.latency_seconds * 2.0);

        let c = PcmStreamConfig {
            sample_rate: 10_000_000,
            channels: 999,
            latency_seconds: 1e9,
            capacity_seconds: 1.0,
        }
        .sanitized();
        assert!(c.sample_rate <= 384_000);
        assert!(c.channels <= 16);
        assert!(c.capacity_seconds >= c.latency_seconds * 2.0);
    }

    #[test]
    fn an_unknown_stream_id_has_no_sink() {
        assert!(sink(u64::MAX).is_none());
    }
}
