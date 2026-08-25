//! Conformance suite for the redjs ABI — every feature the player needs from
//! the JS engine, exercised the way the generated C# bindings will use it.
//!
//! One #[test] entry point (JSC contexts are process-lifetime; cargo test
//! threads + many VMs are fine, but a single ordered run keeps output and
//! teardown deterministic).

use core::ffi::c_void;
use redjs::api::{layout, Context, Error, HostArgs, TaKind, Value};
use redjs::ffi;

fn sec(name: &str) {
    use std::io::Write;
    eprintln!("[conformance] {name}");
    let _ = std::io::stderr().flush();
}

#[test]
fn conformance() {
    let ctx = Context::new();

    sec("eval_basics");
    eval_basics(&ctx);
    sec("value_encoding");
    value_encoding(&ctx);
    sec("strings");
    strings(&ctx);
    sec("properties_objects_arrays");
    properties_objects_arrays(&ctx);
    sec("host_functions");
    host_functions(&ctx);
    sec("callframe_layout_contract");
    callframe_layout_contract(&ctx);
    sec("calls_and_constructors");
    calls_and_constructors(&ctx);
    sec("exceptions");
    exceptions(&ctx);
    sec("typed_arrays");
    typed_arrays(&ctx);
    sec("external_buffers");
    external_buffers(&ctx);
    sec("promises_and_microtasks");
    promises_and_microtasks(&ctx);
    sec("host_objects");
    host_objects(&ctx);
    sec("gc_and_stats");
    gc_and_stats(&ctx);
    sec("host_function_throw");
    host_function_throw(&ctx);
    sec("web_scheduler");
    web_scheduler(&ctx);
    sec("webapi_events");
    webapi_events(&ctx);
    sec("webapi_encoding_url_blob");
    webapi_encoding_url_blob(&ctx);
    sec("webapi_fetch_images_dom");
    webapi_fetch_images_dom(&ctx);
    sec("webapi_platform");
    webapi_platform(&ctx);
    #[cfg(feature = "audio")]
    {
        sec("webapi_audio");
        webapi_audio(&ctx);
    }
    sec("webapi_platform2");
    webapi_platform2(&ctx);
    #[cfg(feature = "mesh")]
    {
        sec("native_meshopt");
        native_meshopt(&ctx);
    }

    println!("redjs conformance: all sections passed");
}

/// The natives that replaced the last bootstrap stubs: atob/btoa,
/// AbortController/AbortSignal, ImageData, BroadcastChannel, queueMicrotask,
/// setImmediate.
fn webapi_platform2(ctx: &Context) {
    unsafe {
        // atob/btoa round trip + error shapes.
        let v = ctx
            .eval(
                "(() => { const b = btoa('Hello \\xFF!'); const a = atob(b); \
                 let badA = 'no'; try { atob('@@'); } catch (e) { badA = 'yes'; } \
                 let badB = 'no'; try { btoa('\\u0100'); } catch (e) { badB = 'yes'; } \
                 return [b, a === 'Hello \\xFF!', badA, badB].join('|'); })()",
            )
            .unwrap();
        assert_eq!(ctx.to_rust_string(v).unwrap(), "SGVsbG8g/yE=|true|yes|yes");

        // AbortController: abort event (listener + onabort), reason, throwIfAborted.
        let v = ctx
            .eval(
                "(() => { const c = new AbortController(); const s = c.signal; \
                 const log = []; \
                 s.addEventListener('abort', () => log.push('listener')); \
                 s.onabort = () => log.push('onabort'); \
                 log.push(s.aborted, s instanceof AbortSignal, s instanceof EventTarget); \
                 c.abort('why'); \
                 log.push(s.aborted, s.reason); \
                 c.abort('again'); /* no re-fire per spec */ \
                 let threw = 'no'; try { s.throwIfAborted(); } catch (e) { threw = e; } \
                 const pre = AbortSignal.abort(); \
                 return [log.join(','), threw, pre.aborted, pre.reason.name].join('|'); })()",
            )
            .unwrap();
        assert_eq!(
            ctx.to_rust_string(v).unwrap(),
            "false,true,true,listener,onabort,true,why|why|true|AbortError"
        );

        // AbortSignal.timeout fires through the scheduler.
        ctx.eval("globalThis.tsig = AbortSignal.timeout(1); globalThis.tlog = []; tsig.onabort = () => tlog.push(tsig.reason.name);").unwrap();
        let mut stats = redjs::ffi::RjsPumpStats::default();
        std::thread::sleep(std::time::Duration::from_millis(3));
        ffi::redjs_pump(ctx.raw(), 7, &mut stats);
        let v = ctx.eval("[tsig.aborted, tlog.join()].join('|')").unwrap();
        assert_eq!(ctx.to_rust_string(v).unwrap(), "true|TimeoutError");

        // ImageData: both constructor forms + validation.
        let v = ctx
            .eval(
                "(() => { const a = new ImageData(2, 3); \
                 const src = new Uint8ClampedArray(16); src[0] = 7; \
                 const b = new ImageData(src, 2); \
                 let bad = 'no'; try { new ImageData(new Uint8ClampedArray(15), 2); } catch (e) { bad = 'yes'; } \
                 return [a.width, a.height, a.data.length, a.colorSpace, b.height, b.data[0], bad, b instanceof ImageData].join('|'); })()",
            )
            .unwrap();
        assert_eq!(ctx.to_rust_string(v).unwrap(), "2|3|24|srgb|2|7|yes|true");

        // BroadcastChannel: async structured-clone delivery to same-name peers.
        ctx.eval(
            "globalThis.bcLog = []; \
             const rx1 = new BroadcastChannel('game'); \
             const rx2 = new BroadcastChannel('game'); \
             const other = new BroadcastChannel('other'); \
             const tx = new BroadcastChannel('game'); \
             rx1.onmessage = e => bcLog.push('rx1:' + e.data.n); \
             rx2.addEventListener('message', e => bcLog.push('rx2:' + e.data.n)); \
             other.onmessage = () => bcLog.push('other'); \
             rx2.close(); \
             tx.postMessage({ n: 5 }); \
             bcLog.push('sync-after-post:' + bcLog.length);",
        )
        .unwrap();
        ffi::redjs_pump(ctx.raw(), 7, &mut stats);
        let v = ctx.eval("bcLog.join('|')").unwrap();
        assert_eq!(ctx.to_rust_string(v).unwrap(), "sync-after-post:0|rx1:5");

        // queueMicrotask runs before timers; setImmediate passes args.
        ctx.eval(
            "globalThis.ord = []; \
             setImmediate((a, b) => ord.push('imm:' + a + b), 'x', 'y'); \
             queueMicrotask(() => ord.push('micro'));",
        )
        .unwrap();
        ffi::redjs_pump(ctx.raw(), 7, &mut stats);
        let v = ctx.eval("ord.join('|')").unwrap();
        let s = ctx.to_rust_string(v).unwrap();
        assert!(s.contains("micro") && s.contains("imm:xy"), "queueMicrotask + setImmediate ran: {s}");
    }
}

/// Web Audio over Firewheel: graph construction, params + automation,
/// buffers, decodeAudioData. Headless-safe: if no audio device exists the
/// stream just doesn't run — the graph/param/buffer API still must work.
#[cfg(feature = "audio")]
fn webapi_audio(ctx: &Context) {
    unsafe {
        // Classes + graph + params (no device required).
        let v = ctx
            .eval(
                "(() => { const ac = new AudioContext(); \
                 const gain = ac.createGain(); const pan = ac.createStereoPanner(); \
                 const src = ac.createBufferSource(); \
                 src.connect(gain); gain.connect(pan); pan.connect(ac.destination); \
                 gain.gain.value = 0.5; pan.pan.value = -1; \
                 const sameParam = gain.gain === gain.gain; \
                 globalThis.ac = ac; globalThis.gain = gain; globalThis.src = src; \
                 return [ac instanceof AudioContext, gain instanceof GainNode, gain instanceof AudioNode, \
                         typeof ac.currentTime, ac.state, sameParam, \
                         Math.round(gain.gain.value * 100) / 100, pan.pan.value].join('|'); })()",
            )
            .unwrap();
        assert_eq!(ctx.to_rust_string(v).unwrap(), "true|true|true|number|running|true|0.5|-1");

        // createBuffer + copyToChannel/getChannelData round trip.
        let v = ctx
            .eval(
                "(() => { const b = ac.createBuffer(2, 4, 48000); \
                 b.copyToChannel(new Float32Array([1, 2, 3, 4]), 1); \
                 const d = b.getChannelData(1); \
                 return [b.numberOfChannels, b.length, b.sampleRate, b.duration > 0, Array.from(d).join()].join('|'); })()",
            )
            .unwrap();
        assert_eq!(ctx.to_rust_string(v).unwrap(), "2|4|48000|true|1,2,3,4");

        // Source start/stop + automation scheduling APIs accept spec shapes.
        ctx.eval(
            "src.buffer = ac.createBuffer(1, 480, 48000); src.loop = false; \
             src.onended = () => { globalThis.endedFired = true; }; \
             src.playbackRate.setValueAtTime(1.5, 0); \
             gain.gain.setValueAtTime(0.5, 0); \
             gain.gain.linearRampToValueAtTime(0, ac.currentTime + 0.01); \
             gain.gain.setTargetAtTime(1, ac.currentTime + 0.02, 0.005); \
             gain.gain.cancelScheduledValues(ac.currentTime + 0.02); \
             src.start(); src.stop();",
        )
        .unwrap();
        let mut stats = redjs::ffi::RjsPumpStats::default();
        ffi::redjs_pump(ctx.raw(), 7, &mut stats);

        // decodeAudioData: 16-bit PCM WAV (8 frames stereo) → AudioBuffer.
        let mut wav: Vec<u8> = Vec::new();
        let (ch, rate, frames) = (2u16, 8000u32, 8u32);
        let data_len = frames * ch as u32 * 2;
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&ch.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * ch as u32 * 2).to_le_bytes());
        wav.extend_from_slice(&(ch * 2).to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..(frames * ch as u32) {
            wav.extend_from_slice(&((i as i16) * 1000).to_le_bytes());
        }
        let hex: String = wav.iter().map(|b| format!("{b:02x}")).collect();
        ctx.eval(&format!(
            "globalThis.decoded = null; globalThis.decodeErr = null; \
             const bytes = new Uint8Array('{hex}'.match(/../g).map(h => parseInt(h, 16))); \
             ac.decodeAudioData(bytes.buffer).then(b => decoded = b, e => decodeErr = String(e));"
        ))
        .unwrap();
        // decode runs as a pump-native task; resolution lands via microtasks.
        ffi::redjs_pump(ctx.raw(), 7, &mut stats);
        ffi::redjs_pump(ctx.raw(), 7, &mut stats);
        let v = ctx
            .eval(
                "decodeErr ? 'err:' + decodeErr : \
                 [decoded instanceof AudioBuffer, decoded.numberOfChannels, decoded.length > 0, decoded.getChannelData(0).length === decoded.length].join('|')",
            )
            .unwrap();
        assert_eq!(ctx.to_rust_string(v).unwrap(), "true|2|true|true");

        // Spatial + listener + biquad + analyser surface.
        let v = ctx
            .eval(
                "(() => { const p = ac.createPanner(); \
                 p.positionX.setValueAtTime(3, 0); p.positionY.value = 4; \
                 ac.listener.positionX.value = 1; \
                 const f = ac.createBiquadFilter(); f.type = 'highpass'; f.frequency.value = 800; f.Q.value = 2; \
                 const an = ac.createAnalyser(); const arr = new Uint8Array(an.frequencyBinCount); an.getByteFrequencyData(arr); \
                 return [p instanceof PannerNode, f.type, Math.round(f.frequency.value), an.fftSize, arr.length].join('|'); })()",
            )
            .unwrap();
        assert_eq!(ctx.to_rust_string(v).unwrap(), "true|highpass|800|2048|1024");
        ffi::redjs_pump(ctx.raw(), 7, &mut stats);

        // suspend/resume/close lifecycle.
        ctx.eval("globalThis.stTrail = []; ac.suspend().then(() => stTrail.push(ac.state));").unwrap();
        ffi::redjs_pump(ctx.raw(), 7, &mut stats);
        ctx.eval("ac.resume().then(() => stTrail.push(ac.state));").unwrap();
        ffi::redjs_pump(ctx.raw(), 7, &mut stats);
        let v = ctx.eval("stTrail.join()").unwrap();
        assert_eq!(ctx.to_rust_string(v).unwrap(), "suspended,running");
    }
}

/// crypto / structuredClone / Element hierarchy / matchMedia /
/// IntersectionObserver / requestIdleCallback — the gaps rp-spatial found in
/// the real player bundle.
fn webapi_platform(ctx: &Context) {
    install_webapi(ctx);

    // crypto: real CSPRNG, spec checks.
    let probe = ctx.eval("[typeof crypto, typeof structuredClone, typeof HTMLElement, typeof matchMedia, typeof IntersectionObserver, typeof requestIdleCallback].join()");
    match probe {
        Ok(p) => {
            let s = ctx.to_rust_string(p).unwrap();
            assert_eq!(s, "object,function,function,function,function,function", "platform globals present: {s}");
        }
        Err(Error::Exception(e)) => {
            let (m, st) = ctx.error_info(e);
            panic!("probe threw: {m}\n{st}");
        }
        Err(e) => panic!("probe failed: {e}"),
    }
    let v = ctx
        .eval(
            "(() => { const a = new Uint8Array(64); const r = crypto.getRandomValues(a); \
             return [r === a, a.some(b => b !== 0), crypto.randomUUID().length, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(crypto.randomUUID()), crypto.randomUUID() !== crypto.randomUUID()].join(); })()",
        )
        .unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "true,true,36,true,true");
    assert_eq!(
        ctx.eval("(() => { try { crypto.getRandomValues(new Float32Array(2)); return 'no'; } catch (e) { return e instanceof TypeError ? 'type' : 'other'; } })()")
            .map(|v| ctx.to_rust_string(v).unwrap())
            .unwrap(),
        "type"
    );

    // structuredClone: deep, cycle-safe, typed arrays, Map/Set/Date preserved.
    let v = ctx
        .eval(
            "(() => { const src = { n: 1, s: 'x', arr: [1, { deep: true }], ta: new Float32Array([1.5, 2.5]), \
                           m: new Map([['k', 5]]), st: new Set([7]), d: new Date(12345) }; \
             src.self = src; \
             const c = structuredClone(src); \
             return [c !== src, c.self === c, c.arr[1].deep, c.arr !== src.arr, c.ta[1], c.ta !== src.ta, \
              c.m.get('k'), c.st.has(7), c.d.getTime(), c.d !== src.d].join(); })()",
        )
        .unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "true,true,true,true,2.5,true,5,true,12345,true");
    assert_eq!(
        ctx.eval("(() => { try { structuredClone({ f: () => 1 }); return 'no'; } catch (e) { return e.message.includes('DataCloneError') ? 'dce' : 'other'; } })()")
            .map(|v| ctx.to_rust_string(v).unwrap())
            .unwrap(),
        "dce"
    );

    // Element hierarchy: instanceof works (and is false for randoms).
    let v = ctx
        .eval(
            "(() => { const div = document.createElement('div'); \
             return [div instanceof HTMLElement, div instanceof Element, div instanceof EventTarget, \
              ({}) instanceof HTMLElement, typeof SVGElement === 'function', \
              (() => { try { new HTMLElement(); return 'no'; } catch (e) { return 'illegal'; } })()].join(); })()",
        )
        .unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "true,true,true,false,true,illegal");

    // matchMedia + IntersectionObserver stubs are callable and inert.
    let v = ctx
        .eval(
            "(() => { const mql = matchMedia('(prefers-reduced-motion)'); mql.addEventListener('change', () => {}); \
             const io = new IntersectionObserver(() => {}); io.observe(document.body); io.disconnect(); \
             return [mql.matches, mql.media.includes('reduced'), io.takeRecords().length].join(); })()",
        )
        .unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "false,true,0");

    // requestIdleCallback: fires on the pump with a live decreasing budget;
    // cancel works.
    ctx.eval(
        "globalThis.idle = null; \
         (() => { requestIdleCallback((d) => { let steps = 0; while (d.timeRemaining() > 0 && steps < 100000) steps++; idle = [d.didTimeout, steps > 0].join(); }); \
         const dead = requestIdleCallback(() => { idle = 'dead'; }); cancelIdleCallback(dead); })();",
    )
    .unwrap();
    unsafe { ffi::redjs_pump(ctx.raw(), 7, core::ptr::null_mut()) };
    assert_eq!(ctx.to_rust_string(ctx.eval("idle").unwrap()).unwrap(), "false,true");
}

/// `RedMesh` — native meshopt decode into JS-owned buffers, the .rmesh runtime
/// path for the native player (`.refactor/RMESH-FORMAT-DESIGN.md` §5). Sources
/// are encoded here with the same meshopt build the global decodes with.
#[cfg(feature = "mesh")]
fn native_meshopt(ctx: &Context) {
    install_webapi(ctx);

    fn csv(bytes: &[u8]) -> String {
        bytes.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(",")
    }

    assert_eq!(
        ctx.to_rust_string(
            ctx.eval("[typeof RedMesh, typeof RedMesh.decodeVertexBuffer, typeof RedMesh.decodeIndexBuffer].join()")
                .unwrap()
        )
        .unwrap(),
        "object,function,function"
    );

    // stride-16 interleaved vertices — the raw-f32 stream the writer emits.
    let verts: Vec<[f32; 4]> = (0..8).map(|i| [i as f32, i as f32 * 2.5, i as f32 * -3.0, 1.0]).collect();
    let plain: Vec<u8> = verts.iter().flat_map(|v| v.iter().flat_map(|f| f.to_le_bytes())).collect();
    let venc = meshopt::encode_vertex_buffer(&verts).unwrap();
    let v = ctx
        .eval(&format!(
            "(() => {{ const src = new Uint8Array([{}]); const t = new Uint8Array({}); \
              RedMesh.decodeVertexBuffer(t, {}, 16, src); return Array.from(t).join(); }})()",
            csv(&venc),
            plain.len(),
            verts.len()
        ))
        .unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), csv(&plain));

    // Index buffer, both index widths the format allows. The codec may rotate
    // triangles, so the reference is the Rust decode of the same blob.
    let indices: Vec<u32> = vec![0, 1, 2, 2, 3, 0, 4, 5, 6, 6, 7, 4];
    let ienc = meshopt::encode_index_buffer(&indices, verts.len()).unwrap();
    let expect_i = meshopt::decode_index_buffer::<u32>(&ienc, indices.len())
        .unwrap()
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    for (index_size, view) in [(4, "Uint32Array"), (2, "Uint16Array")] {
        let v = ctx
            .eval(&format!(
                "(() => {{ const src = new Uint8Array([{}]); const t = new Uint8Array({}); \
                  RedMesh.decodeIndexBuffer(t, {}, {index_size}, src); \
                  return Array.from(new {view}(t.buffer)).join(); }})()",
                csv(&ienc),
                indices.len() * index_size,
                indices.len()
            ))
            .unwrap();
        assert_eq!(ctx.to_rust_string(v).unwrap(), expect_i, "index decode at {index_size} bytes");
    }

    // Octahedral filter: JS decode must match the Rust reference byte for byte
    // (proves the post-decode filter dispatch, not just the codec).
    let normals: Vec<[f32; 4]> =
        vec![[0.0, 0.0, 1.0, 1.0], [1.0, 0.0, 0.0, 1.0], [0.0, -1.0, 0.0, 1.0], [0.577, 0.577, 0.577, 1.0]];
    let count = normals.len();
    let mut filtered = vec![0u8; count * 8];
    unsafe {
        meshopt::ffi::meshopt_encodeFilterOct(
            filtered.as_mut_ptr() as *mut c_void,
            count,
            8,
            16,
            normals.as_ptr() as *const f32,
        )
    };
    let chunks: Vec<[u8; 8]> = filtered.chunks_exact(8).map(|c| c.try_into().unwrap()).collect();
    let fenc = meshopt::encode_vertex_buffer(&chunks).unwrap();
    let mut expect_f = filtered.clone();
    unsafe { meshopt::ffi::meshopt_decodeFilterOct(expect_f.as_mut_ptr() as *mut c_void, count, 8) };
    let v = ctx
        .eval(&format!(
            "(() => {{ const src = new Uint8Array([{}]); const t = new Uint8Array({}); \
              RedMesh.decodeVertexBuffer(t, {count}, 8, src, 'OCTAHEDRAL'); return Array.from(t).join(); }})()",
            csv(&fenc),
            expect_f.len()
        ))
        .unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), csv(&expect_f));

    // Bad arguments throw TypeError; a corrupt source throws Error — never
    // silent garbage. stride %4 and count %3 are the rules meshopt itself only
    // asserts (asserts are compiled out here), so this gate is all that stands
    // between a game script and an out-of-bounds write. A detached target
    // reports NULL + zero length, which with count 0 would pass a size-only gate.
    let ab = ctx.eval("new ArrayBuffer(8)").unwrap();
    ctx.global_set("detachedTarget", ab).unwrap();
    ctx.arraybuffer_detach(ab).unwrap();

    let v = ctx
        .eval(
            "(() => { const name = (f) => { try { f(); return 'none'; } catch (e) { return e.constructor.name; } }; \
              const src = new Uint8Array(64); \
              return [name(() => RedMesh.decodeVertexBuffer(new Uint8Array(8), 4, 16, src)), \
                      name(() => RedMesh.decodeVertexBuffer(new Uint8Array(64), 4, 0, src)), \
                      name(() => RedMesh.decodeVertexBuffer(new Uint8Array(64), 4, 300, src)), \
                      name(() => RedMesh.decodeVertexBuffer(new Uint8Array(64), 4, 6, src)), \
                      name(() => RedMesh.decodeVertexBuffer(new Uint8Array(64), 4, 16, src, 'BOGUS')), \
                      name(() => RedMesh.decodeVertexBuffer(new Uint8Array(64), 4, 16, src, 'QUATERNION')), \
                      name(() => RedMesh.decodeVertexBuffer(new Uint8Array(64), 4, 16, {})), \
                      name(() => RedMesh.decodeVertexBuffer(detachedTarget, 0, 16, src)), \
                      name(() => RedMesh.decodeIndexBuffer(new Uint8Array(64), 12, 3, src)), \
                      name(() => RedMesh.decodeIndexBuffer(new Uint8Array(8), 12, 4, src)), \
                      name(() => RedMesh.decodeIndexBuffer(new Uint8Array(2), 1, 2, src)), \
                      name(() => RedMesh.decodeIndexBuffer(detachedTarget, 0, 4, src)), \
                      name(() => RedMesh.decodeVertexBuffer(new Uint8Array(64), 4, 16, src))].join(); })()",
        )
        .unwrap();
    assert_eq!(
        ctx.to_rust_string(v).unwrap(),
        ["TypeError"; 12].join(",") + ",Error"
    );
}

fn install_webapi(ctx: &Context) -> std::path::PathBuf {
    // Assets dir with a generated PNG + a JSON for fetch/Image tests.
    let dir = std::env::temp_dir().join("redjs-webapi-test");
    let _ = std::fs::create_dir_all(&dir);
    let img = image::RgbaImage::from_fn(4, 2, |x, y| image::Rgba([x as u8 * 10, y as u8 * 10, 200, 255]));
    img.save(dir.join("test.png")).unwrap();
    std::fs::write(dir.join("data.json"), br#"{"hello": 42}"#).unwrap();

    let scripts: Vec<u16> = dir.to_string_lossy().encode_utf16().collect();
    let rc = unsafe {
        ffi::redjs_webshim_install(ctx.raw(), scripts.as_ptr(), scripts.len(), scripts.as_ptr(), scripts.len())
    };
    assert_eq!(rc, 0, "webshim install");
    dir
}

fn webapi_events(ctx: &Context) {
    install_webapi(ctx);

    // Native classes are real classes: constructors, prototypes, instanceof.
    let v = ctx
        .eval(
            "const e = new CustomEvent('boom', { cancelable: true, detail: { n: 7 } }); \
             [e instanceof CustomEvent, e instanceof Event, e.type, e.cancelable, e.bubbles, e.detail.n].join()",
        )
        .unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "true,true,boom,true,false,7");

    // Getters live on the prototype (spec shape), readonly.
    assert_eq!(
        ctx.eval("Object.getOwnPropertyNames(Event.prototype).includes('type') && !Object.getOwnPropertyNames(new Event('x')).includes('type')")
            .unwrap()
            .as_bool(),
        Some(true)
    );
    // Internal slots are invisible (private names, not even symbols).
    assert_eq!(
        ctx.eval("Object.getOwnPropertyNames(new Event('x')).length + Object.getOwnPropertySymbols(new Event('x')).length")
            .unwrap()
            .as_number(),
        Some(0.0)
    );

    // EventTarget: subclassing + once + duplicate suppression + handleEvent +
    // stopImmediatePropagation + preventDefault/dispatch return value.
    let v = ctx
        .eval(
            "globalThis.log = []; \
             class Emitter extends EventTarget {} \
             const t = new Emitter(); \
             const h = (e) => log.push('a:' + e.type); \
             t.addEventListener('ping', h); \
             t.addEventListener('ping', h); /* duplicate — suppressed */ \
             t.addEventListener('ping', { handleEvent(e) { log.push('obj'); } }); \
             t.addEventListener('ping', () => log.push('once'), { once: true }); \
             t.dispatchEvent(new Event('ping')); \
             t.dispatchEvent(new Event('ping')); \
             log.join()",
        )
        .unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "a:ping,obj,once,a:ping,obj");

    let v = ctx
        .eval(
            "const t2 = new EventTarget(); \
             t2.addEventListener('x', (e) => { e.stopImmediatePropagation(); e.preventDefault(); }); \
             t2.addEventListener('x', () => log.push('never')); \
             const r = t2.dispatchEvent(new Event('x', { cancelable: true })); \
             [r, log.includes('never')].join()",
        )
        .unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "false,false", "canceled dispatch returns false, stopImmediate works");

    // onX handler property participates in dispatch.
    let v = ctx
        .eval(
            "const t3 = new EventTarget(); globalThis.onFired = 0; \
             t3.onping = () => { onFired++; }; \
             t3.dispatchEvent(new Event('ping')); onFired",
        )
        .unwrap();
    assert_eq!(v.as_number(), Some(1.0));
}

fn webapi_encoding_url_blob(ctx: &Context) {
    // TextEncoder/TextDecoder.
    let v = ctx
        .eval(
            "const enc = new TextEncoder(); const dec = new TextDecoder(); \
             [enc.encoding, dec.decode(enc.encode('привет 🎮')) === 'привет 🎮', enc.encode('abc').length].join()",
        )
        .unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "utf-8,true,3");
    // Full WHATWG label support (encoding_rs): windows-1251 cyrillic,
    // UTF-16LE, label→canonical-name resolution, fatal mode, unknown labels.
    let v = ctx
        .eval(
            "(() => { \
             const cp1251 = new TextDecoder('windows-1251'); \
             const ru = cp1251.decode(new Uint8Array([0xCF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2])); \
             const u16 = new TextDecoder('utf-16le').decode(new Uint8Array([0x41, 0x00, 0x16, 0x04])); \
             const canon = new TextDecoder('latin1').encoding; \
             let fatal = 'no'; \
             try { new TextDecoder('utf-8', { fatal: true }).decode(new Uint8Array([0xff])); } \
             catch (e) { fatal = e instanceof TypeError ? 'type' : 'other'; } \
             let unknown = 'no'; \
             try { new TextDecoder('klingon'); } catch (e) { unknown = e instanceof RangeError ? 'range' : 'other'; } \
             const into = new TextEncoder().encodeInto('привет', new Uint8Array(7)); \
             return [ru, u16, canon, fatal, unknown, into.read, into.written].join('|'); })()",
        )
        .unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "Привет|AЖ|windows-1252|type|range|3|6");

    // crypto.subtle.digest against known SHA test vectors.
    let probe = ctx
        .eval("(() => { try { crypto.subtle.digest('SHA-256', new Uint8Array([1])); return 'sync-ok'; } catch (e) { return 'sync-throw: ' + e; } })()")
        .unwrap();
    assert_eq!(ctx.to_rust_string(probe).unwrap(), "sync-ok");
    ctx.eval(
        "globalThis.digests = null; \
         (async () => { \
           const data = new TextEncoder().encode('abc'); \
           const hex = b => Array.from(new Uint8Array(b)).map(x => x.toString(16).padStart(2, '0')).join(''); \
           const s256 = hex(await crypto.subtle.digest('SHA-256', data)); \
           const s1 = hex(await crypto.subtle.digest({ name: 'sha-1' }, data)); \
           let bad = 'no'; \
           try { await crypto.subtle.digest('MD5', data); } catch (e) { bad = 'rejected'; } \
           digests = [s256, s1, bad].join('|'); \
         })().catch(e => { digests = 'threw: ' + String(e) + ' @ ' + (e && e.stack); })",
    )
    .unwrap();
    ctx.drain_microtasks();
    let v = ctx.eval("digests").unwrap();
    assert_eq!(
        ctx.to_rust_string(v).unwrap(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad|a9993e364706816aba3e25717850c26c9cd0d89d|rejected"
    );

    // URL — WHATWG parser (rust-url): normalization, components, setters,
    // searchParams live-sync both ways.
    let v = ctx
        .eval(
            "const u = new URL('HTTPS://ExAmPlE.com:8080/a/../b?x=1&y=%D0%B4#frag'); \
             [u.protocol, u.hostname, u.port, u.pathname, u.search, u.hash, u.origin].join('|')",
        )
        .unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "https:|example.com|8080|/b|?x=1&y=%D0%B4|#frag|https://example.com:8080");

    let v = ctx
        .eval(
            "const u2 = new URL('rel/path?q=1', 'https://host/base/dir/'); \
             [u2.href, u2.searchParams.get('q'), URL.canParse('nope'), u2 instanceof URL].join('|')",
        )
        .unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "https://host/base/dir/rel/path?q=1|1|false|true");

    let v = ctx
        .eval(
            "const u3 = new URL('https://h/p'); \
             u3.searchParams.set('a', '1'); u3.searchParams.append('a', '2'); \
             const afterSet = u3.href; \
             u3.search = '?z=9'; \
             [afterSet, u3.searchParams.get('z'), u3.searchParams.size].join('|')",
        )
        .unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "https://h/p?a=1&a=2|9|1");

    // Invalid URL throws TypeError per spec.
    assert_eq!(
        ctx.eval("(() => { try { new URL('not a url'); return 'no'; } catch (e) { return e instanceof TypeError ? 'type' : 'other'; } })()")
            .map(|v| ctx.to_rust_string(v).unwrap())
            .unwrap(),
        "type"
    );

    // URLSearchParams iteration protocol: for..of (= entries), keys/values,
    // spread, live-list semantics (mutations during iteration are observed),
    // and the iterator being its own [Symbol.iterator].
    let v = ctx
        .eval(
            "(() => { const p = new URLSearchParams('a=1&b=2&c=3'); \
             const forOf = [...p].map(kv => kv.join(':')).join(); \
             const keys = [...p.keys()].join(); \
             const values = [...p.values()].join(); \
             const it = p.entries(); it.next(); p.delete('b'); \
             const afterMutate = [...it].map(kv => kv.join(':')).join(); \
             const it2 = p.entries(); \
             const selfIter = it2[Symbol.iterator]() === it2 ? 'self' : 'x'; \
             return [forOf, keys, values, afterMutate, selfIter].join('|'); })()",
        )
        .unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "a:1,b:2,c:3|a,b,c|1,2,3|c:3|self");

    // Blob: parts, size/type, slice (negative indices), text/arrayBuffer.
    ctx.eval(
        "globalThis.blobDone = null; \
         (async () => { \
           const b = new Blob(['hello ', new TextEncoder().encode('world'), '!'], { type: 'Text/Plain' }); \
           const s = b.slice(-6, -1); \
           blobDone = [b.size, b.type, await b.text(), await s.text(), (await b.arrayBuffer()).byteLength, b instanceof Blob].join('|'); \
         })()",
    )
    .unwrap();
    ctx.drain_microtasks();
    let v = ctx.eval("blobDone").unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "12|text/plain|hello world!|world|12|true");
}

fn webapi_fetch_images_dom(ctx: &Context) {
    let dir = install_webapi(ctx); // idempotent; returns the assets dir
    let _ = dir;

    // fetch: local file → json; 404 semantics; data: URI; Response.blob→createImageBitmap.
    ctx.eval(
        "globalThis.fetched = null; \
         (async () => { \
           const r = await fetch('data.json'); \
           const missing = await fetch('nope.bin'); \
           const dataUri = await fetch('data:text/plain;base64,aGk='); \
           fetched = [r.ok, r.status, r.headers.get('Content-Type'), (await r.json()).hello, \
                      missing.ok, missing.status, await dataUri.text()].join('|'); \
         })()",
    )
    .unwrap();
    ctx.drain_microtasks();
    let v = ctx.eval("fetched").unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "true|200|application/json|42|false|404|hi");

    // createImageBitmap from fetch().blob() + region crop — the sprite path.
    ctx.eval(
        "globalThis.imgDone = null; \
         (async () => { \
           const blob = await (await fetch('test.png')).blob(); \
           const bmp = await createImageBitmap(blob); \
           const region = await createImageBitmap(bmp, 1, 0, 2, 2); \
           imgDone = [bmp instanceof ImageBitmap, bmp.width, bmp.height, region.width, region.height, blob.type].join('|'); \
         })()",
    )
    .unwrap();
    ctx.drain_microtasks();
    let v = ctx.eval("imgDone").unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "true|4|2|2|2|image/png");

    // Image/HTMLImageElement: async load through the pump + events + instanceof.
    ctx.eval(
        "globalThis.imgEvents = []; \
         globalThis.im = new Image(); \
         im.addEventListener('load', () => imgEvents.push('listener')); \
         im.onload = () => imgEvents.push('onload:' + im.naturalWidth + 'x' + im.naturalHeight); \
         im.src = 'test.png'; \
         imgEvents.push('sync:' + im.complete);",
    )
    .unwrap();
    unsafe { ffi::redjs_pump(ctx.raw(), 7, core::ptr::null_mut()) };
    let v = ctx.eval("[im instanceof HTMLImageElement, im.complete, imgEvents.join()].join('|')").unwrap();
    assert_eq!(
        ctx.to_rust_string(v).unwrap(),
        "true|true|sync:false,listener,onload:4x2",
        "image loads on pump tick, events in order"
    );

    // Zero-copy pixel access for the GPU upload path.
    unsafe {
        let bmp = ctx.eval("(() => im)()").unwrap();
        let (mut ptr, mut len, mut w, mut h) = (core::ptr::null::<u8>(), 0usize, 0u32, 0u32);
        let rc = ffi::redjs_image_pixels(ctx.raw(), bmp.0, &mut ptr, &mut len, &mut w, &mut h);
        assert_eq!((rc, w, h, len), (0, 4, 2, 32), "image_pixels on HTMLImageElement");
        let px = core::slice::from_raw_parts(ptr, len);
        assert_eq!(px[2], 200, "blue channel of the generated png");
    }

    // document/window natives + host hooks.
    unsafe {
        let canvas = ctx.eval("({ __brand: 'HTMLCanvasElement', width: 640 })").unwrap();
        ctx.protect(canvas);
        let id: Vec<u16> = "canvas".encode_utf16().collect();
        ffi::redjs_dom_register_element(ctx.raw(), id.as_ptr(), id.len(), canvas.0);
        assert_eq!(
            ctx.eval("document.getElementById('canvas').width === 640 && document.getElementById('canvas') instanceof HTMLCanvasElement")
                .unwrap()
                .as_bool(),
            Some(true)
        );

        ctx.eval("globalThis.sizes = []; window.addEventListener('resize', () => sizes.push(innerWidth + 'x' + innerHeight));").unwrap();
        ffi::redjs_dom_resize(ctx.raw(), 1280, 720, 2.0);
        assert_eq!(
            ctx.to_rust_string(ctx.eval("[sizes.join(), devicePixelRatio, window === globalThis].join('|')").unwrap()).unwrap(),
            "1280x720|2|true"
        );

        ctx.eval("globalThis.lifecycle = []; document.addEventListener('DOMContentLoaded', () => lifecycle.push('dcl:' + document.readyState)); window.addEventListener('load', () => lifecycle.push('load'));").unwrap();
        ffi::redjs_dom_content_loaded(ctx.raw());
        assert_eq!(ctx.to_rust_string(ctx.eval("lifecycle.join()").unwrap()).unwrap(), "dcl:interactive,load");

        ctx.eval("globalThis.vis = []; document.addEventListener('visibilitychange', () => vis.push(document.visibilityState));").unwrap();
        ffi::redjs_dom_set_visibility(ctx.raw(), 1);
        ffi::redjs_dom_set_visibility(ctx.raw(), 0);
        assert_eq!(ctx.to_rust_string(ctx.eval("vis.join()").unwrap()).unwrap(), "hidden,visible");
    }

    // The dumb-stub remainder still works (OffscreenCanvas over native pixels).
    let v = ctx
        .eval(
            "(() => { const c = new OffscreenCanvas(4, 2).getContext('2d'); c.drawImage(im, 0, 0); \
             const d = c.getImageData(0, 0, 4, 2); return d.data[2]; })()",
        )
        .unwrap();
    assert_eq!(v.as_number(), Some(200.0), "OffscreenCanvas drawImage from native image");
}

fn web_scheduler(ctx: &Context) {
    unsafe {
        assert_eq!(ffi::redjs_web_install(ctx.raw()), 0);

        // performance.now() is the native clock — same base as redjs_now().
        let before = ffi::redjs_now();
        let v = ctx.eval("performance.now()").unwrap().as_number().unwrap();
        let after = ffi::redjs_now();
        assert!(v >= before && v <= after, "performance.now ({v}) within [{before}, {after}]");
        assert!(ctx.eval("performance.timeOrigin").unwrap().as_number().unwrap() > 0.0);

        // rAF: registered → fires ONCE with the pump timestamp; re-registration
        // during the callback lands in the NEXT frame's batch.
        ctx.eval(
            "globalThis.frames = []; \
             function tick(ts) { frames.push(ts); if (frames.length < 3) requestAnimationFrame(tick); } \
             requestAnimationFrame(tick);",
        )
        .unwrap();
        let mut stats = redjs::ffi::RjsPumpStats::default();
        ffi::redjs_pump(ctx.raw(), 7, &mut stats);
        assert_eq!((stats.raf_fired, stats.raf_pending), (1, 1), "one fired, re-registration pending");
        ffi::redjs_pump(ctx.raw(), 7, &mut stats);
        ffi::redjs_pump(ctx.raw(), 7, &mut stats);
        assert_eq!(stats.raf_pending, 0, "chain stopped after 3 frames");
        ffi::redjs_pump(ctx.raw(), 7, &mut stats);
        assert_eq!(stats.raf_fired, 0);
        let n = ctx.eval("frames.length").unwrap().as_number().unwrap();
        assert_eq!(n, 3.0);
        // Timestamps are the shared clock and monotonic.
        assert_eq!(ctx.eval("frames.every(t => t > 0) && frames[0] <= frames[1] && frames[1] <= frames[2]").unwrap().as_bool(), Some(true));

        // cancelAnimationFrame.
        ctx.eval("globalThis.c = 0; const id = requestAnimationFrame(() => c++); cancelAnimationFrame(id);").unwrap();
        ffi::redjs_pump(ctx.raw(), 7, core::ptr::null_mut());
        assert_eq!(ctx.eval("c").unwrap().as_number(), Some(0.0));

        // Timers: 0-delay fires on next pump; clearTimeout works; interval
        // fires repeatedly until cleared; nested setTimeout waits a tick.
        ctx.eval(
            "globalThis.log = []; \
             setTimeout(() => { log.push('t0'); setTimeout(() => log.push('nested'), 0); }, 0); \
             const dead = setTimeout(() => log.push('dead'), 0); clearTimeout(dead); \
             globalThis.iv = setInterval(() => { log.push('iv'); if (log.filter(x => x === 'iv').length >= 2) clearInterval(iv); }, 1);",
        )
        .unwrap();
        ffi::redjs_pump(ctx.raw(), 7, &mut stats);
        assert_eq!(stats.timers_fired, 1, "only t0 due immediately (interval waits 1ms)");
        assert_eq!(ctx.to_rust_string(ctx.eval("log.join()").unwrap()).unwrap(), "t0");
        std::thread::sleep(std::time::Duration::from_millis(3));
        ffi::redjs_pump(ctx.raw(), 7, &mut stats); // nested + first interval fire
        std::thread::sleep(std::time::Duration::from_millis(3));
        ffi::redjs_pump(ctx.raw(), 7, &mut stats); // second interval fires, clears itself
        let log = ctx.to_rust_string(ctx.eval("log.join()").unwrap()).unwrap();
        assert_eq!(log, "t0,nested,iv,iv", "timer order/semantics (got: {log})");
        ffi::redjs_pump(ctx.raw(), 7, &mut stats);
        assert_eq!(stats.timers_active, 0, "interval cleared itself");
        assert_eq!(stats.next_timer_deadline, -1.0);

        // Extra setTimeout/setInterval arguments are passed to the callback on
        // every fire (spec), including object values (kept alive by the timer).
        ctx.eval(
            "globalThis.targs = []; \
             setTimeout((a, b, c) => targs.push(a, b.k, c), 0, 7, { k: 'obj' }, 'str'); \
             globalThis.iv2 = setInterval(n => { targs.push(n); if (targs.length >= 5) clearInterval(iv2); }, 1, 'i');",
        )
        .unwrap();
        ffi::redjs_pump(ctx.raw(), 7, &mut stats);
        std::thread::sleep(std::time::Duration::from_millis(3));
        ffi::redjs_pump(ctx.raw(), 7, &mut stats);
        std::thread::sleep(std::time::Duration::from_millis(3));
        ffi::redjs_pump(ctx.raw(), 7, &mut stats);
        let targs = ctx.to_rust_string(ctx.eval("targs.join()").unwrap()).unwrap();
        assert_eq!(targs, "7,obj,str,i,i", "timer extra args (got: {targs})");

        // Microtasks drain as part of the pump (async work driven per frame).
        ctx.eval("globalThis.mt = 0; requestAnimationFrame(async () => { await Promise.resolve(); mt = 42; });").unwrap();
        ffi::redjs_pump(ctx.raw(), 7, core::ptr::null_mut());
        assert_eq!(ctx.eval("mt").unwrap().as_number(), Some(42.0));

        // Callback exceptions hit the uncaught handler and don't break the pump.
        static UNCAUGHT_HITS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
        unsafe extern "C" fn on_uncaught(_g: *mut c_void, _exc: u64) {
            UNCAUGHT_HITS.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        }
        ffi::redjs_set_uncaught_handler(on_uncaught as *const c_void);
        ctx.eval("globalThis.after = 0; requestAnimationFrame(() => { throw new Error('cb boom'); }); requestAnimationFrame(() => { after = 1; });").unwrap();
        ffi::redjs_pump(ctx.raw(), 7, &mut stats);
        assert_eq!(stats.raf_fired, 2, "both callbacks ran despite the throw");
        assert_eq!(UNCAUGHT_HITS.load(core::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(ctx.eval("after").unwrap().as_number(), Some(1.0));
    }
}

fn eval_basics(ctx: &Context) {
    let v = ctx.eval("6 * 7").unwrap();
    assert_eq!(v.as_number(), Some(42.0));

    let v = ctx.eval("(function f(){ return 'hi ' + 'there'; })()").unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "hi there");

    // Named source shows up in stacks.
    let err = ctx.eval_named("throw new Error('boom')", "game/main.js").unwrap_err();
    let Error::Exception(e) = err else { panic!("expected exception") };
    let (msg, stack) = ctx.error_info(e);
    assert_eq!(msg, "boom");
    assert!(stack.contains("game/main.js"), "stack should name the source: {stack}");
}

fn value_encoding(ctx: &Context) {
    // Round-trip int32/double/bool/null/undefined through JS and decode with
    // pure-bit logic (the zero-FFI path C# uses).
    let v = ctx.eval("123").unwrap();
    assert!(v.is_int32());
    assert_eq!(v.as_number(), Some(123.0));

    let v = ctx.eval("1.5").unwrap();
    assert!(!v.is_int32() && v.is_number());
    assert_eq!(v.as_number(), Some(1.5));

    let v = ctx.eval("-1").unwrap();
    assert_eq!(v.as_number(), Some(-1.0));

    assert_eq!(ctx.eval("true").unwrap().as_bool(), Some(true));
    assert_eq!(ctx.eval("false").unwrap().as_bool(), Some(false));
    assert!(ctx.eval("null").unwrap().is_null());
    assert!(ctx.eval("undefined").unwrap().is_undefined());

    // Encode side: pass values INTO JS.
    let sum = ctx
        .call(ctx.eval("(function(a,b){ return a+b; })").unwrap(), Value::undefined(), &[
            Value::int32(40),
            Value::double(2.5),
        ])
        .unwrap();
    assert_eq!(sum.as_number(), Some(42.5));

    // kind() classification.
    assert_eq!(ctx.kind(ctx.eval("undefined").unwrap()), 0);
    assert_eq!(ctx.kind(ctx.eval("null").unwrap()), 1);
    assert_eq!(ctx.kind(ctx.eval("true").unwrap()), 2);
    assert_eq!(ctx.kind(ctx.eval("7").unwrap()), 3);
    assert_eq!(ctx.kind(ctx.eval("7.5").unwrap()), 4);
    assert_eq!(ctx.kind(ctx.eval("'s'").unwrap()), 5);
    assert_eq!(ctx.kind(ctx.eval("Symbol('x')").unwrap()), 6);
    assert_eq!(ctx.kind(ctx.eval("10n").unwrap()), 7);
    assert_eq!(ctx.kind(ctx.eval("({})").unwrap()), 8);
    assert_eq!(ctx.kind(ctx.eval("(()=>1)").unwrap()), 9);
    assert_eq!(ctx.kind(ctx.eval("[1,2]").unwrap()), 10);
    assert_eq!(ctx.kind(ctx.eval("new Float32Array(4)").unwrap()), 11);
    assert_eq!(ctx.kind(ctx.eval("new ArrayBuffer(4)").unwrap()), 12);
    assert_eq!(ctx.kind(ctx.eval("Promise.resolve(1)").unwrap()), 13);
}

fn strings(ctx: &Context) {
    // Latin-1 internal representation.
    let v = ctx.string("hello");
    assert_eq!(ctx.read_string(v).unwrap(), "hello");

    // Non-Latin-1 (UTF-16 internal) + round trip.
    let v = ctx.string("привет 🎮");
    assert_eq!(ctx.read_string(v).unwrap(), "привет 🎮");

    // Rope resolution: concatenation builds ropes; string_info must resolve.
    let v = ctx.eval("('a'.repeat(1000) + 'b'.repeat(1000))").unwrap();
    let s = ctx.read_string(v).unwrap();
    assert_eq!(s.len(), 2000);
    assert!(s.starts_with("aaa") && s.ends_with("bbb"));

    // ToString coercion.
    assert_eq!(ctx.to_rust_string(Value::int32(42)).unwrap(), "42");
}

fn properties_objects_arrays(ctx: &Context) {
    sec("  object()");
    let obj = ctx.object();
    sec("  atoms");
    let a_x = ctx.atom("x");
    let a_y = ctx.atom("y");

    sec("  set");
    ctx.set(obj, &a_x, Value::int32(10)).unwrap();
    sec("  define");
    ctx.define(obj, &a_y, ctx.string("val"), 1 /* readonly */).unwrap();

    sec("  get");
    assert_eq!(ctx.get(obj, &a_x).unwrap().as_number(), Some(10.0));
    assert_eq!(ctx.to_rust_string(ctx.get(obj, &a_y).unwrap()).unwrap(), "val");

    // Expose to JS through the global and read from script.
    sec("  global_set");
    ctx.global_set("testObj", obj).unwrap();
    let v = ctx.eval("testObj.x + testObj.y").unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "10val");

    // Getter through get (spec semantics).
    sec("  getter");
    let g = ctx.eval("({ get p() { return 55; } })").unwrap();
    assert_eq!(ctx.get(g, &ctx.atom("p")).unwrap().as_number(), Some(55.0));

    // Arrays.
    sec("  array()");
    let arr = ctx.array(3).unwrap();
    sec("  set_index");
    ctx.set_index(arr, 0, Value::int32(1)).unwrap();
    ctx.set_index(arr, 1, Value::int32(2)).unwrap();
    ctx.set_index(arr, 2, Value::int32(3)).unwrap();
    assert_eq!(ctx.array_length(arr).unwrap(), 3);
    assert_eq!(ctx.get_index(arr, 1).unwrap().as_number(), Some(2.0));

    // Descriptor-style reads (the GPUDevice.createX(descriptor) pattern).
    let desc = ctx.eval("({ size: 256, usage: 0x28, label: 'vbuf', origin: [1,2,3] })").unwrap();
    assert_eq!(ctx.get(desc, &ctx.atom("size")).unwrap().as_number(), Some(256.0));
    assert_eq!(ctx.get(desc, &ctx.atom("usage")).unwrap().as_number(), Some(40.0));
    let origin = ctx.get(desc, &ctx.atom("origin")).unwrap();
    assert_eq!(ctx.get_index(origin, 2).unwrap().as_number(), Some(3.0));
    // Missing property → undefined (ClearScript's IsUndefined pattern).
    assert!(ctx.get(desc, &ctx.atom("mipLevel")).unwrap().is_undefined());
}

fn host_functions(ctx: &Context) {
    // Rust closure via trampoline pool (kind DATA).
    let f = ctx
        .function("hostAdd", 2, move |_g, cf| {
            let args = unsafe { HostArgs::new(cf) };
            let a = args.arg(0).as_number().unwrap_or(0.0);
            let b = args.arg(1).as_number().unwrap_or(0.0);
            Value::double(a + b).0
        })
        .unwrap();
    ctx.global_set("hostAdd", f).unwrap();

    let v = ctx.eval("hostAdd(20, 22.5)").unwrap();
    assert_eq!(v.as_number(), Some(42.5));

    // Hot loop through the trampoline — sanity that the SysV→MS transition
    // holds under JIT tiers (LLInt → DFG/FTL happens within these counts).
    let v = ctx.eval("let s=0; for (let i=0;i<200000;i++) s = hostAdd(s, 1); s").unwrap();
    assert_eq!(v.as_number(), Some(200000.0));

    // Direct sysv64 host function — the zero-overhead Rust path.
    unsafe extern "sysv64" fn native_mul(_g: *mut c_void, cf: *mut c_void) -> u64 {
        let args = HostArgs::new(cf);
        let a = args.arg(0).as_number().unwrap_or(0.0);
        let b = args.arg(1).as_number().unwrap_or(0.0);
        Value::double(a * b).0
    }
    let f = unsafe { ctx.function_sysv("nativeMul", 2, native_mul as *const c_void) }.unwrap();
    ctx.global_set("nativeMul", f).unwrap();
    assert_eq!(ctx.eval("nativeMul(6, 7)").unwrap().as_number(), Some(42.0));

    // MS-ABI function registered through the C ABI exactly like C# will be.
    unsafe extern "C" fn ms_abi_sub(_g: *mut c_void, cf: *mut c_void) -> u64 {
        let args = HostArgs::new(cf);
        let a = args.arg(0).as_number().unwrap_or(0.0);
        let b = args.arg(1).as_number().unwrap_or(0.0);
        Value::double(a - b).0
    }
    let mut out = 0u64;
    let name: Vec<u16> = "msSub".encode_utf16().collect();
    let status = unsafe {
        ffi::redjs_function_create(ctx.raw(), name.as_ptr(), name.len(), 2, ms_abi_sub as *const c_void, &mut out)
    };
    assert_eq!(status, 0);
    ctx.global_set("msSub", Value(out)).unwrap();
    assert_eq!(ctx.eval("msSub(50, 8)").unwrap().as_number(), Some(42.0));

    // with_data variant (instance-bound host fn pattern).
    unsafe extern "C" fn ms_abi_data(_g: *mut c_void, _cf: *mut c_void, data: *mut c_void) -> u64 {
        Value::int32(data as usize as i32).0
    }
    let mut out = 0u64;
    let name: Vec<u16> = "msData".encode_utf16().collect();
    let status = unsafe {
        ffi::redjs_function_create_with_data(
            ctx.raw(), name.as_ptr(), name.len(), 0, ms_abi_data as *const c_void, 777usize as *mut c_void, &mut out,
        )
    };
    assert_eq!(status, 0);
    ctx.global_set("msData", Value(out)).unwrap();
    assert_eq!(ctx.eval("msData()").unwrap().as_number(), Some(777.0));

    let mut used = 0u32;
    let mut total = 0u32;
    unsafe { ffi::redjs_host_slots(&mut used, &mut total) };
    assert!(used >= 3 && total >= 1024, "slots {used}/{total}");
}

fn callframe_layout_contract(ctx: &Context) {
    // Validates the exported RjsLayout offsets by reading a CallFrame raw —
    // exactly what the C# codegen does. If this passes, C# decoding works.
    let l = layout();
    assert_eq!(l.abi_version, 1);
    // JSVALUE64 constants are architectural; pin them so a header change screams.
    assert_eq!(l.tag_number, 0xfffe_0000_0000_0000);
    assert_eq!(l.value_undefined, 0xa);
    assert_eq!(l.value_null, 0x2);
    assert_eq!(l.value_true, 0x7);
    assert_eq!(l.value_false, 0x6);
    // CallFrame: [callerFrame, returnPC, codeBlock, callee, argc, this, args...]
    assert_eq!(l.cf_callee_offset, 24);
    assert_eq!(l.cf_argc_offset, 32);
    assert_eq!(l.cf_this_offset, 40);
    assert_eq!(l.cf_args_offset, 48);

    let f = ctx
        .function("probe", 3, move |_g, cf| {
            let args = unsafe { HostArgs::new(cf) };
            // argc excludes `this`; JS may pass fewer args than arity.
            let argc = args.argc();
            let a0 = args.arg(0).as_number().unwrap_or(f64::NAN);
            let a2 = args.arg(2);
            let a2n = a2.as_number().unwrap_or(-1.0);
            let this_is_obj = args.this().is_cell();
            let ok = argc == 3 && a0 == 11.0 && a2n == 33.0 && this_is_obj;
            Value::boolean(ok).0
        })
        .unwrap();
    ctx.global_set("probe", f).unwrap();
    assert_eq!(ctx.eval("probe.call({o:1}, 11, 22, 33)").unwrap().as_bool(), Some(true));

    // Under-application: arg(1) must read as undefined, argc == 1.
    let f = ctx
        .function("probe2", 2, move |_g, cf| {
            let args = unsafe { HostArgs::new(cf) };
            Value::boolean(args.argc() == 1 && args.arg(1).is_undefined()).0
        })
        .unwrap();
    ctx.global_set("probe2", f).unwrap();
    assert_eq!(ctx.eval("probe2(5)").unwrap().as_bool(), Some(true));
}

fn calls_and_constructors(ctx: &Context) {
    // rust→js call with `this`.
    let obj = ctx.eval("({ n: 40, add(x) { return this.n + x; } })").unwrap();
    let add = ctx.get(obj, &ctx.atom("add")).unwrap();
    let r = ctx.call(add, obj, &[Value::int32(2)]).unwrap();
    assert_eq!(r.as_number(), Some(42.0));

    // construct.
    let ctor = ctx.eval("(class Point { constructor(x, y) { this.x = x; this.y = y; } })").unwrap();
    let p = ctx.construct(ctor, &[Value::int32(3), Value::int32(4)]).unwrap();
    assert_eq!(ctx.get(p, &ctx.atom("y")).unwrap().as_number(), Some(4.0));

    // js exception propagates through call.
    let thrower = ctx.eval("(function(){ throw new TypeError('nope'); })").unwrap();
    let Err(Error::Exception(e)) = ctx.call(thrower, Value::undefined(), &[]) else {
        panic!("expected exception");
    };
    let (msg, _) = ctx.error_info(e);
    assert_eq!(msg, "nope");
}

fn exceptions(ctx: &Context) {
    // Syntax error at eval.
    let Err(Error::Exception(e)) = ctx.eval("function {") else { panic!() };
    let (msg, _) = ctx.error_info(e);
    assert!(!msg.is_empty());

    // Error created host-side, thrown into JS, observed by JS.
    let f = {
        let err_fn = move |g: *mut c_void, _cf: *mut c_void| -> u64 {
            unsafe {
                let msg: Vec<u16> = "host says no".encode_utf16().collect();
                let mut err = 0u64;
                ffi::redjs_error_create(g, 1, msg.as_ptr(), msg.len(), &mut err);
                ffi::redjs_throw(g, err);
                Value::undefined().0
            }
        };
        ctx.function("hostThrow", 0, err_fn).unwrap()
    };
    ctx.global_set("hostThrow", f).unwrap();
    let v = ctx
        .eval("try { hostThrow(); 'no-throw' } catch (e) { e instanceof TypeError ? e.message : 'wrong-type' }")
        .unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "host says no");
}

fn typed_arrays(ctx: &Context) {
    // JS-created typed array introspected from the host (writeBuffer path).
    let v = ctx.eval("(() => { const a = new Float32Array(8); for (let i=0;i<8;i++) a[i]=i*1.5; return a; })()").unwrap();
    let info = ctx.typedarray_info(v).unwrap();
    assert_eq!(info.kind, TaKind::F32 as u32);
    assert_eq!(info.length, 8);
    assert_eq!(info.byte_length, 32);
    assert_eq!(info.detached, 0);
    let floats = unsafe { core::slice::from_raw_parts(info.data as *const f32, info.length as usize) };
    assert_eq!(floats[4], 6.0);

    // Subarray keeps byteOffset.
    let v = ctx.eval("new Uint16Array(new ArrayBuffer(64), 8, 12)").unwrap();
    let info = ctx.typedarray_info(v).unwrap();
    assert_eq!((info.kind, info.length, info.byte_offset), (TaKind::U16 as u32, 12, 8));

    // Host-created typed array visible to JS.
    let ta = ctx.typedarray(TaKind::U8, 4).unwrap();
    let info = ctx.typedarray_info(ta).unwrap();
    unsafe { core::slice::from_raw_parts_mut(info.data as *mut u8, 4).copy_from_slice(&[1, 2, 3, 4]) };
    ctx.global_set("hostBytes", ta).unwrap();
    assert_eq!(ctx.eval("hostBytes[0] + hostBytes[3]").unwrap().as_number(), Some(5.0));

    // ArrayBuffer info.
    let ab = ctx.eval("new ArrayBuffer(128)").unwrap();
    let abi = ctx.arraybuffer_info(ab).unwrap();
    assert_eq!(abi.byte_length, 128);
    assert_eq!(abi.detached, 0);
}

fn external_buffers(ctx: &Context) {
    // The getMappedRange killer feature: JS ArrayBuffer VIEWING host memory,
    // zero copies; JS writes land in the host buffer directly.
    let mut host_mem = vec![0u8; 64].into_boxed_slice();
    let ptr = host_mem.as_mut_ptr() as *mut c_void;

    let ab = unsafe { ctx.arraybuffer_external(ptr, 64, None, core::ptr::null_mut()) }.unwrap();
    ctx.global_set("mapped", ab).unwrap();
    ctx.eval("new Uint8Array(mapped).set([7, 8, 9], 4)").unwrap();
    assert_eq!(&host_mem[4..7], &[7, 8, 9], "JS write must hit host memory directly");

    // unmap(): detach → JS views die, host memory stays ours.
    ctx.arraybuffer_detach(ab).unwrap();
    let v = ctx.eval("mapped.byteLength").unwrap();
    assert_eq!(v.as_number(), Some(0.0));
    drop(host_mem); // safe: buffer detached

    // External buffer deallocator, deterministic path: detach must release
    // the contents and fire the deallocator (this is the unmap() semantics).
    static DEALLOC_HITS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    unsafe extern "C" fn dealloc(bytes: *mut c_void, _ctx: *mut c_void) {
        DEALLOC_HITS.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        drop(Box::from_raw(core::slice::from_raw_parts_mut(bytes as *mut u8, 16)));
    }
    let leaked = Box::into_raw(vec![0u8; 16].into_boxed_slice()) as *mut u8;
    let ab = unsafe {
        ctx.arraybuffer_external(leaked as *mut c_void, 16, Some(dealloc), core::ptr::null_mut())
    }
    .unwrap();
    let abi = ctx.arraybuffer_info(ab).unwrap();
    assert_eq!(abi.byte_length, 16);
    ctx.arraybuffer_detach(ab).unwrap();
    assert_eq!(
        DEALLOC_HITS.load(core::sync::atomic::Ordering::SeqCst),
        1,
        "detach must fire the external deallocator"
    );

    // GC path: an unreferenced external typed array must eventually fire the
    // deallocator once no stack slot roots it (conservative scan).
    make_dead_external_ta(ctx, dealloc);
    clobber_stack();
    for _ in 0..8 {
        ctx.gc();
        if DEALLOC_HITS.load(core::sync::atomic::Ordering::SeqCst) >= 2 {
            break;
        }
    }
    assert!(
        DEALLOC_HITS.load(core::sync::atomic::Ordering::SeqCst) >= 2,
        "external buffer deallocator should run on GC"
    );
}

#[inline(never)]
fn make_dead_external_ta(
    ctx: &Context,
    dealloc: unsafe extern "C" fn(*mut c_void, *mut c_void),
) {
    let leaked = Box::into_raw(vec![0u8; 16].into_boxed_slice()) as *mut u8;
    let ta = unsafe {
        ctx.typedarray_external(TaKind::U8, leaked as *mut c_void, 16, Some(dealloc), core::ptr::null_mut())
    }
    .unwrap();
    let info = ctx.typedarray_info(ta).unwrap();
    assert_eq!(info.byte_length, 16);
}

/// Overwrites a chunk of stack so dead frames' JSValue bits stop being seen
/// by the conservative scan.
#[inline(never)]
fn clobber_stack() {
    let mut buf = [0u8; 64 * 1024];
    std::hint::black_box(&mut buf[..]);
    for b in buf.iter_mut() {
        *b = 0;
    }
    std::hint::black_box(&mut buf[..]);
}

fn promises_and_microtasks(ctx: &Context) {
    // Host-created deferred promise: resolve from the host, JS observes via
    // .then after a microtask drain (the Task→Promise seam).
    let (promise, resolve, _reject) = ctx.promise().unwrap();
    ctx.global_set("hostPromise", promise).unwrap();
    ctx.eval("globalThis.observed = 'pending'; hostPromise.then(v => { observed = 'got:' + v; })").unwrap();

    ctx.call(resolve, Value::undefined(), &[Value::int32(99)]).unwrap();
    ctx.drain_microtasks();
    let v = ctx.eval("observed").unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "got:99");

    // Promise state introspection (both directions of the bridge).
    let (st, res) = ctx.promise_info(promise).unwrap();
    assert_eq!(st, 1);
    assert_eq!(res.as_number(), Some(99.0));

    // JS async function → host observes settlement by polling promise_info
    // (how C# converts a JS promise into a Task without callbacks).
    let p = ctx.eval("(async () => { let x = await Promise.resolve(20); return x + 22; })()").unwrap();
    ctx.protect(p);
    ctx.drain_microtasks();
    let (st, res) = ctx.promise_info(p).unwrap();
    assert_eq!((st, res.as_number()), (1, Some(42.0)));
    ctx.unprotect(p);

    // Rejection.
    let p = ctx.eval("Promise.reject(new Error('rejected!'))").unwrap();
    ctx.drain_microtasks();
    let (st, res) = ctx.promise_info(p).unwrap();
    assert_eq!(st, 2);
    let (msg, _) = ctx.error_info(res);
    assert_eq!(msg, "rejected!");
}

fn host_objects(ctx: &Context) {
    static FINALIZED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    unsafe extern "C" fn on_finalize(data: *mut c_void) {
        FINALIZED.fetch_add(data as usize, core::sync::atomic::Ordering::SeqCst);
    }
    unsafe { ffi::redjs_set_host_finalizer(on_finalize as *const c_void) };

    let cls = unsafe { ffi::redjs_hostclass_create(c"GPUBuffer".as_ptr()) };
    assert!(!cls.is_null());

    // Handle 0xBEEF travels JS and comes back identical (the wrapper pattern:
    // JS object carries a host handle; methods read it back in one call).
    let mut obj = 0u64;
    let status = unsafe { ffi::redjs_hostobject_create(ctx.raw(), cls, 0xBEEF as *mut c_void, 0, &mut obj) };
    assert_eq!(status, 0);
    ctx.global_set("gpuBuf", Value(obj)).unwrap();
    let round = ctx.eval("(() => gpuBuf)()").unwrap();
    assert_eq!(unsafe { ffi::redjs_hostobject_data(round.0) } as usize, 0xBEEF);

    // Not a host object → null.
    assert!(unsafe { ffi::redjs_hostobject_data(ctx.object().0) }.is_null());

    // Attach a prototype with a host method that reads the handle from `this`.
    let proto = ctx.object();
    let getter = ctx
        .function("handle", 0, move |_g, cf| {
            let args = unsafe { HostArgs::new(cf) };
            let h = unsafe { ffi::redjs_hostobject_data(args.this().0) };
            Value::int32(h as usize as i32).0
        })
        .unwrap();
    ctx.define(proto, &ctx.atom("handle"), getter, 0).unwrap();
    let mut obj2 = 0u64;
    unsafe { ffi::redjs_hostobject_create(ctx.raw(), cls, 0x1234 as *mut c_void, proto.0, &mut obj2) };
    ctx.global_set("gpuBuf2", Value(obj2)).unwrap();
    assert_eq!(ctx.eval("gpuBuf2.handle()").unwrap().as_number(), Some(0x1234 as f64));

    // Finalizer: create unreferenced host objects, force GC. Conservative
    // stack scanning may keep a straggler alive, so require ≥3 of 4.
    let before = FINALIZED.load(core::sync::atomic::Ordering::SeqCst);
    make_dead_host_objects(ctx, cls);
    clobber_stack();
    for _ in 0..8 {
        ctx.gc();
        if FINALIZED.load(core::sync::atomic::Ordering::SeqCst) >= before + 75 {
            break;
        }
    }
    assert!(
        FINALIZED.load(core::sync::atomic::Ordering::SeqCst) >= before + 75,
        "host finalizer should have run for the dead objects"
    );
}

#[inline(never)]
fn make_dead_host_objects(ctx: &Context, cls: *mut c_void) {
    for _ in 0..4 {
        let mut tmp = 0u64;
        unsafe { ffi::redjs_hostobject_create(ctx.raw(), cls, 25 as *mut c_void, 0, &mut tmp) };
        std::hint::black_box(&mut tmp);
    }
}

fn gc_and_stats(ctx: &Context) {
    let s0 = ctx.heap_stats();
    assert!(s0.heap_capacity > 0);

    // protect keeps a value across GC.
    let v = ctx.string("survives");
    ctx.protect(v);
    ctx.eval("for (let i = 0; i < 1000; i++) ({ junk: 'x'.repeat(100) })").unwrap();
    ctx.gc();
    assert_eq!(ctx.read_string(v).unwrap(), "survives");
    ctx.unprotect(v);

    let s1 = ctx.heap_stats();
    assert!(s1.object_count > 0);

    unsafe { ffi::redjs_report_extra_memory(ctx.raw(), 1024 * 1024) };
}

fn host_function_throw(ctx: &Context) {
    // Host fn returning after redjs_throw → JS sees the exception; the VM
    // exception state must not leak into subsequent evals.
    let v = ctx.eval("(() => { try { hostThrow(); return 'bad'; } catch { return 'caught'; } })()").unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "caught");
    assert_eq!(ctx.eval("1 + 1").unwrap().as_number(), Some(2.0));

    let mut pending = 0u64;
    assert_eq!(unsafe { ffi::redjs_exception_pending(ctx.raw(), &mut pending) }, 0);
}
