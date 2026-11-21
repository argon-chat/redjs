//! The page's side of JavaScript audio worklets: `audioContext.audioWorklet.addModule()`,
//! `AudioWorkletNode` and its port. The processors run in `worklet_scope`.

use super::effect_nodes::{context_arg, member, proto_for};
use super::worklet_bridge::Bridge;
use super::worklet_codec as codec;
use super::worklet_scope::{Command, ParamInfo, Reply, Scope};
use super::*;

pub(super) struct Classes {
    worklet: Class,
    worklet_cls: crate::capi::JSClassRef,
    node: Class,
    port: Class,
    port_cls: crate::capi::JSClassRef,
}

/// One context's worklet: its thread, what its modules registered, and the loads in flight.
pub(super) struct Host {
    scope: Scope,
    processors: HashMap<String, Vec<ParamInfo>>,
    /// `addModule()` promises' resolve and reject functions, protected.
    loads: HashMap<u64, (u64, u64)>,
    next_load: u64,
}

impl Host {
    pub fn waiting(&self) -> bool {
        !self.loads.is_empty() || self.scope.has_replies()
    }

    pub fn forget(&self, node: u64) {
        self.scope.send(Command::Drop { node });
    }

    pub fn take_replies(&self) -> Vec<Reply> {
        self.scope.take_replies()
    }
}

struct WorkletHandle(u64);

struct PortHandle {
    node: u64,
    closed: bool,
}

/// `load(worklet, url, source)`, the end of `addModule()` once the module's text is fetched.
#[crate::host_fn]
unsafe fn load(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let Some(engine) =
        host_data::<WorkletHandle>(g, c.js_worklet.worklet_cls, cf_arg(cf, 0)).map(|h| h.0)
    else {
        return throw_type_error(g, "AudioWorklet.addModule called on wrong this");
    };
    let (Some(url), Some(source)) = (read_string(g, cf_arg(cf, 1)), read_string(g, cf_arg(cf, 2)))
    else {
        return UNDEF;
    };
    let (mut resolve, mut reject, mut promise) = (UNDEF, UNDEF, UNDEF);
    if crate::ffi::redjs_promise_create(g, &mut resolve, &mut reject, &mut promise) != 0 {
        return UNDEF;
    }
    let refused = {
        let mut gl = glob().lock().unwrap();
        match gl.engines.get_mut(&engine) {
            None => Some(("InvalidStateError", "the context is closed.")),
            Some(eng) if eng.state == ST_CLOSED => {
                Some(("InvalidStateError", "the context is closed."))
            }
            Some(eng) if eng.offline.is_some() => Some((
                "NotSupportedError",
                "an OfflineAudioContext has no audio worklet on this host.",
            )),
            Some(eng) => {
                if eng.worklet.is_none() {
                    let rate = if eng.sample_rate == 0 {
                        48_000
                    } else {
                        eng.sample_rate
                    };
                    eng.worklet = Scope::spawn(rate).map(|scope| Host {
                        scope,
                        processors: HashMap::new(),
                        loads: HashMap::new(),
                        next_load: 1,
                    });
                }
                match eng.worklet.as_mut() {
                    None => Some(("AbortError", "the worklet's thread could not be started.")),
                    Some(host) => {
                        let id = host.next_load;
                        host.next_load += 1;
                        host.loads.insert(id, (resolve, reject));
                        host.scope.send(Command::Module { id, url, source });
                        None
                    }
                }
            }
        }
    };
    match refused {
        Some((name, reason)) => {
            let error = super::super::domexception::create(
                g,
                name,
                &format!("Failed to execute 'addModule' on 'Worklet': {reason}"),
            );
            let _ = call(g, reject, UNDEF, &[error]);
        }
        None => {
            crate::ffi::redjs_protect(g, resolve);
            crate::ffi::redjs_protect(g, reject);
        }
    }
    promise
}

/// The promises and the JS callbacks have to be all JS: the fetch, then the text, then `load`.
const ADD_MODULE: &str = "(function (load) { return function addModule(moduleURL) { \
    const worklet = this; \
    return fetch(moduleURL).then(function (response) { \
        if (!response.ok) throw new DOMException(\"Failed to execute 'addModule' on 'Worklet': \" + moduleURL + ' answered ' + response.status + '.', 'AbortError'); \
        return response.text(); \
    }).then(function (source) { return load(worklet, String(moduleURL), source); }); }; })";

#[crate::host_fn]
pub(super) unsafe fn ctx_get_audio_worklet(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(engine) = this_engine_handle(g, cf) else {
        return throw_type_error(g, "AudioContext.audioWorklet on wrong this");
    };
    let slots = slots_of(g, cf_this(cf));
    let existing = get(g, slots, "audioWorklet");
    if is_cell(existing) {
        return existing;
    }
    let c = classes(g);
    let worklet = host_create(
        g,
        c.js_worklet.worklet_cls,
        c.js_worklet.worklet.proto,
        WorkletHandle(engine),
    );
    define(g, slots, "audioWorklet", worklet, 0);
    worklet
}

/// The reply of the worklet's thread to a context that is gone or closed: nothing to tell.
pub(super) unsafe fn close(g: *mut c_void, host: Host) {
    for (resolve, reject) in host.loads.into_values() {
        crate::ffi::redjs_unprotect(g, resolve);
        crate::ffi::redjs_unprotect(g, reject);
    }
}

unsafe fn u32_member(
    g: *mut c_void,
    dict: Option<u64>,
    name: &str,
    default: u32,
) -> Result<u32, ()> {
    let v = member(g, dict, name)?;
    if v == UNDEF {
        return Ok(default);
    }
    let n = to_number(g, v)?;
    Ok(if n.is_finite() { n.max(0.0) as u32 } else { 0 })
}

#[crate::host_fn]
unsafe fn node_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let iface = "AudioWorkletNode";
    let what = "Failed to construct 'AudioWorkletNode'";
    let not_supported = |reason: &str| {
        super::super::domexception::throw(g, "NotSupportedError", &format!("{what}: {reason}"))
    };
    let Ok(engine) = context_arg(g, cf, iface) else {
        return UNDEF;
    };
    let Ok(name) = to_js_string(g, cf_arg(cf, 1)) else {
        return UNDEF;
    };
    let name = read_string(g, name).unwrap_or_default();
    let dict = {
        let v = cf_arg(cf, 2);
        if v == UNDEF || v == NULL {
            None
        } else if super::super::blob::is_object(g, v) {
            Some(v)
        } else {
            return throw_type_error(
                g,
                &format!("{what}: The provided value is not a dictionary."),
            );
        }
    };
    let (Ok(inputs), Ok(outputs)) = (
        u32_member(g, dict, "numberOfInputs", 1),
        u32_member(g, dict, "numberOfOutputs", 1),
    ) else {
        return UNDEF;
    };
    if inputs > 1 || outputs > 1 {
        return not_supported("a worklet node has at most one input and one output on this host.");
    }
    if inputs == 0 && outputs == 0 {
        return not_supported("a node needs an input or an output.");
    }
    let Ok(counts) = member(g, dict, "outputChannelCount") else {
        return UNDEF;
    };
    let mut mono = false;
    if counts != UNDEF {
        let Ok(length) = try_get(g, counts, "length").and_then(|v| to_number(g, v)) else {
            return UNDEF;
        };
        let Ok(first) = to_number(g, get_i(g, counts, 0)) else {
            return UNDEF;
        };
        if length != outputs as f64 {
            return super::super::domexception::throw(
                g,
                "IndexSizeError",
                &format!("{what}: outputChannelCount does not have one count for each output."),
            );
        }
        if outputs == 1 && first != 1.0 && first != 2.0 {
            return not_supported("an output has one or two channels on this host.");
        }
        mono = first == 1.0;
    }
    let (Ok(data), Ok(processor_options)) = (
        member(g, dict, "parameterData"),
        member(g, dict, "processorOptions"),
    ) else {
        return UNDEF;
    };

    let params: Option<Vec<(String, f64, f64, f64)>> = {
        let gl = glob().lock().unwrap();
        gl.engines
            .get(&engine)
            .and_then(|e| e.worklet.as_ref())
            .and_then(|host| host.processors.get(&name))
            .map(|params| {
                params
                    .iter()
                    .map(|p| (p.name.clone(), p.default, p.min, p.max))
                    .collect()
            })
    };
    let Some(params) = params else {
        return super::super::domexception::throw(
            g,
            "InvalidStateError",
            &format!(
                "{what}: no processor named '{name}' is registered. Wait for audioWorklet.addModule() first."
            ),
        );
    };
    let mut initial = Vec::with_capacity(params.len());
    for (param, default, min, max) in &params {
        let given = if is_cell(data) {
            let Ok(v) = try_get(g, data, param) else {
                return UNDEF;
            };
            v
        } else {
            UNDEF
        };
        let value = if given == UNDEF {
            *default
        } else {
            match to_number(g, given) {
                Ok(n) if n.is_finite() => n.clamp(*min, *max),
                Ok(_) => return throw_type_error(g, &format!("{what}: '{param}' is non-finite.")),
                Err(()) => return UNDEF,
            }
        };
        initial.push(value);
    }

    let for_processor = new_object(g, UNDEF);
    define(
        g,
        for_processor,
        "numberOfInputs",
        enc_f64(inputs as f64),
        0,
    );
    define(
        g,
        for_processor,
        "numberOfOutputs",
        enc_f64(outputs as f64),
        0,
    );
    if outputs == 1 {
        let list = new_array(g);
        set_i(g, list, 0, enc_f64(if mono { 1.0 } else { 2.0 }));
        define(g, for_processor, "outputChannelCount", list, 0);
    }
    if is_cell(data) {
        define(g, for_processor, "parameterData", data, 0);
    }
    if processor_options != UNDEF {
        define(g, for_processor, "processorOptions", processor_options, 0);
    }
    let encoded = match codec::encode(g, for_processor) {
        Ok(bytes) => bytes,
        Err(reason) => {
            return super::super::domexception::throw(
                g,
                "DataCloneError",
                &format!("{what}: the options cannot be given to the processor: {reason}."),
            )
        }
    };

    let bridge = ArcGc::new(Bridge::new(inputs == 1, outputs == 1, mono));
    for (i, value) in initial.iter().enumerate() {
        bridge.set_param(i, *value as f32);
    }
    let jsparams: Vec<(&str, u8, f64)> = params
        .iter()
        .zip(&initial)
        .enumerate()
        .map(|(i, ((param, ..), value))| (param.as_str(), P_WORKLET + i as u8, *value))
        .collect();
    let c = classes(g);
    let object = create_node_on(
        g,
        engine,
        Params::Worklet {
            bridge: ArcGc::clone(&bridge),
            ranges: params.iter().map(|&(_, _, min, max)| (min, max)).collect(),
        },
        proto_for(g, cf, c.js_worklet.node.proto),
        &jsparams,
    );
    if !is_cell(object) {
        return object;
    }
    let Some(handle) = host_data::<NodeHandle>(g, c.node_cls, object).map(|h| h.0) else {
        return UNDEF;
    };
    let slots = slots_of(g, object);
    let port = host_create(
        g,
        c.js_worklet.port_cls,
        c.js_worklet.port.proto,
        PortHandle {
            node: handle,
            closed: false,
        },
    );
    define(g, slots, "port", port, 0);
    define(g, port, "onmessage", NULL, 0);
    // A port a script still holds keeps its node, and so its processor.
    let owner = new_object(g, UNDEF);
    glue::rjs_define(g, port, c.s_audio, owner, 0);
    define(g, owner, "owner", object, 0);
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
    for (param, ..) in &params {
        let args = [js_string(g, param), get(g, slots, param)];
        let _ = call(g, get(g, map, "set"), map, &args);
    }

    let mut gl = glob().lock().unwrap();
    if let Some(rec) = gl.nodes.get_mut(&handle) {
        rec.worklet_obj = object;
        // A source lives for as long as its processor says it has more, as a playing
        // oscillator does; a node with an input lives by what is connected to it.
        if inputs == 0 {
            crate::ffi::redjs_protect(g, object);
            rec.js_obj = object;
        }
    }
    if let Some(eng) = gl.engines.get(&engine) {
        if let Some(host) = &eng.worklet {
            let rate = eng.sample_rate.max(1) as f64;
            host.scope.send(Command::Construct {
                node: handle,
                name,
                options: encoded,
                bridge,
                base_frame: (eng.now().max(0.0) * rate) as u64,
            });
        }
    }
    object
}

#[crate::host_fn]
unsafe fn node_port(g: *mut c_void, cf: *mut c_void) -> u64 {
    if this_node_handle(g, cf).is_none() {
        return throw_type_error(g, "AudioWorkletNode.port on wrong this");
    }
    get(g, slots_of(g, cf_this(cf)), "port")
}

#[crate::host_fn]
unsafe fn node_parameters(g: *mut c_void, cf: *mut c_void) -> u64 {
    if this_node_handle(g, cf).is_none() {
        return throw_type_error(g, "AudioWorkletNode.parameters on wrong this");
    }
    get(g, slots_of(g, cf_this(cf)), "parameters")
}

#[crate::host_fn]
unsafe fn port_post(g: *mut c_void, cf: *mut c_void) -> u64 {
    let what = "Failed to execute 'postMessage' on 'AudioWorkletPort'";
    let Some(port) = host_data::<PortHandle>(g, classes(g).js_worklet.port_cls, cf_this(cf)) else {
        return throw_type_error(g, "AudioWorkletPort.postMessage called on wrong this");
    };
    if port.closed {
        return UNDEF;
    }
    let data = match codec::encode(g, cf_arg(cf, 0)) {
        Ok(data) => data,
        Err(reason) => {
            return super::super::domexception::throw(
                g,
                "DataCloneError",
                &format!("{what}: {reason}."),
            )
        }
    };
    let node = port.node;
    let gl = glob().lock().unwrap();
    if let Some(host) = gl
        .nodes
        .get(&node)
        .and_then(|rec| gl.engines.get(&rec.engine))
        .and_then(|eng| eng.worklet.as_ref())
    {
        host.scope.send(Command::Message { node, data });
    }
    UNDEF
}

#[crate::host_fn]
unsafe fn port_start(_g: *mut c_void, _cf: *mut c_void) -> u64 {
    UNDEF
}

#[crate::host_fn]
unsafe fn port_close(g: *mut c_void, cf: *mut c_void) -> u64 {
    let c = classes(g);
    let object = cf_this(cf);
    let Some(port) = host_data::<PortHandle>(g, c.js_worklet.port_cls, object) else {
        return throw_type_error(g, "AudioWorkletPort.close called on wrong this");
    };
    port.closed = true;
    glue::rjs_define(g, object, c.s_audio, UNDEF, 0);
    UNDEF
}

/// What the worklet's thread had to say, told to the page. Called with nothing locked.
pub(super) unsafe fn deliver(g: *mut c_void, engine: u64, replies: Vec<Reply>) {
    let c = classes(g);
    for reply in replies {
        match reply {
            Reply::Module { id, result } => {
                let functions = {
                    let mut gl = glob().lock().unwrap();
                    let Some(host) = gl.engines.get_mut(&engine).and_then(|e| e.worklet.as_mut())
                    else {
                        continue;
                    };
                    let functions = host.loads.remove(&id);
                    if let Ok(processors) = &result {
                        for p in processors {
                            host.processors.insert(
                                p.name.clone(),
                                p.params
                                    .iter()
                                    .map(|i| ParamInfo {
                                        name: i.name.clone(),
                                        default: i.default,
                                        min: i.min,
                                        max: i.max,
                                    })
                                    .collect(),
                            );
                        }
                    }
                    functions
                };
                let Some((resolve, reject)) = functions else {
                    continue;
                };
                let _ = match result {
                    Ok(_) => call(g, resolve, UNDEF, &[UNDEF]),
                    Err(message) => {
                        let error = super::super::domexception::create(
                            g,
                            "AbortError",
                            &format!("Failed to execute 'addModule' on 'Worklet': {message}"),
                        );
                        call(g, reject, UNDEF, &[error])
                    }
                };
                crate::ffi::redjs_unprotect(g, resolve);
                crate::ffi::redjs_unprotect(g, reject);
            }
            Reply::Message { node, data } => {
                let Some(object) = wrapper_of(node) else {
                    continue;
                };
                let port = get(g, slots_of(g, object), "port");
                if host_data::<PortHandle>(g, c.js_worklet.port_cls, port).is_none_or(|p| p.closed)
                {
                    continue;
                }
                let event = super::super::events::new_trusted_event(g, "message");
                crate::ffi::redjs_protect(g, event);
                define(
                    g,
                    event,
                    "data",
                    codec::decode(g, &data).unwrap_or(UNDEF),
                    0,
                );
                super::super::events::dispatch(g, port, event);
                crate::ffi::redjs_unprotect(g, event);
            }
            Reply::Error { node, message } => {
                eprintln!("[audio worklet] Uncaught in a processor: {message}");
                let Some(object) = wrapper_of(node) else {
                    continue;
                };
                let event = super::super::events::new_trusted_event(g, "processorerror");
                crate::ffi::redjs_protect(g, event);
                define(g, event, "message", js_string(g, &message), 0);
                super::super::events::dispatch(g, object, event);
                crate::ffi::redjs_unprotect(g, event);
            }
        }
    }
}

/// A live node's wrapper. The record's reference is weak: a collected wrapper has already
/// taken its record away.
fn wrapper_of(node: u64) -> Option<u64> {
    let gl = glob().lock().unwrap();
    gl.nodes
        .get(&node)
        .map(|rec| rec.worklet_obj)
        .filter(|&object| object != 0)
}

pub(super) unsafe fn install(g: *mut c_void, node_proto: u64) -> Classes {
    let worklet = define_class(
        g,
        &ClassSpec {
            name: "AudioWorklet",
            arity: 0,
            construct: None,
            parent_proto: 0,
            methods: &[],
            getters: &[],
            accessors: &[],
        },
    );
    let mut make = UNDEF;
    if glue::rjs_eval8(
        g,
        ADD_MODULE.as_ptr(),
        ADD_MODULE.len(),
        "redjs/audio-worklet".as_ptr(),
        "redjs/audio-worklet".len(),
        &mut make,
    ) == 0
    {
        if let Ok(add_module) = call(g, make, UNDEF, &[make_fn(g, "load", 3, load)]) {
            define(g, worklet.proto, "addModule", add_module, 2);
        }
    }
    let node = define_class(
        g,
        &ClassSpec {
            name: "AudioWorkletNode",
            arity: 2,
            construct: Some(node_construct),
            parent_proto: node_proto,
            methods: &[],
            getters: &[("port", node_port), ("parameters", node_parameters)],
            accessors: &[],
        },
    );
    let port = define_class(
        g,
        &ClassSpec {
            name: "AudioWorkletPort",
            arity: 0,
            construct: None,
            parent_proto: super::super::state(g).event_target.proto,
            methods: &[
                ("postMessage", 1, port_post),
                ("start", 0, port_start),
                ("close", 0, port_close),
            ],
            getters: &[],
            accessors: &[],
        },
    );
    Classes {
        worklet,
        worklet_cls: make_host_class::<WorkletHandle>("AudioWorklet"),
        node,
        port,
        port_cls: make_host_class::<PortHandle>("AudioWorkletPort"),
    }
}

#[cfg(test)]
mod tests {
    use crate::webapi::testing::*;

    const MODULE: &str = r#"
        class Scale extends AudioWorkletProcessor {
          static get parameterDescriptors() {
            return [{ name: 'amount', defaultValue: 0.5, minValue: 0, maxValue: 1 }];
          }
          constructor(options) {
            super();
            this.seen = 0;
            this.port.onmessage = (e) => {
              this.port.postMessage({ echo: e.data, options: options.processorOptions,
                rate: sampleRate, frame: currentFrame, shape: [options.numberOfInputs, options.numberOfOutputs] });
            };
          }
          process(inputs, outputs, parameters) {
            const amount = parameters.amount[0];
            for (const channel of outputs[0]) channel.fill(amount);
            if (++this.seen === 4) this.port.postMessage({ rendered: outputs[0].length, frames: outputs[0][0].length });
            return true;
          }
        }
        registerProcessor('scale', Scale);
        registerProcessor('broken', class extends AudioWorkletProcessor {
          process() { throw new Error('no good'); }
        });
    "#;

    fn load(c: &crate::api::Context, module: &str) {
        c.eval(&format!(
            "globalThis.__ctx = new AudioContext(); globalThis.__loaded = 'pending';
             __ctx.audioWorklet.addModule(URL.createObjectURL(new Blob([{module:?}])))
               .then(() => {{ __loaded = 'ok'; }}, (e) => {{ __loaded = e.name + ': ' + e.message; }}); 0"
        ))
        .unwrap();
        for _ in 0..300 {
            unsafe { super::pump(c.raw()) };
            c.drain_microtasks();
            if s(c, "__loaded") != "pending" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn wait_for(c: &crate::api::Context, condition: &str) {
        for _ in 0..400 {
            unsafe { super::pump(c.raw()) };
            c.drain_microtasks();
            if s(c, condition) == "true" {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("never became true: {condition}");
    }

    #[test]
    fn a_module_registers_processors_and_a_node_talks_to_its_processor() {
        let c = ctx();
        load(&c, MODULE);
        assert_eq!(s(&c, "__loaded"), "ok");
        ok(
            &c,
            "{ globalThis.__n = new AudioWorkletNode(__ctx, 'scale', { processorOptions: { depth: [1, 2] },
                 parameterData: { amount: 0.25 }, numberOfInputs: 0, outputChannelCount: [2] });
               globalThis.__got = [];
               __n.port.onmessage = (e) => __got.push(e.data);
               __n.port.postMessage({ hello: new Float32Array([1.5, 2.5]), text: 'привет' });
               const amount = __n.parameters.get('amount');
               amount.value = 7;
               __n instanceof AudioNode && __n.port instanceof EventTarget && __n.port === __n.port
                 && amount instanceof AudioParam && amount.value === 1 && __n.numberOfOutputs === 1
                 && __ctx.audioWorklet === __ctx.audioWorklet && __ctx.audioWorklet instanceof AudioWorklet }",
        );
        wait_for(&c, "__got.some((m) => m.echo)");
        assert_eq!(
            s(
                &c,
                "{ const m = __got.find((m) => m.echo);
                   JSON.stringify([m.echo.hello instanceof Float32Array, Array.from(m.echo.hello), m.echo.text,
                     m.options, m.rate === __ctx.sampleRate, m.shape]) }"
            ),
            r#"[true,[1.5,2.5],"привет",{"depth":[1,2]},true,[0,1]]"#
        );
        s(&c, "__n.port.close(); __ctx.close(); 'ok'");
    }

    #[test]
    fn a_processor_renders_what_the_graph_hears() {
        let c = ctx();
        load(&c, MODULE);
        s(
            &c,
            "globalThis.__n = new AudioWorkletNode(__ctx, 'scale', { numberOfInputs: 0 });
             globalThis.__an = __ctx.createAnalyser(); __an.fftSize = 1024;
             globalThis.__got = []; __n.port.onmessage = (e) => __got.push(e.data);
             __n.connect(__an); 'ok'",
        );
        // No connection to the destination: nothing reaches the speakers.
        if super::super::jsc_tests::pump_for(&c, 150) {
            wait_for(
                &c,
                "__got.some((m) => m.rendered === 2 && m.frames === 128)",
            );
            use super::super::jsc_tests::{analyser_rms, pump_until_rendered};
            let now: f64 = s(&c, "__ctx.currentTime").parse().unwrap();
            assert!(pump_until_rendered(&c, now + 0.3, 3000));
            let rms = analyser_rms(&c, "__an");
            assert!(
                (rms - 0.5).abs() < 1e-3,
                "the processor's 0.5 came out as {rms}"
            );
            s(&c, "__n.parameters.get('amount').value = 0.125; 'ok'");
            let now: f64 = s(&c, "__ctx.currentTime").parse().unwrap();
            assert!(pump_until_rendered(&c, now + 0.4, 3000));
            let rms = analyser_rms(&c, "__an");
            assert!(
                (rms - 0.125).abs() < 1e-3,
                "the parameter's 0.125 came out as {rms}"
            );
        } else {
            eprintln!("[audio-worklet-probe] no hardware clock; the processor was not heard");
        }
        s(&c, "__ctx.close(); 'ok'");
    }

    #[test]
    fn what_goes_wrong_is_told() {
        let c = ctx();
        load(
            &c,
            "registerProcessor('twice', class extends AudioWorkletProcessor {}); syntax error here",
        );
        assert!(
            s(&c, "__loaded").starts_with("AbortError"),
            "{}",
            s(&c, "__loaded")
        );
        s(&c, "__ctx.close(); 'ok'");
        load(&c, MODULE);
        ok(
            &c,
            "{ let refused = 0;
               for (const bad of [() => new AudioWorkletNode(__ctx, 'unknown'),
                 () => new AudioWorkletNode(__ctx, 'scale', { numberOfInputs: 0, numberOfOutputs: 0 }),
                 () => new AudioWorkletNode(__ctx, 'scale', { numberOfInputs: 2 }),
                 () => new AudioWorkletNode(__ctx, 'scale', { outputChannelCount: [6] }),
                 () => new AudioWorkletNode(__ctx, 'scale', { processorOptions: { f() {} } }),
                 () => new AudioWorkletNode(__ctx, 'scale').port.postMessage(Symbol())]) {
                 try { bad() } catch { refused++ }
               }
               refused === 6 }",
        );
        s(
            &c,
            "globalThis.__errors = [];
             globalThis.__b = new AudioWorkletNode(__ctx, 'broken', { numberOfInputs: 0 });
             __b.onprocessorerror = (e) => __errors.push(e.message);
             __b.connect(__ctx.createAnalyser()); 'ok'",
        );
        if super::super::jsc_tests::pump_for(&c, 150) {
            wait_for(&c, "__errors.length === 1 && __errors[0] === 'no good'");
        }
        s(&c, "__ctx.close(); 'ok'");
    }
}
