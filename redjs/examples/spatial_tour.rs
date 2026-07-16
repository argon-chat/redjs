//! AUDIBLE spatial validation tour (W-MIX MIX-16 helper) — headphones on!
//!
//! A pulsing beep orbits the listener with `panningModel: 'HRTF'`:
//!
//!   Phase 1 — HORIZONTAL circle (X/Z plane, 2 laps):
//!             front → RIGHT → behind → LEFT → front
//!   Phase 2 — VERTICAL circle (Y/Z plane, 2 laps):
//!             front → ABOVE → behind → BELOW → front
//!             (the HRIR set covers −45°..90° elevation; "below" clamps to −45°)
//!
//! What you should hear: phase 1 pans hard around your head including a
//! front/back timbre difference; phase 2 keeps L/R centered while the timbre
//! shifts up-over-behind (binaural elevation cues are subtler than azimuth).
//!
//! Run: `cargo run --release --example spatial_tour`
//! Quick smoke (CI/agents, ~3 s): set `REDJS_TOUR_FAST=1`.

fn eval(g: *mut core::ffi::c_void, src: &str) -> String {
    unsafe {
        let s16: Vec<u16> = src.encode_utf16().collect();
        let url: Vec<u16> = "spatial-tour.js".encode_utf16().collect();
        let mut out = 0u64;
        let rc = redjs::glue::rjs_eval16(g, s16.as_ptr(), s16.len(), url.as_ptr(), url.len(), &mut out);
        let mut info = redjs::ffi::RjsStr::default();
        let sv = if rc != 0 {
            let (mut m, mut st) = (0u64, 0u64);
            redjs::glue::rjs_error_info(g, out, &mut m, &mut st);
            m
        } else {
            out
        };
        let msg = if redjs::glue::rjs_string_info(g, sv, &mut info) == 0 {
            if info.is8bit != 0 {
                core::slice::from_raw_parts(info.ptr as *const u8, info.len as usize)
                    .iter().map(|&b| b as char).collect()
            } else {
                String::from_utf16_lossy(core::slice::from_raw_parts(info.ptr as *const u16, info.len as usize))
            }
        } else {
            String::from("<non-string>")
        };
        if rc != 0 {
            panic!("[tour] eval threw: {msg}");
        }
        msg
    }
}

const RADIUS: f32 = 2.0;
const TICK_MS: u64 = 16;

/// Drive one full-circle phase. `pos(theta) -> (x, y, z)` in game/Web-Audio
/// space; `labels` are printed at each quarter (0°, 90°, 180°, 270°).
fn run_phase(
    g: *mut core::ffi::c_void,
    name: &str,
    laps: u32,
    secs_per_lap: f32,
    labels: [&str; 4],
    pos: impl Fn(f32) -> (f32, f32, f32),
) {
    println!("[tour] ── {name}: {laps} lap(s), {secs_per_lap:.0}s each ──");
    let steps = ((secs_per_lap * 1000.0) as u64 / TICK_MS).max(8);
    for lap in 0..laps {
        let mut next_quarter = 0u32;
        for s in 0..steps {
            let theta = s as f32 / steps as f32 * core::f32::consts::TAU;
            let quarter = (s * 4 / steps) as u32;
            if quarter == next_quarter {
                println!("[tour]   lap {} — {}", lap + 1, labels[quarter as usize]);
                next_quarter += 1;
            }
            let (x, y, z) = pos(theta);
            eval(g, &format!(
                "pan.positionX.value = {x}; pan.positionY.value = {y}; pan.positionZ.value = {z}; 0"
            ));
            unsafe { redjs::ffi::redjs_pump(g, 7, core::ptr::null_mut()) };
            std::thread::sleep(std::time::Duration::from_millis(TICK_MS));
        }
    }
}

fn main() {
    let fast = std::env::var("REDJS_TOUR_FAST").is_ok();
    let (laps, secs) = if fast { (1, 1.5) } else { (2, 5.0) };

    unsafe {
        assert_eq!(redjs::ffi::redjs_init(core::ptr::null()), 0);
        let g = redjs::ffi::redjs_context_create();
        assert!(!g.is_null());
        assert_eq!(redjs::ffi::redjs_web_install(g), 0);
        let root: Vec<u16> = std::env::temp_dir().to_string_lossy().encode_utf16().collect();
        assert_eq!(redjs::ffi::redjs_webshim_install(g, root.as_ptr(), root.len(), root.as_ptr(), root.len()), 0);

        // A pulsing beep is far easier to localize than a steady tone:
        // four 120 ms 880 Hz pips per second, looped.
        eval(g, r#"
            globalThis.ac = new AudioContext();
            const rate = ac.sampleRate;
            const buf = ac.createBuffer(1, rate, rate);
            const ch = new Float32Array(rate);
            for (let p = 0; p < 4; p++) {
                const start = Math.floor(p * rate / 4);
                const len = Math.floor(rate * 0.12);
                for (let i = 0; i < len; i++) {
                    const env = Math.min(1, i / 200, (len - i) / 200); // click-free edges
                    ch[start + i] = Math.sin(i * 880 * 2 * Math.PI / rate) * 0.5 * env;
                }
            }
            buf.copyToChannel(ch, 0);
            const src = ac.createBufferSource();
            src.buffer = buf;
            src.loop = true;
            globalThis.pan = ac.createPanner();
            pan.panningModel = 'HRTF';
            pan.distanceModel = 'inverse';
            pan.refDistance = 1;
            src.connect(pan); pan.connect(ac.destination);
            pan.positionZ.value = -2; // start in front
            src.start();
            'started'
        "#);

        for _ in 0..10 {
            redjs::ffi::redjs_pump(g, 7, core::ptr::null_mut());
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        if eval(g, "ac.currentTime.toFixed(2)") == "0.00" {
            println!("[tour] no audio stream (headless?) — nothing to hear, exiting");
            return;
        }
        println!("[tour] HRTF spatial tour — headphones strongly recommended");

        // Phase 1: horizontal circle in the X/Z plane (listener faces −z).
        // θ=0 front → θ=90° RIGHT (+x) → 180° behind (+z) → 270° LEFT (−x).
        run_phase(g, "phase 1 · horizontal (X circle)", laps, secs,
            ["front", "RIGHT →", "behind", "← LEFT"],
            |t| (RADIUS * t.sin(), 0.0, -RADIUS * t.cos()),
        );

        // Phase 2: vertical circle in the Y/Z plane.
        // θ=0 front → 90° ABOVE (+y) → 180° behind → 270° BELOW (−y, clamps at −45°).
        run_phase(g, "phase 2 · vertical (Y circle)", laps, secs,
            ["front", "ABOVE ↑", "behind", "BELOW ↓"],
            |t| (0.0, RADIUS * t.sin(), -RADIUS * t.cos()),
        );

        eval(g, "pan.positionY.value = 0; pan.positionZ.value = -2; 'done'");
        println!("[tour] done — front-center again. Did the beep truly orbit you?");
    }
}
