//! Multichannel graphs and the output backend hook, rendered offline: the test's backend takes
//! the context's output and the test pulls each block, so every sample is known and nothing
//! plays. Contexts of other tests (other threads) are refused and go to cpal as before.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::*;
use crate::api::Context;
use crate::webapi::testing::*;

const RATE: u32 = 48_000;
const BLOCK: usize = 128;

#[derive(Clone)]
enum Plan {
    Refuse(&'static str),
    /// Refuse a context without object panners, so it starts on cpal.
    Objects {
        block: u32,
        channels: u32,
        mask: u32,
    },
    Accept {
        channels: u32,
        mask: u32,
        objects: u32,
    },
}

thread_local! {
    static PLAN: RefCell<Option<Plan>> = const { RefCell::new(None) };
    static RENDERER: RefCell<Option<Renderer>> = const { RefCell::new(None) };
    static LOST: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
    static ASKED: RefCell<Vec<OutputRequest>> = const { RefCell::new(Vec::new()) };
}

struct TestBackend;

struct TestStream(Arc<AtomicBool>);

impl OutputStream for TestStream {
    fn poll(&mut self) -> Result<(), String> {
        if self.0.load(Ordering::Relaxed) {
            Err("the test reset it".into())
        } else {
            Ok(())
        }
    }
}

impl Drop for TestStream {
    fn drop(&mut self) {
        RENDERER.with(|r| r.borrow_mut().take());
    }
}

impl OutputBackend for TestBackend {
    fn object_model(&self) -> &'static str {
        "spatial"
    }

    fn open(&self, request: &OutputRequest, renderer: Renderer) -> Result<OutputOpen, String> {
        ASKED.with(|a| a.borrow_mut().push(request.clone()));
        match PLAN.with(|p| p.borrow().clone()) {
            None => Err("not a context of this test".into()),
            Some(Plan::Refuse(why)) => Err(why.into()),
            Some(Plan::Objects { .. }) if !request.objects => Err("no objects asked for".into()),
            Some(Plan::Objects {
                block,
                channels,
                mask,
            }) => {
                let lost = Arc::new(AtomicBool::new(false));
                LOST.with(|l| *l.borrow_mut() = Some(Arc::clone(&lost)));
                RENDERER.with(|r| *r.borrow_mut() = Some(renderer));
                Ok(OutputOpen {
                    stream: Box::new(TestStream(lost)),
                    sample_rate: RATE,
                    channels,
                    channel_mask: mask,
                    max_block_frames: block,
                    max_objects: 1,
                    label: "test-objects".into(),
                })
            }
            Some(Plan::Accept {
                channels,
                mask,
                objects,
            }) => {
                let lost = Arc::new(AtomicBool::new(false));
                LOST.with(|l| *l.borrow_mut() = Some(Arc::clone(&lost)));
                RENDERER.with(|r| *r.borrow_mut() = Some(renderer));
                Ok(OutputOpen {
                    stream: Box::new(TestStream(lost)),
                    sample_rate: RATE,
                    channels,
                    channel_mask: mask,
                    max_block_frames: BLOCK as u32,
                    max_objects: objects,
                    label: "test-objects".into(),
                })
            }
        }
    }
}

fn context(plan: Plan) -> Context {
    set_output_backend(Arc::new(TestBackend));
    PLAN.with(|p| *p.borrow_mut() = Some(plan));
    ctx()
}

/// Ship the graph and render `blocks` blocks; the last one comes back.
fn render(c: &Context, channels: usize, blocks: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; BLOCK * channels];
    for _ in 0..blocks {
        unsafe { super::pump(c.raw()) };
        c.drain_microtasks();
        RENDERER.with(|r| {
            r.borrow_mut()
                .as_mut()
                .expect("the backend has the output")
                .render(&mut out, channels, BLOCK)
        });
    }
    out
}

fn last_frame(block: &[f32], channels: usize) -> Vec<f32> {
    block[block.len() - channels..].to_vec()
}

fn close(v: f32, want: f32) -> bool {
    (v - want).abs() < 1e-4
}

fn engine_of_test(c: &Context) -> u64 {
    let gl = glob().lock().unwrap();
    gl.engines
        .iter()
        .find(|(_, e)| e.g == c.raw() as usize)
        .map(|(&h, _)| h)
        .unwrap()
}

/// A source that plays channel k at the constant (k + 1) / 10.
const LEVELS: &str = "function levels(a, channels) {
    const b = a.createBuffer(channels, 48000, 48000);
    for (let k = 0; k < channels; k++) b.copyToChannel(new Float32Array(48000).fill((k + 1) / 10), k);
    const s = a.createBufferSource(); s.buffer = b; s.loop = true; return s;
}";

#[test]
fn a_six_channel_output_carries_a_five_one_buffer_and_down_mixes_it_for_stereo() {
    let c = context(Plan::Accept {
        channels: 6,
        mask: 0,
        objects: 0,
    });
    s(&c, LEVELS);
    ok(
        &c,
        "globalThis.__a = new AudioContext();
         __a.destination.maxChannelCount === 6 && __a.destination.channelCount === 2
           && __a.destination.channelCountMode === 'explicit'
           && __a.destination.channelInterpretation === 'speakers'",
    );
    let refused = throws(&c, "__a.destination.channelCount = 7");
    assert!(refused.contains("outside the range [1, 6]"), "{refused}");
    s(&c, "__a.destination.channelCount = 6; globalThis.__s = levels(__a, 6); __s.connect(__a.destination); __s.start(); 0");
    let f = last_frame(&render(&c, 6, 8), 6);
    for (k, v) in f.iter().enumerate() {
        assert!(
            close(*v, (k + 1) as f32 / 10.0),
            "channel {k} is {v}: {f:?}"
        );
    }

    s(&c, "__a.destination.channelCount = 2; 0");
    let f = last_frame(&render(&c, 6, 4), 6);
    let h = core::f32::consts::FRAC_1_SQRT_2;
    assert!(close(f[0], 0.1 + h * (0.3 + 0.5)), "{f:?}");
    assert!(close(f[1], 0.2 + h * (0.3 + 0.6)), "{f:?}");
    assert!(
        f[2..].iter().all(|&v| v == 0.0),
        "a stereo destination plays on L and R only: {f:?}"
    );
    s(&c, "__a.close(); 0");
}

#[test]
fn a_mono_source_plays_from_the_centre_of_a_five_one_destination() {
    let c = context(Plan::Accept {
        channels: 6,
        mask: 0,
        objects: 0,
    });
    s(&c, LEVELS);
    s(
        &c,
        "globalThis.__a = new AudioContext(); const g = __a.createGain();
         const m = levels(__a, 1); m.connect(g); g.connect(__a.destination); m.start();
         __a.destination.channelCount = 6; 0",
    );
    let f = last_frame(&render(&c, 6, 8), 6);
    assert_eq!(
        f.iter()
            .map(|v| (v * 10.0).round() as i32)
            .collect::<Vec<_>>(),
        [0, 0, 1, 0, 0, 0],
        "{f:?}"
    );
    s(&c, "__a.close(); 0");
}

#[test]
fn every_node_has_the_channel_attributes_and_their_spec_rules() {
    let c = context(Plan::Accept {
        channels: 2,
        mask: 0,
        objects: 0,
    });
    ok(
        &c,
        "const a = new AudioContext(); const g = a.createGain(); const p = a.createStereoPanner();
         const r = g.channelCount === 2 && g.channelCountMode === 'max' && g.channelInterpretation === 'speakers'
           && p.channelCountMode === 'clamped-max';
         g.channelCount = 6; g.channelCountMode = 'explicit'; g.channelInterpretation = 'discrete';
         g.channelCountMode = 'nonsense';
         const r2 = g.channelCount === 6 && g.channelCountMode === 'explicit' && g.channelInterpretation === 'discrete';
         a.close(); r && r2",
    );
    for (stmt, name) in [
        ("{ const a = new AudioContext(); try { a.createGain().channelCount = 0; } finally { a.close(); } }", "NotSupportedError"),
        ("{ const a = new AudioContext(); try { a.createGain().channelCount = 33; } finally { a.close(); } }", "NotSupportedError"),
        ("{ const a = new AudioContext(); try { a.createPanner().channelCount = 3; } finally { a.close(); } }", "NotSupportedError"),
        ("{ const a = new AudioContext(); try { a.createStereoPanner().channelCountMode = 'max'; } finally { a.close(); } }", "NotSupportedError"),
        ("{ const a = new AudioContext(); try { a.destination.channelCount = 3; } finally { a.close(); } }", "IndexSizeError"),
    ] {
        let got = s(&c, &format!("(() => {{ try {{ {stmt} return 'no throw'; }} catch (e) {{ return e.name; }} }})()"));
        assert_eq!(got, name, "{stmt}");
    }
}

#[test]
fn a_stereo_graph_takes_no_mixer_and_no_rebuild() {
    let c = context(Plan::Accept {
        channels: 2,
        mask: 0,
        objects: 0,
    });
    s(&c, LEVELS);
    s(
        &c,
        "globalThis.__a = new AudioContext();
         const src = levels(__a, 2), o = __a.createOscillator(), g = __a.createGain(), f = __a.createBiquadFilter(),
           p = __a.createPanner(), an = __a.createAnalyser();
         src.connect(f).connect(p).connect(g); o.connect(g); g.connect(an).connect(__a.destination);
         src.start(); o.start(); 0",
    );
    let eh = engine_of_test(&c);
    let gl = glob().lock().unwrap();
    let eng = &gl.engines[&eh];
    for r in gl.nodes.values().filter(|r| r.engine == eh) {
        assert!(r.in_ch <= 2 && r.out_ch <= 2, "a node grew: {:?}", r.id);
        for w in r.wires.values() {
            assert!(
                matches!(w.route(), mixing::Route::Ports(_)),
                "a stereo connection took a mixer"
            );
        }
    }
    assert!(
        matches!(eng.out_wire.as_ref().map(|w| w.route()), Some(mixing::Route::Ports(p)) if p == &vec![(0, 0), (1, 1)])
    );
    drop(gl);
    s(&c, "__a.close(); 0");
}

#[test]
fn a_splitter_and_merger_carry_six_channels_through_a_gain() {
    let c = context(Plan::Accept {
        channels: 6,
        mask: 0,
        objects: 0,
    });
    s(&c, LEVELS);
    s(
        &c,
        "globalThis.__a = new AudioContext(); __a.destination.channelCount = 6;
         const src = levels(__a, 6), sp = __a.createChannelSplitter(6), m = __a.createChannelMerger(6),
           g = __a.createGain();
         src.connect(sp);
         for (let k = 0; k < 6; k++) sp.connect(m, k, 5 - k);
         m.connect(g).connect(__a.destination); src.start(); 0",
    );
    let f = last_frame(&render(&c, 6, 8), 6);
    for (k, v) in f.iter().enumerate() {
        assert!(
            close(*v, (6 - k) as f32 / 10.0),
            "channel {k} is {v}: {f:?}"
        );
    }
    s(&c, "__a.close(); 0");
}

#[test]
fn a_panner_in_the_object_model_reaches_the_backend_and_the_rest_fall_back_to_hrtf() {
    let c = context(Plan::Accept {
        channels: 2,
        mask: 0,
        objects: 1,
    });
    s(&c, LEVELS);
    s(
        &c,
        "globalThis.__a = new AudioContext();
         globalThis.__p1 = __a.createPanner(); globalThis.__p2 = __a.createPanner();
         __p1.panningModel = 'spatial'; __p2.panningModel = 'spatial';
         __p1.positionX.value = 3; __p1.positionZ.value = -4;
         const s1 = levels(__a, 1); s1.connect(__p1).connect(__a.destination); s1.start(); 0",
    );
    assert_eq!(
        s(&c, "__p1.panningModel + ' ' + __p2.panningModel"),
        "spatial spatial"
    );
    let asked = ASKED.with(|a| a.borrow().last().cloned()).unwrap();
    assert!(asked.objects, "the reopened output asks for objects");

    let bed = render(&c, 2, 8);
    assert!(
        bed.iter().all(|&v| v == 0.0),
        "the object's panner left the bed silent"
    );
    let objects: Vec<(u32, u32, Vec<f32>, ObjectPlacement)> = RENDERER.with(|r| {
        r.borrow()
            .as_ref()
            .unwrap()
            .objects()
            .map(|o| (o.index, o.channels, o.left.to_vec(), o.placement))
            .collect()
    });
    assert_eq!(objects.len(), 1, "one object for one slot");
    let (index, channels, samples, placement) = &objects[0];
    assert_eq!((*index, *channels, samples.len()), (0, 1, BLOCK));
    assert!(
        samples.iter().all(|&v| close(v, 0.1)),
        "the object carries the panner's input"
    );
    assert_eq!(placement.position, [3.0, 0.0, -4.0]);
    assert_eq!(placement.listener_forward, [0.0, 0.0, -1.0]);
    assert!(
        close(placement.gain, 1.0 / 5.0),
        "inverse distance at 5: {}",
        placement.gain
    );

    s(
        &c,
        "const s2 = levels(__a, 1); s2.connect(__p2).connect(__a.destination); s2.start(); 0",
    );
    let bed = render(&c, 2, 8);
    assert!(
        bed.iter().any(|&v| v != 0.0),
        "the panner without an object renders into the bed"
    );

    let status = unsafe { output_status(c.raw(), c.eval("__a").unwrap().0) }.unwrap();
    assert_eq!(
        status.objects,
        Some(ObjectOutput {
            label: "test-objects".into(),
            max_objects: 1,
            in_use: 1
        })
    );

    s(&c, "__p1.panningModel = 'HRTF'; 0");
    render(&c, 2, 2);
    let gl = glob().lock().unwrap();
    let p2 = gl
        .nodes
        .values()
        .filter(|r| r.engine == engine_of_test_locked(&gl, &c))
        .filter(|r| r.object != 0)
        .count();
    assert_eq!(p2, 1, "the freed object passes to the waiting panner");
    drop(gl);
    s(&c, "__a.close(); 0");
}

fn engine_of_test_locked(gl: &Glob, c: &Context) -> u64 {
    gl.engines
        .iter()
        .find(|(_, e)| e.g == c.raw() as usize)
        .map(|(&h, _)| h)
        .unwrap()
}

#[test]
fn a_refusing_backend_leaves_the_panner_on_hrtf_and_says_why() {
    let c = context(Plan::Refuse("no spatial format is enabled"));
    s(
        &c,
        "globalThis.__a = new AudioContext(); globalThis.__p = __a.createPanner(); __p.panningModel = 'spatial'; 0",
    );
    assert_eq!(s(&c, "__p.panningModel"), "spatial");
    let status = unsafe { output_status(c.raw(), c.eval("__a").unwrap().0) }.unwrap();
    assert!(status.objects_requested && status.objects.is_none());
    assert_eq!(
        status.refused.as_deref(),
        Some("no spatial format is enabled")
    );
    let gl = glob().lock().unwrap();
    let eh = engine_of_test_locked(&gl, &c);
    let hrtf = gl
        .nodes
        .values()
        .any(|r| r.engine == eh && matches!(r.params, Params::Spatial(p) if p.hrtf));
    assert!(hrtf, "the panner renders with HRTF");
    drop(gl);
    s(&c, "__a.close(); 0");
}

#[test]
fn a_plain_context_does_not_ask_for_objects_and_an_unknown_model_is_ignored() {
    let c = context(Plan::Accept {
        channels: 2,
        mask: 0,
        objects: 4,
    });
    s(
        &c,
        "globalThis.__a = new AudioContext(); globalThis.__p = __a.createPanner(); __p.panningModel = 'Spatial'; 0",
    );
    assert_eq!(s(&c, "__p.panningModel"), "equalpower");
    let asked = ASKED.with(|a| a.borrow().clone());
    assert!(asked.iter().all(|r| !r.objects), "{asked:?}");
    s(&c, "__a.close(); 0");
}

#[test]
fn a_lost_backend_stream_is_reopened() {
    let c = context(Plan::Accept {
        channels: 2,
        mask: 0,
        objects: 2,
    });
    s(
        &c,
        "globalThis.__a = new AudioContext(); globalThis.__p = __a.createPanner(); __p.panningModel = 'spatial'; 0",
    );
    let before = ASKED.with(|a| a.borrow().len());
    LOST.with(|l| l.borrow().as_ref().unwrap().store(true, Ordering::Relaxed));
    unsafe { super::pump(c.raw()) };
    unsafe { super::pump(c.raw()) };
    assert_eq!(
        ASKED.with(|a| a.borrow().len()),
        before + 1,
        "the backend is asked again"
    );
    let status = unsafe { output_status(c.raw(), c.eval("__a").unwrap().0) }.unwrap();
    assert_eq!(
        status.objects.map(|o| o.in_use),
        Some(1),
        "the panner has an object of the new stream"
    );
    s(&c, "__a.close(); 0");
}

#[test]
fn an_object_the_backend_gives_up_moves_the_panner_on_and_then_back_to_hrtf() {
    let c = context(Plan::Accept {
        channels: 2,
        mask: 0,
        objects: 2,
    });
    s(&c, LEVELS);
    s(
        &c,
        "globalThis.__a = new AudioContext(); const p = __a.createPanner(); p.panningModel = 'spatial';
         const s1 = levels(__a, 1); s1.connect(p).connect(__a.destination); s1.start(); 0",
    );
    let slots = || {
        let gl = glob().lock().unwrap();
        let eh = engine_of_test_locked(&gl, &c);
        gl.nodes
            .values()
            .filter(|r| r.engine == eh && matches!(r.params, Params::Spatial(_)))
            .map(|r| r.object)
            .collect::<Vec<_>>()
    };
    render(&c, 2, 2);
    assert_eq!(slots(), [1]);

    RENDERER.with(|r| r.borrow().as_ref().unwrap().lose_object(0));
    let bed = render(&c, 2, 4);
    assert_eq!(slots(), [2], "the panner takes the object left");
    assert!(bed.iter().all(|&v| v == 0.0));

    RENDERER.with(|r| r.borrow().as_ref().unwrap().lose_object(1));
    let bed = render(&c, 2, 4);
    assert_eq!(slots(), [0], "no object is left");
    assert!(
        bed.iter().any(|&v| v != 0.0),
        "the panner renders into the bed"
    );
    let status = unsafe { output_status(c.raw(), c.eval("__a").unwrap().0) }.unwrap();
    assert_eq!(status.objects.map(|o| o.in_use), Some(0));
    s(&c, "__a.close(); 0");
}

#[test]
fn the_gains_after_an_object_panner_reach_its_object_and_a_dead_end_silences_it() {
    let c = context(Plan::Accept {
        channels: 2,
        mask: 0,
        objects: 1,
    });
    s(&c, LEVELS);
    s(
        &c,
        "globalThis.__a = new AudioContext(); globalThis.__p = __a.createPanner(); __p.panningModel = 'spatial';
         globalThis.__g = __a.createGain(); __g.gain.value = 0.5; globalThis.__h = __a.createGain(); __h.gain.value = 0.25;
         const s1 = levels(__a, 1); s1.connect(__p); __p.connect(__g).connect(__a.destination);
         __p.connect(__h).connect(__a.destination); s1.start(); 0",
    );
    let gain = || {
        render(&c, 2, 2);
        RENDERER.with(|r| {
            r.borrow()
                .as_ref()
                .unwrap()
                .objects()
                .next()
                .map(|o| o.placement.gain)
        })
    };
    assert_eq!(gain(), Some(0.5), "the louder of the two paths");
    s(&c, "__g.gain.value = 0.1; 0");
    assert_eq!(gain(), Some(0.25));
    s(&c, "__a.suspend(); 0");
    assert_eq!(gain(), Some(0.0), "a suspended context is silent");
    s(
        &c,
        "__a.resume(); __p.disconnect(); __p.connect(__a.createAnalyser()); 0",
    );
    assert_eq!(
        gain(),
        Some(0.0),
        "a panner that reaches no destination is silent"
    );
    s(&c, "__a.close(); 0");
}

#[test]
fn a_context_moves_from_cpal_to_the_backend_when_a_panner_asks() {
    let c = context(Plan::Objects {
        block: 333,
        channels: 2,
        mask: 0,
    });
    s(&c, LEVELS);
    s(
        &c,
        "globalThis.__a = new AudioContext(); const g = __a.createGain(); g.connect(__a.destination);
         const s1 = levels(__a, 1); s1.connect(g); s1.start(); 0",
    );
    s(
        &c,
        "const p = __a.createPanner(); p.panningModel = 'spatial'; p.connect(__a.destination); 0",
    );
    let mut out = vec![0.0f32; 333 * 2];
    for _ in 0..8 {
        unsafe { super::pump(c.raw()) };
        RENDERER.with(|r| {
            r.borrow_mut()
                .as_mut()
                .expect("the backend took the output")
                .render(&mut out, 2, 333)
        });
    }
    assert!(
        out.iter().any(|&v| v != 0.0),
        "the graph plays on the backend's stream"
    );
    s(&c, "__a.close(); 0");
}

#[test]
fn an_object_panner_on_a_7_1_4_bed_leaves_the_bed_to_the_other_sources_and_follows_its_position() {
    const FRAMES: usize = 480;
    const CH: usize = 12;
    let c = context(Plan::Objects {
        block: FRAMES as u32,
        channels: CH as u32,
        mask: 0x2d63f,
    });
    s(&c, LEVELS);
    s(
        &c,
        "globalThis.__a = new AudioContext();
         const n = 48000, d = new Float32Array(n);
         for (let i = 0; i < n; i++) d[i] = Math.sin(i * 12.9898) * 43758.5453 % 1;
         const nb = __a.createBuffer(1, n, n); nb.copyToChannel(d, 0);
         const noise = __a.createBufferSource(); noise.buffer = nb; noise.loop = true;
         globalThis.__p = __a.createPanner(); __p.panningModel = 'spatial'; __p.rolloffFactor = 0;
         noise.connect(__p).connect(__a.destination); noise.start();
         const bed = levels(__a, 1); bed.connect(__a.destination); bed.start(); 0",
    );
    let mut out = vec![0.0f32; FRAMES * CH];
    let mut pass = |c: &Context| {
        for _ in 0..4 {
            unsafe { super::pump(c.raw()) };
            c.drain_microtasks();
            RENDERER.with(|r| {
                r.borrow_mut()
                    .as_mut()
                    .expect("the backend took the output")
                    .render(&mut out, CH, FRAMES)
            });
        }
        let object = RENDERER.with(|r| {
            r.borrow().as_ref().unwrap().objects().next().map(|o| {
                let energy: f32 = o.left.iter().map(|v| v * v).sum();
                (o.left.len(), (energy / FRAMES as f32).sqrt(), o.placement)
            })
        });
        (out.clone(), object)
    };

    for (x, z) in [(-2.0f32, 0.0f32), (0.0, 2.0), (1.5, -1.5)] {
        s(
            &c,
            &format!("__p.positionX.value = {x}; __p.positionZ.value = {z}; 0"),
        );
        let (bed, object) = pass(&c);
        for frame in bed.chunks(CH) {
            assert!(
                close(frame[0], 0.1) && close(frame[1], 0.1) && frame[2..].iter().all(|&v| v == 0.0),
                "the bed carries the other source on L and R only, not the object's noise: {frame:?}"
            );
        }
        let (len, rms, placement) = object.expect("the panner has an object");
        assert_eq!(len, FRAMES, "the object block covers the whole pass");
        assert!(rms > 0.3, "the object carries the noise: rms {rms}");
        assert_eq!(placement.position, [x, 0.0, z]);
        assert!(close(placement.gain, 1.0), "no rolloff: {}", placement.gain);
    }
    s(&c, "__a.close(); 0");
}
