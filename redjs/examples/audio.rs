//! Audible end-to-end probe for the native Web Audio implementation.
//!
//! Stage 1 — pure Firewheel: master VolumeNode → out, SamplerNode with a
//! synthesized 440 Hz sine, polled via SamplerState. Validates our Firewheel
//! usage (graph, events, stream) with no JS involved.
//!
//! Stage 2 — the JS surface: AudioContext + createBuffer + copyToChannel +
//! source.start() through redjs, pumped like a host does. Validates
//! webapi/audio.rs end to end.
//!
//! Run: `cargo run --release --example audio` — you should hear two beeps.

use std::num::NonZeroUsize;
use std::sync::Arc;

use firewheel::{
    collector::ArcGc,
    nodes::{sampler::SamplerNode, volume::VolumeNode},
    sample_resource::{SampleResource, SampleResourceInfo},
    FirewheelContext,
};

struct Sine {
    data: Vec<f32>,
    rate: u32,
}

impl SampleResourceInfo for Sine {
    fn num_channels(&self) -> NonZeroUsize {
        NonZeroUsize::new(1).unwrap()
    }
    fn len_frames(&self) -> u64 {
        self.data.len() as u64
    }
    fn sample_rate(&self) -> Option<std::num::NonZeroU32> {
        std::num::NonZeroU32::new(self.rate)
    }
}

impl SampleResource for Sine {
    fn fill_buffers(&self, buffers: &mut [&mut [f32]], range: std::ops::Range<usize>, start: u64) {
        for out in buffers.iter_mut() {
            let out = &mut out[range.clone()];
            let s = (start as usize).min(self.data.len());
            let n = out.len().min(self.data.len() - s);
            out[..n].copy_from_slice(&self.data[s..s + n]);
            out[n..].fill(0.0);
        }
    }
}

fn stage1() {
    println!("[stage1] pure firewheel…");
    let mut fw = FirewheelContext::new(firewheel::FirewheelConfig::default());
    let master = fw.add_node(VolumeNode::default(), None);
    let out = fw.graph_out_node_id();
    fw.connect(master, out, &[(0, 0), (1, 1)], true).expect("master->out");

    match fw.start_stream(firewheel::cpal::CpalConfig::default()) {
        Ok(()) => println!("[stage1] stream started, rate={:?}", fw.stream_info().map(|i| i.sample_rate.get())),
        Err(e) => {
            println!("[stage1] STREAM FAILED: {e:?}");
            return;
        }
    }

    let rate = fw.stream_info().unwrap().sample_rate.get();
    let sine: Vec<f32> =
        (0..rate as usize).map(|i| (i as f32 * 440.0 * core::f32::consts::TAU / rate as f32).sin() * 0.3).collect();

    let mut sampler = SamplerNode::default();
    sampler.set_sample(ArcGc::new_unsized(|| Arc::new(Sine { data: sine, rate }) as Arc<dyn SampleResource>));
    sampler.start_or_restart();
    let node = fw.add_node(sampler.clone(), None);
    fw.connect(node, master, &[(0, 0), (1, 1)], true).expect("sampler->master");

    for i in 0..30 {
        fw.update().expect("update");
        if i % 5 == 0 {
            let st = fw.node_state::<firewheel::nodes::sampler::SamplerState>(node);
            println!(
                "[stage1] t={:.2}s playing={:?} clock={:.3}",
                i as f64 * 0.05,
                st.map(|s| s.playing()),
                fw.audio_clock().seconds.0
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    fw.stop_stream();
    println!("[stage1] done — did you hear beep #1?");
}

/// DELIBERATE BUG REPRO (firewheel 0.10): starting via the `sync_*_event`
/// helpers stays SILENT — `sync_play_event()` emits `ParamData::Bool`, but
/// the derived Patch for `play: Notify<bool>` only downcasts
/// `ParamData::any(Notify<bool>)`, so the processor drops the event.
/// webapi/audio.rs therefore uses Diff-generated events (see stage2).
fn stage1b() {
    println!("[stage1b] sync_*_event start (expected SILENT — firewheel bug repro)…");
    let mut fw = FirewheelContext::new(firewheel::FirewheelConfig::default());
    let master = fw.add_node(VolumeNode::default(), None);
    let out = fw.graph_out_node_id();
    fw.connect(master, out, &[(0, 0), (1, 1)], true).unwrap();
    if fw.start_stream(firewheel::cpal::CpalConfig::default()).is_err() {
        println!("[stage1b] no stream");
        return;
    }
    let rate = fw.stream_info().unwrap().sample_rate.get();
    let sine: Vec<f32> =
        (0..rate as usize).map(|i| (i as f32 * 550.0 * core::f32::consts::TAU / rate as f32).sin() * 0.3).collect();

    let mut sampler = SamplerNode::default();
    let node = fw.add_node(sampler.clone(), None);
    fw.connect(node, master, &[(0, 0), (1, 1)], true).unwrap();
    fw.update().unwrap();

    // Now start it the webapi way: mutate the local copy + queue sync events.
    sampler.set_sample(ArcGc::new_unsized(|| Arc::new(Sine { data: sine, rate }) as Arc<dyn SampleResource>));
    sampler.start_from(firewheel::nodes::sampler::PlayFrom::Seconds(0.0));
    for ev in [
        sampler.sync_sample_event(),
        sampler.sync_repeat_mode_event(),
        sampler.sync_speed_event(),
        sampler.sync_volume_event(),
        sampler.sync_play_from_event(),
        sampler.sync_play_event(),
    ] {
        fw.queue_event_for(node, ev);
    }

    for i in 0..30 {
        fw.update().expect("update");
        if i % 5 == 0 {
            let st = fw.node_state::<firewheel::nodes::sampler::SamplerState>(node);
            println!("[stage1b] t={:.2}s playing={:?}", i as f64 * 0.05, st.map(|s| s.playing()));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    fw.stop_stream();
    println!("[stage1b] done — did you hear beep #1b?");
}

fn stage2() {
    unsafe {
        println!("[stage2] JS AudioContext through redjs…");
        assert_eq!(redjs::ffi::redjs_init(core::ptr::null()), 0);
        let g = redjs::ffi::redjs_context_create();
        assert!(!g.is_null());
        assert_eq!(redjs::ffi::redjs_web_install(g), 0);
        let root: Vec<u16> = std::env::temp_dir().to_string_lossy().encode_utf16().collect();
        assert_eq!(redjs::ffi::redjs_webshim_install(g, root.as_ptr(), root.len(), root.as_ptr(), root.len()), 0);

        let src = r#"
            globalThis.log = [];
            const ac = new AudioContext();
            log.push('state=' + ac.state, 'rate=' + ac.sampleRate);
            const rate = ac.sampleRate;
            const buf = ac.createBuffer(1, rate, rate);
            const ch = new Float32Array(rate);
            for (let i = 0; i < rate; i++) ch[i] = Math.sin(i * 660 * 2 * Math.PI / rate) * 0.3;
            buf.copyToChannel(ch, 0);
            const src = ac.createBufferSource();
            src.buffer = buf;
            const gain = ac.createGain();
            gain.gain.value = 1.0;
            src.connect(gain); gain.connect(ac.destination);
            src.onended = () => log.push('ended');
            src.start();
            log.push('started');
        "#;
        let s16: Vec<u16> = src.encode_utf16().collect();
        let url: Vec<u16> = "probe.js".encode_utf16().collect();
        let mut out = 0u64;
        let rc = redjs::glue::rjs_eval16(g, s16.as_ptr(), s16.len(), url.as_ptr(), url.len(), &mut out);
        if rc != 0 {
            let (mut m, mut st) = (0u64, 0u64);
            redjs::glue::rjs_error_info(g, out, &mut m, &mut st);
            let mut info = redjs::ffi::RjsStr::default();
            redjs::glue::rjs_string_info(g, m, &mut info);
            let msg: String = if info.is8bit != 0 {
                core::slice::from_raw_parts(info.ptr as *const u8, info.len as usize).iter().map(|&b| b as char).collect()
            } else {
                String::from_utf16_lossy(core::slice::from_raw_parts(info.ptr as *const u16, info.len as usize))
            };
            panic!("[stage2] eval threw: {msg}");
        }

        for i in 0..30 {
            redjs::ffi::redjs_pump(g, 7, core::ptr::null_mut());
            if i % 10 == 0 {
                let mut lv = 0u64;
                let a = redjs::glue::rjs_atom8(g, "log".as_ptr(), 3);
                redjs::glue::rjs_get(g, redjs::glue::rjs_global(g), a, &mut lv);
                redjs::glue::rjs_atom_free(g, a);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // Read the log + currentTime.
        let probe = "log.join('|') + '|t=' + ac.currentTime.toFixed(2)";
        let p16: Vec<u16> = probe.encode_utf16().collect();
        let mut out = 0u64;
        let rc = redjs::glue::rjs_eval16(g, p16.as_ptr(), p16.len(), core::ptr::null(), 0, &mut out);
        if rc == 0 {
            let mut info = redjs::ffi::RjsStr::default();
            if redjs::glue::rjs_string_info(g, out, &mut info) == 0 {
                let msg: String = if info.is8bit != 0 {
                    core::slice::from_raw_parts(info.ptr as *const u8, info.len as usize).iter().map(|&b| b as char).collect()
                } else {
                    String::from_utf16_lossy(core::slice::from_raw_parts(info.ptr as *const u16, info.len as usize))
                };
                println!("[stage2] js log: {msg}");
            }
        }
        println!("[stage2] done — did you hear beep #2?");
    }
}

fn main() {
    stage1();
    stage1b();
    stage2();
}
