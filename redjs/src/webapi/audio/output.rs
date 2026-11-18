//! Where a context's rendered blocks go: a cpal output stream, or an output backend the embedder
//! installed. A backend receives the destination's channels and, for each `PannerNode` the page put
//! in the backend's object model, that panner's input and placement, which it renders itself.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use firewheel::backend::{AudioBackend, BackendProcessInfo};
use firewheel::cpal::cpal;
use firewheel::cpal::cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use firewheel::node::StreamStatus;
use firewheel::processor::FirewheelProcessor;
use firewheel::StreamInfo;

const DEFAULT_BLOCK_FRAMES: u32 = 1024;
const BUILD_STREAM_TIMEOUT: Duration = Duration::from_secs(5);

/// What a context asks an [`OutputBackend`] for.
#[derive(Clone, Debug)]
pub struct OutputRequest {
    /// The platform's id of the output device (on Windows the endpoint id), `None` for the default.
    pub device_id: Option<String>,
    /// The rate the context runs at; a backend may open at another.
    pub sample_rate: Option<u32>,
    /// Whether a panner of the context is in the backend's object model. A backend that only
    /// renders objects refuses the rest, which then play through cpal.
    pub objects: bool,
}

/// A stream an [`OutputBackend`] opened.
pub struct OutputOpen {
    pub stream: Box<dyn OutputStream>,
    pub sample_rate: u32,
    /// Channels of each interleaved block [`Renderer::render`] fills.
    pub channels: u32,
    /// Their speaker positions as a WAVEFORMATEXTENSIBLE mask, 0 for the usual layout of `channels`.
    pub channel_mask: u32,
    /// The most frames one [`Renderer::render`] call renders.
    pub max_block_frames: u32,
    /// How many panners can render as objects at once.
    pub max_objects: u32,
    /// What renders the objects, for the page (`"windows-sonic"`, …).
    pub label: String,
}

/// An open backend stream. Dropping it must stop calling [`Renderer::render`] and drop the
/// renderer before it returns.
pub trait OutputStream: Send {
    /// `Err` once the stream has stopped for good (device gone, renderer reset); the context then
    /// opens its output again.
    fn poll(&mut self) -> Result<(), String>;
}

/// An embedder's output, asked first whenever a context opens its output. `open` runs on the
/// page's thread; `Err` sends the context to cpal.
pub trait OutputBackend: Send + Sync + 'static {
    /// The `PannerNode.panningModel` value that renders a panner as one of this backend's objects.
    fn object_model(&self) -> &'static str;

    fn open(&self, request: &OutputRequest, renderer: Renderer) -> Result<OutputOpen, String>;
}

fn backend_slot() -> &'static RwLock<Option<Arc<dyn OutputBackend>>> {
    static B: OnceLock<RwLock<Option<Arc<dyn OutputBackend>>>> = OnceLock::new();
    B.get_or_init(|| RwLock::new(None))
}

/// Install the embedder's output backend, once per process, before the page builds its contexts.
pub fn set_output_backend(backend: Arc<dyn OutputBackend>) -> bool {
    let mut slot = backend_slot().write().unwrap();
    if slot.is_some() {
        return false;
    }
    *slot = Some(backend);
    true
}

pub(crate) fn backend() -> Option<Arc<dyn OutputBackend>> {
    backend_slot().read().unwrap().clone()
}

/// The panning model the installed backend renders as objects.
pub fn object_model() -> Option<&'static str> {
    backend().map(|b| b.object_model())
}

// ---------------------------------------------------------------------------
// Objects
// ---------------------------------------------------------------------------

/// Where a panner is, in the context's coordinates (the listener's included), and the
/// distance × cone gain the spec gives it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ObjectPlacement {
    pub position: [f32; 3],
    pub orientation: [f32; 3],
    pub listener_position: [f32; 3],
    pub listener_forward: [f32; 3],
    pub listener_up: [f32; 3],
    pub gain: f32,
}

const PLACEMENT_FLOATS: usize = 16;

impl ObjectPlacement {
    fn to_floats(self) -> [f32; PLACEMENT_FLOATS] {
        let mut f = [0.0; PLACEMENT_FLOATS];
        for (k, v) in [
            self.position,
            self.orientation,
            self.listener_position,
            self.listener_forward,
            self.listener_up,
        ]
        .into_iter()
        .enumerate()
        {
            f[3 * k..3 * k + 3].copy_from_slice(&v);
        }
        f[15] = self.gain;
        f
    }

    fn from_floats(f: &[f32; PLACEMENT_FLOATS]) -> Self {
        let v = |k: usize| [f[3 * k], f[3 * k + 1], f[3 * k + 2]];
        Self {
            position: v(0),
            orientation: v(1),
            listener_position: v(2),
            listener_forward: v(3),
            listener_up: v(4),
            gain: f[15],
        }
    }
}

struct SlotAudio {
    left: Vec<f32>,
    right: Vec<f32>,
    len: usize,
    channels: u32,
    writer: u64,
}

struct Slot {
    /// The panner (node handle) the page's thread gave this slot to, 0 when free.
    owner: AtomicU64,
    lost: AtomicBool,
    placement: [AtomicU32; PLACEMENT_FLOATS],
    audio: UnsafeCell<SlotAudio>,
}

/// The objects of one backend stream.
pub(crate) struct ObjectBus {
    slots: Box<[Slot]>,
    capacity: usize,
    lost: AtomicU32,
}

// SAFETY: `Slot::audio` is touched only on the thread rendering the stream (the panners'
// processors and the `Renderer` run there, one after the other); the rest is atomics.
unsafe impl Sync for ObjectBus {}

impl ObjectBus {
    fn new(objects: u32, frames: u32) -> Self {
        let capacity = frames.max(1) as usize;
        let slots = (0..objects)
            .map(|_| Slot {
                owner: AtomicU64::new(0),
                lost: AtomicBool::new(false),
                placement: core::array::from_fn(|_| AtomicU32::new(0)),
                audio: UnsafeCell::new(SlotAudio {
                    left: vec![0.0; capacity],
                    right: vec![0.0; capacity],
                    len: 0,
                    channels: 1,
                    writer: 0,
                }),
            })
            .collect();
        Self {
            slots,
            capacity,
            lost: AtomicU32::new(0),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    pub(crate) fn set_owner(&self, slot: usize, owner: u64) {
        if let Some(s) = self.slots.get(slot) {
            s.owner.store(owner, Ordering::Release);
        }
    }

    pub(crate) fn is_lost(&self, slot: usize) -> bool {
        self.slots.get(slot).is_none_or(|s| s.lost.load(Ordering::Acquire))
    }

    /// How many slots the backend gave up; the page's thread reassigns when it changes.
    pub(crate) fn lost_count(&self) -> u32 {
        self.lost.load(Ordering::Acquire)
    }

    pub(crate) fn release_all(&self) {
        for s in self.slots.iter() {
            s.owner.store(0, Ordering::Release);
        }
    }

    pub(crate) fn place(&self, slot: usize, p: ObjectPlacement) {
        if let Some(s) = self.slots.get(slot) {
            for (a, v) in s.placement.iter().zip(p.to_floats()) {
                a.store(v.to_bits(), Ordering::Relaxed);
            }
        }
    }

    /// Append a panner's input. Returns false when the slot is not `owner`'s (the panner then
    /// renders itself).
    ///
    /// # Safety
    /// Only on the thread rendering the stream.
    pub(crate) unsafe fn write(&self, slot: usize, owner: u64, left: &[f32], right: Option<&[f32]>) -> bool {
        let Some(s) = self.slots.get(slot) else { return false };
        if owner == 0 || s.owner.load(Ordering::Acquire) != owner {
            return false;
        }
        let a = &mut *s.audio.get();
        if a.writer != owner {
            a.writer = owner;
            a.len = 0;
        }
        let n = left.len().min(self.capacity - a.len);
        a.left[a.len..a.len + n].copy_from_slice(&left[..n]);
        match right {
            Some(r) => {
                a.right[a.len..a.len + n].copy_from_slice(&r[..n]);
                a.channels = 2;
            }
            None => a.right[a.len..a.len + n].copy_from_slice(&left[..n]),
        }
        a.len += n;
        true
    }

    /// # Safety
    /// Only on the thread rendering the stream.
    unsafe fn begin(&self) {
        for s in self.slots.iter() {
            let a = &mut *s.audio.get();
            a.len = 0;
            a.writer = 0;
            a.channels = 1;
        }
    }
}

pub(crate) type BusCell = OnceLock<ObjectBus>;

/// One panner's block, after [`Renderer::render`].
pub struct ObjectBlock<'a> {
    /// The object's index, `0..max_objects`.
    pub index: u32,
    /// Changes when the slot passes to another panner: a backend starts a fresh object then.
    pub owner: u64,
    /// 1 or 2: a stereo panner input keeps its two channels.
    pub channels: u32,
    pub left: &'a [f32],
    pub right: &'a [f32],
    pub placement: ObjectPlacement,
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

/// The context's half of a backend stream: render the graph into the backend's blocks.
pub struct Renderer {
    rx: mpsc::Receiver<FirewheelProcessor<RedBackend>>,
    processor: Option<FirewheelProcessor<RedBackend>>,
    bus: Arc<BusCell>,
    started: Instant,
}

impl Renderer {
    /// Render `frames` frames of the destination, interleaved over the stream's channels, into
    /// `out`. `frames` above the stream's `max_block_frames` are rendered silent.
    pub fn render(&mut self, out: &mut [f32], channels: usize, frames: usize) {
        while let Ok(p) = self.rx.try_recv() {
            self.processor = Some(p);
        }
        let frames = frames.min(out.len() / channels.max(1));
        let out = &mut out[..frames * channels];
        let bus = self.bus.get();
        let max = bus.map(|b| b.capacity).unwrap_or(usize::MAX);
        let (head, tail) = out.split_at_mut(frames.min(max) * channels);
        tail.fill(0.0);
        if let Some(b) = bus {
            // SAFETY: this is the rendering thread.
            unsafe { b.begin() };
        }
        let Some(p) = self.processor.as_mut() else {
            head.fill(0.0);
            return;
        };
        let now = Instant::now();
        p.process_interleaved(
            &[],
            head,
            BackendProcessInfo {
                num_in_channels: 0,
                num_out_channels: channels,
                frames: head.len() / channels.max(1),
                process_timestamp: now,
                duration_since_stream_start: now - self.started,
                input_stream_status: StreamStatus::empty(),
                output_stream_status: StreamStatus::empty(),
                dropped_frames: 0,
            },
        );
    }

    /// The backend cannot render object `index` any more (it ran out): its panner goes back to
    /// the context's own panning, and the object is not handed out again on this stream.
    pub fn lose_object(&self, index: u32) {
        let Some(bus) = self.bus.get() else { return };
        let Some(s) = bus.slots.get(index as usize) else { return };
        if !s.lost.swap(true, Ordering::AcqRel) {
            s.owner.store(0, Ordering::Release);
            bus.lost.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// The panners rendered as objects in the last [`Renderer::render`].
    pub fn objects(&self) -> impl Iterator<Item = ObjectBlock<'_>> {
        self.bus.get().into_iter().flat_map(|bus| {
            bus.slots.iter().enumerate().filter_map(|(i, s)| {
                let owner = s.owner.load(Ordering::Acquire);
                // SAFETY: this is the rendering thread, and nothing renders while it reads.
                let a = unsafe { &*s.audio.get() };
                if owner == 0 || a.writer != owner {
                    return None;
                }
                let mut f = [0.0f32; PLACEMENT_FLOATS];
                for (v, at) in f.iter_mut().zip(s.placement.iter()) {
                    *v = f32::from_bits(at.load(Ordering::Relaxed));
                }
                Some(ObjectBlock {
                    index: i as u32,
                    owner,
                    channels: a.channels,
                    left: &a.left[..a.len],
                    right: &a.right[..a.len],
                    placement: ObjectPlacement::from_floats(&f),
                })
            })
        })
    }
}

// ---------------------------------------------------------------------------
// The Firewheel backend
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) struct OutputError(pub String);

impl core::fmt::Display for OutputError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for OutputError {}

#[derive(Default)]
pub(crate) struct RedConfig {
    pub device: Option<cpal::DeviceId>,
    pub sample_rate: Option<u32>,
    /// Open the default device when `device` is gone.
    pub fallback: bool,
    /// A panner asked for the backend's object model.
    pub objects: bool,
}

pub(crate) struct CpalOut {
    errors: mpsc::Receiver<cpal::StreamError>,
    to_stream: mpsc::SyncSender<FirewheelProcessor<RedBackend>>,
    _stream: cpal::Stream,
    pub channels: u32,
    /// Why the embedder's backend did not take the stream.
    pub refused: Option<String>,
}

pub(crate) struct EmbedderOut {
    stream: Box<dyn OutputStream>,
    to_renderer: mpsc::SyncSender<FirewheelProcessor<RedBackend>>,
    pub channels: u32,
    pub channel_mask: u32,
    pub label: String,
    pub bus: Option<Arc<BusCell>>,
}

pub(crate) enum RedBackend {
    Cpal(CpalOut),
    Embedder(EmbedderOut),
}

impl AudioBackend for RedBackend {
    type Enumerator = ();
    type Config = RedConfig;
    type StartStreamError = OutputError;
    type StreamError = OutputError;
    type Instant = Instant;

    fn enumerator() -> Self::Enumerator {}

    fn start_stream(config: RedConfig) -> Result<(Self, StreamInfo), OutputError> {
        let mut refused = None;
        if let Some(b) = backend() {
            match open_embedder(&*b, &config) {
                Ok(v) => return Ok(v),
                Err(e) => refused = config.objects.then_some(e),
            }
        }
        let (mut out, info) = open_cpal(&config)?;
        out.refused = refused;
        Ok((RedBackend::Cpal(out), info))
    }

    fn set_processor(&mut self, processor: FirewheelProcessor<Self>) {
        let tx = match self {
            RedBackend::Cpal(c) => &c.to_stream,
            RedBackend::Embedder(e) => &e.to_renderer,
        };
        // The receiver lives as long as the stream does; a send can only fail after it stopped,
        // and then the processor goes back to the context with this drop.
        let _ = tx.try_send(processor);
    }

    fn poll_status(&mut self) -> Result<(), OutputError> {
        match self {
            RedBackend::Cpal(c) => match c.errors.try_recv() {
                Ok(e) => Err(OutputError(e.to_string())),
                Err(_) => Ok(()),
            },
            RedBackend::Embedder(e) => e.stream.poll().map_err(OutputError),
        }
    }

    fn delay_from_last_process(&self, process_timestamp: Instant) -> Option<Duration> {
        Some(process_timestamp.elapsed())
    }
}

fn open_embedder(b: &dyn OutputBackend, config: &RedConfig) -> Result<(RedBackend, StreamInfo), String> {
    let (tx, rx) = mpsc::sync_channel(1);
    let bus: Arc<BusCell> = Arc::new(OnceLock::new());
    let renderer = Renderer {
        rx,
        processor: None,
        bus: Arc::clone(&bus),
        started: Instant::now(),
    };
    let request = OutputRequest {
        device_id: config.device.as_ref().map(|d| d.1.clone()),
        sample_rate: config.sample_rate,
        objects: config.objects,
    };
    let open = b.open(&request, renderer)?;
    let (Some(rate), Some(block)) = (
        core::num::NonZeroU32::new(open.sample_rate),
        core::num::NonZeroU32::new(open.max_block_frames),
    ) else {
        return Err(format!(
            "the output backend opened a stream of {} Hz and {} frames",
            open.sample_rate, open.max_block_frames
        ));
    };
    if open.channels == 0 || open.channels > super::mixing::MAX_CHANNELS {
        return Err(format!(
            "the output backend opened a stream of {} channels",
            open.channels
        ));
    }
    let _ = bus.set(ObjectBus::new(open.max_objects, open.max_block_frames));
    let info = StreamInfo {
        sample_rate: rate,
        prev_sample_rate: rate,
        max_block_frames: block,
        num_stream_out_channels: open.channels,
        output_device_id: request.device_id.unwrap_or_default(),
        ..Default::default()
    };
    let out = EmbedderOut {
        stream: open.stream,
        to_renderer: tx,
        channels: open.channels,
        channel_mask: open.channel_mask,
        label: open.label,
        bus: (open.max_objects > 0).then_some(bus),
    };
    Ok((RedBackend::Embedder(out), info))
}

fn open_cpal(config: &RedConfig) -> Result<(CpalOut, StreamInfo), OutputError> {
    let err = |e: &dyn core::fmt::Display| OutputError(e.to_string());
    let host = cpal::default_host();
    let mut device = config
        .device
        .as_ref()
        .and_then(|id| host.device_by_id(id))
        .filter(|d| d.supports_output());
    if device.is_none() {
        if config.device.is_some() && !config.fallback {
            return Err(OutputError("the output device is gone".into()));
        }
        device = host.default_output_device();
    }
    let device = device.ok_or_else(|| OutputError("there is no output device".into()))?;
    let id = device.id().map(|d| d.to_string()).unwrap_or_default();
    let default = device.default_output_config().map_err(|e| err(&e))?;

    let mut rate = default.sample_rate();
    let wanted: Vec<u32> = match config.sample_rate {
        Some(r) => vec![r],
        None if rate != 44_100 && rate != 48_000 => vec![48_000, 44_100],
        None => Vec::new(),
    };
    if !wanted.is_empty() {
        if let Ok(configs) = device.supported_output_configs() {
            let configs: Vec<_> = configs.collect();
            if let Some(r) = wanted
                .into_iter()
                .find(|&r| configs.iter().any(|c| c.try_with_sample_rate(r).is_some()))
            {
                rate = r;
            }
        }
    }
    let channels = default.channels().max(1);
    let buffer_size = match default.buffer_size() {
        &cpal::SupportedBufferSize::Range { min, max } => cpal::BufferSize::Fixed(DEFAULT_BLOCK_FRAMES.clamp(min, max)),
        cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Default,
    };
    let max_block = match buffer_size {
        cpal::BufferSize::Fixed(f) => f,
        cpal::BufferSize::Default => DEFAULT_BLOCK_FRAMES,
    };
    let stream_config = cpal::StreamConfig {
        channels,
        sample_rate: rate,
        buffer_size,
    };

    let (to_stream, rx) = mpsc::sync_channel::<FirewheelProcessor<RedBackend>>(1);
    let (err_tx, errors) = mpsc::channel();
    let mut cb = CpalCallback {
        rx,
        processor: None,
        channels: channels as usize,
        rate: rate as f64,
        started: Instant::now(),
        last: None,
        budget: Duration::ZERO,
    };
    let stream = device
        .build_output_stream(
            &stream_config,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| cb.run(out),
            move |e| {
                let _ = err_tx.send(e);
            },
            Some(BUILD_STREAM_TIMEOUT),
        )
        .map_err(|e| err(&e))?;
    stream.play().map_err(|e| err(&e))?;
    let info = StreamInfo {
        sample_rate: core::num::NonZeroU32::new(rate).unwrap_or(core::num::NonZeroU32::new(48_000).unwrap()),
        max_block_frames: core::num::NonZeroU32::new(max_block.max(1)).unwrap(),
        num_stream_out_channels: channels as u32,
        output_device_id: id,
        ..Default::default()
    };
    Ok((
        CpalOut {
            errors,
            to_stream,
            _stream: stream,
            channels: channels as u32,
            refused: None,
        },
        info,
    ))
}

struct CpalCallback {
    rx: mpsc::Receiver<FirewheelProcessor<RedBackend>>,
    processor: Option<FirewheelProcessor<RedBackend>>,
    channels: usize,
    rate: f64,
    started: Instant,
    last: Option<Instant>,
    budget: Duration,
}

impl CpalCallback {
    fn run(&mut self, out: &mut [f32]) {
        let now = Instant::now();
        while let Ok(p) = self.rx.try_recv() {
            self.processor = Some(p);
        }
        let frames = out.len() / self.channels;
        let late = self.last.is_some_and(|t| now - t > self.budget);
        let dropped = if late {
            ((now - self.last.unwrap()).as_secs_f64() * self.rate).round() as u32
        } else {
            0
        };
        self.last = Some(now);
        self.budget = Duration::from_secs_f64(1.5 * frames as f64 / self.rate);
        let Some(p) = self.processor.as_mut() else {
            out.fill(0.0);
            return;
        };
        p.process_interleaved(
            &[],
            out,
            BackendProcessInfo {
                num_in_channels: 0,
                num_out_channels: self.channels,
                frames,
                process_timestamp: now,
                duration_since_stream_start: now - self.started,
                input_stream_status: StreamStatus::empty(),
                output_stream_status: if late {
                    StreamStatus::OUTPUT_UNDERFLOW
                } else {
                    StreamStatus::empty()
                },
                dropped_frames: dropped,
            },
        );
    }
}
