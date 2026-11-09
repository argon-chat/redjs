//! Explicit native DSP factories. These nodes execute Rust compiled into the host,
//! not JavaScript on a second VM. The receiving port is an EventTarget, not a
//! transferable MessagePort; only the latest state crosses into each VM pump.
use super::super::events;
use super::*;

unsafe fn to_string(g: *mut c_void, value: u64) -> Result<String, ()> {
    let value = to_js_string(g, value)?;
    read_string(g, value).ok_or(())
}

struct PortHandle {
    closed: bool,
}
struct Root {
    g: *mut c_void,
    value: u64,
}
impl Root {
    unsafe fn new(g: *mut c_void, value: u64) -> Self {
        crate::ffi::redjs_protect(g, value);
        Self { g, value }
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        unsafe {
            crate::ffi::redjs_unprotect(self.g, self.value);
        }
    }
}

unsafe fn slot(g: *mut c_void, cf: *mut c_void, name: &str) -> u64 {
    if this_node_handle(g, cf).is_none() {
        return throw_type_error(g, "NativeAudioWorkletNode getter called on wrong this");
    }
    get(g, slots_of(g, cf_this(cf)), name)
}
#[crate::host_fn]
unsafe fn parameters(g: *mut c_void, cf: *mut c_void) -> u64 {
    slot(g, cf, "parameters")
}
#[crate::host_fn]
unsafe fn port(g: *mut c_void, cf: *mut c_void) -> u64 {
    slot(g, cf, "port")
}
#[crate::host_fn]
unsafe fn name(g: *mut c_void, cf: *mut c_void) -> u64 {
    slot(g, cf, "processorName")
}
#[crate::host_fn]
unsafe fn channel_count(_g: *mut c_void, _cf: *mut c_void) -> u64 {
    enc_i32(2)
}
#[crate::host_fn]
unsafe fn channel_mode(g: *mut c_void, _cf: *mut c_void) -> u64 {
    js_string(g, "explicit")
}
#[crate::host_fn]
unsafe fn interpretation(g: *mut c_void, cf: *mut c_void) -> u64 {
    slot(g, cf, "channelInterpretation")
}
#[crate::host_fn]
unsafe fn close_port(g: *mut c_void, cf: *mut c_void) -> u64 {
    let object = cf_this(cf);
    let Some(port) = host_data::<PortHandle>(g, classes(g).worklet_port_cls, object) else {
        return throw_type_error(g, "NativeAudioWorkletPort.close called on wrong this");
    };
    port.closed = true;
    define(g, object, "onmessage", NULL, 0);
    // Drop listener/owner edges, including callbacks that close over the node.
    glue::rjs_define(g, object, super::super::state(g).s_listeners, UNDEF, 0);
    glue::rjs_define(g, object, classes(g).s_audio, UNDEF, 0);
    UNDEF
}
#[crate::host_fn]
unsafe fn start_port(g: *mut c_void, cf: *mut c_void) -> u64 {
    if host_data::<PortHandle>(g, classes(g).worklet_port_cls, cf_this(cf)).is_none() {
        return throw_type_error(g, "NativeAudioWorkletPort.start called on wrong this");
    }
    UNDEF // Native state notifications start immediately, including addEventListener.
}
#[crate::host_fn]
unsafe fn post_message(g: *mut c_void, _cf: *mut c_void) -> u64 {
    super::super::domexception::throw(
        g,
        "NotSupportedError",
        "Native worklet ports receive DSP state only; update the node's AudioParams instead",
    )
}

pub(super) unsafe fn install(
    g: *mut c_void,
    parent: u64,
) -> (Class, Class, crate::capi::JSClassRef) {
    let node = define_class(
        g,
        &ClassSpec {
            name: "NativeAudioWorkletNode",
            arity: 0,
            construct: None,
            parent_proto: parent,
            methods: &[],
            getters: &[
                ("parameters", parameters),
                ("port", port),
                ("processorName", name),
                ("channelCount", channel_count),
                ("channelCountMode", channel_mode),
                ("channelInterpretation", interpretation),
            ],
            accessors: &[],
        },
    );
    let port_cls = make_host_class::<PortHandle>("NativeAudioWorkletPort");
    let port = define_class(
        g,
        &ClassSpec {
            name: "NativeAudioWorkletPort",
            arity: 0,
            construct: None,
            parent_proto: super::super::state(g).event_target.proto,
            methods: &[
                ("close", 0, close_port),
                ("start", 0, start_port),
                ("postMessage", 1, post_message),
            ],
            getters: &[],
            accessors: &[],
        },
    );
    (node, port, port_cls)
}

unsafe fn option(g: *mut c_void, options: u64, key: &str) -> Result<u64, ()> {
    if options == UNDEF || options == NULL {
        Ok(UNDEF)
    } else {
        try_get(g, options, key)
    }
}
unsafe fn validate(g: *mut c_void, options: u64) -> Result<bool, ()> {
    if options != UNDEF && options != NULL && glue::rjs_kind(g, options) < 8 {
        throw_type_error(g, "Native audio worklet options must be an object");
        return Err(());
    }
    if option(g, options, "processorOptions")? != UNDEF {
        super::super::domexception::throw(
            g,
            "NotSupportedError",
            "Native DSP has no processorOptions; use parameterData/AudioParams",
        );
        return Err(());
    }
    for (key, expected) in [
        ("numberOfInputs", 1.0),
        ("numberOfOutputs", 1.0),
        ("channelCount", 2.0),
    ] {
        let value = option(g, options, key)?;
        if value != UNDEF && to_number(g, value)? != expected {
            super::super::domexception::throw(
                g,
                "NotSupportedError",
                "Native DSP requires one stereo input and one stereo output",
            );
            return Err(());
        }
    }
    let counts = option(g, options, "outputChannelCount")?;
    if counts != UNDEF {
        let _counts_root = is_cell(counts).then(|| Root::new(g, counts));
        let len = try_get(g, counts, "length")?;
        let _len_root = is_cell(len).then(|| Root::new(g, len));
        let len = to_number(g, len)?;
        let first = try_get(g, counts, "0")?;
        let _first_root = is_cell(first).then(|| Root::new(g, first));
        if len != 1.0 || to_number(g, first)? != 2.0 {
            super::super::domexception::throw(
                g,
                "NotSupportedError",
                "Native DSP outputChannelCount must be [2]",
            );
            return Err(());
        }
    }
    let mode = option(g, options, "channelCountMode")?;
    if mode != UNDEF && to_string(g, mode)? != "explicit" {
        super::super::domexception::throw(
            g,
            "NotSupportedError",
            "Native DSP channelCountMode is explicit",
        );
        return Err(());
    }
    let interpretation = option(g, options, "channelInterpretation")?;
    if interpretation == UNDEF {
        return Ok(false);
    }
    match to_string(g, interpretation)?.as_str() {
        "speakers" => Ok(false),
        "discrete" => Ok(true),
        _ => {
            throw_type_error(
                g,
                "Native DSP channelInterpretation must be speakers or discrete",
            );
            Err(())
        }
    }
}

#[crate::host_fn]
pub(super) unsafe fn load(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(engine) = this_engine_handle(g, cf) else {
        return throw_type_error(
            g,
            "AudioContext.loadNativeAudioWorklet called on wrong this",
        );
    };
    let Ok(name) = to_string(g, cf_arg(cf, 0)) else {
        return UNDEF;
    };
    let Some(kind) = NativeKind::from_name(&name) else {
        return super::super::domexception::throw(g, "NotSupportedError", "Unknown native audio processor; available: peak-meter-stereo, peak-meter-mono, stereo-to-mono, input-gate");
    };
    let options = cf_arg(cf, 1);
    let Ok(discrete) = validate(g, options) else {
        return UNDEF;
    };
    let shared = ArcGc::new(NativeShared::new(kind));
    let jsparams: &[(&str, u8, f64)] = match kind {
        NativeKind::StereoToMono => &[("enabled", P_NATIVE_ENABLED, 1.0)],
        NativeKind::InputGate => &[
            ("enabled", P_NATIVE_ENABLED, 0.0),
            ("threshold", P_NATIVE_THRESHOLD, -40.0),
        ],
        _ => &[],
    };
    let Ok(data) = option(g, options, "parameterData") else {
        return UNDEF;
    };
    let _data_root = is_cell(data).then(|| Root::new(g, data));
    let mut values = Vec::with_capacity(jsparams.len());
    for &(name, param, initial) in jsparams {
        let Ok(value) = option(g, data, name) else {
            return UNDEF;
        };
        let number = if value == UNDEF {
            initial
        } else {
            let Ok(value) = to_number(g, value) else {
                return UNDEF;
            };
            if !value.is_finite() {
                return throw_type_error(g, "Native audio parameters must be finite");
            }
            value
        };
        let number = if param == P_NATIVE_ENABLED {
            number.clamp(0.0, 1.0)
        } else {
            number.clamp(-100.0, 0.0)
        };
        if param == P_NATIVE_ENABLED {
            shared.set_enabled(number as f32);
        } else {
            shared.set_threshold(number as f32);
        }
        values.push((name, param, number));
    }
    let c = classes(g);
    let object = create_node_on(
        g,
        engine,
        Params::Native {
            kind,
            shared,
            discrete,
        },
        c.worklet.proto,
        &values,
    );
    if !is_cell(object) {
        return object;
    }
    let _root = Root::new(g, object);
    let slots = slots_of(g, object);
    let port = host_create(
        g,
        c.worklet_port_cls,
        c.worklet_port.proto,
        PortHandle { closed: false },
    );
    define(g, slots, "port", port, 0);
    // Keeping only a receiving port alive must also keep its native node alive.
    let owner = new_object(g, UNDEF);
    glue::rjs_define(g, port, c.s_audio, owner, 0);
    define(g, owner, "owner", object, 0);
    define(g, port, "onmessage", NULL, 0);
    define(g, slots, "processorName", js_string(g, kind.name()), 0);
    define(
        g,
        slots,
        "channelInterpretation",
        js_string(g, if discrete { "discrete" } else { "speakers" }),
        0,
    );
    define(g, object, "onprocessorerror", NULL, 0);
    let mut map = UNDEF;
    if glue::rjs_construct(
        g,
        get(g, glue::rjs_global(g), "Map"),
        core::ptr::null(),
        0,
        &mut map,
    ) != 0
    {
        return UNDEF;
    }
    define(g, slots, "parameters", map, 0);
    for &(name, _, _) in &values {
        let args = [js_string(g, name), get(g, slots, name)];
        if let Err(error) = call(g, get(g, map, "set"), map, &args) {
            glue::rjs_throw(g, error);
            return UNDEF;
        }
    }
    if let Some(handle) = host_data::<NodeHandle>(g, c.node_cls, object) {
        if let Some(rec) = glob().lock().unwrap().nodes.get_mut(&handle.0) {
            rec.worklet_obj = object;
        }
    }
    object
}

pub(super) unsafe fn dispatch_message(g: *mut c_void, object: u64, message: NativeMessage) {
    let c = classes(g);
    let Some(handle) = host_data::<NodeHandle>(g, c.node_cls, object) else {
        return;
    };
    // Another listener in this batch may have closed the context.
    let live = {
        let gl = glob().lock().unwrap();
        gl.nodes
            .get(&handle.0)
            .and_then(|n| gl.engines.get(&n.engine))
            .is_some_and(|e| e.g == g as usize && e.state != ST_CLOSED)
    };
    if !live {
        return;
    }
    let port = get(g, slots_of(g, object), "port");
    if host_data::<PortHandle>(g, c.worklet_port_cls, port).is_none_or(|p| p.closed) {
        return;
    }
    let event = events::new_trusted_event(g, "message");
    let _event_root = Root::new(g, event);
    let value = match message {
        NativeMessage::Stereo(levels) => {
            let mut array = UNDEF;
            if crate::ffi::redjs_typedarray_create(g, 8 /* f32 */, 2, &mut array) != 0 {
                return;
            }
            // Attach immediately; the event is rooted through listener reentry and GC.
            define(g, event, "data", array, 0);
            set_i(g, array, 0, enc_f64(levels[0] as f64));
            set_i(g, array, 1, enc_f64(levels[1] as f64));
            array
        }
        NativeMessage::Mono(level) => enc_f64(level as f64),
        NativeMessage::Gate(is_open) => {
            let data = new_object(g, UNDEF);
            define(g, event, "data", data, 0);
            define(g, data, "isOpen", enc_bool(is_open), 0);
            data
        }
    };
    define(g, event, "data", value, 0);
    events::dispatch(g, port, event);
}

#[cfg(test)]
mod tests {
    use crate::webapi::testing::*;
    #[test]
    fn native_factory_has_explicit_shape_parameters_and_errors() {
        let c = ctx();
        ok(
            &c,
            r#"(() => {
          const a = new AudioContext();
          const n = a.loadNativeAudioWorklet('input-gate', {parameterData:{enabled:1,threshold:-25}});
          const s = a.loadNativeAudioWorklet('stereo-to-mono');
          const good = n instanceof AudioNode && n instanceof NativeAudioWorkletNode &&
            n.port instanceof EventTarget && n.port === n.port && n.parameters === n.parameters &&
            n.parameters.get('enabled') instanceof AudioParam && n.parameters.get('enabled').value === 1 &&
            n.parameters.get('threshold').value === -25 && s.parameters.get('enabled').value === 1 &&
            n.numberOfInputs === 1 && n.numberOfOutputs === 1 && n.processorName === 'input-gate' &&
            n.channelCount === 2 && n.channelCountMode === 'explicit' && n.channelInterpretation === 'speakers' &&
            typeof a.audioWorklet === 'undefined' && typeof AudioWorkletNode === 'undefined';
          let errors = 0;
          for (const f of [() => a.loadNativeAudioWorklet('unknown'),
            () => a.loadNativeAudioWorklet('input-gate',{outputChannelCount:[1]}),
            () => n.port.postMessage({}),
            () => a.loadNativeAudioWorklet('input-gate',{parameterData:{threshold:NaN}}),
            () => {n.parameters.get('threshold').value=Infinity;},
            () => n.parameters.get('enabled').setValueAtTime(NaN,0),
            () => a.loadNativeAudioWorklet('input-gate',{processorOptions:{}})]) {
            try { f(); } catch { errors++; }
          }
          n.parameters.get('threshold').setValueAtTime(-30,a.currentTime);
          n.port.close(); n.port.close(); a.close();
          try { a.loadNativeAudioWorklet('input-gate'); } catch { errors++; }
          return good && errors === 8;
        })()"#,
        );
    }
    #[test]
    fn telemetry_is_a_real_event_with_gc_owned_data_and_reentrant_close() {
        let c = ctx();
        s(
            &c,
            r#"globalThis.__ctx = new AudioContext();
          globalThis.__n = __ctx.loadNativeAudioWorklet('peak-meter-stereo');
          globalThis.__second = __ctx.loadNativeAudioWorklet('peak-meter-mono');
          globalThis.__seen=[];
          __n.port.addEventListener('message', e => { __seen.push(e); __ctx.close(); });
          __second.port.onmessage=() => __seen.push('late'); 'ok'"#,
        );
        let first = c.eval("__n").unwrap().0;
        let second = c.eval("__second").unwrap().0;
        unsafe {
            super::dispatch_message(c.raw(), first, super::NativeMessage::Stereo([0.25, 0.75]));
            super::dispatch_message(c.raw(), second, super::NativeMessage::Mono(1.0));
        }
        c.gc();
        ok(&c, "__seen.length===1 && __seen[0] instanceof Event && __seen[0].data instanceof Float32Array && __seen[0].data[0]===0.25 && __seen[0].data[1]===0.75 && __seen[0].target===__n.port");
    }
    #[test]
    fn port_close_suppresses_late_state_and_releases_owner_edge() {
        let c = ctx();
        s(&c, "globalThis.__ctx=new AudioContext();globalThis.__n=__ctx.loadNativeAudioWorklet('input-gate');globalThis.__count=0;__n.port.onmessage=()=>__count++;'ok'");
        let object = c.eval("__n").unwrap().0;
        unsafe {
            super::dispatch_message(c.raw(), object, super::NativeMessage::Gate(true));
        }
        ok(&c, "__count===1");
        s(&c, "__n.port.close();__n.port.onmessage=()=>__count++;'ok'");
        unsafe {
            super::dispatch_message(c.raw(), object, super::NativeMessage::Gate(false));
        }
        ok(&c, "__count===1");
        s(&c, "__ctx.close();'ok'");
    }
    #[test]
    fn real_graph_native_gate_meter_and_parameter_automation() {
        let c = ctx();
        s(
            &c,
            r#"globalThis.__ctx = new AudioContext();
          globalThis.__osc = __ctx.createOscillator(); __osc.frequency.value=240;
          globalThis.__gain=__ctx.createGain(); __gain.gain.value=0.25;
          globalThis.__gate=__ctx.loadNativeAudioWorklet('input-gate',{parameterData:{enabled:1,threshold:-20}});
          globalThis.__mono=__ctx.loadNativeAudioWorklet('stereo-to-mono');
          globalThis.__meter=__ctx.loadNativeAudioWorklet('peak-meter-stereo');
          globalThis.__an=__ctx.createAnalyser(); __an.fftSize=1024;
          globalThis.__messages=[];globalThis.__states=[];
          __meter.port.onmessage=e=>__messages.push(Array.from(e.data));
          __gate.port.onmessage=e=>__states.push(e.data.isOpen);
          __osc.connect(__gain).connect(__gate).connect(__mono).connect(__an);
          __mono.connect(__meter);__osc.start();'ok'"#,
        );
        // No connection to destination: this probe never plays a tone to speakers.
        let running = super::super::jsc_tests::pump_for(&c, 150);
        if running {
            ok(&c, "__messages.length>0 && __messages.at(-1)[0]>0.2 && __messages.at(-1)[0]<0.3 && __states.includes(true)");
            s(
                &c,
                "__gate.parameters.get('threshold').setValueAtTime(0,__ctx.currentTime);'ok'",
            );
            super::super::jsc_tests::pump_for(&c, 260);
            ok(&c, "__states.at(-1)===false");
            let rms = super::super::jsc_tests::analyser_rms(&c, "__an");
            assert!(rms < 1e-5, "closed gate leaked PCM: {rms}");
            s(&c, "__gate.parameters.get('enabled').value=0;'ok'");
            super::super::jsc_tests::pump_for(&c, 80);
            assert!(super::super::jsc_tests::analyser_rms(&c, "__an") > 0.1);
            eprintln!("[native-worklet-probe] device-backed gate/mono/meter/automation passed");
        } else {
            eprintln!("[native-worklet-probe] no hardware clock; pure DSP tests cover PCM");
        }
        s(
            &c,
            "__meter.port.close();__gate.port.close();__osc.stop();__ctx.close();'ok'",
        );
    }

    #[test]
    fn temporary_option_dictionaries_survive_gc_in_getters_and_coercions() {
        let c = ctx();
        #[crate::host_fn]
        unsafe fn collect(g: *mut super::c_void, _cf: *mut super::c_void) -> u64 {
            crate::ffi::redjs_gc(g, 0);
            super::UNDEF
        }
        unsafe {
            let f = super::make_fn(c.raw(), "forceGC", 0, collect);
            super::define(c.raw(), crate::glue::rjs_global(c.raw()), "forceGC", f, 0);
        }
        ok(
            &c,
            r#"(() => {
          const a=new AudioContext();
          for(let i=0;i<20;i++) {
            const node=a.loadNativeAudioWorklet('input-gate', {
              get parameterData(){ return {
                enabled:{valueOf(){forceGC();return 1;}},
                get threshold(){ forceGC();return -25; }
              }; },
              get outputChannelCount(){return {
                get length(){forceGC();return {valueOf(){forceGC();return 1;}};},
                get 0(){forceGC();return {valueOf(){forceGC();return 2;}};}
              };}
            });
            if(node.parameters.get('enabled').value!==1 || node.parameters.get('threshold').value!==-25) return false;
            node.port.close();node.disconnect();
          }
          a.close();return true;
        })()"#,
        );
    }
    #[test]
    fn finite_double_parameters_clamp_before_float_narrowing() {
        let c = ctx();
        ok(
            &c,
            r#"(() => { const a=new AudioContext();const n=a.loadNativeAudioWorklet('input-gate');
          const t=n.parameters.get('threshold'),e=n.parameters.get('enabled');
          t.value=-1e300;e.value=1e300;const direct=t.value===-100&&e.value===1;
          t.setValueAtTime(1e300,a.currentTime);e.setValueAtTime(-1e300,a.currentTime);
          const scheduled=t.value===0&&e.value===0;a.close();return direct&&scheduled; })()"#,
        );
    }
    #[test]
    fn options_getter_exceptions_propagate_before_allocating_a_node() {
        let c = ctx();
        ok(
            &c,
            r#"(() => { const a=new AudioContext(); const marker={}; let caught=false;
          try { a.loadNativeAudioWorklet('input-gate',{get parameterData(){throw marker;}}); }
          catch(e) {caught=e===marker;} a.close();return caught; })()"#,
        );
    }
}
