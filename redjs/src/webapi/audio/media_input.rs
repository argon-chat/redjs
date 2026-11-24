use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use firewheel::channel_config::{ChannelConfig, ChannelCount};
use firewheel::mask::SilenceMask;
use firewheel::node::*;

use super::*;

#[derive(Default)]
struct Buffer {
    samples: VecDeque<f32>,
    rate: u32,
    channels: usize,
    converter: super::super::mediastream::Rerate,
}

/// Interleaved PCM from a native media decoder or a graph-backed track.
pub struct MediaInput {
    rate: AtomicU32,
    channels: AtomicU32,
    buffer: Mutex<Buffer>,
}

impl MediaInput {
    pub(super) fn new(rate: u32, channels: u32) -> Arc<Self> {
        Arc::new(Self {
            rate: AtomicU32::new(rate),
            channels: AtomicU32::new(channels),
            buffer: Mutex::default(),
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.rate.load(Ordering::Relaxed)
    }

    pub(super) fn channels(&self) -> u32 {
        self.channels.load(Ordering::Relaxed)
    }

    pub fn flush(&self) {
        let mut b = self.buffer.lock().unwrap();
        b.samples.clear();
        b.converter = Default::default();
    }

    pub fn push(&self, data: &[f32], channels: usize, rate: u32, gain: f32) {
        if !(1..=mixing::MAX_CHANNELS as usize).contains(&channels) || rate == 0 {
            return;
        }
        let Ok(mut guard) = self.buffer.try_lock() else {
            return;
        };
        let b = &mut *guard;
        let output_rate = self.sample_rate();
        if (b.channels, b.rate) != (channels, rate) {
            b.samples.clear();
            b.converter = Default::default();
            b.channels = channels;
            b.rate = rate;
        }
        self.channels.store(channels as u32, Ordering::Relaxed);
        let data = &data[..data.len() / channels * channels];
        let data = if rate == output_rate {
            data
        } else {
            b.converter.run(data, channels, rate, output_rate)
        };
        // Keep the newest 200 ms if the context stalls or is suspended.
        let capacity = (output_rate as usize / 5).max(1) * channels;
        let data = &data[data.len().saturating_sub(capacity)..];
        let discard = (b.samples.len() + data.len()).saturating_sub(capacity);
        b.samples.drain(..discard);
        b.samples.extend(data.iter().map(|v| v * gain));
    }

    fn read(&self, out: &mut [f32], frames: usize) -> usize {
        let Ok(mut b) = self.buffer.try_lock() else {
            return 0;
        };
        if b.samples.is_empty() {
            return 0;
        }
        let channels = b.channels;
        let count = (frames * channels).min(b.samples.len());
        for (o, s) in out.iter_mut().zip(b.samples.drain(..count)) {
            *o = s
        }
        out[count..frames * channels].fill(0.0);
        channels
    }
}

pub(super) struct InputNode(pub Arc<MediaInput>);

impl AudioNode for InputNode {
    type Configuration = EmptyConfig;
    fn info(&self, _: &EmptyConfig) -> AudioNodeInfo {
        AudioNodeInfo::new()
            .debug_name("media_source")
            .channel_config(ChannelConfig {
                num_inputs: ChannelCount::ZERO,
                num_outputs: ChannelCount::new(mixing::MAX_CHANNELS).unwrap(),
            })
    }
    fn construct_processor(
        &self,
        _: &EmptyConfig,
        cx: ConstructProcessorContext,
    ) -> impl AudioNodeProcessor {
        Processor {
            input: self.0.clone(),
            block: vec![
                0.0;
                cx.stream_info.max_block_frames.get() as usize
                    * mixing::MAX_CHANNELS as usize
            ],
        }
    }
}

struct Processor {
    input: Arc<MediaInput>,
    block: Vec<f32>,
}

impl AudioNodeProcessor for Processor {
    fn process(
        &mut self,
        info: &ProcInfo,
        buffers: ProcBuffers,
        _: &mut firewheel::event::ProcEvents,
        _: &mut ProcExtra,
    ) -> ProcessStatus {
        let width = self.input.read(&mut self.block, info.frames);
        if width == 0 {
            return ProcessStatus::ClearAllOutputs;
        }
        let mut mask = SilenceMask::NONE_SILENT;
        for (ch, out) in buffers.outputs.iter_mut().enumerate() {
            if ch < width {
                for (o, frame) in out[..info.frames]
                    .iter_mut()
                    .zip(self.block.chunks_exact(width))
                {
                    *o = frame[ch]
                }
            } else {
                out[..info.frames].fill(0.0);
                mask.set_channel(ch, true);
            }
        }
        ProcessStatus::outputs_modified_with_silence_mask(mask)
    }
    fn new_stream(&mut self, stream: &firewheel::StreamInfo, _: &mut ProcStreamCtx) {
        if self
            .input
            .rate
            .swap(stream.sample_rate.get(), Ordering::Relaxed)
            != stream.sample_rate.get()
        {
            self.input.flush();
        }
        self.block.resize(
            stream.max_block_frames.get() as usize * mixing::MAX_CHANNELS as usize,
            0.0,
        );
    }
}

pub enum ElementError {
    InvalidElement,
    AlreadyConnected,
    Unavailable(String),
}

pub type ElementHost = unsafe fn(*mut c_void, u64, Arc<MediaInput>) -> Result<(), ElementError>;

/// Installs the embedder's media-element resolver for this JS realm.
pub fn set_media_element_host(g: *mut c_void, host: ElementHost) {
    let _ = classes(g).element_host.set(host);
}

unsafe fn make(g: *mut c_void, engine: u64, element: u64, proto: u64) -> u64 {
    if !is_cell(element) {
        return throw_type_error(g, "Expected an HTMLMediaElement");
    }
    let rate = {
        let gl = glob().lock().unwrap();
        let Some(e) = gl.engines.get(&engine).filter(|e| e.state != ST_CLOSED) else {
            return super::super::domexception::throw(
                g,
                "InvalidStateError",
                "The AudioContext is closed",
            );
        };
        e.sample_rate
    };
    let c = classes(g);
    let Some(attach) = c.element_host.get() else {
        return super::super::domexception::throw(
            g,
            "NotSupportedError",
            "This host has no media element player",
        );
    };
    let input = MediaInput::new(rate, 1);
    match attach(g, element, input.clone()) {
        Ok(()) => {}
        Err(ElementError::InvalidElement) => {
            return throw_type_error(g, "Expected an HTMLMediaElement")
        }
        Err(ElementError::AlreadyConnected) => {
            return super::super::domexception::throw(
                g,
                "InvalidStateError",
                "This media element already has an audio source node",
            )
        }
        Err(ElementError::Unavailable(reason)) => {
            return super::super::domexception::throw(g, "NotSupportedError", &reason)
        }
    }
    let node = create_node_on(g, engine, Params::MediaInput(input), proto, &[]);
    if is_cell(node) {
        define(g, slots_of(g, node), "mediaElement", element, 0);
        glue::rjs_define(g, element, c.s_element_source, node, 0);
    }
    node
}

#[crate::host_fn]
pub(super) unsafe fn create(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(engine) = this_engine_handle(g, cf) else {
        return throw_type_error(g, "Expected an AudioContext");
    };
    make(g, engine, cf_arg(cf, 0), classes(g).element_src.proto)
}

#[crate::host_fn]
unsafe fn construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Ok(engine) = effect_nodes::context_arg(g, cf, "MediaElementAudioSourceNode") else {
        return UNDEF;
    };
    let Ok(dict) = effect_nodes::options_arg(g, cf, "MediaElementAudioSourceNode") else {
        return UNDEF;
    };
    let Ok(el) = effect_nodes::member(g, dict, "mediaElement") else {
        return UNDEF;
    };
    make(
        g,
        engine,
        el,
        effect_nodes::proto_for(g, cf, classes(g).element_src.proto),
    )
}

#[crate::host_fn]
unsafe fn element(g: *mut c_void, cf: *mut c_void) -> u64 {
    let obj = cf_this(cf);
    if is_cell(obj) {
        let slots = slots_of(g, obj);
        if is_cell(slots) {
            let el = get(g, slots, "mediaElement");
            if is_cell(el) {
                return el;
            }
        }
    }
    throw_type_error(g, "Illegal invocation")
}

pub(super) unsafe fn install(g: *mut c_void, parent_proto: u64) -> Class {
    define_class(
        g,
        &ClassSpec {
            name: "MediaElementAudioSourceNode",
            arity: 2,
            construct: Some(construct),
            parent_proto,
            methods: &[],
            getters: &[("mediaElement", element)],
            accessors: &[],
        },
    )
}
