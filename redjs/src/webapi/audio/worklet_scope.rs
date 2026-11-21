//! The `AudioWorkletGlobalScope` of one context: a thread with a JavaScript VM of its own.
//! The VM has the language and what a processor is written against (`registerProcessor`,
//! `AudioWorkletProcessor`, `sampleRate`, `currentFrame`, `currentTime`, a port, a console) and
//! none of the web API. A module is evaluated as a script in a scope of its own, so it has no
//! `import`. The page reaches the thread through [`Command`]s and hears back through [`Reply`]s.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use core::ffi::c_void;
use firewheel::collector::ArcGc;

use super::worklet_bridge::{Bridge, LEAD_FRAMES, MAX_PARAMS, QUANTUM};
use super::worklet_codec as codec;
use crate::ffi::RjsTaInfo;
use crate::glue;
use crate::webapi::util::*;

pub(super) struct ParamInfo {
    pub name: String,
    pub default: f64,
    pub min: f64,
    pub max: f64,
}

pub(super) struct Processor {
    pub name: String,
    pub params: Vec<ParamInfo>,
}

pub(super) enum Command {
    Module {
        id: u64,
        url: String,
        source: String,
    },
    Construct {
        node: u64,
        name: String,
        options: Vec<u8>,
        bridge: ArcGc<Bridge>,
        base_frame: u64,
    },
    Message {
        node: u64,
        data: Vec<u8>,
    },
    Drop {
        node: u64,
    },
}

pub(super) enum Reply {
    /// The processors the module registered, or why it did not load.
    Module {
        id: u64,
        result: Result<Vec<Processor>, String>,
    },
    Message {
        node: u64,
        data: Vec<u8>,
    },
    /// The processor's constructor or its `process()` threw.
    Error {
        node: u64,
        message: String,
    },
}

pub(super) struct Scope {
    commands: Sender<Command>,
    replies: Arc<Mutex<Vec<Reply>>>,
    thread: std::thread::Thread,
}

impl Scope {
    pub fn spawn(sample_rate: u32) -> Option<Scope> {
        let (commands, rx) = mpsc::channel();
        let replies = Arc::new(Mutex::new(Vec::new()));
        let theirs = Arc::clone(&replies);
        let handle = std::thread::Builder::new()
            .name("audio-worklet".into())
            .spawn(move || unsafe { run(rx, theirs, sample_rate) })
            .ok()?;
        Some(Scope {
            commands,
            replies,
            thread: handle.thread().clone(),
        })
    }

    pub fn send(&self, command: Command) {
        if let Command::Construct { bridge, .. } = &command {
            bridge.attach(self.thread.clone());
        }
        let _ = self.commands.send(command);
        self.thread.unpark();
    }

    pub fn take_replies(&self) -> Vec<Reply> {
        core::mem::take(&mut *self.replies.lock().unwrap())
    }

    pub fn has_replies(&self) -> bool {
        !self.replies.lock().unwrap().is_empty()
    }
}

struct Instance {
    object: u64,
    port: u64,
    keep: u64,
    bridge: ArcGc<Bridge>,
    inputs: u64,
    outputs: u64,
    parameters: u64,
    in_channels: [u64; 2],
    out_channels: [u64; 2],
    param_values: Vec<u64>,
    produced: u64,
    base_frame: u64,
}

struct State {
    replies: Arc<Mutex<Vec<Reply>>>,
    /// The constructor and its parameters' names, in the order of their values.
    processors: HashMap<String, (u64, Vec<String>)>,
    /// What the module being evaluated has registered so far.
    fresh: Vec<Processor>,
    /// The port of the processor being constructed, taken by `AudioWorkletProcessor`.
    pending_port: u64,
    processor_proto: u64,
    port_class: crate::capi::JSClassRef,
    port_proto: u64,
}

struct ScopePort {
    node: u64,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|state| state.borrow_mut().as_mut().map(f))
}

unsafe fn exception_text(g: *mut c_void, exception: u64) -> String {
    let (mut message, mut stack) = (UNDEF, UNDEF);
    glue::rjs_error_info(g, exception, &mut message, &mut stack);
    let text = read_string(g, message).unwrap_or_default();
    if !text.is_empty() {
        return text;
    }
    to_js_string(g, exception)
        .ok()
        .and_then(|s| read_string(g, s))
        .unwrap_or_else(|| "an exception was thrown".into())
}

unsafe fn float_array(g: *mut c_void, length: usize) -> u64 {
    let mut array = UNDEF;
    crate::ffi::redjs_typedarray_create(g, 8, length, &mut array);
    array
}

/// The samples of `holder[index]`, which a processor may have replaced or detached.
unsafe fn samples<'a>(
    g: *mut c_void,
    holder: u64,
    index: u32,
    array: &mut u64,
    length: usize,
) -> &'a mut [f32] {
    let mut info = RjsTaInfo::default();
    let usable = glue::rjs_typedarray_info(g, *array, &mut info) == 0
        && info.kind == 8
        && info.length as usize == length
        && !info.data.is_null();
    if !usable {
        *array = float_array(g, length);
        set_i(g, holder, index, *array);
        glue::rjs_typedarray_info(g, *array, &mut info);
    }
    if info.data.is_null() {
        return &mut [];
    }
    core::slice::from_raw_parts_mut(info.data as *mut f32, length)
}

#[crate::host_fn]
unsafe fn register_processor(g: *mut c_void, cf: *mut c_void) -> u64 {
    let what = "Failed to execute 'registerProcessor' on 'AudioWorkletGlobalScope'";
    let Ok(name) = to_js_string(g, cf_arg(cf, 0)) else {
        return UNDEF;
    };
    let name = read_string(g, name).unwrap_or_default();
    let ctor = cf_arg(cf, 1);
    if name.is_empty() {
        return throw_error(g, 0, &format!("{what}: the name is empty."));
    }
    if glue::rjs_kind(g, ctor) != 9 {
        return throw_type_error(g, &format!("{what}: the processor is not a class."));
    }
    if with(|s| s.processors.contains_key(&name)) == Some(true) {
        return throw_error(
            g,
            0,
            &format!("{what}: a processor named '{name}' is already registered."),
        );
    }
    let Ok(descriptors) = try_get(g, ctor, "parameterDescriptors") else {
        return UNDEF;
    };
    let mut params = Vec::new();
    if descriptors != UNDEF {
        if glue::rjs_kind(g, descriptors) != 10 {
            return throw_type_error(g, &format!("{what}: parameterDescriptors is not an array."));
        }
        for i in 0..array_len(g, descriptors) {
            let descriptor = get_i(g, descriptors, i);
            let Ok(param) = try_get(g, descriptor, "name").and_then(|v| to_js_string(g, v)) else {
                return UNDEF;
            };
            let param = read_string(g, param).unwrap_or_default();
            let mut numbers = [0.0, -f32::MAX as f64, f32::MAX as f64];
            for (slot, key) in numbers
                .iter_mut()
                .zip(["defaultValue", "minValue", "maxValue"])
            {
                let Ok(value) = try_get(g, descriptor, key) else {
                    return UNDEF;
                };
                if value != UNDEF {
                    let Ok(number) = to_number(g, value) else {
                        return UNDEF;
                    };
                    *slot = number;
                }
            }
            let [default, min, max] = numbers;
            let refused = if params.iter().any(|p: &ParamInfo| p.name == param) {
                Some(format!("the parameter '{param}' is described twice."))
            } else if !(min <= default && default <= max) {
                Some(format!(
                    "the default of '{param}' is outside its minValue and maxValue."
                ))
            } else if params.len() == MAX_PARAMS {
                Some(format!(
                    "a processor has at most {MAX_PARAMS} parameters here."
                ))
            } else {
                None
            };
            if let Some(reason) = refused {
                return throw_error(g, 0, &format!("{what}: {reason}"));
            }
            params.push(ParamInfo {
                name: param,
                default,
                min,
                max,
            });
        }
    }
    crate::ffi::redjs_protect(g, ctor);
    with(|s| {
        let names = params.iter().map(|p| p.name.clone()).collect();
        s.processors.insert(name.clone(), (ctor, names));
        s.fresh.push(Processor { name, params });
    });
    UNDEF
}

#[crate::host_fn]
unsafe fn processor_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some((port, proto)) = with(|s| {
        (
            core::mem::replace(&mut s.pending_port, 0),
            s.processor_proto,
        )
    }) else {
        return UNDEF;
    };
    if port == 0 {
        return throw_type_error(
            g,
            "Failed to construct 'AudioWorkletProcessor': a processor is made by its AudioWorkletNode.",
        );
    }
    let object = ordinary_create(g, cf_new_target(cf), proto);
    define(g, object, "port", port, 0);
    object
}

#[crate::host_fn]
unsafe fn port_post(g: *mut c_void, cf: *mut c_void) -> u64 {
    let Some(class) = with(|s| s.port_class) else {
        return UNDEF;
    };
    let Some(port) = host_data::<ScopePort>(g, class, cf_this(cf)) else {
        return throw_type_error(g, "MessagePort.postMessage called on wrong this");
    };
    match codec::encode(g, cf_arg(cf, 0)) {
        Ok(data) => {
            let node = port.node;
            with(|s| {
                s.replies
                    .lock()
                    .unwrap()
                    .push(Reply::Message { node, data })
            });
            UNDEF
        }
        Err(reason) => throw_error(
            g,
            0,
            &format!("Failed to execute 'postMessage' on 'MessagePort': {reason}."),
        ),
    }
}

#[crate::host_fn]
unsafe fn port_nothing(_g: *mut c_void, _cf: *mut c_void) -> u64 {
    UNDEF
}

#[crate::host_fn]
unsafe fn console_print(g: *mut c_void, cf: *mut c_void) -> u64 {
    let mut line = String::from("[audio worklet]");
    for i in 0..cf_argc(cf) {
        let text = to_js_string(g, cf_arg(cf, i))
            .ok()
            .and_then(|s| read_string(g, s));
        line.push(' ');
        line.push_str(text.as_deref().unwrap_or("?"));
    }
    eprintln!("{line}");
    UNDEF
}

unsafe fn install(g: *mut c_void, replies: Arc<Mutex<Vec<Reply>>>, sample_rate: u32) {
    let global = glue::rjs_global(g);
    let processor = define_class(
        g,
        &ClassSpec {
            name: "AudioWorkletProcessor",
            arity: 0,
            construct: Some(processor_construct),
            parent_proto: 0,
            methods: &[],
            getters: &[],
            accessors: &[],
        },
    );
    let port = define_class(
        g,
        &ClassSpec {
            name: "MessagePort",
            arity: 0,
            construct: None,
            parent_proto: 0,
            methods: &[
                ("postMessage", 1, port_post),
                ("start", 0, port_nothing),
                ("close", 0, port_nothing),
            ],
            getters: &[],
            accessors: &[],
        },
    );
    define(
        g,
        global,
        "registerProcessor",
        make_fn(g, "registerProcessor", 2, register_processor),
        2,
    );
    let console = new_object(g, UNDEF);
    for name in ["log", "info", "debug", "warn", "error"] {
        define(g, console, name, make_fn(g, name, 0, console_print), 0);
    }
    define(g, global, "console", console, 2);
    define(g, global, "sampleRate", enc_f64(sample_rate as f64), 0);
    define(g, global, "currentFrame", enc_f64(0.0), 0);
    define(g, global, "currentTime", enc_f64(0.0), 0);
    STATE.with(|state| {
        *state.borrow_mut() = Some(State {
            replies,
            processors: HashMap::new(),
            fresh: Vec::new(),
            pending_port: 0,
            processor_proto: processor.proto,
            port_class: make_host_class::<ScopePort>("MessagePort"),
            port_proto: port.proto,
        })
    });
}

fn reply(reply: Reply) {
    with(|s| s.replies.lock().unwrap().push(reply));
}

unsafe fn load_module(g: *mut c_void, id: u64, url: &str, source: &str) {
    // A function of its own: a module's top-level names stay its own, and it is strict.
    let wrapped = format!("(function () {{ \"use strict\"; {source}\n}})();");
    let mut out = UNDEF;
    let status = glue::rjs_eval8(
        g,
        wrapped.as_ptr(),
        wrapped.len(),
        url.as_ptr(),
        url.len(),
        &mut out,
    );
    let fresh = with(|s| core::mem::take(&mut s.fresh)).unwrap_or_default();
    let result = if status == 0 {
        Ok(fresh)
    } else {
        Err(exception_text(g, out))
    };
    reply(Reply::Module { id, result });
}

unsafe fn construct(
    g: *mut c_void,
    node: u64,
    name: &str,
    options: &[u8],
    bridge: ArcGc<Bridge>,
    base_frame: u64,
) -> Result<Instance, String> {
    let (ctor, param_names, class, proto) = with(|s| {
        s.processors
            .get(name)
            .map(|(ctor, params)| (*ctor, params.clone(), s.port_class, s.port_proto))
    })
    .flatten()
    .ok_or_else(|| format!("no processor named '{name}' is registered"))?;
    let port = host_create(g, class, proto, ScopePort { node });
    crate::ffi::redjs_protect(g, port);
    define(g, port, "onmessage", NULL, 0);
    let options = codec::decode(g, options).unwrap_or(UNDEF);
    with(|s| s.pending_port = port);
    let mut object = UNDEF;
    let status = glue::rjs_construct(g, ctor, &options, 1, &mut object);
    with(|s| s.pending_port = 0);
    if status != 0 {
        crate::ffi::redjs_unprotect(g, port);
        return Err(exception_text(g, object));
    }
    crate::ffi::redjs_protect(g, object);

    // One array keeps every buffer alive, whatever the processor does to `inputs` and `outputs`.
    let keep = new_array(g);
    crate::ffi::redjs_protect(g, keep);
    let (inputs, outputs, parameters) = (new_array(g), new_array(g), new_object(g, UNDEF));
    let mut kept = 0;
    let mut hold = |value: u64| {
        set_i(g, keep, kept, value);
        kept += 1;
        value
    };
    hold(inputs);
    hold(outputs);
    hold(parameters);
    let mut in_channels = [UNDEF; 2];
    if bridge.has_input {
        let input = hold(new_array(g));
        set_i(g, inputs, 0, input);
        for (c, slot) in in_channels.iter_mut().enumerate() {
            *slot = hold(float_array(g, QUANTUM));
            set_i(g, input, c as u32, *slot);
        }
    }
    let mut out_channels = [UNDEF; 2];
    if bridge.has_output {
        let output = hold(new_array(g));
        set_i(g, outputs, 0, output);
        for (c, slot) in out_channels.iter_mut().enumerate().take(bridge.channels()) {
            *slot = hold(float_array(g, QUANTUM));
            set_i(g, output, c as u32, *slot);
        }
    }
    let mut param_values = Vec::with_capacity(param_names.len());
    for name in &param_names {
        let value = hold(float_array(g, 1));
        define(g, parameters, name, value, 0);
        param_values.push(value);
    }
    Ok(Instance {
        object,
        port,
        keep,
        bridge,
        inputs,
        outputs,
        parameters,
        in_channels,
        out_channels,
        param_values,
        produced: 0,
        base_frame,
    })
}

impl Instance {
    fn due(&self) -> bool {
        if !self.bridge.active.load(Ordering::Relaxed) {
            return false;
        }
        if self.bridge.has_input {
            self.bridge.input.frames() >= QUANTUM
        } else {
            self.produced < self.bridge.demand.load(Ordering::Acquire) + LEAD_FRAMES as u64
        }
    }

    /// One render quantum. `Err` is what `process()` threw.
    unsafe fn render(&mut self, g: *mut c_void, sample_rate: u32) -> Result<(), String> {
        let global = glue::rjs_global(g);
        if self.bridge.has_input {
            let input = get_i(g, self.inputs, 0);
            let [left, right] = &mut self.in_channels;
            let left = samples(g, input, 0, left, QUANTUM);
            let right = samples(g, input, 1, right, QUANTUM);
            if left.len() == QUANTUM && right.len() == QUANTUM {
                self.bridge.input.pop(left, right);
            }
        }
        let output = get_i(g, self.outputs, 0);
        if self.bridge.has_output {
            let count = self.bridge.channels();
            for (c, channel) in self.out_channels.iter_mut().enumerate().take(count) {
                samples(g, output, c as u32, channel, QUANTUM).fill(0.0);
            }
        }
        for (i, value) in self.param_values.iter().enumerate() {
            set_i(g, *value, 0, enc_f64(self.bridge.param(i) as f64));
        }
        let frame = self.base_frame + self.produced;
        define(g, global, "currentFrame", enc_f64(frame as f64), 0);
        define(
            g,
            global,
            "currentTime",
            enc_f64(frame as f64 / sample_rate.max(1) as f64),
            0,
        );
        let process = get(g, self.object, "process");
        if glue::rjs_kind(g, process) != 9 {
            return Err("the processor has no process() method".into());
        }
        let more = call(
            g,
            process,
            self.object,
            &[self.inputs, self.outputs, self.parameters],
        )
        .map_err(|exception| exception_text(g, exception))?;
        if self.bridge.has_output {
            let [left, right] = &mut self.out_channels;
            let left = samples(g, output, 0, left, QUANTUM);
            if self.bridge.mono {
                self.bridge.output.push(left, left);
            } else {
                self.bridge
                    .output
                    .push(left, samples(g, output, 1, right, QUANTUM));
            }
        }
        self.produced += QUANTUM as u64;
        if !self.bridge.has_input && !to_bool(g, more) {
            self.bridge.active.store(false, Ordering::Relaxed);
        }
        Ok(())
    }
}

unsafe fn deliver(g: *mut c_void, instance: &Instance, data: &[u8]) {
    let handler = get(g, instance.port, "onmessage");
    if glue::rjs_kind(g, handler) != 9 {
        return;
    }
    let event = new_object(g, UNDEF);
    define(g, event, "data", codec::decode(g, data).unwrap_or(UNDEF), 0);
    if let Err(exception) = call(g, handler, instance.port, &[event]) {
        eprintln!(
            "[audio worklet] Uncaught in port.onmessage: {}",
            exception_text(g, exception)
        );
    }
}

unsafe fn run(commands: Receiver<Command>, replies: Arc<Mutex<Vec<Reply>>>, sample_rate: u32) {
    let g = crate::ffi::context_create_plain();
    if g.is_null() {
        return;
    }
    install(g, replies, sample_rate);
    let mut instances: HashMap<u64, Instance> = HashMap::new();
    loop {
        loop {
            match commands.try_recv() {
                Ok(Command::Module { id, url, source }) => load_module(g, id, &url, &source),
                Ok(Command::Construct {
                    node,
                    name,
                    options,
                    bridge,
                    base_frame,
                }) => {
                    let made =
                        construct(g, node, &name, &options, ArcGc::clone(&bridge), base_frame);
                    match made {
                        Ok(instance) => {
                            instances.insert(node, instance);
                        }
                        Err(message) => {
                            bridge.active.store(false, Ordering::Relaxed);
                            reply(Reply::Error { node, message });
                        }
                    }
                }
                Ok(Command::Message { node, data }) => {
                    if let Some(instance) = instances.get(&node) {
                        deliver(g, instance, &data);
                    }
                }
                Ok(Command::Drop { node }) => {
                    if let Some(instance) = instances.remove(&node) {
                        crate::ffi::redjs_unprotect(g, instance.object);
                        crate::ffi::redjs_unprotect(g, instance.port);
                        crate::ffi::redjs_unprotect(g, instance.keep);
                    }
                }
                Err(TryRecvError::Empty) => break,
                // The context is closed. The VM is left as it is: tearing one down is not safe.
                Err(TryRecvError::Disconnected) => return,
            }
        }
        let mut worked = false;
        for (&node, instance) in instances.iter_mut() {
            // A bounded burst, so commands are not kept waiting by a processor catching up.
            for _ in 0..16 {
                if !instance.due() {
                    break;
                }
                worked = true;
                if let Err(message) = instance.render(g, sample_rate) {
                    instance.bridge.active.store(false, Ordering::Relaxed);
                    reply(Reply::Error { node, message });
                }
            }
        }
        glue::rjs_drain_microtasks(g);
        if !worked {
            std::thread::park_timeout(Duration::from_millis(20));
        }
    }
}
