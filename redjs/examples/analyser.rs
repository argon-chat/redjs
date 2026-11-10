//! AnalyserNode end-to-end probe (W-MIX MIX-13).
//!
//! Plays a 660 Hz sine through gain → analyser → destination via the JS
//! surface, pumps like a host does, then validates that the analyser
//! serves REAL data:
//!   - getFloatTimeDomainData: RMS ≈ amplitude/√2, peak ≈ amplitude
//!   - getByteFrequencyData: the dominant FFT bin sits at ≈ 660 Hz
//!
//! Run: `cargo run --release --example analyser`

fn eval(g: *mut core::ffi::c_void, src: &str) -> String {
    unsafe {
        let s16: Vec<u16> = src.encode_utf16().collect();
        let url: Vec<u16> = "analyser-probe.js".encode_utf16().collect();
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
            panic!("[analyser] eval threw: {msg}");
        }
        msg
    }
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
            for (let i = 0; i < rate; i++) ch[i] = Math.sin(i * 660 * 2 * Math.PI / rate) * 0.3;
            buf.copyToChannel(ch, 0);
            const src = ac.createBufferSource();
            src.buffer = buf;
            src.loop = true;
            const gain = ac.createGain();
            globalThis.an = ac.createAnalyser();
            an.fftSize = 2048;
            src.connect(gain); gain.connect(an); an.connect(ac.destination);
            src.start();
            'started, state=' + ac.state
        "#);

        // Let the stream run so the ring fills.
        for _ in 0..30 {
            redjs::ffi::redjs_pump(g, 7, core::ptr::null_mut());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let report = eval(g, r#"
            (() => {
                const n = an.fftSize;
                const time = new Float32Array(n);
                an.getFloatTimeDomainData(time);
                let peak = 0, sum = 0;
                for (let i = 0; i < n; i++) {
                    peak = Math.max(peak, Math.abs(time[i]));
                    sum += time[i] * time[i];
                }
                const rms = Math.sqrt(sum / n);

                const bins = new Uint8Array(an.frequencyBinCount);
                an.getByteFrequencyData(bins);
                let best = 0;
                for (let i = 1; i < bins.length; i++) if (bins[i] > bins[best]) best = i;
                const hz = best * ac.sampleRate / n;

                const bytes = new Uint8Array(n);
                an.getByteTimeDomainData(bytes);
                let bmin = 255, bmax = 0;
                for (const b of bytes) { bmin = Math.min(bmin, b); bmax = Math.max(bmax, b); }

                return JSON.stringify({
                    peak: +peak.toFixed(3), rms: +rms.toFixed(3),
                    domHz: Math.round(hz), byteMin: bmin, byteMax: bmax,
                    bins: an.frequencyBinCount,
                });
            })()
        "#);
        println!("[analyser] {report}");

        // Assertions (skip when no audio device — the clock never advances).
        let t = eval(g, "ac.currentTime.toFixed(2)");
        if t == "0.00" {
            println!("[analyser] no audio stream (headless?) — data checks skipped");
            return;
        }

        let v: serde_free::Report = serde_free::parse(&report);
        assert!(v.peak > 0.2 && v.peak < 0.4, "peak {} out of range (expected ~0.3)", v.peak);
        assert!(v.rms > 0.15 && v.rms < 0.28, "rms {} out of range (expected ~0.21)", v.rms);
        assert!((v.dom_hz - 660.0).abs() < 50.0, "dominant bin {} Hz (expected ~660)", v.dom_hz);
        assert!(v.byte_max > 160 && v.byte_min < 96, "byte time-domain not oscillating: {}..{}", v.byte_min, v.byte_max);
        println!("[analyser] OK — real time-domain + FFT verified");
    }
}

/// Tiny hand-rolled JSON field extraction (no serde dependency for an example).
mod serde_free {
    pub struct Report {
        pub peak: f64,
        pub rms: f64,
        pub dom_hz: f64,
        pub byte_min: u32,
        pub byte_max: u32,
    }

    fn field(s: &str, name: &str) -> f64 {
        let key = format!("\"{name}\":");
        let i = s.find(&key).unwrap_or_else(|| panic!("missing {name} in {s}")) + key.len();
        s[i..]
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect::<String>()
            .parse()
            .unwrap_or_else(|_| panic!("bad {name} in {s}"))
    }

    pub fn parse(s: &str) -> Report {
        Report {
            peak: field(s, "peak"),
            rms: field(s, "rms"),
            dom_hz: field(s, "domHz"),
            byte_min: field(s, "byteMin") as u32,
            byte_max: field(s, "byteMax") as u32,
        }
    }
}
