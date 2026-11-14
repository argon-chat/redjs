//! `MediaStream` / `MediaStreamTrack` (Media Capture and Streams), audio only:
//! the sources, the tracks that carry them, and the consumers that read them.
//! The JS classes are `mediastream.js`, built over the natives at the bottom
//! of this file. The two Web Audio nodes that meet a stream are in `audio.rs`
//! (`MediaStreamAudioSourceNode` reads a track, `MediaStreamAudioDestinationNode`
//! is one).
//!
//! ## The model
//!
//! A **source** produces interleaved `f32` frames at its own rate and channel
//! count. There are two kinds:
//!
//! - a **microphone**, opened by `getUserMedia({ audio })`: a CPAL input
//!   stream on a thread of its own, pushing from the device callback;
//! - a **graph tap**, the input of a `MediaStreamAudioDestinationNode`,
//!   pushing from the audio thread once per render block.
//!
//! A **track** is one view of a source, with its own `enabled` and
//! `readyState`. `clone()` makes another track of the same source. A disabled
//! track delivers silence, and an ended one delivers nothing (Media Capture
//! §4.3.1). When no live track of a microphone is left, because each has
//! ended or been collected, the device is closed, as a browser releases it.
//!
//! A **consumer** reads a track:
//!
//! - a `MediaStreamAudioSourceNode` pushes into its streaming-PCM node
//!   (`pcm.rs`), which resamples to its own graph's rate;
//! - an embedder subscribes through [`subscribe`]. skathi's voice layer
//!   reads the track a page publishes to a LiveKit room that way.
//!
//! Consumers run on the producing thread (the device callback or the audio
//! thread), so they must not block. Every lock on that path is a `try_lock`:
//! a block that meets a contended list is dropped rather than waited for. The
//! lists only change when something subscribes, stops or is collected.
//!
//! ## The capture gate
//!
//! A browser asks the user before it opens a microphone. The asking is the
//! embedder's (its `PermissionHost`, `permissions.rs`); the order is this
//! runtime's ([`gate`]):
//!
//! 1. the process has not been forbidden capture ([`forbid_capture`], a latch
//!    an embedder sets from its own kill switch): otherwise a refusal, never
//!    a prompt;
//! 2. the embedder enabled capture ([`set_capture_allowed`]; off by default,
//!    so an application cannot reach a device unless its host decided it may);
//! 3. the host's state for `microphone`:
//!    - `Denied` refuses, as Chrome does, with "Permission denied by system"
//!      when the OS's own settings say no ([`set_system_denied_hint`] adds
//!      where to change them, in an advisory) and "Permission denied" when
//!      the user said no to the host's prompt;
//!    - `Prompt` asks: `getUserMedia` calls the host's `request`, waits for
//!      the answer (the pump settles it) and opens on `Granted`. Any other
//!      answer is a `NotAllowedError`, "Permission dismissed" when nothing
//!      was decided;
//!    - `Granted` and `Unsupported` (no notion of consent) open.
//!
//! A refusal is a `NotAllowedError`, which is what a page sees when the user
//! says no.
//!
//! [`set_fake_capture`] swaps every device for a synthetic one (a 440 Hz
//! sine), for automated runs that exercise the whole path without a
//! microphone: the gate still applies.
//!
//! ## Divergences
//!
//! - Audio only. `getUserMedia({ video })` rejects with `NotSupportedError`.
//! - The device opens in its own shared-mode format, usually 48 kHz at its
//!   own channel count. `sampleRate`, `channelCount`, `sampleSize` and
//!   `latency` constraints are read but not applied.
//! - `echoCancellation`, `noiseSuppression` and `autoGainControl` default as
//!   in Chrome (on unless a constraint turns them off) and run on the capture
//!   side, before any consumer, but only through the embedder's processing
//!   ([`set_capture_processing`]): this runtime has none of its own. The echo
//!   canceller's far end is what the page's `AudioContext`s play
//!   (`audio::set_render_reference`). `getSettings()` reports what runs; a
//!   processed track is mono at the device's rate.
//! - The tracks are not transferable, and `MediaStreamTrack` has no `stats`.
//!
//! ## A microphone on the default follows it
//!
//! A track opened without a `deviceId` (or with `"default"`) captures the
//! system default input, and keeps doing so when the user changes it, as
//! Chrome's does: its capture thread reopens the stream on the new default
//! and the source carries on, with the same tracks, at the new device's
//! format. When its device goes away, it moves to whichever input is default
//! then, and its tracks end only when no input is left. A track opened on a
//! chosen device stays on it and ends when it goes away. Following needs the
//! embedder's notifications (`mediadevices::notify_default_changed`).
//! Without them, a capture whose device goes away still tries the default
//! once before its tracks end.

use core::ffi::c_void;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use super::permissions::{self, Capability, PermissionState};
use super::util::*;
use crate::glue;

/// A consumer of a track's frames: `(interleaved, channels, sample_rate)`.
pub type Consumer = Box<dyn FnMut(&[f32], usize, u32) + Send>;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// The capture gate
// ---------------------------------------------------------------------------

static ALLOWED: AtomicBool = AtomicBool::new(false);
static FORBIDDEN: AtomicBool = AtomicBool::new(false);

/// Let `getUserMedia` open input devices. Off by default. Has no effect after
/// [`forbid_capture`].
pub fn set_capture_allowed(on: bool) {
    ALLOWED.store(on, Ordering::Release);
}

/// Refuse every capture in this process from now on, whatever else says.
/// A latch: nothing turns it back off.
pub fn forbid_capture() {
    FORBIDDEN.store(true, Ordering::Release);
}

/// Has [`forbid_capture`] latched? An embedder's permission layer reads it so
/// that it never prompts for what this process will refuse anyway.
pub fn capture_forbidden() -> bool {
    FORBIDDEN.load(Ordering::Acquire)
}

static FAKE: AtomicBool = AtomicBool::new(false);

/// Capture from a synthetic microphone (a 440 Hz sine at 48 kHz, mono)
/// instead of any device, for the whole process. `enumerateDevices` lists
/// that one input (`"fake"`, "Fake Microphone"), and no device is ever
/// opened. The gate is unchanged: a test of the consent path runs it as a
/// user would meet it. A latch, like [`forbid_capture`].
pub fn set_fake_capture() {
    FAKE.store(true, Ordering::Release);
}

/// Has [`set_fake_capture`] latched?
pub fn fake_capture() -> bool {
    FAKE.load(Ordering::Acquire)
}

/// What the embedder adds to a "not enabled" refusal, so the developer reading
/// it knows which switch is theirs (skathi names its config key).
fn hint() -> &'static Mutex<String> {
    static H: OnceLock<Mutex<String>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(String::new()))
}

pub fn set_capture_hint(text: &str) {
    *hint().lock().unwrap() = text.to_string();
}

/// Where the user changes the operating system's microphone setting, for the
/// advisory a "Permission denied by system" refusal logs (skathi names the
/// Windows Settings page). The rejection's own message stays Chrome's.
fn system_hint() -> &'static Mutex<String> {
    static H: OnceLock<Mutex<String>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(String::new()))
}

pub fn set_system_denied_hint(text: &str) {
    *system_hint().lock().unwrap() = text.to_string();
}

/// Per-context overrides of [`set_capture_allowed`] (tests).
fn ctx_allowed() -> &'static Mutex<HashMap<usize, bool>> {
    static M: OnceLock<Mutex<HashMap<usize, bool>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The gate's answer for one `getUserMedia` call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    Open,
    /// [`forbid_capture`] latched.
    Forbidden,
    /// The embedder never enabled capture.
    NotEnabled,
    /// The host says no: the OS's own settings (`system`), or the user's
    /// answer to the host's prompt.
    Denied { system: bool },
    /// Nobody has decided: ask the host, open on a grant.
    Ask,
}

/// The gate, as a pure function of its inputs. `os` is the host's state for
/// `microphone` (`None` with no `PermissionHost` installed) and `system`
/// whether a `Denied` is the OS's ([`permissions::PermissionHost::denied_by_system`]).
/// `Unsupported` is open: where there is no notion of consent there is
/// nothing to ask.
pub fn gate(forbidden: bool, allowed: bool, os: Option<PermissionState>, system: bool) -> Gate {
    if forbidden {
        Gate::Forbidden
    } else if !allowed {
        Gate::NotEnabled
    } else {
        match os {
            Some(PermissionState::Denied) => Gate::Denied { system },
            Some(PermissionState::Prompt) => Gate::Ask,
            _ => Gate::Open,
        }
    }
}

fn gate_for(ctx: usize) -> Gate {
    let allowed = ctx_allowed()
        .lock()
        .unwrap()
        .get(&ctx)
        .copied()
        .unwrap_or_else(|| ALLOWED.load(Ordering::Acquire));
    let forbidden = FORBIDDEN.load(Ordering::Acquire);
    // The host is not asked at all when the answer cannot matter.
    if forbidden || !allowed {
        return gate(forbidden, allowed, None, true);
    }
    let host = permissions::host();
    let os = host.map(|h| h.status(Capability::Microphone));
    let system = match (host, os) {
        (Some(h), Some(PermissionState::Denied)) => h.denied_by_system(Capability::Microphone),
        _ => true,
    };
    gate(forbidden, allowed, os, system)
}

/// Chrome's messages, verbatim: what a page already matches on.
const DENIED: &str = "Permission denied";
const DENIED_BY_SYSTEM: &str = "Permission denied by system";
const DISMISSED: &str = "Permission dismissed";

/// The refusal a closed gate turns into: `(DOMException name, message)`.
/// `None` for a gate that opens or asks.
fn refusal(g: Gate) -> Option<(&'static str, String)> {
    match g {
        Gate::Open | Gate::Ask => None,
        Gate::Forbidden => Some((
            "NotAllowedError",
            "Permission denied: microphone capture is switched off for this process.".into(),
        )),
        Gate::NotEnabled => {
            let h = hint().lock().unwrap().clone();
            Some((
                "NotAllowedError",
                if h.is_empty() {
                    "Permission denied: this host has not enabled microphone capture.".into()
                } else {
                    format!("Permission denied: this host has not enabled microphone capture. {h}")
                },
            ))
        }
        Gate::Denied { system: true } => {
            let h = system_hint().lock().unwrap().clone();
            advise_once(&if h.is_empty() {
                "getUserMedia: the operating system's privacy settings do not let this app use the \
                 microphone (NotAllowedError: Permission denied by system)."
                    .to_string()
            } else {
                format!(
                    "getUserMedia: the operating system's privacy settings do not let this app use the \
                     microphone (NotAllowedError: Permission denied by system). {h}"
                )
            });
            Some(("NotAllowedError", DENIED_BY_SYSTEM.into()))
        }
        Gate::Denied { system: false } => Some(("NotAllowedError", DENIED.into())),
    }
}

/// Should `enumerateDevices` name the inputs? Only when they could be opened
/// without asking; otherwise it lists them the way a browser does before
/// permission (one entry, no id, no label).
pub(crate) fn inputs_visible(ctx: usize) -> bool {
    gate_for(ctx) == Gate::Open
}

// ---------------------------------------------------------------------------
// Sources and tracks
// ---------------------------------------------------------------------------

/// What produces a source's frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// A capture device. `device_id` is what `getSettings().deviceId` says:
    /// the CPAL id `enumerateDevices` handed out, or `"default"`.
    Microphone { device_id: String },
    /// A `MediaStreamAudioDestinationNode`.
    Graph,
    /// Frames an embedder receives from the network and delivers itself
    /// ([`remote_source`]): skathi's remote call participants.
    Remote,
}

/// The three processing switches of an audio track. In an [`AudioRequest`],
/// what the page asked for (Chrome's defaults applied: each is on unless the
/// constraints turn it off). On a source, what is in effect, which
/// `getSettings()` reports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub echo_cancellation: bool,
    pub noise_suppression: bool,
    pub auto_gain_control: bool,
}

impl Settings {
    pub fn any(&self) -> bool {
        self.echo_cancellation || self.noise_suppression || self.auto_gain_control
    }
}

// ---------------------------------------------------------------------------
// Processing (the embedder's)
// ---------------------------------------------------------------------------

/// Echo cancellation, noise suppression and gain control on a microphone, as
/// a browser runs them on the capture side, so every consumer of the track
/// (a Web Audio graph, a published call track) hears the processed signal.
/// This runtime has none of its own: the embedder installs one
/// ([`set_capture_processing`]).
pub trait CaptureProcessor: Send {
    /// One 10 ms block, mono, at the rate the processor was made for,
    /// processed in place. Runs on the capture thread.
    fn process(&mut self, block: &mut [f32]);
}

/// What a processor is asked to do for one microphone.
#[derive(Clone, Debug)]
pub struct ProcessingRequest {
    /// The context whose page opened it.
    pub owner: usize,
    /// The device's rate; blocks are `rate / 100` samples.
    pub rate: u32,
    /// What the page asked for, Chrome's defaults applied.
    pub wanted: Settings,
}

/// Makes the processor for one open, or `None` for the raw signal. The
/// [`Settings`] it returns are what it does, and what `getSettings()` says.
pub type ProcessorFactory = fn(&ProcessingRequest) -> Option<(Box<dyn CaptureProcessor>, Settings)>;

static PROCESSING: OnceLock<ProcessorFactory> = OnceLock::new();

/// Install the embedder's capture processing, once per process, before the
/// first `getUserMedia`. Without it a microphone is the raw device signal and
/// `getSettings()` reports all three switches off.
pub fn set_capture_processing(f: ProcessorFactory) {
    let _ = PROCESSING.set(f);
}

/// Whether an embedder installed processing (`getCapabilities()` offers the
/// switches only then).
pub fn capture_processing_available() -> bool {
    PROCESSING.get().is_some()
}

#[cfg(test)]
thread_local! {
    /// A test's factory, for the opens it starts on its own thread.
    static TEST_PROCESSING: std::cell::Cell<Option<ProcessorFactory>> = const { std::cell::Cell::new(None) };
}

/// Run on the thread that starts an open: the processor, and the settings
/// in effect (all off for the raw signal).
fn processing_for(owner: usize, rate: u32, wanted: &Settings) -> (Option<Chunker>, Settings) {
    #[cfg(test)]
    let factory = TEST_PROCESSING.with(|t| t.get()).or_else(|| PROCESSING.get().copied());
    #[cfg(not(test))]
    let factory = PROCESSING.get().copied();
    let Some(factory) = factory.filter(|_| wanted.any()) else { return (None, Settings::default()) };
    match factory(&ProcessingRequest { owner, rate, wanted: wanted.clone() }) {
        Some((p, s)) => (Some(Chunker::new(p, rate)), s),
        None => (None, Settings::default()),
    }
}

/// Cuts a device's blocks (any length, any channel count) into the 10 ms
/// mono blocks a processor takes, and hands each one on processed. The
/// down-mix is the average of the channels, as Chrome's processed track is
/// mono.
pub(crate) struct Chunker {
    proc: Box<dyn CaptureProcessor>,
    block: usize,
    pending: Vec<f32>,
}

impl Chunker {
    fn new(proc: Box<dyn CaptureProcessor>, rate: u32) -> Chunker {
        let block = (rate as usize / 100).max(1);
        Chunker { proc, block, pending: Vec::with_capacity(block * 8) }
    }

    fn feed(&mut self, data: &[f32], channels: usize, mut out: impl FnMut(&[f32])) {
        let channels = channels.max(1);
        let scale = 1.0 / channels as f32;
        self.pending.extend(data.chunks_exact(channels).map(|f| f.iter().sum::<f32>() * scale));
        let mut at = 0;
        while self.pending.len() - at >= self.block {
            let b = &mut self.pending[at..at + self.block];
            self.proc.process(b);
            out(b);
            at += self.block;
        }
        self.pending.drain(..at);
    }
}

/// What a capture thread hands its blocks to: the source, through the
/// processing when there is some.
struct Feeder {
    source: Weak<Source>,
    chunker: Option<Chunker>,
    channels: usize,
    rate: u32,
}

impl Feeder {
    fn push(&mut self, data: &[f32]) {
        let Some(s) = self.source.upgrade() else { return };
        match &mut self.chunker {
            Some(c) => c.feed(data, self.channels, |b| s.deliver(b, 1, self.rate)),
            None => s.deliver(data, self.channels, self.rate),
        }
    }
}

pub struct Source {
    pub kind: SourceKind,
    pub label: String,
    channels: AtomicU32,
    rate: AtomicU32,
    settings: Settings,
    tracks: Mutex<Vec<Weak<TrackCore>>>,
    /// The microphone's capture thread waits on this. Sending releases the
    /// device.
    stop: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    failed: AtomicBool,
}

impl Source {
    pub fn new(kind: SourceKind, label: String, channels: u32, rate: u32, settings: Settings) -> Arc<Source> {
        Arc::new(Source {
            kind,
            label,
            channels: AtomicU32::new(channels.max(1)),
            rate: AtomicU32::new(rate.max(1)),
            settings,
            tracks: Mutex::new(Vec::new()),
            stop: Mutex::new(None),
            failed: AtomicBool::new(false),
        })
    }

    pub fn channels(&self) -> u32 {
        self.channels.load(Ordering::Relaxed)
    }

    pub fn sample_rate(&self) -> u32 {
        self.rate.load(Ordering::Relaxed)
    }

    /// The graph tap learns its format from the stream its graph runs in.
    pub fn set_format(&self, channels: u32, rate: u32) {
        self.channels.store(channels.max(1), Ordering::Relaxed);
        self.rate.store(rate.max(1), Ordering::Relaxed);
    }

    pub(crate) fn set_stop(&self, tx: std::sync::mpsc::Sender<()>) {
        *self.stop.lock().unwrap() = Some(tx);
    }

    /// Hand one block to every track. Realtime path: never blocks. An upgrade
    /// here can be the last strong reference to a track; its `Drop` takes no
    /// lock this function holds.
    pub fn deliver(&self, data: &[f32], channels: usize, rate: u32) {
        let Ok(tracks) = self.tracks.try_lock() else { return };
        for w in tracks.iter() {
            if let Some(t) = w.upgrade() {
                t.deliver(data, channels, rate);
            }
        }
    }

    /// The device went away: every track ends, and the page hears `ended`.
    pub fn fail(&self) {
        if self.failed.swap(true, Ordering::AcqRel) {
            return;
        }
        let tracks: Vec<Arc<TrackCore>> = self.tracks.lock().unwrap().iter().filter_map(|w| w.upgrade()).collect();
        for t in &tracks {
            if !t.ended.swap(true, Ordering::AcqRel) {
                ended_queue().lock().unwrap().push((t.owner, t.id));
                WORK.store(true, Ordering::Release);
            }
        }
        drop(tracks);
        self.release_if_unused();
    }

    /// Close the device once nothing can hear it.
    fn release_if_unused(&self) {
        let live = {
            let tracks: Vec<Arc<TrackCore>> = {
                let mut list = self.tracks.lock().unwrap();
                list.retain(|w| w.strong_count() > 0);
                list.iter().filter_map(|w| w.upgrade()).collect()
            };
            // Counted (and the temporaries dropped) outside the list's lock.
            tracks.iter().filter(|t| t.is_live()).count()
        };
        if live == 0 {
            if let Some(tx) = self.stop.lock().unwrap().take() {
                let _ = tx.send(());
            }
        }
    }
}

pub struct TrackCore {
    pub id: u64,
    /// The context whose page holds this track.
    owner: usize,
    pub source: Arc<Source>,
    enabled: AtomicBool,
    ended: AtomicBool,
    /// The JS object (while it lives) plus every [`Subscription`]. A track
    /// nothing holds keeps no device open.
    holders: AtomicUsize,
    consumers: Mutex<Vec<(u64, Consumer)>>,
}

thread_local! {
    /// Silence for a disabled track, per producing thread, grown on demand.
    static ZEROS: std::cell::RefCell<Vec<f32>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl TrackCore {
    fn new(source: &Arc<Source>, owner: usize, enabled: bool) -> Arc<TrackCore> {
        let t = Arc::new(TrackCore {
            id: next_id(),
            owner,
            source: Arc::clone(source),
            enabled: AtomicBool::new(enabled),
            ended: AtomicBool::new(false),
            holders: AtomicUsize::new(0),
            consumers: Mutex::new(Vec::new()),
        });
        source.tracks.lock().unwrap().push(Arc::downgrade(&t));
        registry().lock().unwrap().insert(t.id, Arc::downgrade(&t));
        t
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    fn is_live(&self) -> bool {
        !self.ended() && self.holders.load(Ordering::Acquire) > 0
    }

    fn deliver(&self, data: &[f32], channels: usize, rate: u32) {
        if self.ended() {
            return;
        }
        let Ok(mut consumers) = self.consumers.try_lock() else { return };
        if consumers.is_empty() {
            return;
        }
        if self.enabled() {
            for (_, c) in consumers.iter_mut() {
                c(data, channels, rate);
            }
        } else {
            ZEROS.with(|z| {
                let mut z = z.borrow_mut();
                if z.len() < data.len() {
                    z.resize(data.len(), 0.0);
                }
                for (_, c) in consumers.iter_mut() {
                    c(&z[..data.len()], channels, rate);
                }
            });
        }
    }

    fn stop(&self) {
        if !self.ended.swap(true, Ordering::AcqRel) {
            self.consumers.lock().unwrap().clear();
            self.source.release_if_unused();
        }
    }

    fn unhold(&self) {
        if self.holders.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.source.release_if_unused();
        }
    }
}

impl Drop for TrackCore {
    fn drop(&mut self) {
        // Can run on the audio thread or a GC thread: one short lock, never
        // the source's track list (`deliver` may hold it).
        registry().lock().unwrap().remove(&self.id);
    }
}

/// Every track by id, weakly: what [`subscribe`] and the natives look up.
fn registry() -> &'static Mutex<HashMap<u64, Weak<TrackCore>>> {
    static R: OnceLock<Mutex<HashMap<u64, Weak<TrackCore>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The tracks a page holds, strongly, until their JS objects are collected.
fn held() -> &'static Mutex<HashMap<u64, Arc<TrackCore>>> {
    static H: OnceLock<Mutex<HashMap<u64, Arc<TrackCore>>>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `(owner, track)` whose `ended` the page has not heard yet.
fn ended_queue() -> &'static Mutex<Vec<(usize, u64)>> {
    static Q: OnceLock<Mutex<Vec<(usize, u64)>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(Vec::new()))
}

/// Something for [`pump`] to deliver. One relaxed load per pump otherwise.
static WORK: AtomicBool = AtomicBool::new(false);

/// The track with this id, if it is alive.
pub fn track(id: u64) -> Option<Arc<TrackCore>> {
    let t = registry().lock().unwrap().get(&id).and_then(|w| w.upgrade());
    t
}

/// A new track of `source` for a page in context `owner`. Returns its id; the
/// page's handle object holds it ([`release`] lets go).
pub fn new_track(source: &Arc<Source>, owner: usize) -> u64 {
    let t = TrackCore::new(source, owner, true);
    t.holders.fetch_add(1, Ordering::AcqRel);
    let id = t.id;
    held().lock().unwrap().insert(id, t);
    id
}

/// `track.clone()`: a new track of the same source, with the same `enabled`
/// and `readyState`.
pub fn clone_track(id: u64) -> Option<u64> {
    let t = track(id)?;
    let c = TrackCore::new(&t.source, t.owner, t.enabled());
    if t.ended() {
        c.ended.store(true, Ordering::Release);
    }
    c.holders.fetch_add(1, Ordering::AcqRel);
    let cid = c.id;
    held().lock().unwrap().insert(cid, c);
    Some(cid)
}

/// The page's object for this track went away.
pub fn release(id: u64) {
    let t = held().lock().unwrap().remove(&id);
    if let Some(t) = t {
        t.unhold();
    }
}

pub fn stop(id: u64) {
    if let Some(t) = track(id) {
        t.stop();
    }
}

pub fn set_enabled(id: u64, on: bool) {
    if let Some(t) = track(id) {
        t.enabled.store(on, Ordering::Relaxed);
    }
}

/// Tracks of context `owner` that ended by themselves since the last call.
pub fn drain_ended(owner: usize) -> Vec<u64> {
    let mut q = ended_queue().lock().unwrap();
    let mut mine = Vec::new();
    q.retain(|&(o, id)| {
        if o == owner {
            mine.push(id);
            false
        } else {
            true
        }
    });
    mine
}

/// A consumer registration; dropping it unsubscribes. It holds the track, so
/// a published track keeps sounding after the page drops its last reference
/// to the JS object, as a browser's `RTCRtpSender` keeps it.
pub struct Subscription {
    track: Arc<TrackCore>,
    id: u64,
}

impl Subscription {
    pub fn track(&self) -> &Arc<TrackCore> {
        &self.track
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.track.consumers.lock().unwrap().retain(|(id, _)| *id != self.id);
        self.track.unhold();
    }
}

/// Read a track's frames. `None` if no such track is alive, or it ended.
///
/// The consumer runs on the producing thread, once per block, in the
/// source's own format, and must not block.
pub fn subscribe(track_id: u64, consumer: Consumer) -> Option<Subscription> {
    let t = track(track_id)?;
    if t.ended() {
        return None;
    }
    let id = next_id();
    t.holders.fetch_add(1, Ordering::AcqRel);
    t.consumers.lock().unwrap().push((id, consumer));
    Some(Subscription { track: t, id })
}

/// What an embedder needs to know about a track before reading it.
#[derive(Clone, Debug)]
pub struct TrackInfo {
    pub label: String,
    pub channels: u32,
    pub sample_rate: u32,
    pub enabled: bool,
    pub ended: bool,
    pub kind: SourceKind,
}

pub fn info(track_id: u64) -> Option<TrackInfo> {
    let t = track(track_id)?;
    Some(TrackInfo {
        label: t.source.label.clone(),
        channels: t.source.channels(),
        sample_rate: t.source.sample_rate(),
        enabled: t.enabled(),
        ended: t.ended(),
        kind: t.source.kind.clone(),
    })
}

// ---------------------------------------------------------------------------
// MediaStreamAudioSourceNode pipes: node handle → its subscription
// ---------------------------------------------------------------------------

fn pipes() -> &'static Mutex<HashMap<u64, Subscription>> {
    static P: OnceLock<Mutex<HashMap<u64, Subscription>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Feed the streaming-PCM node `node` from track `track_id`. The node was
/// built at the track's rate (`pcm` resamples to the graph's) and at most
/// stereo: a wider device keeps channels 0 and 1, and a mono one fills a
/// stereo node on both sides.
pub fn pipe(node: u64, track_id: u64) -> bool {
    let Some(sink) = super::pcm::sink(node) else { return false };
    let want = sink.channels() as usize;
    let node_rate = sink.sample_rate();
    let mut fit: Vec<f32> = Vec::new();
    let mut rerate = Rerate::default();
    let sub = subscribe(
        track_id,
        Box::new(move |data, channels, rate| {
            let data = if channels == want {
                data
            } else {
                fit_channels(data, channels, want, &mut fit);
                &fit[..]
            };
            if rate == node_rate {
                let _ = sink.push_interleaved(data);
            } else {
                // The source changed its rate under the node: a microphone
                // that followed the default to another device.
                let _ = sink.push_interleaved(rerate.run(data, want, rate, node_rate));
            }
        }),
    );
    match sub {
        Some(s) => {
            pipes().lock().unwrap().insert(node, s);
            true
        }
        None => false,
    }
}

/// Converts interleaved blocks from one rate to another by linear
/// interpolation, carrying its phase and the last frame across blocks.
#[derive(Default)]
struct Rerate {
    from: u32,
    to: u32,
    channels: usize,
    /// The next output frame's position, in input frames from the start of
    /// the current block. -1.0 is `last`.
    pos: f64,
    last: Vec<f32>,
    out: Vec<f32>,
}

impl Rerate {
    fn run(&mut self, data: &[f32], channels: usize, from: u32, to: u32) -> &[f32] {
        let channels = channels.max(1);
        if (from, to, channels) != (self.from, self.to, self.channels) {
            *self = Rerate { from, to, channels, pos: 0.0, last: vec![0.0; channels], out: std::mem::take(&mut self.out) };
        }
        self.out.clear();
        let frames = data.len() / channels;
        if frames == 0 || from == 0 || to == 0 {
            return &self.out;
        }
        let step = from as f64 / to as f64;
        let frame = |i: isize, c: usize| if i < 0 { self.last[c] } else { data[i as usize * channels + c] };
        while self.pos <= (frames - 1) as f64 {
            let i = self.pos.floor() as isize;
            let k = (self.pos - i as f64) as f32;
            let j = (i + 1).min(frames as isize - 1);
            for c in 0..channels {
                let a = frame(i, c);
                self.out.push(a + (frame(j, c) - a) * k);
            }
            self.pos += step;
        }
        self.pos -= frames as f64;
        self.last.copy_from_slice(&data[(frames - 1) * channels..frames * channels]);
        &self.out
    }
}

/// Re-interleave `data` (`channels` wide) to `want` channels into `out`:
/// extra channels are dropped, a missing one repeats the last.
fn fit_channels(data: &[f32], channels: usize, want: usize, out: &mut Vec<f32>) {
    let channels = channels.max(1);
    out.clear();
    for f in 0..data.len() / channels {
        for c in 0..want {
            out.push(data[f * channels + c.min(channels - 1)]);
        }
    }
}

/// The node is gone: stop feeding it.
pub fn unpipe(node: u64) {
    let s = pipes().lock().unwrap().remove(&node);
    drop(s);
}

// ---------------------------------------------------------------------------
// Remote tracks: an embedder's network audio as a page's MediaStreamTrack
// ---------------------------------------------------------------------------
//
// A browser hands a page every remote WebRTC track as a `MediaStreamTrack`:
// the page can put it in a `MediaStream`, read it with an analyser, or play it
// through its own graph. An embedder that receives such audio itself
// (skathi's LiveKit rooms) makes a [`remote_source`], delivers the
// decoded frames to it ([`Source::deliver`], from any thread, never
// blocking), and hands the page a track of it with [`js_track`].
// [`Source::fail`] ends every track of it when the network track goes away.
//
// [`page_plays`] says whether the page plays the source itself, so an
// embedder that would otherwise play it can step aside rather than play it
// twice.

/// A source whose frames the embedder delivers.
pub fn remote_source(label: &str, channels: u32, rate: u32) -> Arc<Source> {
    Source::new(SourceKind::Remote, label.to_string(), channels, rate, Settings::default())
}

/// A new track of `source`, owned by context `g`'s page, as a
/// `MediaStreamTrack` object; `undefined` when the context has no
/// `MediaStreamTrack` (mediastream.js did not install).
///
/// # Safety
/// `g` is a live context, on its thread.
pub unsafe fn js_track(g: *mut c_void, source: &Arc<Source>) -> u64 {
    let Some(st) = state_of(g) else { return UNDEF };
    let wrap = get(g, st.hooks, "wrap");
    if !is_cell(wrap) {
        return UNDEF;
    }
    let id = new_track(source, owner_of(g));
    let h = new_handle(g, id);
    match call(g, wrap, st.hooks, &[h]) {
        Ok(v) => v,
        Err(e) => {
            report(g, e);
            UNDEF
        }
    }
}

/// Whether the page plays `source` itself: a `MediaStreamAudioSourceNode`
/// reads a live track of it and is connected, through any chain of nodes, to
/// its context's destination. A gain of 0 on the way still counts (the page
/// muted it); an analyser that leads nowhere does not.
pub fn page_plays(source: &Arc<Source>) -> bool {
    let nodes: Vec<u64> = pipes()
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, s)| Arc::ptr_eq(&s.track().source, source) && s.track().is_live())
        .map(|(n, _)| *n)
        .collect();
    nodes.into_iter().any(super::audio::reaches_destination)
}

// ---------------------------------------------------------------------------
// Opening a microphone
// ---------------------------------------------------------------------------

/// What `getUserMedia({ audio })` asked for.
#[derive(Clone, Debug, Default)]
pub struct AudioRequest {
    /// `None`: the default device.
    pub device_id: Option<String>,
    /// `deviceId: { exact }`: a missing device is an `OverconstrainedError`
    /// rather than the default.
    pub exact: bool,
    /// The processing the page wants, Chrome's defaults applied.
    pub settings: Settings,
}

pub enum OpenResult {
    /// The new track's id, already held for the page.
    Ok(u64),
    /// `(DOMException name, message)`.
    Err(&'static str, String),
}

#[derive(Default)]
struct Opens {
    /// `(owner, token)` still running. A context that goes away removes its
    /// own, and a capture thread that finds its entry gone releases what it
    /// opened instead of publishing it.
    inflight: HashSet<(usize, u32)>,
    done: Vec<(usize, u32, OpenResult)>,
}

fn opens() -> &'static Mutex<Opens> {
    static O: OnceLock<Mutex<Opens>> = OnceLock::new();
    O.get_or_init(|| Mutex::new(Opens::default()))
}

fn finish(owner: usize, token: u32, result: OpenResult) {
    let mut o = opens().lock().unwrap();
    if o.inflight.remove(&(owner, token)) {
        o.done.push((owner, token, result));
        WORK.store(true, Ordering::Release);
    } else if let OpenResult::Ok(id) = result {
        drop(o);
        stop(id);
        release(id);
    }
}

fn drain_opened(owner: usize) -> Vec<(u32, OpenResult)> {
    let mut o = opens().lock().unwrap();
    let mut mine = Vec::new();
    let mut i = 0;
    while i < o.done.len() {
        if o.done[i].0 == owner {
            let (_, token, r) = o.done.remove(i);
            mine.push((token, r));
        } else {
            i += 1;
        }
    }
    mine
}

/// Start opening a microphone for context `owner`. The gate is open. The
/// result arrives through [`pump`] under `token`.
fn open_microphone(owner: usize, token: u32, req: AudioRequest) {
    opens().lock().unwrap().inflight.insert((owner, token));
    start_open(owner, token, req);
}

/// The open itself, for an `(owner, token)` already in flight. One the page
/// abandoned meanwhile (it unloaded while the prompt was up) never starts.
fn start_open(owner: usize, token: u32, req: AudioRequest) {
    if !opens().lock().unwrap().inflight.contains(&(owner, token)) {
        return;
    }
    if fake::enabled(owner) {
        fake::open(owner, token, req);
        return;
    }
    #[cfg(test)]
    {
        // `cargo test` never opens a device, whatever a test forgot to set.
        finish(owner, token, OpenResult::Err("NotAllowedError", "Tests never open a real device.".into()));
    }
    #[cfg(not(test))]
    {
        let spawned = std::thread::Builder::new().name("redjs-mic".into()).spawn(move || {
            let (result, park) = capture::open(owner, req);
            finish(owner, token, result);
            // Holds the stream until every track has ended or been collected.
            park();
        });
        if spawned.is_err() {
            finish(owner, token, OpenResult::Err("NotReadableError", "Could not start the capture thread.".into()));
        }
    }
}

// ---------------------------------------------------------------------------
// Asking first (Gate::Ask)
// ---------------------------------------------------------------------------

/// A `getUserMedia` waiting for the host's prompt: the host's request token,
/// and what to open once it is answered.
struct Asking {
    perm: u32,
    owner: usize,
    token: u32,
    req: AudioRequest,
}

fn asking() -> &'static Mutex<Vec<Asking>> {
    static A: OnceLock<Mutex<Vec<Asking>>> = OnceLock::new();
    A.get_or_init(|| Mutex::new(Vec::new()))
}

/// Something is waiting for an answer: the pump looks for one.
static ASKING: AtomicBool = AtomicBool::new(false);

/// `getUserMedia` on a microphone nobody has decided on: ask the host, as a
/// browser shows its prompt, and open once it says yes. The open is in
/// flight from now, so a page that goes away meanwhile abandons it like any
/// other ([`forget`], [`stop_all`]).
fn ask_then_open(owner: usize, token: u32, req: AudioRequest) {
    opens().lock().unwrap().inflight.insert((owner, token));
    match permissions::request_for_runtime(Capability::Microphone) {
        permissions::RequestOutcome::Settled(state) => answered(owner, token, req, state),
        permissions::RequestOutcome::Pending(perm) => {
            asking().lock().unwrap().push(Asking { perm, owner, token, req });
            ASKING.store(true, Ordering::Release);
        }
    }
}

/// The host answered: open, or refuse the way Chrome does.
fn answered(owner: usize, token: u32, req: AudioRequest, state: PermissionState) {
    match state {
        PermissionState::Granted | PermissionState::Unsupported => start_open(owner, token, req),
        PermissionState::Denied => {
            let system = permissions::host().is_none_or(|h| h.denied_by_system(Capability::Microphone));
            finish(owner, token, OpenResult::Err("NotAllowedError", (if system { DENIED_BY_SYSTEM } else { DENIED }).into()));
        }
        PermissionState::Prompt => finish(owner, token, OpenResult::Err("NotAllowedError", DISMISSED.into())),
    }
}

/// Answers that have come in, for every context (each open then reaches its
/// own page through `finish` and [`pump`]). One relaxed load when nothing is
/// waiting.
fn settle_asked() {
    if !ASKING.load(Ordering::Acquire) {
        return;
    }
    let settled = permissions::take_runtime_settled();
    let ready: Vec<(Asking, PermissionState)> = {
        let mut a = asking().lock().unwrap();
        let mut ready = Vec::new();
        for (perm, state) in settled {
            if let Some(i) = a.iter().position(|x| x.perm == perm) {
                ready.push((a.remove(i), state));
            }
        }
        if a.is_empty() {
            ASKING.store(false, Ordering::Release);
        }
        ready
    };
    for (x, state) in ready {
        answered(x.owner, x.token, x.req, state);
    }
}

/// The format a capture of `device_id` (an `enumerateDevices` id, or
/// `"default"`) would run at, `(sample rate, channels)`: the device's own mix
/// format, which [`capture::open`] opens it with. Read off the endpoint without
/// starting a stream. `None` when the device is gone or reports no input
/// format. For `InputDeviceInfo.getCapabilities()` (webapi/mediadevices.rs).
pub(crate) fn input_format(owner: usize, device_id: &str) -> Option<(u32, u32)> {
    if fake::enabled(owner) {
        // The fake microphone's; nothing touches a real endpoint.
        return Some((48_000, 1));
    }
    #[cfg(test)]
    {
        let _ = device_id;
        None
    }
    #[cfg(not(test))]
    {
        capture::format(device_id)
    }
}

// ---------------------------------------------------------------------------
// A microphone on the default follows it
// ---------------------------------------------------------------------------

/// How often a capture on the system default wakes to see whether the
/// default moved or its device went away. Each wake reads two atomics.
#[cfg_attr(test, allow(dead_code))] // read by the capture thread, which tests never start
const FOLLOW_WAKE: std::time::Duration = std::time::Duration::from_millis(100);

/// What a capture on the system default does when it wakes.
#[derive(Debug, PartialEq, Eq)]
enum Follow {
    /// Keep the stream it has.
    Stay,
    /// Open the default and move the source onto it.
    Move,
    /// No input is left: the source fails, and its tracks end.
    End,
}

/// `on`: the endpoint the stream runs on. `default`: the system's default
/// input now, if there is one. `moved`: the default moved since the last
/// wake. `lost`: the stream reported its device gone.
fn follow(on: &str, default: Option<&str>, moved: bool, lost: bool) -> Follow {
    match default {
        None if lost => Follow::End,
        None => Follow::Stay,
        // Even the same id: a device that came back after a reset reopens.
        Some(_) if lost => Follow::Move,
        Some(d) if moved && d != on => Follow::Move,
        Some(_) => Follow::Stay,
    }
}

#[cfg(not(test))]
mod capture {
    use super::super::mediadevices::{self, Flow};
    use super::*;
    use firewheel::cpal::cpal;
    use firewheel::cpal::cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    type Park = Box<dyn FnOnce()>;

    /// See [`super::input_format`]: the device [`open`] would pick for this
    /// id, and its default input config.
    pub(super) fn format(device_id: &str) -> Option<(u32, u32)> {
        let host = cpal::default_host();
        let device = if device_id.is_empty() || device_id == super::super::mediadevices::DEFAULT_SINK {
            host.default_input_device()
        } else {
            <cpal::DeviceId as std::str::FromStr>::from_str(device_id)
                .ok()
                .and_then(|did| host.device_by_id(&did))
                .filter(|d| d.supports_input())
        }?;
        let config = device.default_input_config().ok()?.config();
        Some((config.sample_rate, config.channels as u32))
    }

    fn fail(name: &'static str, msg: String) -> (OpenResult, Park) {
        (OpenResult::Err(name, msg), Box::new(|| {}))
    }

    /// The endpoint id of `device`, as `enumerateDevices` spells it.
    fn id_of(device: &cpal::Device) -> String {
        device.id().map(|i| i.to_string()).unwrap_or_default()
    }

    /// Open the device and start its stream. On success the returned closure
    /// keeps the stream alive on the calling thread until the source is
    /// released. A capture on the default follows it there.
    pub(super) fn open(owner: usize, req: AudioRequest) -> (OpenResult, Park) {
        let host = cpal::default_host();
        let wanted = req
            .device_id
            .clone()
            .filter(|d| !d.is_empty() && d != super::super::mediadevices::DEFAULT_SINK);
        let device = match &wanted {
            Some(id) => {
                let found = <cpal::DeviceId as std::str::FromStr>::from_str(id)
                    .ok()
                    .and_then(|did| host.device_by_id(&did))
                    .filter(|d| d.supports_input());
                match found {
                    Some(d) => Some(d),
                    None if req.exact => {
                        return fail("OverconstrainedError", format!("No audio input device has deviceId {id:?}."))
                    }
                    None => host.default_input_device(),
                }
            }
            None => host.default_input_device(),
        };
        let Some(device) = device else {
            return fail("NotFoundError", "Requested device not found".into());
        };
        let label = device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "Microphone".into());
        let following = wanted.is_none();
        let device_id = if following { super::super::mediadevices::DEFAULT_SINK.to_string() } else { id_of(&device) };
        let (config, sample) = match input_config(&device) {
            Ok(c) => c,
            Err(msg) => return fail("NotReadableError", msg),
        };
        let rate = config.sample_rate;
        let (chunker, settings) = processing_for(owner, rate, &req.settings);
        let channels = track_channels(&config, &chunker);
        let source = Source::new(SourceKind::Microphone { device_id }, label, channels, rate, settings);
        let lost = Arc::new(AtomicBool::new(false));
        let feeder = Feeder { source: Arc::downgrade(&source), chunker, channels: config.channels as usize, rate };
        let on_lost = if following { Lost::Flag(Arc::clone(&lost)) } else { Lost::Fail(Arc::downgrade(&source)) };
        let stream = match build(&device, &config, sample, feeder, on_lost) {
            Ok(s) => s,
            Err((name, msg)) => return fail(name, msg),
        };
        if let Err(e) = stream.play() {
            return fail("NotReadableError", format!("The microphone could not be started: {e}"));
        }
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        source.set_stop(tx);
        let id = new_track(&source, owner);
        let park: Park = if following {
            let on = id_of(&device);
            let asked = req.settings;
            Box::new(move || follow_default(owner, &asked, &source, stream, on, lost, &rx))
        } else {
            Box::new(move || {
                let _keep = source;
                let _ = rx.recv();
                drop(stream);
            })
        };
        (OpenResult::Ok(id), park)
    }

    /// The device's own mix format, which a capture opens it with.
    fn input_config(device: &cpal::Device) -> Result<(cpal::StreamConfig, cpal::SampleFormat), String> {
        match device.default_input_config() {
            Ok(c) => Ok((c.config(), c.sample_format())),
            Err(e) => Err(format!("The device has no usable input format: {e}")),
        }
    }

    /// What the source carries: mono when processed, as Chrome's processed
    /// track is, the device's channels otherwise.
    fn track_channels(config: &cpal::StreamConfig, chunker: &Option<Chunker>) -> u32 {
        if chunker.is_some() {
            1
        } else {
            config.channels as u32
        }
    }

    /// What a stream does when its device goes away.
    enum Lost {
        /// A capture on a chosen device: its tracks end.
        Fail(Weak<Source>),
        /// A capture on the default: its thread moves it ([`follow_default`]).
        Flag(Arc<AtomicBool>),
    }

    /// A stream on `device` feeding `feeder`, not yet started.
    fn build(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        sample: cpal::SampleFormat,
        mut feeder: Feeder,
        lost: Lost,
    ) -> Result<cpal::Stream, (&'static str, String)> {
        let on_error = move |e: cpal::StreamError| {
            eprintln!("[redjs] microphone stream error: {e}");
            if matches!(e, cpal::StreamError::DeviceNotAvailable) {
                match &lost {
                    Lost::Fail(source) => {
                        if let Some(s) = source.upgrade() {
                            s.fail();
                        }
                    }
                    Lost::Flag(flag) => flag.store(true, Ordering::Release),
                }
            }
        };
        let stream = match sample {
            cpal::SampleFormat::F32 => device.build_input_stream(
                config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| feeder.push(data),
                on_error,
                None,
            ),
            cpal::SampleFormat::I16 => {
                let mut buf: Vec<f32> = Vec::new();
                device.build_input_stream(
                    config,
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        buf.clear();
                        buf.extend(data.iter().map(|&v| v as f32 / 32768.0));
                        feeder.push(&buf);
                    },
                    on_error,
                    None,
                )
            }
            cpal::SampleFormat::I32 => {
                let mut buf: Vec<f32> = Vec::new();
                device.build_input_stream(
                    config,
                    move |data: &[i32], _: &cpal::InputCallbackInfo| {
                        buf.clear();
                        buf.extend(data.iter().map(|&v| v as f32 / 2_147_483_648.0));
                        feeder.push(&buf);
                    },
                    on_error,
                    None,
                )
            }
            cpal::SampleFormat::U16 => {
                let mut buf: Vec<f32> = Vec::new();
                device.build_input_stream(
                    config,
                    move |data: &[u16], _: &cpal::InputCallbackInfo| {
                        buf.clear();
                        buf.extend(data.iter().map(|&v| (v as f32 - 32768.0) / 32768.0));
                        feeder.push(&buf);
                    },
                    on_error,
                    None,
                )
            }
            other => return Err(("NotReadableError", format!("Unsupported input sample format {other:?}."))),
        };
        stream.map_err(|e| {
            // WASAPI's E_ACCESSDENIED is the privacy settings refusing
            // desktop apps the microphone.
            let text = e.to_string();
            let name = if text.contains("0x80070005") || text.to_ascii_lowercase().contains("access is denied") {
                "NotAllowedError"
            } else {
                "NotReadableError"
            };
            (name, format!("The microphone could not be opened: {text}"))
        })
    }

    /// The capture thread of a microphone on the default, until the source is
    /// released (see "A microphone on the default follows it" above). `on` is
    /// the endpoint the stream runs on, `lost` its device-gone flag.
    fn follow_default(
        owner: usize,
        wanted: &Settings,
        source: &Arc<Source>,
        mut stream: cpal::Stream,
        mut on: String,
        mut lost: Arc<AtomicBool>,
        rx: &std::sync::mpsc::Receiver<()>,
    ) {
        let mut seen = mediadevices::default_generation(Flow::Input);
        loop {
            match rx.recv_timeout(FOLLOW_WAKE) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                // Stopped, or the source released.
                _ => break,
            }
            let generation = mediadevices::default_generation(Flow::Input);
            let moved = generation != seen;
            seen = generation;
            let gone = lost.load(Ordering::Acquire);
            if !moved && !gone {
                continue;
            }
            let device = cpal::default_host().default_input_device();
            let id = device.as_ref().map(id_of);
            let device = match (follow(&on, id.as_deref(), moved, gone), device) {
                (Follow::Move, Some(d)) => d,
                (Follow::End, _) => {
                    eprintln!("[redjs] the microphone went away and no input is left: its tracks end");
                    source.fail();
                    break;
                }
                _ => continue,
            };
            // A new stream is built before the old one goes, so a failure
            // keeps the old one, and the old one goes before the new one
            // starts, so the two never feed the source at once.
            let next = (|| {
                let (config, sample) = input_config(&device)?;
                let rate = config.sample_rate;
                let (chunker, _) = processing_for(owner, rate, wanted);
                let channels = track_channels(&config, &chunker);
                let flag = Arc::new(AtomicBool::new(false));
                let feeder =
                    Feeder { source: Arc::downgrade(source), chunker, channels: config.channels as usize, rate };
                let s = build(&device, &config, sample, feeder, Lost::Flag(Arc::clone(&flag))).map_err(|(_, m)| m)?;
                Ok::<_, String>((s, flag, channels, rate))
            })();
            match next {
                Ok((s, flag, channels, rate)) => {
                    drop(std::mem::replace(&mut stream, s));
                    source.set_format(channels, rate);
                    if let Err(e) = stream.play() {
                        eprintln!("[redjs] the default microphone could not be started: {e}; its tracks end");
                        source.fail();
                        break;
                    }
                    lost = flag;
                    on = id.unwrap_or_default();
                    let name = device.description().map(|d| d.name().to_string()).unwrap_or_default();
                    eprintln!("[redjs] the microphone follows the system default to {name:?} ({rate} Hz x {channels})");
                }
                Err(e) => {
                    eprintln!("[redjs] the default microphone could not be opened: {e}");
                    if gone {
                        source.fail();
                        break;
                    }
                }
            }
        }
        drop(stream);
    }
}

/// The synthetic input: [`set_fake_capture`] for a whole process (skathi's
/// `SK_MEDIA_CAPTURE=fake`), or one context in a test. `cargo test` never
/// opens a device.
pub(crate) mod fake {
    use super::*;

    /// The one device a fake capture lists and opens.
    pub(crate) const DEVICE_ID: &str = "fake";
    pub(crate) const LABEL: &str = "Fake Microphone";

    #[cfg(test)]
    fn on() -> &'static Mutex<HashSet<usize>> {
        static S: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
        S.get_or_init(|| Mutex::new(HashSet::new()))
    }

    /// Context `g` captures from a 440 Hz, 48 kHz mono sine, gate open.
    #[cfg(test)]
    pub(crate) fn enable(g: *mut c_void) {
        on().lock().unwrap().insert(g as usize);
        ctx_allowed().lock().unwrap().insert(g as usize, true);
    }

    pub(crate) fn enabled(owner: usize) -> bool {
        #[cfg(test)]
        if on().lock().unwrap().contains(&owner) {
            return true;
        }
        let _ = owner;
        fake_capture()
    }

    /// Fake devices open right now, per owner.
    fn open_count() -> &'static Mutex<HashMap<usize, usize>> {
        static S: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();
        S.get_or_init(|| Mutex::new(HashMap::new()))
    }

    #[cfg(test)]
    pub(crate) fn open_devices(g: *mut c_void) -> usize {
        open_count().lock().unwrap().get(&(g as usize)).copied().unwrap_or(0)
    }

    pub(super) fn open(owner: usize, token: u32, req: AudioRequest) {
        if req.exact && req.device_id.as_deref().is_some_and(|d| d != DEVICE_ID) {
            finish(owner, token, OpenResult::Err("OverconstrainedError", "No such device.".into()));
            return;
        }
        let device_id = if req.device_id.as_deref() == Some(DEVICE_ID) { DEVICE_ID } else { "default" };
        // Processed like a device, so an automated run covers the processing.
        let (chunker, settings) = processing_for(owner, 48_000, &req.settings);
        let source =
            Source::new(SourceKind::Microphone { device_id: device_id.into() }, LABEL.into(), 1, 48_000, settings);
        let mut feeder = Feeder { source: Arc::downgrade(&source), chunker, channels: 1, rate: 48_000 };
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        source.set_stop(tx);
        let id = new_track(&source, owner);
        *open_count().lock().unwrap().entry(owner).or_default() += 1;
        let spawned = std::thread::Builder::new().name("redjs-fake-mic".into()).spawn(move || {
            const TAU: f32 = 2.0 * std::f32::consts::PI;
            let mut phase = 0.0f32;
            let mut block = vec![0.0f32; 480];
            while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                rx.recv_timeout(std::time::Duration::from_millis(10))
            {
                for s in block.iter_mut() {
                    *s = 0.25 * phase.sin();
                    // Kept in one turn: a phase that only grows loses its
                    // precision within hours, and the tone with it.
                    phase = (phase + TAU * 440.0 / 48_000.0) % TAU;
                }
                feeder.push(&block);
            }
            drop(source);
            *open_count().lock().unwrap().entry(owner).or_default() -= 1;
        });
        if spawned.is_err() {
            *open_count().lock().unwrap().entry(owner).or_default() -= 1;
            stop(id);
            release(id);
            finish(owner, token, OpenResult::Err("NotReadableError", "Could not start the capture thread.".into()));
            return;
        }
        finish(owner, token, OpenResult::Ok(id));
    }
}

// ---------------------------------------------------------------------------
// The JS surface: track handles, natives, the pump
// ---------------------------------------------------------------------------

/// Host data of the object a `MediaStreamTrack` keeps its track through.
/// Collected with the track, and releases it.
struct TrackHandle(u64);

impl Drop for TrackHandle {
    fn drop(&mut self) {
        release(self.0);
    }
}

struct MsState {
    handle_cls: crate::capi::JSClassRef,
    /// `{ settle(token, handle, name, message), ended(id) }` from the script.
    hooks: u64,
}

unsafe impl Send for MsState {}
unsafe impl Sync for MsState {}

fn states() -> &'static Mutex<HashMap<usize, &'static MsState>> {
    static R: OnceLock<Mutex<HashMap<usize, &'static MsState>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn state_of(g: *mut c_void) -> Option<&'static MsState> {
    states().lock().unwrap().get(&(g as usize)).copied()
}

/// The page's handle for track `id` (which it now holds).
pub(crate) unsafe fn new_handle(g: *mut c_void, id: u64) -> u64 {
    match state_of(g) {
        Some(st) => host_create(g, st.handle_cls, UNDEF, TrackHandle(id)),
        None => {
            release(id);
            NULL
        }
    }
}

/// The track id behind a handle, if `v` is one.
pub(crate) unsafe fn handle_id(g: *mut c_void, v: u64) -> Option<u64> {
    let st = state_of(g)?;
    host_data::<TrackHandle>(g, st.handle_cls, v).map(|h| h.0)
}

/// The context this page runs in, as `new_track` wants it.
pub(crate) fn owner_of(g: *mut c_void) -> usize {
    g as usize
}

pub(crate) fn forget(gone: crate::context::Gone) {
    states().lock().unwrap().remove(&gone.key);
    ctx_allowed().lock().unwrap().remove(&gone.key);
    // A prompt it was waiting on stays the host's; its answer opens nothing.
    asking().lock().unwrap().retain(|x| x.owner != gone.key);
    // Opens still running are abandoned (their thread releases what it gets),
    // finished ones are released here, and so is every track the page held.
    let finished = {
        let mut o = opens().lock().unwrap();
        o.inflight.retain(|(owner, _)| *owner != gone.key);
        let mut mine = Vec::new();
        o.done.retain(|(owner, _, r)| {
            if *owner == gone.key {
                if let OpenResult::Ok(id) = r {
                    mine.push(*id);
                }
                false
            } else {
                true
            }
        });
        mine
    };
    let tracks: Vec<Arc<TrackCore>> = {
        let mut h = held().lock().unwrap();
        let ids: Vec<u64> = h.iter().filter(|(_, t)| t.owner == gone.key).map(|(&id, _)| id).collect();
        ids.into_iter().filter_map(|id| h.remove(&id)).collect()
    };
    for id in finished {
        stop(id);
        release(id);
    }
    for t in tracks {
        t.stop();
        t.unhold();
    }
    ended_queue().lock().unwrap().retain(|(o, _)| *o != gone.key);
}

/// Stop every track a page in `g` holds, as unloading a document does: for
/// an embedder that replaces the page without replacing the context. A
/// microphone with no live track left is released. No `ended` fires: the
/// document that would hear it is gone. A `getUserMedia` still opening is
/// abandoned the way a closing context abandons it (its thread releases the
/// device it gets), so the old page's promise never hands out a live track.
/// Returns how many tracks were live.
pub fn stop_all(g: *mut c_void) -> usize {
    let owner = g as usize;
    asking().lock().unwrap().retain(|x| x.owner != owner);
    let unclaimed: Vec<u64> = {
        let mut o = opens().lock().unwrap();
        o.inflight.retain(|(o_, _)| *o_ != owner);
        let mut mine = Vec::new();
        o.done.retain(|(o_, _, r)| {
            if *o_ != owner {
                return true;
            }
            if let OpenResult::Ok(id) = r {
                mine.push(*id);
            }
            false
        });
        mine
    };
    for id in unclaimed {
        stop(id);
        release(id);
    }
    let ids: Vec<u64> = held()
        .lock()
        .unwrap()
        .values()
        .filter(|t| t.owner == g as usize && !t.ended())
        .map(|t| t.id)
        .collect();
    for &id in &ids {
        stop(id);
    }
    ids.len()
}

unsafe fn arg_bool(cf: *mut c_void, i: u32) -> bool {
    truthy_flag(cf_arg(cf, i))
}

unsafe fn pair(g: *mut c_void, name: &str, msg: &str) -> u64 {
    let arr = new_array(g);
    set_i(g, arr, 0, js_string(g, name));
    set_i(g, arr, 1, js_string(g, msg));
    arr
}

/// `open(token, deviceId, exact, ec, ns, agc)` → `undefined` (the result comes
/// through `settle`, after the host's prompt when it asks one), or
/// `[name, message]` when the gate refuses.
#[crate::host_fn]
unsafe fn n_open(g: *mut c_void, cf: *mut c_void) -> u64 {
    let token = read_number(g, cf_arg(cf, 0)) as u32;
    let gate = gate_for(g as usize);
    if let Some((name, msg)) = refusal(gate) {
        return pair(g, name, &msg);
    }
    let dev = cf_arg(cf, 1);
    let req = AudioRequest {
        device_id: if is_cell(dev) { read_string(g, dev) } else { None },
        exact: arg_bool(cf, 2),
        settings: Settings {
            echo_cancellation: arg_bool(cf, 3),
            noise_suppression: arg_bool(cf, 4),
            auto_gain_control: arg_bool(cf, 5),
        },
    };
    if gate == Gate::Ask {
        ask_then_open(g as usize, token, req);
    } else {
        open_microphone(g as usize, token, req);
    }
    UNDEF
}

#[crate::host_fn]
unsafe fn n_clone(g: *mut c_void, cf: *mut c_void) -> u64 {
    match handle_id(g, cf_arg(cf, 0)).and_then(clone_track) {
        Some(id) => new_handle(g, id),
        None => NULL,
    }
}

#[crate::host_fn]
unsafe fn n_stop(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Some(id) = handle_id(g, cf_arg(cf, 0)) {
        stop(id);
    }
    UNDEF
}

#[crate::host_fn]
unsafe fn n_set_enabled(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Some(id) = handle_id(g, cf_arg(cf, 0)) {
        set_enabled(id, arg_bool(cf, 1));
    }
    UNDEF
}

/// `info(handle)` → `{ id, kind, label, deviceId, channelCount, sampleRate,
/// echoCancellation, noiseSuppression, autoGainControl, canProcess, ended }`,
/// or `null`. The three switches are what runs, not what was asked.
#[crate::host_fn]
unsafe fn n_info(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(t) = handle_id(g, cf_arg(cf, 0)).and_then(track) else { return NULL };
    let o = new_object(g, UNDEF);
    define(g, o, "id", enc_f64(t.id as f64), 0);
    let (kind, device) = match &t.source.kind {
        SourceKind::Microphone { device_id } => ("microphone", device_id.as_str()),
        SourceKind::Graph => ("graph", ""),
        SourceKind::Remote => ("remote", ""),
    };
    define(g, o, "kind", js_string(g, kind), 0);
    define(g, o, "label", js_string(g, &t.source.label), 0);
    define(g, o, "deviceId", js_string(g, device), 0);
    define(g, o, "channelCount", enc_f64(t.source.channels() as f64), 0);
    define(g, o, "sampleRate", enc_f64(t.source.sample_rate() as f64), 0);
    let s = &t.source.settings;
    define(g, o, "echoCancellation", enc_bool(s.echo_cancellation), 0);
    define(g, o, "noiseSuppression", enc_bool(s.noise_suppression), 0);
    define(g, o, "autoGainControl", enc_bool(s.auto_gain_control), 0);
    define(g, o, "canProcess", enc_bool(can_process()), 0);
    define(g, o, "ended", enc_bool(t.ended()), 0);
    o
}

/// A one-shot advisory on stderr (each text once per process).
fn advise_once(text: &str) {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    if SEEN.get_or_init(|| Mutex::new(HashSet::new())).lock().unwrap().insert(text.to_string()) {
        eprintln!("[redjs] advisory: {text}");
    }
}

/// `advise(text)`: [`advise_once`].
#[crate::host_fn]
unsafe fn n_advise(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Some(text) = read_string(g, cf_arg(cf, 0)) {
        advise_once(&text);
    }
    UNDEF
}

/// [`capture_processing_available`], or a test's own factory.
fn can_process() -> bool {
    #[cfg(test)]
    if TEST_PROCESSING.with(|t| t.get()).is_some() {
        return true;
    }
    capture_processing_available()
}

/// `canProcess()`: [`can_process`].
#[crate::host_fn]
unsafe fn n_can_process(_g: *mut c_void, _cf: *mut c_void) -> u64 {
    enc_bool(can_process())
}

/// Deliver finished opens and self-ended tracks, after starting the opens a
/// prompt has just allowed. A relaxed load or two when idle.
pub unsafe fn pump(g: *mut c_void) {
    settle_asked();
    if !WORK.load(Ordering::Acquire) {
        return;
    }
    let Some(st) = state_of(g) else { return };
    WORK.store(false, Ordering::Release);
    let opened = drain_opened(g as usize);
    let ended = drain_ended(g as usize);
    // Another context's results stay queued: keep the flag up for its pump.
    let others = !opens().lock().unwrap().done.is_empty() || !ended_queue().lock().unwrap().is_empty();
    if others {
        WORK.store(true, Ordering::Release);
    }
    let settle = get(g, st.hooks, "settle");
    for (token, r) in opened {
        let args = match r {
            OpenResult::Ok(id) => [enc_f64(token as f64), new_handle(g, id), UNDEF, UNDEF],
            OpenResult::Err(name, msg) => [enc_f64(token as f64), NULL, js_string(g, name), js_string(g, &msg)],
        };
        if let Err(e) = call(g, settle, st.hooks, &args) {
            report(g, e);
        }
    }
    let on_ended = get(g, st.hooks, "ended");
    for id in ended {
        if let Err(e) = call(g, on_ended, st.hooks, &[enc_f64(id as f64)]) {
            report(g, e);
        }
    }
}

unsafe fn report(g: *mut c_void, e: u64) {
    let (mut msg, mut stack) = (0u64, 0u64);
    glue::rjs_error_info(g, e, &mut msg, &mut stack);
    eprintln!("[redjs] mediastream: {}", read_string(g, msg).unwrap_or_default());
}

const MS_JS: &str = include_str!("mediastream.js");

/// After `audio::install` and `mediadevices::install`: the script extends
/// `AudioContext.prototype` and replaces `MediaDevices.prototype.getUserMedia`.
pub unsafe fn install(g: *mut c_void) {
    let handle_cls = make_host_class::<TrackHandle>("MediaStreamTrackHandle");
    let natives = new_object(g, UNDEF);
    let fns: &[(&str, u32, HostFn)] = &[
        ("open", 6, n_open),
        ("clone", 1, n_clone),
        ("stop", 1, n_stop),
        ("setEnabled", 2, n_set_enabled),
        ("info", 1, n_info),
        ("advise", 1, n_advise),
        ("canProcess", 0, n_can_process),
        ("sourceNode", 3, super::audio::ms_source_node),
        ("destNode", 3, super::audio::ms_dest_node),
    ];
    for (name, arity, f) in fns {
        define(g, natives, name, make_fn(g, name, *arity, *f), 0);
    }
    // The handle class must be reachable before the script runs.
    let pending: &'static MsState = Box::leak(Box::new(MsState { handle_cls, hooks: UNDEF }));
    states().lock().unwrap().insert(g as usize, pending);

    let mut factory = 0u64;
    let name = "redjs/mediastream.js";
    let rc = glue::rjs_eval8(g, MS_JS.as_ptr(), MS_JS.len(), name.as_ptr(), name.len(), &mut factory);
    if rc == 1 {
        report(g, factory);
        return;
    }
    match call(g, factory, UNDEF, &[glue::rjs_global(g), natives]) {
        Ok(hooks) if is_cell(hooks) => {
            crate::ffi::redjs_protect(g, hooks);
            let st: &'static MsState = Box::leak(Box::new(MsState { handle_cls, hooks }));
            states().lock().unwrap().insert(g as usize, st);
        }
        Ok(_) => eprintln!("[redjs] mediastream.js returned no hooks"),
        Err(e) => report(g, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph_source(ch: u32) -> Arc<Source> {
        Source::new(SourceKind::Graph, "tap".into(), ch, 48_000, Settings::default())
    }

    fn collect(id: u64) -> (Subscription, Arc<Mutex<Vec<f32>>>) {
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&got);
        let sub = subscribe(id, Box::new(move |d, _, _| sink.lock().unwrap().extend_from_slice(d))).unwrap();
        (sub, got)
    }

    #[test]
    fn the_gate_opens_asks_or_refuses_in_its_order() {
        use PermissionState::*;
        assert_eq!(gate(false, true, None, true), Gate::Open, "no consent store: nothing to ask");
        assert_eq!(gate(false, true, Some(Granted), true), Gate::Open);
        assert_eq!(gate(false, true, Some(Unsupported), true), Gate::Open);
        assert_eq!(gate(false, true, Some(Prompt), true), Gate::Ask, "nobody decided: the host asks");
        assert_eq!(gate(false, true, Some(Denied), true), Gate::Denied { system: true });
        assert_eq!(gate(false, true, Some(Denied), false), Gate::Denied { system: false });
        assert_eq!(gate(false, false, Some(Granted), true), Gate::NotEnabled, "off unless the embedder says so");
        assert_eq!(gate(false, false, Some(Prompt), true), Gate::NotEnabled, "and then nothing is asked");
        assert_eq!(gate(true, true, Some(Granted), true), Gate::Forbidden, "the kill switch beats everything");
        assert_eq!(gate(true, true, Some(Prompt), true), Gate::Forbidden, "including the prompt");
        assert_eq!(gate(true, false, Some(Denied), true), Gate::Forbidden);
        for g in [Gate::Forbidden, Gate::NotEnabled, Gate::Denied { system: true }, Gate::Denied { system: false }] {
            assert_eq!(refusal(g).unwrap().0, "NotAllowedError", "{g:?}");
        }
        // Chrome's words, which pages match on.
        assert_eq!(refusal(Gate::Denied { system: false }).unwrap().1, "Permission denied");
        assert_eq!(refusal(Gate::Denied { system: true }).unwrap().1, "Permission denied by system");
        assert!(refusal(Gate::Open).is_none());
        assert!(refusal(Gate::Ask).is_none(), "asking is not a refusal");
    }

    #[test]
    fn a_track_hands_its_source_frames_to_every_consumer() {
        let src = graph_source(2);
        let id = new_track(&src, 1);
        let (_a, got_a) = collect(id);
        let (_b, got_b) = collect(id);
        src.deliver(&[0.5, -0.5, 0.25, -0.25], 2, 48_000);
        assert_eq!(*got_a.lock().unwrap(), vec![0.5, -0.5, 0.25, -0.25]);
        assert_eq!(*got_b.lock().unwrap(), vec![0.5, -0.5, 0.25, -0.25]);
        release(id);
    }

    #[test]
    fn a_disabled_track_is_silence_and_an_ended_one_is_nothing() {
        let src = graph_source(1);
        let id = new_track(&src, 1);
        let (_s, got) = collect(id);
        set_enabled(id, false);
        src.deliver(&[1.0, 1.0], 1, 48_000);
        assert_eq!(*got.lock().unwrap(), vec![0.0, 0.0]);
        set_enabled(id, true);
        stop(id);
        src.deliver(&[1.0], 1, 48_000);
        assert_eq!(*got.lock().unwrap(), vec![0.0, 0.0]);
        assert!(subscribe(id, Box::new(|_, _, _| {})).is_none(), "an ended track takes no consumer");
        release(id);
    }

    #[test]
    fn a_clone_is_an_independent_track_of_the_same_source() {
        let src = graph_source(1);
        let a = new_track(&src, 1);
        let b = clone_track(a).unwrap();
        assert_ne!(a, b);
        let (_sa, got_a) = collect(a);
        let (_sb, got_b) = collect(b);
        set_enabled(a, false);
        stop(b);
        src.deliver(&[0.5], 1, 48_000);
        assert_eq!(*got_a.lock().unwrap(), vec![0.0]);
        assert!(got_b.lock().unwrap().is_empty());
        release(a);
        release(b);
    }

    #[test]
    fn the_device_is_released_when_nothing_can_hear_it() {
        let src = Source::new(
            SourceKind::Microphone { device_id: "default".into() },
            "mic".into(),
            1,
            48_000,
            Settings::default(),
        );
        let (tx, rx) = std::sync::mpsc::channel();
        src.set_stop(tx);
        let a = new_track(&src, 1);
        let b = clone_track(a).unwrap();
        stop(a);
        assert!(rx.try_recv().is_err(), "one track is still live");
        // A published track outlives its page object: the subscription holds it.
        let sub = subscribe(b, Box::new(|_, _, _| {})).unwrap();
        release(b);
        assert!(rx.try_recv().is_err(), "the subscriber still hears it");
        drop(sub);
        assert!(rx.try_recv().is_ok(), "the last listener went: the device closes");
        release(a);
    }

    #[test]
    fn a_dropped_subscription_stops_reading() {
        let src = graph_source(1);
        let id = new_track(&src, 1);
        let (sub, got) = collect(id);
        drop(sub);
        src.deliver(&[1.0], 1, 48_000);
        assert!(got.lock().unwrap().is_empty());
        release(id);
    }

    #[test]
    fn a_failed_device_ends_its_tracks_once() {
        let owner = 0xF00D;
        let src = Source::new(SourceKind::Microphone { device_id: "x".into() }, "mic".into(), 1, 48_000, Settings::default());
        let id = new_track(&src, owner);
        src.fail();
        src.fail();
        assert!(info(id).unwrap().ended);
        assert_eq!(drain_ended(owner), vec![id]);
        assert!(drain_ended(owner).is_empty());
        release(id);
    }

    #[test]
    fn a_capture_on_the_default_moves_with_it_and_ends_only_when_no_input_is_left() {
        // Nothing happened.
        assert_eq!(follow("mic-a", Some("mic-a"), false, false), Follow::Stay);
        // The user picked another default: the capture moves.
        assert_eq!(follow("mic-a", Some("mic-b"), true, false), Follow::Move);
        // A default "change" back to the endpoint it is on (another role, or
        // A -> B -> A between two wakes) does not reopen it.
        assert_eq!(follow("mic-a", Some("mic-a"), true, false), Follow::Stay);
        // The default is gone but the stream still runs: it keeps running,
        // and ends when its own device reports itself gone.
        assert_eq!(follow("mic-a", None, true, false), Follow::Stay);
        // Its device went away: it moves to whichever input is default now...
        assert_eq!(follow("mic-a", Some("mic-b"), false, true), Follow::Move);
        // ...including the same one, back after a device reset...
        assert_eq!(follow("mic-a", Some("mic-a"), false, true), Follow::Move);
        // ...and its tracks end only when no input is left.
        assert_eq!(follow("mic-a", None, true, true), Follow::End);
    }

    #[test]
    fn a_node_reading_a_source_that_changed_its_rate_hears_it_at_its_own() {
        // A microphone that followed the default from a 48 kHz device to a
        // 16 kHz headset: the node was built at 48 kHz.
        let mut r = Rerate::default();
        let mut out: Vec<f32> = Vec::new();
        // A ramp, 160 frames per 10 ms block, stereo: left = t, right = -t.
        for b in 0..10 {
            let block: Vec<f32> =
                (0..160).flat_map(|i| [(b * 160 + i) as f32, -((b * 160 + i) as f32)]).collect();
            out.extend_from_slice(r.run(&block, 2, 16_000, 48_000));
        }
        // Three times the frames, give or take the one the phase holds back.
        let frames = out.len() / 2;
        assert!((4_797..=4_800).contains(&frames), "{frames} frames");
        // A ramp stays a ramp across every block boundary: each output frame
        // is a third of an input frame further on, on both channels.
        for (n, f) in out.chunks_exact(2).enumerate() {
            let t = n as f32 / 3.0;
            assert!((f[0] - t).abs() < 1e-3 && (f[1] + t).abs() < 1e-3, "frame {n}: {f:?}, expected {t}");
        }
        // Down again, 48 kHz to 16 kHz: a third of the frames.
        let mut d = Rerate::default();
        let block: Vec<f32> = (0..480).map(|i| i as f32).collect();
        let got = d.run(&block, 1, 48_000, 16_000).to_vec();
        assert_eq!(got.len(), 160);
        assert_eq!(got[1], 3.0);
    }

    #[test]
    fn a_remote_source_ends_its_tracks_and_then_carries_nothing() {
        // The network track was unsubscribed while the page still holds a
        // track of it: `ended` once, and a graph reading it hears silence.
        let owner = 0xD00D;
        let src = remote_source("Remote Bob", 1, 48_000);
        let id = new_track(&src, owner);
        let (_sub, got) = collect(id);
        src.deliver(&[0.5; 480], 1, 48_000);
        assert_eq!(got.lock().unwrap().len(), 480);
        assert_eq!(info(id).unwrap().kind, SourceKind::Remote);
        src.fail();
        assert_eq!(drain_ended(owner), vec![id]);
        src.deliver(&[0.5; 480], 1, 48_000);
        assert_eq!(got.lock().unwrap().len(), 480, "an ended track delivers nothing");
        release(id);
    }

    #[test]
    fn a_mono_track_fills_a_stereo_node_and_a_wide_one_keeps_two_channels() {
        let fit = |data: &[f32], channels: usize, want: usize| -> Vec<f32> {
            let mut out = Vec::new();
            fit_channels(data, channels, want, &mut out);
            out
        };
        assert_eq!(fit(&[0.1, 0.2], 1, 2), vec![0.1, 0.1, 0.2, 0.2]);
        assert_eq!(fit(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 3, 2), vec![1.0, 2.0, 4.0, 5.0]);
    }

    #[test]
    fn an_abandoned_open_releases_what_it_opened() {
        let owner = 0xBEEF;
        let src = graph_source(1);
        let id = new_track(&src, owner);
        // The context went away while the device was opening: no entry.
        finish(owner, 7, OpenResult::Ok(id));
        assert!(drain_opened(owner).is_empty());
        assert!(track(id).is_none(), "the abandoned track is stopped and let go");
    }

    /// Doubles its block and counts the blocks it saw.
    struct Doubler(Arc<Mutex<Vec<usize>>>);

    impl CaptureProcessor for Doubler {
        fn process(&mut self, block: &mut [f32]) {
            self.0.lock().unwrap().push(block.len());
            for v in block.iter_mut() {
                *v *= 2.0;
            }
        }
    }

    #[test]
    fn processing_takes_10ms_mono_blocks_whatever_the_device_hands_over() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut c = Chunker::new(Box::new(Doubler(seen.clone())), 48_000);
        let mut out: Vec<f32> = Vec::new();
        // 7 ms of stereo at a time: (0.25, 0.75) averages to 0.5.
        let block: Vec<f32> = [0.25f32, 0.75].repeat(336);
        for _ in 0..3 {
            c.feed(&block, 2, |b| out.extend_from_slice(b));
        }
        // 21 ms in: two whole blocks out, 1 ms waiting for the next.
        assert_eq!(*seen.lock().unwrap(), vec![480, 480]);
        assert_eq!(out.len(), 960);
        assert!(out.iter().all(|&v| (v - 1.0).abs() < 1e-6), "averaged, then processed");
        c.feed(&block, 2, |b| out.extend_from_slice(b));
        assert_eq!(*seen.lock().unwrap(), vec![480, 480], "28 ms: still two");
        c.feed(&block, 2, |b| out.extend_from_slice(b));
        assert_eq!(seen.lock().unwrap().len(), 3, "35 ms: the third");
        // 44.1 kHz cuts at 441.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut c = Chunker::new(Box::new(Doubler(seen.clone())), 44_100);
        c.feed(&vec![0.1f32; 900], 1, |_| {});
        assert_eq!(*seen.lock().unwrap(), vec![441, 441]);
    }
}

/// The JS surface, over the fake input: no test here opens a device.
#[cfg(test)]
mod jsc_tests {
    use super::*;
    use crate::api::Context;
    use crate::webapi::testing::*;

    /// One pump under the API lock, as an embedder's frame runs it: the pump
    /// allocates, and a collection started without the lock is a JSC crash.
    unsafe fn pump_locked(c: &Context) {
        glue::rjs_lock(c.raw());
        crate::web::pump(c.raw(), crate::web::PUMP_TIMERS | crate::web::PUMP_MICROTASKS, core::ptr::null_mut());
        glue::rjs_unlock(c.raw());
    }

    /// An async body's settled value, stringified; `<pending>` if it never
    /// settles. Pumps the loop (opens settle from `mediastream::pump`).
    fn run(c: &Context, body: &str) -> String {
        s(
            c,
            &format!(
                "globalThis.__out = '<pending>'; (async () => {{ {body} }})().then(\
                   v => {{ globalThis.__out = String(v); }}, \
                   e => {{ globalThis.__out = 'ERROR: ' + (e && e.name) + ': ' + ((e && e.message) || e); }}); 'started'"
            ),
        );
        for _ in 0..200 {
            unsafe {
                pump_locked(c)
            };
            if s(c, "globalThis.__out") != "<pending>" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        s(c, "globalThis.__out")
    }

    /// Pump for about `ms`; true if the context's clock moved (a real output
    /// stream exists). Signal assertions are conditional on it, as in audio.rs.
    fn pump_for(c: &Context, ms: u64) -> bool {
        let before = s(c, "globalThis.__ctx.currentTime");
        for _ in 0..(ms / 10).max(1) {
            unsafe {
                pump_locked(c)
            };
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let after = s(c, "globalThis.__ctx.currentTime");
        before != after && after != "0"
    }

    #[test]
    fn the_classes_exist_with_their_tags_and_track_is_not_constructible() {
        let c = ctx();
        for name in [
            "MediaStream",
            "MediaStreamTrack",
            "MediaStreamAudioSourceNode",
            "MediaStreamAudioDestinationNode",
            "OverconstrainedError",
        ] {
            ok(&c, &format!("typeof {name} === 'function' && Object.prototype.toString.call({name}.prototype) === '[object {name}]'"));
        }
        ok(&c, "typeof AudioContext.prototype.createMediaStreamSource === 'function'");
        ok(&c, "typeof AudioContext.prototype.createMediaStreamDestination === 'function'");
        ok(&c, "MediaStreamAudioSourceNode.prototype instanceof AudioNode");
        assert!(throws(&c, "new MediaStreamTrack()").contains("Illegal constructor"));
        ok(&c, "new OverconstrainedError('deviceId', 'm').constraint === 'deviceId'");
        ok(&c, "new OverconstrainedError('x') instanceof DOMException && new OverconstrainedError('x').name === 'OverconstrainedError'");
    }

    #[test]
    fn get_user_media_refuses_what_it_cannot_do_by_name() {
        let c = ctx();
        assert!(run(&c, "await navigator.mediaDevices.getUserMedia({}); return 'resolved';").contains("TypeError"));
        assert!(run(&c, "await navigator.mediaDevices.getUserMedia(); return 'resolved';").contains("TypeError"));
        let video = run(&c, "await navigator.mediaDevices.getUserMedia({ video: true }); return 'resolved';");
        assert!(video.contains("NotSupportedError"), "{video}");
    }

    #[test]
    fn a_stream_constructor_takes_tracks_and_refuses_anything_else() {
        let c = ctx();
        fake::enable(c.raw());
        assert_eq!(
            run(
                &c,
                "const ms = await navigator.mediaDevices.getUserMedia({ audio: true });
                 const [t] = ms.getAudioTracks();
                 if (!(ms instanceof MediaStream) || !(t instanceof MediaStreamTrack)) return 'classes';
                 if (!(ms instanceof EventTarget)) return 'not an EventTarget';
                 if (ms.getVideoTracks().length !== 0 || ms.getTracks().length !== 1) return 'tracks';
                 if (!ms.active || typeof ms.id !== 'string' || ms.id.length < 32) return 'id/active';
                 if (ms.getTrackById(t.id) !== t || ms.getTrackById('x') !== null) return 'byId';
                 const empty = new MediaStream();
                 if (empty.getTracks().length !== 0 || empty.active) return 'empty';
                 const two = new MediaStream([t, t]);
                 if (two.getTracks().length !== 1) return 'dedupe';
                 two.addTrack(t); if (two.getTracks().length !== 1) return 'addTrack dedupe';
                 two.removeTrack(t); if (two.getTracks().length !== 0) return 'removeTrack';
                 const copy = new MediaStream(ms);
                 if (copy.getTracks()[0] !== t || copy.id === ms.id) return 'from a stream';
                 try { new MediaStream([null]); return 'took null'; } catch (e) { if (!(e instanceof TypeError)) return 'null: ' + e; }
                 try { two.addTrack({}); return 'took {}'; } catch (e) { if (!(e instanceof TypeError)) return '{}: ' + e; }
                 t.stop();
                 return 'ok';"
            ),
            "ok"
        );
    }

    #[test]
    fn a_track_reports_its_device_and_the_processing_it_does_not_do() {
        let c = ctx();
        fake::enable(c.raw());
        assert_eq!(
            run(
                &c,
                "const ms = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: true } });
                 const t = ms.getAudioTracks()[0];
                 const st = t.getSettings();
                 const out = [t.kind, t.label, t.readyState, t.enabled, t.muted, st.sampleRate, st.channelCount,
                              st.deviceId, st.echoCancellation, t.getConstraints().echoCancellation].join('|');
                 t.stop();
                 return out;"
            ),
            "audio|Fake Microphone|live|true|false|48000|1|default|false|true"
        );
    }

    static PROCESSED: AtomicUsize = AtomicUsize::new(0);

    struct Counting;

    impl CaptureProcessor for Counting {
        fn process(&mut self, block: &mut [f32]) {
            assert_eq!(block.len(), 480);
            PROCESSED.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Runs echo cancellation and noise suppression, never gain control, so
    /// `getSettings()` has to report what runs rather than what was asked.
    fn no_agc(req: &ProcessingRequest) -> Option<(Box<dyn CaptureProcessor>, Settings)> {
        assert_eq!(req.rate, 48_000);
        let s = Settings { auto_gain_control: false, ..req.wanted.clone() };
        Some((Box::new(Counting), s))
    }

    #[test]
    fn processing_defaults_on_as_in_chrome_and_get_settings_reports_what_runs() {
        let c = ctx();
        fake::enable(c.raw());
        TEST_PROCESSING.with(|t| t.set(Some(no_agc)));
        let settings = |audio: &str| {
            let out = run(
                &c,
                &format!(
                    "const ms = await navigator.mediaDevices.getUserMedia({{ audio: {audio} }});
                     const t = globalThis.__t = ms.getAudioTracks()[0];
                     const st = t.getSettings(), cap = t.getCapabilities();
                     return [st.echoCancellation, st.noiseSuppression, st.autoGainControl, st.channelCount,
                             cap.echoCancellation.join('/')].join('|');"
                ),
            );
            // A few of the fake's 10 ms blocks.
            std::thread::sleep(std::time::Duration::from_millis(60));
            s(&c, "globalThis.__t && globalThis.__t.stop(), 0");
            out
        };
        let before = PROCESSED.load(Ordering::Relaxed);
        assert_eq!(settings("true"), "true|true|false|1|true/false", "audio: true is all three asked");
        assert!(PROCESSED.load(Ordering::Relaxed) > before, "the fake microphone ran through the processor");
        assert_eq!(settings("{}"), "true|true|false|1|true/false", "unset means on");
        assert_eq!(
            settings("{ echoCancellation: false, noiseSuppression: { ideal: false } }"),
            "false|false|false|1|true/false",
            "AGC alone was asked, and this processor does not do it"
        );
        assert_eq!(
            settings("{ echoCancellation: { exact: false }, noiseSuppression: false, autoGainControl: false }"),
            "false|false|false|1|true/false",
            "nothing asked: no processor"
        );
        TEST_PROCESSING.with(|t| t.set(None));
    }

    #[test]
    fn stop_and_clone_follow_the_spec_and_the_device_closes_with_the_last_track() {
        let c = ctx();
        fake::enable(c.raw());
        assert_eq!(
            run(
                &c,
                "const ms = await navigator.mediaDevices.getUserMedia({ audio: true });
                 const a = ms.getAudioTracks()[0];
                 globalThis.__a = a;
                 let ended = 0; a.onended = () => ended++; a.addEventListener('ended', () => ended++);
                 const b = a.clone();
                 globalThis.__b = b;
                 if (b === a || b.id === a.id || b.label !== a.label) return 'clone identity';
                 b.enabled = false;
                 if (a.enabled !== true || b.enabled !== false) return 'enabled is per track';
                 a.stop();
                 if (a.readyState !== 'ended' || b.readyState !== 'live') return 'stop is per track';
                 if (ended !== 0) return 'stop() fired ended';
                 return 'ok';"
            ),
            "ok"
        );
        assert_eq!(fake::open_devices(c.raw()), 1, "the clone keeps the device open");
        s(&c, "__b.stop(); 0");
        let mut open = 1;
        for _ in 0..100 {
            open = fake::open_devices(c.raw());
            if open == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(open, 0, "the last track stopped: the device closes");
    }

    #[test]
    fn unloading_the_page_stops_its_tracks_and_closes_the_device() {
        // `location.reload()` in one realm: the old page's capture ends
        // without an `ended` event and the device is released.
        let c = ctx();
        fake::enable(c.raw());
        assert_eq!(
            run(
                &c,
                "const ms = await navigator.mediaDevices.getUserMedia({ audio: true });
                 globalThis.__t = ms.getAudioTracks()[0];
                 globalThis.__c = __t.clone();
                 globalThis.__ended = 0; __t.onended = () => __ended++;
                 return __t.readyState;"
            ),
            "live"
        );
        assert_eq!(fake::open_devices(c.raw()), 1);
        assert_eq!(stop_all(c.raw()), 2, "both live tracks are stopped");
        let mut open = 1;
        for _ in 0..100 {
            open = fake::open_devices(c.raw());
            if open == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(open, 0, "the device closes with the page");
        unsafe { pump_locked(&c) };
        // No `ended` reaches the old page (its JS objects are orphaned by the
        // reload; their cached readyState is not what this is about).
        ok(&c, "__ended === 0");
        assert_eq!(stop_all(c.raw()), 0, "nothing is left to stop");
    }

    #[test]
    fn a_required_device_that_does_not_exist_is_overconstrained() {
        let c = ctx();
        fake::enable(c.raw());
        let got = run(
            &c,
            "try { await navigator.mediaDevices.getUserMedia({ audio: { deviceId: { exact: 'nope' } } }); return 'resolved'; }
             catch (e) { return [e instanceof OverconstrainedError, e instanceof DOMException, e.name, e.constraint].join('|'); }",
        );
        assert_eq!(got, "true|true|OverconstrainedError|deviceId");
        // A bare deviceId is only ideal: the default answers.
        assert_eq!(
            run(&c, "const ms = await navigator.mediaDevices.getUserMedia({ audio: { deviceId: 'nope' } }); ms.getTracks()[0].stop(); return 'ok';"),
            "ok"
        );
    }

    /// `Object.prototype.toString` of the two nodes names their own
    /// interfaces, as Chromium's do (fixes-host-media-1 §4: the source node
    /// said `[object AudioNode]`), and a track's capabilities are the ones
    /// its device's `InputDeviceInfo` reports (webapi/mediadevices.rs).
    #[test]
    fn the_nodes_are_tagged_as_themselves_and_a_track_has_its_devices_capabilities() {
        let c = ctx();
        fake::enable(c.raw());
        assert_eq!(
            run(
                &c,
                "const ctx = globalThis.__ctx = new AudioContext();
                 const ms = await navigator.mediaDevices.getUserMedia({ audio: true });
                 const tag = v => Object.prototype.toString.call(v);
                 const src = ctx.createMediaStreamSource(ms);
                 const made = new MediaStreamAudioSourceNode(ctx, { mediaStream: ms });
                 const dst = ctx.createMediaStreamDestination();
                 const out = [tag(src), tag(made), tag(dst), tag(ctx.createGain()), JSON.stringify(ms.getAudioTracks()[0].getCapabilities())].join('|');
                 ms.getTracks()[0].stop();
                 return out;"
            ),
            format!(
                "[object MediaStreamAudioSourceNode]|[object MediaStreamAudioSourceNode]|[object MediaStreamAudioDestinationNode]|[object GainNode]|{}",
                crate::webapi::mediadevices::FAKE_CAPABILITIES
            )
        );
    }

    /// A remote source's track, handed to the page as `globalThis[name]`.
    fn hand_over(c: &Context, src: &Arc<Source>, name: &str) {
        unsafe {
            glue::rjs_lock(c.raw());
            let t = js_track(c.raw(), src);
            define(c.raw(), glue::rjs_global(c.raw()), name, t, 0);
            glue::rjs_unlock(c.raw());
        }
    }

    #[test]
    fn a_remote_track_is_the_pages_and_its_playing_it_is_seen() {
        let c = ctx();
        let src = remote_source("Remote Bob", 1, 48_000);
        hand_over(&c, &src, "__rt");
        ok(
            &c,
            "__rt instanceof MediaStreamTrack && __rt.kind === 'audio' && __rt.label === 'Remote Bob' \
             && __rt.readyState === 'live' && JSON.stringify(__rt.getSettings()) === '{\"channelCount\":1,\"sampleRate\":48000}'",
        );
        // Argon's remote graph: a stream, a source node, an analyser.
        s(
            &c,
            "globalThis.__ctx = new AudioContext();
             globalThis.__ms = new MediaStream([__rt]);
             globalThis.__src = __ctx.createMediaStreamSource(__ms);
             globalThis.__an = __ctx.createAnalyser();
             __src.connect(__an); 0",
        );
        assert!(!page_plays(&src), "an analyser that leads nowhere does not play it");
        // ... into a gain the page mutes, then the destination: the page
        // plays it (silently: that is its deafen).
        s(&c, "globalThis.__g = __ctx.createGain(); __g.gain.value = 0; __an.connect(__g); __g.connect(__ctx.destination); 0");
        assert!(page_plays(&src), "muted by a gain of 0 is still the page's");
        // Disconnected mid-call: the embedder plays it again.
        s(&c, "__g.disconnect(); 0");
        assert!(!page_plays(&src));
        s(&c, "__g.connect(__ctx.destination); 0");
        assert!(page_plays(&src));
        // A second, independent source is not mistaken for this one.
        assert!(!page_plays(&remote_source("Remote Carol", 1, 48_000)));
        // Unsubscribed while the graph is alive: `ended`, and no longer played.
        s(&c, "globalThis.__ended = 0; __rt.addEventListener('ended', () => __ended++); 0");
        src.fail();
        unsafe { pump_locked(&c) };
        ok(&c, "__rt.readyState === 'ended' && __ended === 1");
        assert!(!page_plays(&src), "an ended track is nobody's to play");
        s(&c, "__ctx.close(); 0");
    }

    #[test]
    fn the_source_node_plays_the_track_into_the_graph() {
        let c = ctx();
        fake::enable(c.raw());
        assert_eq!(
            run(
                &c,
                "const ctx = globalThis.__ctx = new AudioContext();
                 const ms = globalThis.__ms = await navigator.mediaDevices.getUserMedia({ audio: true });
                 const src = globalThis.__src = ctx.createMediaStreamSource(ms);
                 if (!(src instanceof MediaStreamAudioSourceNode) || !(src instanceof AudioNode)) return 'class';
                 if (src.mediaStream !== ms) return 'mediaStream';
                 if (src.numberOfInputs !== 0 || src.numberOfOutputs !== 1) return 'ports';
                 const ctor = new MediaStreamAudioSourceNode(ctx, { mediaStream: ms });
                 if (!(ctor instanceof MediaStreamAudioSourceNode)) return 'constructor';
                 const an = globalThis.__an = ctx.createAnalyser();
                 src.connect(an);
                 an.connect(ctx.destination);
                 return 'ok';"
            ),
            "ok"
        );
        if pump_for(&c, 400) {
            // A slow machine (a CI VM's virtual output) can need longer than
            // the first 400 ms to carry the track through; give it up to 3 s.
            let heard = "(() => { const b = new Float32Array(__an.fftSize); __an.getFloatTimeDomainData(b);
                   let e = 0; for (const v of b) e += v * v; return Math.sqrt(e / b.length) > 0.05; })()";
            for _ in 0..26 {
                if s(&c, heard) == "true" {
                    break;
                }
                pump_for(&c, 100);
            }
            ok(&c, heard);
        } else {
            eprintln!("no output stream on this machine: the signal check is skipped");
        }
        s(&c, "__ms.getTracks()[0].stop(); 0");
    }

    #[test]
    fn a_stream_without_audio_is_an_invalid_state() {
        let c = ctx();
        let got = run(&c, "new AudioContext().createMediaStreamSource(new MediaStream()); return 'made';");
        assert!(got.contains("InvalidStateError"), "{got}");
        let got = run(&c, "new AudioContext().createMediaStreamSource({}); return 'made';");
        assert!(got.contains("TypeError"), "{got}");
    }

    #[test]
    fn the_destination_node_is_a_track_an_embedder_can_read() {
        let c = ctx();
        assert_eq!(
            run(
                &c,
                "const ctx = globalThis.__ctx = new AudioContext();
                 const dest = globalThis.__dest = ctx.createMediaStreamDestination();
                 if (!(dest instanceof MediaStreamAudioDestinationNode) || !(dest instanceof AudioNode)) return 'class';
                 if (dest.numberOfInputs !== 1 || dest.numberOfOutputs !== 0) return 'ports';
                 if (!(dest.stream instanceof MediaStream) || dest.stream !== dest.stream) return 'stream';
                 const t = dest.stream.getAudioTracks()[0];
                 if (!t || t.readyState !== 'live' || t.kind !== 'audio') return 'track';
                 try { dest.connect(ctx.destination); return 'connected from a sink'; }
                 catch (e) { if (e.name !== 'IndexSizeError') return 'connect: ' + e.name; }
                 const osc = globalThis.__osc = ctx.createOscillator();
                 osc.connect(dest);
                 osc.start();
                 const ctor = new MediaStreamAudioDestinationNode(ctx, { channelCount: 2 });
                 if (!(ctor.stream instanceof MediaStream)) return 'constructor';
                 return String(__rjs_mediaTrackId(t) > 0) + '|' + __rjs_mediaTrackId({});"
            ),
            "true|0"
        );
        let id: u64 = s(&c, "__rjs_mediaTrackId(__dest.stream.getAudioTracks()[0])").parse().unwrap();
        let energy = Arc::new(Mutex::new((0usize, 0.0f64)));
        let sink = Arc::clone(&energy);
        let sub = subscribe(
            id,
            Box::new(move |data, channels, rate| {
                assert_eq!(channels, 2);
                assert!(rate >= 8_000);
                let mut e = sink.lock().unwrap();
                e.0 += data.len() / channels;
                e.1 += data.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>();
            }),
        )
        .expect("the destination's track is live");
        if pump_for(&c, 400) {
            let (frames, e) = *energy.lock().unwrap();
            assert!(frames > 4_000, "the tap delivered {frames} frames");
            assert!(e > 1.0, "an oscillator into the tap is not silence");
        } else {
            eprintln!("no output stream on this machine: the tap check is skipped");
        }
        drop(sub);
    }

    /// A host with a prompt of its own, answered by the test: `microphone` is
    /// `prompt` until an answer is stored, every request is pending, and the
    /// pending ones settle with the answer once one is given.
    struct PromptHost {
        decided: Mutex<Option<PermissionState>>,
        answer: Mutex<Option<PermissionState>>,
        pending: Mutex<Vec<u32>>,
        asked: AtomicU32,
    }

    impl PromptHost {
        fn leak() -> &'static PromptHost {
            Box::leak(Box::new(PromptHost {
                decided: Mutex::new(None),
                answer: Mutex::new(None),
                pending: Mutex::new(Vec::new()),
                asked: AtomicU32::new(0),
            }))
        }
        /// What the user picks: `Prompt` is a dismissal (nothing stored).
        fn answer(&self, st: PermissionState) {
            *self.answer.lock().unwrap() = Some(st);
        }
    }

    impl permissions::PermissionHost for PromptHost {
        fn platform(&self) -> &'static str {
            "prompting"
        }
        fn status(&self, _: Capability) -> PermissionState {
            self.decided.lock().unwrap().unwrap_or(PermissionState::Prompt)
        }
        fn request(&self, _: Capability) -> permissions::RequestOutcome {
            self.asked.fetch_add(1, Ordering::SeqCst);
            // Unique across every test's host: the runtime's tokens are
            // process-wide, as a real embedder has one host.
            static NEXT: AtomicU32 = AtomicU32::new(9_000);
            let token = NEXT.fetch_add(1, Ordering::SeqCst);
            self.pending.lock().unwrap().push(token);
            permissions::RequestOutcome::Pending(token)
        }
        fn drain_settled(&self) -> Vec<(u32, PermissionState)> {
            let Some(st) = self.answer.lock().unwrap().take() else { return Vec::new() };
            if st != PermissionState::Prompt {
                *self.decided.lock().unwrap() = Some(st);
            }
            let st = self.status(Capability::Microphone);
            self.pending.lock().unwrap().drain(..).map(|t| (t, st)).collect()
        }
        fn denied_by_system(&self, _: Capability) -> bool {
            false
        }
    }

    fn inputs(c: &Context) -> String {
        run(
            c,
            "const ds = await navigator.mediaDevices.enumerateDevices();
             return ds.filter(d => d.kind === 'audioinput').map(d => d.deviceId + '/' + d.label).join(',');",
        )
    }

    #[test]
    fn get_user_media_waits_for_the_hosts_prompt_and_opens_on_allow() {
        let host = PromptHost::leak();
        permissions::set_thread_host(Some(host));
        let c = ctx();
        fake::enable(c.raw());
        // Before anyone decided, the inputs are anonymous, as in a browser.
        assert_eq!(inputs(&c), "/");
        s(
            &c,
            "globalThis.__got = [];
             for (let i = 0; i < 2; i++) navigator.mediaDevices.getUserMedia({ audio: true }).then(
               ms => { __got.push(ms.getAudioTracks()[0].readyState + ':' + ms.getAudioTracks()[0].label); globalThis['__ms' + i] = ms; },
               e => { __got.push(e.name + ': ' + e.message); });
             0",
        );
        for _ in 0..20 {
            unsafe { pump_locked(&c) };
        }
        ok(&c, "__got.length === 0");
        assert_eq!(host.asked.load(Ordering::SeqCst), 2, "each call asks; the host shows one prompt");
        host.answer(PermissionState::Granted);
        for _ in 0..200 {
            unsafe { pump_locked(&c) };
            if s(&c, "String(__got.length)") == "2" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(s(&c, "__got.join('|')"), "live:Fake Microphone|live:Fake Microphone");
        // Granted now: the next call opens without asking, and the inputs have names.
        assert_eq!(
            run(&c, "const ms = await navigator.mediaDevices.getUserMedia({ audio: true }); ms.getTracks()[0].stop(); return 'ok';"),
            "ok"
        );
        assert_eq!(host.asked.load(Ordering::SeqCst), 2);
        assert!(inputs(&c).starts_with("default/"), "named once granted: {}", inputs(&c));
        s(&c, "__ms0.getTracks()[0].stop(); __ms1.getTracks()[0].stop(); 0");
        permissions::set_thread_host(None);
    }

    #[test]
    fn a_dismissed_prompt_asks_again_and_a_blocked_one_never_does() {
        let host = PromptHost::leak();
        permissions::set_thread_host(Some(host));
        let c = ctx();
        fake::enable(c.raw());
        let ask = |c: &Context| {
            s(
                c,
                "globalThis.__out = '<pending>';
                 navigator.mediaDevices.getUserMedia({ audio: true }).then(
                   ms => { ms.getTracks()[0].stop(); __out = 'opened'; },
                   e => { __out = e.name + ': ' + e.message; });
                 0",
            );
        };
        let settle = |c: &Context| {
            for _ in 0..200 {
                unsafe { pump_locked(c) };
                if s(c, "__out") != "<pending>" {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            s(c, "__out")
        };
        // Esc: nothing stored, Chrome's "dismissed", and the next call asks again.
        ask(&c);
        host.answer(PermissionState::Prompt);
        assert_eq!(settle(&c), "NotAllowedError: Permission dismissed");
        ask(&c);
        assert_eq!(host.asked.load(Ordering::SeqCst), 2, "a dismissal is asked again");
        // Block: stored, and from then on refused at once, without a prompt.
        host.answer(PermissionState::Denied);
        assert_eq!(settle(&c), "NotAllowedError: Permission denied");
        assert_eq!(run(&c, "await navigator.mediaDevices.getUserMedia({ audio: true }); return 'opened';"),
                   "ERROR: NotAllowedError: Permission denied");
        assert_eq!(host.asked.load(Ordering::SeqCst), 2, "a block is never asked again by getUserMedia");
        assert_eq!(inputs(&c), "/", "blocked: still anonymous");
        permissions::set_thread_host(None);
    }

    #[test]
    fn a_page_that_unloads_while_asked_opens_nothing_when_the_answer_comes() {
        let host = PromptHost::leak();
        permissions::set_thread_host(Some(host));
        let c = ctx();
        fake::enable(c.raw());
        s(&c, "globalThis.__out = '<pending>'; navigator.mediaDevices.getUserMedia({ audio: true }).then(() => { __out = 'opened'; }, e => { __out = e.name; }); 0");
        unsafe { pump_locked(&c) };
        assert_eq!(stop_all(c.raw()), 0);
        host.answer(PermissionState::Granted);
        for _ in 0..20 {
            unsafe { pump_locked(&c) };
        }
        assert_eq!(fake::open_devices(c.raw()), 0, "the answer opened no device for a page that is gone");
        ok(&c, "__out === '<pending>'");
        permissions::set_thread_host(None);
    }
}

