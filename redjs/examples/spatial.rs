//! PannerNode spatial parity probe (W-MIX MIX-14).
//!
//! Drives a looping sine through source → panner → analyser → destination and
//! validates the Web Audio spatial math natively:
//!   - inverse distance model: gain(d=10) ≈ gain(d=1) / 10  (ref=1, rolloff=1)
//!   - linear distance model: silent beyond maxDistance
//!   - cone: source facing away (outerGain 0) is silent, facing back is loud
//!
//! Run: `cargo run --release --example spatial`

fn eval(g: *mut core::ffi::c_void, src: &str) -> String {
    unsafe {
        let s16: Vec<u16> = src.encode_utf16().collect();
        let url: Vec<u16> = "spatial-probe.js".encode_utf16().collect();
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
            panic!("[spatial] eval threw: {msg}");
        }
        msg
    }
}

fn settle(g: *mut core::ffi::c_void) {
    // Param smoothers (~15 ms) + a full analyser window need real stream time.
    for _ in 0..20 {
        unsafe { redjs::ffi::redjs_pump(g, 7, core::ptr::null_mut()) };
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn rms(g: *mut core::ffi::c_void) -> f64 {
    settle(g);
    eval(g, r#"
        (() => {
            const buf = new Float32Array(an.fftSize);
            an.getFloatTimeDomainData(buf);
            let sum = 0;
            for (const v of buf) sum += v * v;
            return Math.sqrt(sum / buf.length).toFixed(4);
        })()
    "#).parse().unwrap()
}

fn main() {
    unsafe {
        assert_eq!(redjs::ffi::redjs_init(core::ptr::null()), 0);
        let g = redjs::ffi::redjs_context_create();
        assert!(!g.is_null());
        assert_eq!(redjs::ffi::redjs_web_install(g), 0);
        let root: Vec<u16> = std::env::temp_dir().to_string_lossy().encode_utf16().collect();
        assert_eq!(redjs::ffi::redjs_webshim_install(g, root.as_ptr(), root.len(), root.as_ptr(), root.len()), 0);

        eval(g, r#"
            globalThis.ac = new AudioContext();
            const rate = ac.sampleRate;
            const buf = ac.createBuffer(1, rate, rate);
            const ch = new Float32Array(rate);
            for (let i = 0; i < rate; i++) ch[i] = Math.sin(i * 440 * 2 * Math.PI / rate) * 0.5;
            buf.copyToChannel(ch, 0);
            const src = ac.createBufferSource();
            src.buffer = buf;
            src.loop = true;
            globalThis.pan = ac.createPanner();
            pan.distanceModel = 'inverse';
            pan.refDistance = 1;
            pan.rolloffFactor = 1;
            globalThis.an = ac.createAnalyser();
            an.fftSize = 2048;
            src.connect(pan); pan.connect(an); an.connect(ac.destination);
            pan.positionX.value = 1; // distance 1 = refDistance → full volume
            src.start();
            'started'
        "#);

        settle(g);
        if eval(g, "ac.currentTime.toFixed(2)") == "0.00" {
            println!("[spatial] no audio stream (headless?) — checks skipped");
            return;
        }

        // 1. Inverse distance: d=1 vs d=10 → 10× gain drop
        let near = rms(g);
        eval(g, "pan.positionX.value = 10; 'd10'");
        let far = rms(g);
        println!("[spatial] inverse: rms(d=1)={near} rms(d=10)={far} ratio={:.1}", near / far);
        // Hard-right pan leaves the mono analyser mix at (0.5/√2)/2 ≈ 0.177 —
        // the halving itself proves equal-power panning is active.
        assert!(near > 0.12, "near rms {near} too low — signal missing");
        let ratio = near / far;
        assert!((6.0..16.0).contains(&ratio), "inverse ratio {ratio} (expected ~10)");

        // 2. Listener rotation rotates the panning space: facing +x puts the
        // x=10 source dead ahead — hard-right (mono mix s/2) becomes centered
        // equal-power (mono mix s·cos45°) → exactly √2 louder in the mono mix.
        eval(g, "ac.listener.forwardX.value = 1; ac.listener.forwardZ.value = 0; 'rot'");
        let rotated = rms(g);
        println!("[spatial] rotated listener: rms={rotated} (×{:.2} vs hard-right — expect ~1.41)", rotated / far);
        assert!(
            (rotated / far - core::f64::consts::SQRT_2).abs() < 0.25,
            "listener rotation should re-center the pan (√2 mono gain), got ×{:.2}",
            rotated / far,
        );
        eval(g, "ac.listener.forwardX.value = 0; ac.listener.forwardZ.value = -1; 'unrot'");

        // 3. Cone: source 5 units ahead, facing AWAY from the listener → silent
        eval(g, r#"
            pan.positionX.value = 0;
            pan.positionZ.value = -5;
            pan.coneInnerAngle = 60;
            pan.coneOuterAngle = 90;
            pan.coneOuterGain = 0;
            pan.orientationX.value = 0; // default is +x — clear it
            pan.orientationZ.value = -1; // away
            'cone-away'
        "#);
        let away = rms(g);
        eval(g, "pan.orientationZ.value = 1; 'cone-toward'"); // toward the listener
        let toward = rms(g);
        println!("[spatial] cone: away={away} toward={toward}");
        assert!(away < 0.02, "cone facing away should be silent, rms={away}");
        // d=5 inverse (1/5) × center pan (cos45°) × sine rms 0.354 ≈ 0.050
        assert!(toward > 0.035, "cone facing toward should be audible, rms={toward}");

        // 4. Linear model: beyond maxDistance → silence
        eval(g, r#"
            pan.coneInnerAngle = 360; pan.coneOuterAngle = 360;
            pan.distanceModel = 'linear';
            pan.refDistance = 1;
            pan.maxDistance = 5;
            pan.positionZ.value = 0;
            pan.positionX.value = 8; // beyond max
            'linear-far'
        "#);
        let beyond = rms(g);
        println!("[spatial] linear beyond max: rms={beyond}");
        assert!(beyond < 0.02, "linear model past maxDistance should be silent, rms={beyond}");

        // 5. HRTF (MIX-20): switching the panning model live keeps sound flowing
        // and audibly changes the render (kernel coloration shifts the mono RMS).
        eval(g, r#"
            pan.distanceModel = 'inverse';
            pan.positionX.value = 3;
            pan.positionZ.value = 0;
            'pre-hrtf'
        "#);
        let ep = rms(g);
        eval(g, "pan.panningModel = 'HRTF'; 'hrtf-on'");
        let hrtf = rms(g);
        let model = eval(g, "pan.panningModel");
        println!("[spatial] hrtf: equalpower rms={ep} hrtf rms={hrtf} model={model}");
        assert_eq!(model, "HRTF", "panningModel should read back as HRTF");
        assert!(hrtf > 0.005, "HRTF mode should produce sound, rms={hrtf}");
        assert!((hrtf / ep - 1.0).abs() > 0.02, "HRTF render should differ from equal-power");

        println!("[spatial] OK — distance models, cone, listener rotation, HRTF verified");
    }
}
