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
//! - an embedder subscribes through [`subscribe`]. vue-native's voice layer
//!   reads the track a page publishes to a LiveKit room that way.
//!
//! Consumers run on the producing thread (the device callback or the audio
//! thread), so they must not block. Every lock on that path is a `try_lock`:
//! a block that meets a contended list is dropped rather than waited for. The
//! lists only change when something subscribes, stops or is collected.
//!
//! ## The capture gate
//!
//! A browser asks the user before it opens a microphone. This runtime has no
//! prompt of its own, so it opens one only when **all** of these hold:
//!
//! 1. the process has not been forbidden capture ([`forbid_capture`], a latch
//!    an embedder sets from its own kill switch);
//! 2. the embedder enabled capture ([`set_capture_allowed`]; off by default,
//!    so an application cannot reach a device unless its host decided it may);
//! 3. the platform's consent store, read through the `PermissionHost`
//!    (`permissions.rs`), does not say `Denied`.
//!
//! A refusal is a `NotAllowedError`, which is what a page sees when the user
//! says no. See [`gate`].
//!
//! ## Divergences
//!
//! - Audio only. `getUserMedia({ video })` rejects with `NotSupportedError`.
//! - The device opens in its own shared-mode format, usually 48 kHz at its
//!   own channel count. `sampleRate`, `channelCount`, `sampleSize` and
//!   `latency` constraints are read but not applied. No processing runs on
//!   the capture path: `echoCancellation`, `noiseSuppression` and
//!   `autoGainControl` are recorded as asked ([`Settings`]), `getSettings()`
//!   reports all three `false`, and asking for one logs an advisory.
//! - The tracks are not transferable, and `MediaStreamTrack` has no `stats`.

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

/// What the embedder adds to a "not enabled" refusal, so the developer reading
/// it knows which switch is theirs (vue-native names its config key).
fn hint() -> &'static Mutex<String> {
    static H: OnceLock<Mutex<String>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(String::new()))
}

pub fn set_capture_hint(text: &str) {
    *hint().lock().unwrap() = text.to_string();
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
    /// The platform's consent store says no.
    OsDenied,
}

/// The gate, as a pure function of its three inputs. `os` is `None` with no
/// `PermissionHost` installed. `Prompt` and `Unsupported` are open: where the
/// OS has a prompt, opening the device raises it, and where it has no notion
/// of consent there is nothing to ask.
pub fn gate(forbidden: bool, allowed: bool, os: Option<PermissionState>) -> Gate {
    if forbidden {
        Gate::Forbidden
    } else if !allowed {
        Gate::NotEnabled
    } else if os == Some(PermissionState::Denied) {
        Gate::OsDenied
    } else {
        Gate::Open
    }
}

fn gate_for(ctx: usize) -> Gate {
    let allowed = ctx_allowed()
        .lock()
        .unwrap()
        .get(&ctx)
        .copied()
        .unwrap_or_else(|| ALLOWED.load(Ordering::Acquire));
    gate(
        FORBIDDEN.load(Ordering::Acquire),
        allowed,
        permissions::host().map(|h| h.status(Capability::Microphone)),
    )
}

/// The refusal a closed gate turns into: `(DOMException name, message)`.
fn refusal(g: Gate) -> Option<(&'static str, String)> {
    match g {
        Gate::Open => None,
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
        Gate::OsDenied => Some((
            "NotAllowedError",
            "Permission denied: the operating system's privacy settings do not let this app use \
             the microphone."
                .into(),
        )),
    }
}

/// Should `enumerateDevices` name the inputs? Only when they could be opened;
/// otherwise it lists them the way a browser does before permission (one
/// entry, no id, no label).
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
}

/// What `getSettings()` reports beyond the format, fixed at open.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub echo_cancellation: bool,
    pub noise_suppression: bool,
    pub auto_gain_control: bool,
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
    let mut fit: Vec<f32> = Vec::new();
    let sub = subscribe(
        track_id,
        Box::new(move |data, channels, _rate| {
            if channels == want {
                let _ = sink.push_interleaved(data);
                return;
            }
            fit_channels(data, channels, want, &mut fit);
            let _ = sink.push_interleaved(&fit);
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

/// Start opening a microphone for context `owner`. The gate has been checked.
/// The result arrives through [`pump`] under `token`.
fn open_microphone(owner: usize, token: u32, req: AudioRequest) {
    opens().lock().unwrap().inflight.insert((owner, token));
    #[cfg(test)]
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

#[cfg(not(test))]
mod capture {
    use super::*;
    use firewheel::cpal::cpal;
    use firewheel::cpal::cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    type Park = Box<dyn FnOnce()>;

    fn fail(name: &'static str, msg: String) -> (OpenResult, Park) {
        (OpenResult::Err(name, msg), Box::new(|| {}))
    }

    /// Open the device and start its stream. On success the returned closure
    /// keeps the stream alive on the calling thread until the source is
    /// released.
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
        let device_id = match &wanted {
            Some(_) => device.id().map(|i| i.to_string()).unwrap_or_default(),
            None => super::super::mediadevices::DEFAULT_SINK.to_string(),
        };
        let supported = match device.default_input_config() {
            Ok(c) => c,
            Err(e) => return fail("NotReadableError", format!("The device has no usable input format: {e}")),
        };
        let config = supported.config();
        let channels = config.channels as usize;
        let rate = config.sample_rate;
        let source =
            Source::new(SourceKind::Microphone { device_id }, label, channels as u32, rate, req.settings.clone());

        let failed = Arc::downgrade(&source);
        let on_error = move |e: cpal::StreamError| {
            eprintln!("[redjs] microphone stream error: {e}");
            if matches!(e, cpal::StreamError::DeviceNotAvailable) {
                if let Some(s) = failed.upgrade() {
                    s.fail();
                }
            }
        };
        let weak = Arc::downgrade(&source);
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => device.build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    if let Some(s) = weak.upgrade() {
                        s.deliver(data, channels, rate);
                    }
                },
                on_error,
                None,
            ),
            cpal::SampleFormat::I16 => {
                let mut buf: Vec<f32> = Vec::new();
                device.build_input_stream(
                    &config,
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        buf.clear();
                        buf.extend(data.iter().map(|&v| v as f32 / 32768.0));
                        if let Some(s) = weak.upgrade() {
                            s.deliver(&buf, channels, rate);
                        }
                    },
                    on_error,
                    None,
                )
            }
            cpal::SampleFormat::I32 => {
                let mut buf: Vec<f32> = Vec::new();
                device.build_input_stream(
                    &config,
                    move |data: &[i32], _: &cpal::InputCallbackInfo| {
                        buf.clear();
                        buf.extend(data.iter().map(|&v| v as f32 / 2_147_483_648.0));
                        if let Some(s) = weak.upgrade() {
                            s.deliver(&buf, channels, rate);
                        }
                    },
                    on_error,
                    None,
                )
            }
            cpal::SampleFormat::U16 => {
                let mut buf: Vec<f32> = Vec::new();
                device.build_input_stream(
                    &config,
                    move |data: &[u16], _: &cpal::InputCallbackInfo| {
                        buf.clear();
                        buf.extend(data.iter().map(|&v| (v as f32 - 32768.0) / 32768.0));
                        if let Some(s) = weak.upgrade() {
                            s.deliver(&buf, channels, rate);
                        }
                    },
                    on_error,
                    None,
                )
            }
            other => return fail("NotReadableError", format!("Unsupported input sample format {other:?}.")),
        };
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                // WASAPI's E_ACCESSDENIED is the privacy settings refusing
                // desktop apps the microphone.
                let text = e.to_string();
                let name = if text.contains("0x80070005") || text.to_ascii_lowercase().contains("access is denied") {
                    "NotAllowedError"
                } else {
                    "NotReadableError"
                };
                return fail(name, format!("The microphone could not be opened: {text}"));
            }
        };
        if let Err(e) = stream.play() {
            return fail("NotReadableError", format!("The microphone could not be started: {e}"));
        }
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        source.set_stop(tx);
        let id = new_track(&source, owner);
        let park: Park = Box::new(move || {
            let _keep = source;
            let _ = rx.recv();
            drop(stream);
        });
        (OpenResult::Ok(id), park)
    }
}

/// A synthetic input for the tests: `cargo test` never opens a device.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;

    fn on() -> &'static Mutex<HashSet<usize>> {
        static S: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
        S.get_or_init(|| Mutex::new(HashSet::new()))
    }

    /// Context `g` captures from a 440 Hz, 48 kHz mono sine, gate open.
    pub(crate) fn enable(g: *mut c_void) {
        on().lock().unwrap().insert(g as usize);
        ctx_allowed().lock().unwrap().insert(g as usize, true);
    }

    pub(super) fn enabled(owner: usize) -> bool {
        on().lock().unwrap().contains(&owner)
    }

    /// Fake devices open right now, per owner.
    fn open_count() -> &'static Mutex<HashMap<usize, usize>> {
        static S: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();
        S.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(crate) fn open_devices(g: *mut c_void) -> usize {
        open_count().lock().unwrap().get(&(g as usize)).copied().unwrap_or(0)
    }

    pub(super) fn open(owner: usize, token: u32, req: AudioRequest) {
        if req.exact && req.device_id.as_deref().is_some_and(|d| d != "fake") {
            finish(owner, token, OpenResult::Err("OverconstrainedError", "No such device.".into()));
            return;
        }
        let source = Source::new(
            SourceKind::Microphone { device_id: "default".into() },
            "Fake Microphone".into(),
            1,
            48_000,
            req.settings.clone(),
        );
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        source.set_stop(tx);
        let id = new_track(&source, owner);
        *open_count().lock().unwrap().entry(owner).or_default() += 1;
        std::thread::spawn(move || {
            let mut phase = 0.0f32;
            let mut block = vec![0.0f32; 480];
            while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                rx.recv_timeout(std::time::Duration::from_millis(10))
            {
                for s in block.iter_mut() {
                    *s = 0.25 * phase.sin();
                    phase += 2.0 * std::f32::consts::PI * 440.0 / 48_000.0;
                }
                source.deliver(&block, 1, 48_000);
            }
            *open_count().lock().unwrap().entry(owner).or_default() -= 1;
        });
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
/// an embedder that replaces the page without replacing the context.
pub fn stop_all(g: *mut c_void) {
    let ids: Vec<u64> = held().lock().unwrap().values().filter(|t| t.owner == g as usize).map(|t| t.id).collect();
    for id in ids {
        stop(id);
    }
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
/// through `settle`), or `[name, message]` when the gate refuses.
unsafe extern "sysv64" fn n_open(g: *mut c_void, cf: *mut c_void) -> u64 {
    let token = read_number(g, cf_arg(cf, 0)) as u32;
    if let Some((name, msg)) = refusal(gate_for(g as usize)) {
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
    open_microphone(g as usize, token, req);
    UNDEF
}

unsafe extern "sysv64" fn n_clone(g: *mut c_void, cf: *mut c_void) -> u64 {
    match handle_id(g, cf_arg(cf, 0)).and_then(clone_track) {
        Some(id) => new_handle(g, id),
        None => NULL,
    }
}

unsafe extern "sysv64" fn n_stop(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Some(id) = handle_id(g, cf_arg(cf, 0)) {
        stop(id);
    }
    UNDEF
}

unsafe extern "sysv64" fn n_set_enabled(g: *mut c_void, cf: *mut c_void) -> u64 {
    if let Some(id) = handle_id(g, cf_arg(cf, 0)) {
        set_enabled(id, arg_bool(cf, 1));
    }
    UNDEF
}

/// `info(handle)` → `{ id, kind, label, deviceId, channelCount, sampleRate,
/// echoCancellation, noiseSuppression, autoGainControl, ended }`, or `null`.
unsafe extern "sysv64" fn n_info(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(t) = handle_id(g, cf_arg(cf, 0)).and_then(track) else { return NULL };
    let o = new_object(g, UNDEF);
    define(g, o, "id", enc_f64(t.id as f64), 0);
    let (kind, device) = match &t.source.kind {
        SourceKind::Microphone { device_id } => ("microphone", device_id.as_str()),
        SourceKind::Graph => ("graph", ""),
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
    define(g, o, "ended", enc_bool(t.ended()), 0);
    o
}

/// `advise(text)`: a one-shot advisory on stderr (each text once per process).
unsafe extern "sysv64" fn n_advise(g: *mut c_void, cf: *mut c_void) -> u64 {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    if let Some(text) = read_string(g, cf_arg(cf, 0)) {
        if SEEN.get_or_init(|| Mutex::new(HashSet::new())).lock().unwrap().insert(text.clone()) {
            eprintln!("[redjs] advisory: {text}");
        }
    }
    UNDEF
}

/// Deliver finished opens and self-ended tracks. One relaxed load when idle.
pub unsafe fn pump(g: *mut c_void) {
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
    fn the_gate_opens_only_when_enabled_not_forbidden_and_not_denied() {
        use PermissionState::*;
        assert_eq!(gate(false, true, None), Gate::Open, "no consent store: nothing to ask");
        assert_eq!(gate(false, true, Some(Granted)), Gate::Open);
        assert_eq!(gate(false, true, Some(Prompt)), Gate::Open, "the OS asks when the device opens");
        assert_eq!(gate(false, true, Some(Unsupported)), Gate::Open);
        assert_eq!(gate(false, true, Some(Denied)), Gate::OsDenied);
        assert_eq!(gate(false, false, Some(Granted)), Gate::NotEnabled, "off unless the embedder says so");
        assert_eq!(gate(true, true, Some(Granted)), Gate::Forbidden, "the kill switch beats everything");
        assert_eq!(gate(true, false, Some(Denied)), Gate::Forbidden);
        for g in [Gate::Forbidden, Gate::NotEnabled, Gate::OsDenied] {
            assert_eq!(refusal(g).unwrap().0, "NotAllowedError", "{g:?}");
        }
        assert!(refusal(Gate::Open).is_none());
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
            ok(
                &c,
                "(() => { const b = new Float32Array(__an.fftSize); __an.getFloatTimeDomainData(b);
                   let e = 0; for (const v of b) e += v * v; return Math.sqrt(e / b.length) > 0.05; })()",
            );
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
}

