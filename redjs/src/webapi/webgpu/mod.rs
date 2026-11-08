//! Native WebGPU over the same wgpu revision as the host renderer.
//!
//! Opt-in while canvas presentation is being integrated. JSC objects own logical
//! clients, not the physical device: embedders may supply an existing context.
//! WGSL always goes through wgpu validation; no shader passthrough is exposed.
pub mod backend;
mod buffer;
pub mod canvas;
mod compute;
mod readback;
#[cfg(test)]
mod tests;
mod texture;

use super::util::*;
use crate::{capi, glue};
use backend::{Adapter, Client};
use core::ffi::c_void;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Gpu,
    Canvas,
    Adapter,
    Device,
    Queue,
    Features,
    Limits,
    AdapterInfo,
    LostInfo,
    Buffer,
    Shader,
    BindGroupLayout,
    PipelineLayout,
    BindGroup,
    ComputePipeline,
    Encoder,
    ComputePass,
    CommandBuffer,
    Texture,
    TextureView,
    Sampler,
    RenderPipeline,
    RenderPass,
}
#[derive(Clone)]
enum Resource {
    Gpu,
    Canvas(Arc<canvas::Surface>),
    Adapter(Arc<Adapter>),
    Device,
    Queue,
    Features(wgpu::Features),
    Buffer(Arc<buffer::Buffer>),
    Shader(compute::Shader),
    BindGroupLayout(wgpu::BindGroupLayout),
    PipelineLayout(wgpu::PipelineLayout),
    BindGroup(compute::BindGroup),
    ComputePipeline(wgpu::ComputePipeline),
    Encoder(Arc<Mutex<compute::Encoder>>),
    ComputePass(Arc<Mutex<compute::ComputePass>>),
    CommandBuffer(Arc<Mutex<compute::CommandBuffer>>),
    Texture(Arc<texture::Texture>),
    TextureView(texture::TextureView),
    Sampler(wgpu::Sampler),
    RenderPipeline(wgpu::RenderPipeline),
    RenderPass(Arc<Mutex<texture::RenderPass>>),
}
#[derive(Clone)]
struct Handle {
    pub kind: Kind,
    pub client: Option<Arc<Client>>,
    pub resource: Resource,
}
struct Classes {
    array_values: u64,
    s_features: usize,
    s_limits: usize,
    s_info: usize,
    weak_ctor: u64,
    weak_deref: u64,
    error_protos: HashMap<&'static str, u64>,
    hosts: HashMap<Kind, usize>,
    types: HashMap<Kind, Class>,
    array_from: u64,
    s_gpu_canvas: usize,
    s_gpu_device: usize,
    s_gpu_texture: usize,
    s_ranges: usize,
    s_lost: usize,
    s_resolve_lost: usize,
}
unsafe impl Send for Classes {}
struct Pending {
    resolve: u64,
    reject: u64,
    roots: Vec<u64>,
}
struct ClientRef {
    weak: u64,
    client: std::sync::Weak<Client>,
    buffers: Vec<u64>,
    lost: bool,
}
struct Scope {
    filter: String,
    error: Option<backend::GpuError>,
}
#[derive(Default)]
struct State {
    classes: Option<Classes>,
    pending: HashMap<u64, Pending>,
    ready: VecDeque<Box<dyn FnOnce(*mut c_void) + Send>>,
    clients: HashMap<u64, ClientRef>,
    scopes: HashMap<u64, Vec<Scope>>,
}
fn states() -> &'static Mutex<HashMap<usize, State>> {
    static S: OnceLock<Mutex<HashMap<usize, State>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// A temporary GC root. Descriptor getters may allocate, force GC, or reenter.
struct Val {
    pub g: *mut c_void,
    pub v: u64,
}
impl Val {
    pub unsafe fn new(g: *mut c_void, v: u64) -> Self {
        if is_cell(v) {
            crate::ffi::redjs_protect(g, v);
        }
        Self { g, v }
    }
}
impl Drop for Val {
    fn drop(&mut self) {
        if is_cell(self.v) {
            unsafe {
                crate::ffi::redjs_unprotect(self.g, self.v);
            }
        }
    }
}
// Internal arrays are GC edges. Create own data properties rather than invoking
// an application-defined Array.prototype[index] setter.
unsafe fn set_i(g: *mut c_void, obj: u64, index: u32, value: u64) {
    assert_eq!(
        glue::rjs_define_index(g, obj, index, value),
        0,
        "WebGPU internal array property creation failed"
    );
}
unsafe fn field(g: *mut c_void, obj: u64, key: &str) -> Result<Val, ()> {
    if obj == UNDEF || obj == NULL {
        return Ok(Val::new(g, UNDEF));
    }
    Ok(Val::new(g, try_get(g, obj, key)?))
}
unsafe fn text(g: *mut c_void, v: u64, default: &str) -> Result<String, ()> {
    if v == UNDEF {
        return Ok(default.into());
    }
    let s = to_js_string(g, v)?;
    Ok(read_string(g, s).unwrap_or_default())
}
unsafe fn num(g: *mut c_void, v: u64, default: u64) -> Result<u64, ()> {
    if v == UNDEF {
        Ok(default)
    } else {
        enforce_range_u64(g, v, "WebGPU integer")
    }
}
unsafe fn uint(g: *mut c_void, v: u64, default: u32) -> Result<u32, ()> {
    let n = num(g, v, default as u64)?;
    if n > u32::MAX as u64 {
        throw_type_error(g, "WebGPU integer exceeds unsigned long");
        return Err(());
    }
    Ok(n as u32)
}
unsafe fn boolean(g: *mut c_void, v: u64) -> bool {
    glue::rjs_to_boolean(g, v) != 0
}
unsafe fn seq(g: *mut c_void, v: u64) -> Result<Vec<Val>, ()> {
    if v == UNDEF {
        return Ok(Vec::new());
    }
    let f = states().lock().unwrap()[&(g as usize)]
        .classes
        .as_ref()
        .unwrap()
        .array_from;
    let arr = match call(g, f, UNDEF, &[v]) {
        Ok(a) => Val::new(g, a),
        Err(e) => {
            glue::rjs_throw(g, e);
            return Err(());
        }
    };
    let n = uint(g, field(g, arr.v, "length")?.v, 0)?;
    if n > 65536 {
        throw_type_error(g, "WebGPU descriptor sequence is too large");
        return Err(());
    }
    (0..n).map(|i| field(g, arr.v, &i.to_string())).collect()
}
unsafe fn handle(g: *mut c_void, v: u64, kind: Kind) -> Result<Handle, ()> {
    let cls = states()
        .lock()
        .unwrap()
        .get(&(g as usize))
        .and_then(|s| s.classes.as_ref())
        .and_then(|c| c.hosts.get(&kind).copied())
        .map(|c| c as capi::JSClassRef);
    if let Some(cls) = cls {
        if let Some(h) = host_data::<Handle>(g, cls, v) {
            if h.kind == kind {
                return Ok(h.clone());
            }
        }
    }
    throw_type_error(g, "WebGPU object has the wrong interface or receiver");
    Err(())
}
unsafe fn this(g: *mut c_void, cf: *mut c_void, kind: Kind) -> Result<Handle, ()> {
    handle(g, cf_this(cf), kind)
}
unsafe fn client(g: *mut c_void, h: &Handle) -> Result<Arc<Client>, ()> {
    let c = h.client.as_ref().unwrap().clone();
    if let Err(e) = c.check() {
        super::domexception::throw(g, "InvalidStateError", &e);
        return Err(());
    }
    Ok(c)
}
unsafe fn owned(g: *mut c_void, v: u64, kind: Kind, c: &Arc<Client>) -> Result<Handle, ()> {
    let h = handle(g, v, kind)?;
    if h.client.as_ref().map(|x| x.id) != Some(c.id) {
        throw_type_error(g, "WebGPU resource belongs to a different device");
        return Err(());
    }
    Ok(h)
}
unsafe fn wrap(g: *mut c_void, kind: Kind, c: Option<Arc<Client>>, r: Resource) -> u64 {
    let (cls, proto) = {
        let s = states().lock().unwrap();
        let cs = s[&(g as usize)].classes.as_ref().unwrap();
        (cs.hosts[&kind] as capi::JSClassRef, cs.types[&kind].proto)
    };
    host_create(
        g,
        cls,
        proto,
        Handle {
            kind,
            client: c,
            resource: r,
        },
    )
}
unsafe fn invalid(g: *mut c_void, msg: &str) -> Result<(), ()> {
    throw_type_error(g, msg);
    Err(())
}
// Canvas texture destruction closes JS write access while its drawing buffer
// remains readable by the presenter. Recorded commands must honor that logical
// lifetime even though the underlying native texture is still alive.
fn resource_alive(r: &Resource) -> bool {
    match r {
        Resource::Buffer(b) => !b.destroyed.load(Ordering::Acquire),
        Resource::Texture(t) => !t.destroyed.load(Ordering::Acquire),
        Resource::TextureView(v) => !v._owner.destroyed.load(Ordering::Acquire),
        Resource::BindGroup(b) => b._resources.iter().all(resource_alive),
        _ => true,
    }
}
unsafe fn validation(g: *mut c_void, c: &Arc<Client>, msg: &str) {
    route_errors(
        g,
        c.id,
        vec![backend::GpuError {
            kind: "validation",
            message: msg.into(),
        }],
    );
}
unsafe fn checked<T>(g: *mut c_void, c: &Arc<Client>, f: impl FnOnce() -> T) -> T {
    let out = c.checked(f);
    route_errors(g, c.id, c.take_errors());
    out
}
unsafe fn error_object(g: *mut c_void, e: backend::GpuError) -> u64 {
    let name = match e.kind {
        "out-of-memory" => "GPUOutOfMemoryError",
        "internal" => "GPUInternalError",
        _ => "GPUValidationError",
    };
    let proto = states().lock().unwrap()[&(g as usize)]
        .classes
        .as_ref()
        .unwrap()
        .error_protos[name];
    let o = Val::new(g, new_object(g, proto));
    define(g, o.v, "message", js_string(g, &e.message), 1 | 2);
    o.v
}
unsafe fn route_errors(g: *mut c_void, id: u64, errors: Vec<backend::GpuError>) {
    for e in errors {
        let uncaptured = {
            let mut ss = states().lock().unwrap();
            let scopes = ss
                .get_mut(&(g as usize))
                .unwrap()
                .scopes
                .entry(id)
                .or_default();
            if let Some(scope) = scopes.iter_mut().rev().find(|x| x.filter == e.kind) {
                if scope.error.is_none() {
                    scope.error = Some(e);
                }
                None
            } else {
                Some(e)
            }
        };
        if let Some(e) = uncaptured {
            states()
                .lock()
                .unwrap()
                .get_mut(&(g as usize))
                .unwrap()
                .ready
                .push_back(Box::new(move |g| unsafe {
                    if let Some(target) = device_target(g, id) {
                        let error = Val::new(g, error_object(g, e));
                        let event =
                            Val::new(g, super::events::new_trusted_event(g, "uncapturederror"));
                        define(g, event.v, "error", error.v, 1 | 2);
                        let proto = states().lock().unwrap()[&(g as usize)]
                            .classes
                            .as_ref()
                            .unwrap()
                            .error_protos["GPUUncapturedErrorEvent"];
                        capi::JSObjectSetPrototype(
                            g,
                            event.v as usize as capi::JSObjectRef,
                            proto as usize as capi::JSValueRef,
                        );
                        super::events::dispatch(g, target.v, event.v);
                    }
                }));
        }
    }
}
unsafe extern "sysv64" fn gpu_error_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let message = text(g, cf_arg(cf, 0), "")?;
        let o = Val::new(g, ordinary_create(g, cf_new_target(cf), UNDEF));
        define(g, o.v, "message", js_string(g, &message), 1 | 2);
        Ok(o.v)
    })
}
unsafe extern "sysv64" fn error_event_construct(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let init = Val::new(g, cf_arg(cf, 1));
        let error = field(g, init.v, "error")?;
        if !is_cell(error.v) {
            invalid(g, "GPUUncapturedErrorEvent requires an error")?;
        }
        let o = Val::new(
            g,
            ordinary_create(g, cf_new_target(cf), super::util::state(g).event.proto),
        );
        super::events::init_event(g, o.v, cf_arg(cf, 0), init.v, false);
        define(g, o.v, "error", error.v, 1 | 2);
        Ok(o.v)
    })
}

/// Promise completions own only protected values while their context is alive.
/// Workers carry Rust resources, never JS heap pointers; recycled addresses are
/// fenced by ContextId before enqueueing on the context pump.
unsafe fn async_op<T, F, C>(g: *mut c_void, roots: Vec<u64>, work: F, convert: C) -> u64
where
    T: Send + 'static,
    F: std::future::Future<Output = Result<T, String>> + Send + 'static,
    C: FnOnce(*mut c_void, T) -> u64 + Send + 'static,
{
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let (mut resolve, mut reject, mut p) = (UNDEF, UNDEF, UNDEF);
    if crate::ffi::redjs_promise_create(g, &mut resolve, &mut reject, &mut p) != 0 {
        return UNDEF;
    }
    for &v in roots.iter().chain([&resolve, &reject]) {
        if is_cell(v) {
            crate::ffi::redjs_protect(g, v);
        }
    }
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let id = crate::context::id_of(g);
    let address = g as usize;
    states()
        .lock()
        .unwrap()
        .get_mut(&address)
        .unwrap()
        .pending
        .insert(
            n,
            Pending {
                resolve,
                reject,
                roots,
            },
        );
    let worker = std::thread::Builder::new()
        .name("redjs-webgpu".into())
        .spawn(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pollster::block_on(work)))
                    .unwrap_or_else(|_| Err("Native WebGPU asynchronous operation failed".into()));
            let mut ss = states().lock().unwrap();
            if crate::context::id_of(address as *mut c_void) != id {
                return;
            }
            let Some(st) = ss.get_mut(&address) else {
                return;
            };
            st.ready.push_back(Box::new(move |g| unsafe {
                let pending = states()
                    .lock()
                    .unwrap()
                    .get_mut(&(g as usize))
                    .and_then(|s| s.pending.remove(&n));
                let Some(pending) = pending else {
                    return;
                };
                match result {
                    Ok(value) => {
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            convert(g, value)
                        })) {
                            Ok(value) => {
                                let value = Val::new(g, value);
                                let _ = call(g, pending.resolve, UNDEF, &[value.v]);
                            }
                            Err(_) => {
                                let err = Val::new(
                                    g,
                                    super::domexception::create(
                                        g,
                                        "OperationError",
                                        "Native WebGPU completion failed",
                                    ),
                                );
                                let _ = call(g, pending.reject, UNDEF, &[err.v]);
                            }
                        }
                    }
                    Err(message) => {
                        let err = Val::new(
                            g,
                            super::domexception::create(g, "OperationError", &message),
                        );
                        let _ = call(g, pending.reject, UNDEF, &[err.v]);
                    }
                }
                for v in pending
                    .roots
                    .into_iter()
                    .chain([pending.resolve, pending.reject])
                {
                    if is_cell(v) {
                        crate::ffi::redjs_unprotect(g, v);
                    }
                }
            }));
            drop(ss);
            if let Some(id) = id {
                crate::web::wake(id);
            }
        });
    if let Err(error) = worker {
        let pending = states()
            .lock()
            .unwrap()
            .get_mut(&address)
            .unwrap()
            .pending
            .remove(&n)
            .unwrap();
        let e = Val::new(
            g,
            super::domexception::create(g, "OperationError", &error.to_string()),
        );
        let _ = call(g, pending.reject, UNDEF, &[e.v]);
        for v in pending
            .roots
            .into_iter()
            .chain([pending.resolve, pending.reject])
        {
            if is_cell(v) {
                crate::ffi::redjs_unprotect(g, v);
            }
        }
    }
    p
}
unsafe fn inflight(g: *mut c_void) -> bool {
    states()
        .lock()
        .unwrap()
        .get(&(g as usize))
        .is_some_and(|s| !s.pending.is_empty())
}
unsafe fn ready(g: *mut c_void) -> bool {
    states()
        .lock()
        .unwrap()
        .get(&(g as usize))
        .is_some_and(|s| {
            !s.ready.is_empty()
                || s.clients.values().any(|r| {
                    r.client
                        .upgrade()
                        .is_some_and(|c| c.has_errors() || (!r.lost && c.context.loss().is_some()))
                })
        })
}
unsafe fn drain(g: *mut c_void) -> usize {
    let tasks = states()
        .lock()
        .unwrap()
        .get_mut(&(g as usize))
        .map(|s| std::mem::take(&mut s.ready))
        .unwrap_or_default();
    let mut n = tasks.len();
    for task in tasks {
        task(g);
        crate::web::checkpoint(g);
    }
    let clients: Vec<_> = states()
        .lock()
        .unwrap()
        .get(&(g as usize))
        .map(|s| {
            s.clients
                .values()
                .filter_map(|r| r.client.upgrade())
                .collect()
        })
        .unwrap_or_default();
    for c in clients {
        route_errors(g, c.id, c.take_errors());
    }
    let ids: Vec<_> = states()
        .lock()
        .unwrap()
        .get(&(g as usize))
        .map(|s| {
            s.clients
                .iter()
                .filter_map(|(&id, r)| {
                    (!r.lost)
                        .then(|| r.client.upgrade())
                        .flatten()
                        .and_then(|c| c.context.loss().map(|l| (id, l)))
                })
                .collect()
        })
        .unwrap_or_default();
    for (id, loss) in ids {
        release_buffers(g, id);
        canvas::device_destroyed(id);
        if let Some(target) = device_target(g, id) {
            resolve_loss(g, target.v, loss.reason, &loss.message);
        } else if let Some(r) = states()
            .lock()
            .unwrap()
            .get_mut(&(g as usize))
            .and_then(|s| s.clients.get_mut(&id))
        {
            r.lost = true;
        }
        n += 1;
    }
    n
}

unsafe extern "sysv64" fn request_adapter(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        this(g, cf, Kind::Gpu)?;
        let o = Val::new(g, cf_arg(cf, 0));
        let power = text(g, field(g, o.v, "powerPreference")?.v, "")?;
        let power_preference = match power.as_str() {
            "" => wgpu::PowerPreference::None,
            "low-power" => wgpu::PowerPreference::LowPower,
            "high-performance" => wgpu::PowerPreference::HighPerformance,
            _ => {
                invalid(g, "Invalid GPUPowerPreference")?;
                unreachable!()
            }
        };
        let fallback = boolean(g, field(g, o.v, "forceFallbackAdapter")?.v);
        Ok(async_op(
            g,
            vec![],
            async move {
                backend::request_adapter(backend::AdapterOptions {
                    power_preference,
                    force_fallback_adapter: fallback,
                })
                .await
            },
            |g, a| match a {
                Some(a) => wrap(g, Kind::Adapter, None, Resource::Adapter(Arc::new(a))),
                None => NULL,
            },
        ))
    })
}
unsafe extern "sysv64" fn preferred_format(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        this(g, cf, Kind::Gpu)?;
        Ok(js_string(g, "bgra8unorm"))
    })
}
unsafe fn features_object(g: *mut c_void, features: wgpu::Features) -> u64 {
    wrap(g, Kind::Features, None, Resource::Features(features))
}
unsafe fn feature_flags(g: *mut c_void, cf: *mut c_void) -> Result<wgpu::Features, ()> {
    let h = this(g, cf, Kind::Features)?;
    let Resource::Features(f) = h.resource else {
        unreachable!()
    };
    Ok(f)
}
unsafe extern "sysv64" fn feature_size(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let f = feature_flags(g, cf)?;
        Ok(enc_f64(
            feature_names()
                .iter()
                .filter(|(_, flag)| f.contains(*flag))
                .count() as f64,
        ))
    })
}
unsafe extern "sysv64" fn feature_has(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let f = feature_flags(g, cf)?;
        let name = text(g, cf_arg(cf, 0), "")?;
        Ok(enc_bool(
            feature_names()
                .iter()
                .any(|(n, flag)| *n == name && f.contains(*flag)),
        ))
    })
}
unsafe fn feature_iterator(g: *mut c_void, cf: *mut c_void, entries: bool) -> Result<u64, ()> {
    let f = feature_flags(g, cf)?;
    let a = Val::new(g, new_array(g));
    for (i, (name, _)) in feature_names()
        .into_iter()
        .filter(|(_, flag)| f.contains(*flag))
        .enumerate()
    {
        let value = Val::new(g, js_string(g, name));
        if entries {
            let pair = Val::new(g, new_array(g));
            set_i(g, pair.v, 0, value.v);
            set_i(g, pair.v, 1, value.v);
            set_i(g, a.v, i as u32, pair.v);
        } else {
            set_i(g, a.v, i as u32, value.v);
        }
    }
    let values = states().lock().unwrap()[&(g as usize)]
        .classes
        .as_ref()
        .unwrap()
        .array_values;
    call(g, values, a.v, &[]).map_err(|e| {
        glue::rjs_throw(g, e);
    })
}
unsafe extern "sysv64" fn feature_values(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || feature_iterator(g, cf, false))
}
unsafe extern "sysv64" fn feature_entries(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || feature_iterator(g, cf, true))
}
unsafe extern "sysv64" fn feature_each(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let f = feature_flags(g, cf)?;
        let cb = cf_arg(cf, 0);
        if glue::rjs_kind(g, cb) != 9 {
            invalid(g, "GPUSupportedFeatures.forEach requires a callback")?;
        }
        for (name, _) in feature_names()
            .into_iter()
            .filter(|(_, flag)| f.contains(*flag))
        {
            let s = Val::new(g, js_string(g, name));
            call(g, cb, cf_arg(cf, 1), &[s.v, s.v, cf_this(cf)]).map_err(|e| {
                glue::rjs_throw(g, e);
            })?;
        }
        Ok(UNDEF)
    })
}
unsafe fn cached(g: *mut c_void, obj: u64, slot: usize, build: impl FnOnce() -> u64) -> u64 {
    let old = get_a(g, obj, slot as *mut c_void);
    if old != UNDEF {
        return old;
    }
    let v = Val::new(g, build());
    glue::rjs_define(g, obj, slot as *mut c_void, v.v, 1 | 2);
    v.v
}
unsafe fn plain(g: *mut c_void, kind: Kind) -> u64 {
    let proto = states().lock().unwrap()[&(g as usize)]
        .classes
        .as_ref()
        .unwrap()
        .types[&kind]
        .proto;
    new_object(g, proto)
}
// Capability advertising follows implemented APIs. Optional query, compressed
// texture and f16 surfaces will be enabled together with their validation/tests.
fn feature_names() -> Vec<(&'static str, wgpu::Features)> {
    Vec::new()
}
unsafe extern "sysv64" fn adapter_features(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Adapter)?;
        let Resource::Adapter(a) = h.resource else {
            unreachable!()
        };
        let slot = states().lock().unwrap()[&(g as usize)]
            .classes
            .as_ref()
            .unwrap()
            .s_features;
        Ok(cached(g, cf_this(cf), slot, || {
            features_object(
                g,
                a.shared
                    .as_ref()
                    .map_or_else(|| a.raw.features(), |c| c.device.features()),
            )
        }))
    })
}
unsafe extern "sysv64" fn device_features(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Device)?;
        let slot = states().lock().unwrap()[&(g as usize)]
            .classes
            .as_ref()
            .unwrap()
            .s_features;
        Ok(cached(g, cf_this(cf), slot, || {
            features_object(g, h.client.unwrap().features)
        }))
    })
}
macro_rules! limits_fields {($m:ident)=>{$m!{maxTextureDimension1D=>max_texture_dimension_1d,maxTextureDimension2D=>max_texture_dimension_2d,maxTextureDimension3D=>max_texture_dimension_3d,maxTextureArrayLayers=>max_texture_array_layers,maxBindGroups=>max_bind_groups,maxBindingsPerBindGroup=>max_bindings_per_bind_group,maxDynamicUniformBuffersPerPipelineLayout=>max_dynamic_uniform_buffers_per_pipeline_layout,maxDynamicStorageBuffersPerPipelineLayout=>max_dynamic_storage_buffers_per_pipeline_layout,maxSampledTexturesPerShaderStage=>max_sampled_textures_per_shader_stage,maxSamplersPerShaderStage=>max_samplers_per_shader_stage,maxStorageBuffersPerShaderStage=>max_storage_buffers_per_shader_stage,maxStorageTexturesPerShaderStage=>max_storage_textures_per_shader_stage,maxUniformBuffersPerShaderStage=>max_uniform_buffers_per_shader_stage,maxUniformBufferBindingSize=>max_uniform_buffer_binding_size,maxStorageBufferBindingSize=>max_storage_buffer_binding_size,minUniformBufferOffsetAlignment=>min_uniform_buffer_offset_alignment,minStorageBufferOffsetAlignment=>min_storage_buffer_offset_alignment,maxVertexBuffers=>max_vertex_buffers,maxBufferSize=>max_buffer_size,maxVertexAttributes=>max_vertex_attributes,maxVertexBufferArrayStride=>max_vertex_buffer_array_stride,maxInterStageShaderVariables=>max_inter_stage_shader_variables,maxColorAttachments=>max_color_attachments,maxColorAttachmentBytesPerSample=>max_color_attachment_bytes_per_sample,maxComputeWorkgroupStorageSize=>max_compute_workgroup_storage_size,maxComputeInvocationsPerWorkgroup=>max_compute_invocations_per_workgroup,maxComputeWorkgroupSizeX=>max_compute_workgroup_size_x,maxComputeWorkgroupSizeY=>max_compute_workgroup_size_y,maxComputeWorkgroupSizeZ=>max_compute_workgroup_size_z,maxComputeWorkgroupsPerDimension=>max_compute_workgroups_per_dimension}};}
unsafe fn limits_object(g: *mut c_void, l: &wgpu::Limits) -> u64 {
    let o = Val::new(g, plain(g, Kind::Limits));
    macro_rules! write {($($js:ident=>$rust:ident),*)=>{$(define(g,o.v,stringify!($js),enc_f64(l.$rust as f64),1|2);)*};}
    limits_fields!(write);
    o.v
}
unsafe extern "sysv64" fn adapter_limits(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Adapter)?;
        let Resource::Adapter(a) = h.resource else {
            unreachable!()
        };
        let slot = states().lock().unwrap()[&(g as usize)]
            .classes
            .as_ref()
            .unwrap()
            .s_limits;
        Ok(cached(g, cf_this(cf), slot, || {
            limits_object(
                g,
                &a.shared
                    .as_ref()
                    .map_or_else(|| a.raw.limits(), |c| c.device.limits()),
            )
        }))
    })
}
unsafe extern "sysv64" fn device_limits(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Device)?;
        let slot = states().lock().unwrap()[&(g as usize)]
            .classes
            .as_ref()
            .unwrap()
            .s_limits;
        Ok(cached(g, cf_this(cf), slot, || {
            limits_object(g, &h.client.unwrap().limits)
        }))
    })
}
unsafe extern "sysv64" fn adapter_info(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Adapter)?;
        let Resource::Adapter(a) = h.resource else {
            unreachable!()
        };
        let slot = states().lock().unwrap()[&(g as usize)]
            .classes
            .as_ref()
            .unwrap()
            .s_info;
        let existing = get_a(g, cf_this(cf), slot as *mut c_void);
        if existing != UNDEF {
            return Ok(existing);
        }
        let i = a.raw.get_info();
        let o = Val::new(g, plain(g, Kind::AdapterInfo));
        for (k, v) in [
            ("vendor", format!("{:x}", i.vendor)),
            ("architecture", String::new()),
            ("device", format!("{:x}", i.device)),
            ("description", i.name),
        ] {
            define(g, o.v, k, js_string(g, &v), 1 | 2);
        }
        define(
            g,
            o.v,
            "isFallbackAdapter",
            enc_bool(i.device_type == wgpu::DeviceType::Cpu),
            1 | 2,
        );
        glue::rjs_define(g, cf_this(cf), slot as *mut c_void, o.v, 1 | 2);
        Ok(o.v)
    })
}
unsafe extern "sysv64" fn request_device(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Adapter)?;
        let Resource::Adapter(a) = h.resource else {
            unreachable!()
        };
        let d = Val::new(g, cf_arg(cf, 0));
        let mut features = wgpu::Features::empty();
        for v in seq(g, field(g, d.v, "requiredFeatures")?.v)? {
            let name = text(g, v.v, "")?;
            let Some((_, flag)) = feature_names().into_iter().find(|(n, _)| *n == name) else {
                return Ok(super::domexception::rejected(
                    g,
                    "NotSupportedError",
                    &format!("Unsupported WebGPU feature: {name}"),
                ));
            };
            features |= flag;
        }
        let l = field(g, d.v, "requiredLimits")?;
        let mut limits = wgpu::Limits::default();
        macro_rules! read {($($js:ident=>$rust:ident),*)=>{$(let v=field(g,l.v,stringify!($js))?;if v.v!=UNDEF {limits.$rust=num(g,v.v,0)?.try_into().map_err(|_|{throw_type_error(g,"WebGPU limit out of range");})?;})*};}
        limits_fields!(read);
        let client_limits = limits.clone();
        Ok(async_op(
            g,
            vec![],
            async move { a.request_device(features, limits).await },
            move |g, context| {
                make_device(
                    g,
                    Client::new_with_capabilities(context, features, client_limits),
                )
            },
        ))
    })
}
unsafe fn make_device(g: *mut c_void, c: Arc<Client>) -> u64 {
    prune_clients(g);
    let o = Val::new(g, wrap(g, Kind::Device, Some(c.clone()), Resource::Device));
    let queue = Val::new(g, wrap(g, Kind::Queue, Some(c.clone()), Resource::Queue));
    define(g, o.v, "queue", queue.v, 1 | 2);
    let (mut res, mut rej, mut p) = (UNDEF, UNDEF, UNDEF);
    crate::ffi::redjs_promise_create(g, &mut res, &mut rej, &mut p);
    let pr = Val::new(g, p);
    let rr = Val::new(g, res);
    let (sl, sr) = {
        let s = states().lock().unwrap();
        let c = s[&(g as usize)].classes.as_ref().unwrap();
        (c.s_lost, c.s_resolve_lost)
    };
    glue::rjs_define(g, o.v, sl as *mut c_void, pr.v, 1 | 2);
    glue::rjs_define(g, pr.v, sl as *mut c_void, o.v, 1 | 2);
    glue::rjs_define(g, o.v, sr as *mut c_void, rr.v, 1 | 2);
    let weak = weak_ref(g, o.v);
    states()
        .lock()
        .unwrap()
        .get_mut(&(g as usize))
        .unwrap()
        .clients
        .insert(
            c.id,
            ClientRef {
                weak,
                client: Arc::downgrade(&c),
                buffers: Vec::new(),
                lost: false,
            },
        );
    o.v
}
unsafe extern "sysv64" fn lost(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        this(g, cf, Kind::Device)?;
        let sl = states().lock().unwrap()[&(g as usize)]
            .classes
            .as_ref()
            .unwrap()
            .s_lost;
        Ok(get_a(g, cf_this(cf), sl as *mut c_void))
    })
}
unsafe fn prune_clients(g: *mut c_void) {
    let dead = {
        let mut s = states().lock().unwrap();
        let s = s.get_mut(&(g as usize)).unwrap();
        let ids: Vec<_> = s
            .clients
            .iter()
            .filter_map(|(&id, r)| (r.client.strong_count() == 0).then_some(id))
            .collect();
        ids.into_iter()
            .map(|id| {
                s.scopes.remove(&id);
                s.clients.remove(&id).unwrap()
            })
            .collect::<Vec<_>>()
    };
    for r in dead {
        for v in r.buffers.into_iter().chain([r.weak]) {
            crate::ffi::redjs_unprotect(g, v);
        }
    }
}
unsafe fn weak_ref(g: *mut c_void, obj: u64) -> u64 {
    let ctor = states().lock().unwrap()[&(g as usize)]
        .classes
        .as_ref()
        .unwrap()
        .weak_ctor;
    let mut out = UNDEF;
    if glue::rjs_construct(g, ctor, &obj, 1, &mut out) != 0 {
        panic!("Intrinsic WeakRef construction failed");
    }
    crate::ffi::redjs_protect(g, out);
    out
}
unsafe fn deref(g: *mut c_void, weak: u64) -> Val {
    let f = states().lock().unwrap()[&(g as usize)]
        .classes
        .as_ref()
        .unwrap()
        .weak_deref;
    Val::new(g, call(g, f, weak, &[]).unwrap_or(UNDEF))
}
unsafe fn device_target(g: *mut c_void, id: u64) -> Option<Val> {
    let weak = states()
        .lock()
        .unwrap()
        .get(&(g as usize))?
        .clients
        .get(&id)?
        .weak;
    let v = deref(g, weak);
    is_cell(v.v).then_some(v)
}
unsafe fn track_buffer_object(g: *mut c_void, c: &Arc<Client>, obj: u64) {
    let weak = weak_ref(g, obj);
    let old = {
        let mut s = states().lock().unwrap();
        let r = s
            .get_mut(&(g as usize))
            .unwrap()
            .clients
            .get_mut(&c.id)
            .unwrap();
        r.buffers.push(weak);
        if r.buffers.len() % 64 == 0 {
            std::mem::take(&mut r.buffers)
        } else {
            Vec::new()
        }
    };
    let mut live = Vec::new();
    for w in old {
        if is_cell(deref(g, w).v) {
            live.push(w)
        } else {
            crate::ffi::redjs_unprotect(g, w);
        }
    }
    if !live.is_empty() {
        states()
            .lock()
            .unwrap()
            .get_mut(&(g as usize))
            .unwrap()
            .clients
            .get_mut(&c.id)
            .unwrap()
            .buffers
            .extend(live);
    }
}
unsafe fn release_buffers(g: *mut c_void, id: u64) {
    let buffers = states()
        .lock()
        .unwrap()
        .get_mut(&(g as usize))
        .and_then(|s| s.clients.get_mut(&id))
        .map(|r| std::mem::take(&mut r.buffers))
        .unwrap_or_default();
    for w in buffers {
        let o = deref(g, w);
        if is_cell(o.v) {
            buffer::release_on_device_destroy(g, o.v);
        }
        crate::ffi::redjs_unprotect(g, w);
    }
}
unsafe fn resolve_loss(g: *mut c_void, target: u64, reason: &str, message: &str) {
    let sr = states().lock().unwrap()[&(g as usize)]
        .classes
        .as_ref()
        .unwrap()
        .s_resolve_lost;
    let res = Val::new(g, get_a(g, target, sr as *mut c_void));
    let info = Val::new(g, plain(g, Kind::LostInfo));
    define(g, info.v, "reason", js_string(g, reason), 1 | 2);
    define(g, info.v, "message", js_string(g, message), 1 | 2);
    let _ = call(g, res.v, UNDEF, &[info.v]);
    if let Ok(h) = handle(g, target, Kind::Device) {
        let id = h.client.unwrap().id;
        if let Some(r) = states()
            .lock()
            .unwrap()
            .get_mut(&(g as usize))
            .and_then(|s| s.clients.get_mut(&id))
        {
            r.lost = true;
        }
    }
}
unsafe extern "sysv64" fn destroy(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Device)?;
        let c = h.client.unwrap();
        release_buffers(g, c.id);
        c.destroy();
        canvas::device_destroyed(c.id);
        resolve_loss(
            g,
            cf_this(cf),
            "destroyed",
            "Logical WebGPU device was destroyed",
        );
        Ok(UNDEF)
    })
}
unsafe extern "sysv64" fn push_scope(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Device)?;
        let c = h.client.as_ref().unwrap();
        route_errors(g, c.id, c.take_errors());
        let filter = text(g, cf_arg(cf, 0), "")?;
        if !["validation", "out-of-memory", "internal"].contains(&filter.as_str()) {
            invalid(g, "Invalid GPUErrorFilter")?;
        }
        states()
            .lock()
            .unwrap()
            .get_mut(&(g as usize))
            .unwrap()
            .scopes
            .entry(h.client.unwrap().id)
            .or_default()
            .push(Scope {
                filter,
                error: None,
            });
        Ok(UNDEF)
    })
}
unsafe extern "sysv64" fn pop_scope(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Device)?;
        let scope = states()
            .lock()
            .unwrap()
            .get_mut(&(g as usize))
            .unwrap()
            .scopes
            .entry(h.client.unwrap().id)
            .or_default()
            .pop();
        let Some(scope) = scope else {
            return Ok(super::domexception::rejected(
                g,
                "OperationError",
                "No WebGPU error scope to pop",
            ));
        };
        let value = Val::new(g, scope.error.map_or(NULL, |e| error_object(g, e)));
        Ok(resolved_promise(g, value.v))
    })
}
/// No Rust unwind is allowed to cross a JSC host callback. Normal validation
/// errors are captured by wgpu scopes; internal bugs still return a JS error.
unsafe fn run(g: *mut c_void, f: impl FnOnce() -> Result<u64, ()>) -> u64 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(v)) => v,
        Ok(Err(())) => UNDEF,
        Err(_) => super::domexception::throw(g, "OperationError", "Native WebGPU operation failed"),
    }
}

unsafe extern "C" fn finalize_handle(obj: capi::JSObjectRef) {
    let p = capi::JSObjectGetPrivate(obj);
    if !p.is_null() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(Box::from_raw(p as *mut Handle))
        }));
    }
}
unsafe fn make_gpu_host_class(name: &str) -> capi::JSClassRef {
    let name = std::ffi::CString::new(name).unwrap();
    let mut d = capi::JSClassDefinition::empty();
    d.class_name = name.as_ptr();
    d.finalize = Some(finalize_handle);
    capi::JSClassCreate(&d)
}

pub(crate) unsafe fn install(g: *mut c_void) {
    states()
        .lock()
        .unwrap()
        .insert(g as usize, State::default());
    let global = glue::rjs_global(g);
    let array = Val::new(g, get(g, global, "Array"));
    let from = Val::new(g, get(g, array.v, "from"));
    crate::ffi::redjs_protect(g, from.v);
    let weak_ctor = Val::new(g, get(g, global, "WeakRef"));
    let weak_proto = field(g, weak_ctor.v, "prototype").unwrap();
    let weak_deref = field(g, weak_proto.v, "deref").unwrap();
    for v in [weak_ctor.v, weak_deref.v] {
        crate::ffi::redjs_protect(g, v);
    }
    let ap = field(g, array.v, "prototype").unwrap();
    let array_values = field(g, ap.v, "values").unwrap();
    crate::ffi::redjs_protect(g, array_values.v);
    let classes = Classes {
        array_values: array_values.v,
        s_features: atom_priv(g, "webgpu.features") as usize,
        s_limits: atom_priv(g, "webgpu.limits") as usize,
        s_info: atom_priv(g, "webgpu.info") as usize,
        weak_ctor: weak_ctor.v,
        weak_deref: weak_deref.v,
        error_protos: HashMap::new(),
        hosts: HashMap::new(),
        types: HashMap::new(),
        array_from: from.v,
        s_gpu_canvas: atom_priv(g, "webgpu.canvas") as usize,
        s_gpu_device: atom_priv(g, "webgpu.canvasDevice") as usize,
        s_gpu_texture: atom_priv(g, "webgpu.canvasTexture") as usize,
        s_ranges: atom_priv(g, "webgpu.ranges") as usize,
        s_lost: atom_priv(g, "webgpu.lost") as usize,
        s_resolve_lost: atom_priv(g, "webgpu.resolveLost") as usize,
    };
    states()
        .lock()
        .unwrap()
        .get_mut(&(g as usize))
        .unwrap()
        .classes = Some(classes);
    class(
        g,
        Kind::Gpu,
        "GPU",
        &[
            ("requestAdapter", 0, request_adapter),
            ("getPreferredCanvasFormat", 0, preferred_format),
        ],
        &[],
    );
    class(
        g,
        Kind::Adapter,
        "GPUAdapter",
        &[("requestDevice", 0, request_device)],
        &[
            ("features", adapter_features),
            ("limits", adapter_limits),
            ("info", adapter_info),
        ],
    );
    class(
        g,
        Kind::Device,
        "GPUDevice",
        &[
            ("destroy", 0, destroy),
            ("pushErrorScope", 1, push_scope),
            ("popErrorScope", 0, pop_scope),
        ],
        &[
            ("features", device_features),
            ("limits", device_limits),
            ("lost", lost),
        ],
    );
    class(g, Kind::Queue, "GPUQueue", &[], &[]);
    class(
        g,
        Kind::Features,
        "GPUSupportedFeatures",
        &[
            ("has", 1, feature_has),
            ("keys", 0, feature_values),
            ("values", 0, feature_values),
            ("entries", 0, feature_entries),
            ("forEach", 1, feature_each),
        ],
        &[("size", feature_size)],
    );
    class(g, Kind::Limits, "GPUSupportedLimits", &[], &[]);
    class(g, Kind::AdapterInfo, "GPUAdapterInfo", &[], &[]);
    class(g, Kind::LostInfo, "GPUDeviceLostInfo", &[], &[]);
    let feature_proto = states().lock().unwrap()[&(g as usize)]
        .classes
        .as_ref()
        .unwrap()
        .types[&Kind::Features]
        .proto;
    let iter = glue::rjs_atom_iterator(g);
    glue::rjs_define(
        g,
        feature_proto,
        iter,
        make_fn(g, "values", 0, feature_values),
        2,
    );
    glue::rjs_atom_free(g, iter);
    let base = define_class(
        g,
        &ClassSpec {
            name: "GPUError",
            arity: 0,
            construct: None,
            parent_proto: 0,
            methods: &[],
            getters: &[],
            accessors: &[],
        },
    );
    for name in [
        "GPUValidationError",
        "GPUOutOfMemoryError",
        "GPUInternalError",
    ] {
        let c = define_class(
            g,
            &ClassSpec {
                name,
                arity: 1,
                construct: Some(gpu_error_construct),
                parent_proto: base.proto,
                methods: &[],
                getters: &[],
                accessors: &[],
            },
        );
        states()
            .lock()
            .unwrap()
            .get_mut(&(g as usize))
            .unwrap()
            .classes
            .as_mut()
            .unwrap()
            .error_protos
            .insert(name, c.proto);
    }
    let event_class = define_class(
        g,
        &ClassSpec {
            name: "GPUUncapturedErrorEvent",
            arity: 2,
            construct: Some(error_event_construct),
            parent_proto: super::util::state(g).event.proto,
            methods: &[],
            getters: &[],
            accessors: &[],
        },
    );
    states()
        .lock()
        .unwrap()
        .get_mut(&(g as usize))
        .unwrap()
        .classes
        .as_mut()
        .unwrap()
        .error_protos
        .insert("GPUUncapturedErrorEvent", event_class.proto);
    buffer::install(g);
    compute::install(g);
    texture::install(g);
    canvas::install(g);
    let nav = Val::new(g, get(g, global, "navigator"));
    let gpu = wrap(g, Kind::Gpu, None, Resource::Gpu);
    define(g, nav.v, "gpu", gpu, 1 | 2);
    crate::web::register_pump_source(crate::web::PumpSource {
        inflight,
        ready,
        drain,
    });
}
unsafe fn class(
    g: *mut c_void,
    kind: Kind,
    name: &str,
    methods: &[(&str, u32, HostFn)],
    getters: &[(&str, HostFn)],
) {
    let host = make_gpu_host_class(name) as usize;
    let parent = if kind == Kind::Device {
        super::util::state(g).event_target.proto
    } else {
        0
    };
    let c = define_class(
        g,
        &ClassSpec {
            name,
            arity: 0,
            construct: None,
            parent_proto: parent,
            methods,
            getters,
            accessors: &[],
        },
    );
    states()
        .lock()
        .unwrap()
        .get_mut(&(g as usize))
        .unwrap()
        .classes
        .as_mut()
        .unwrap()
        .hosts
        .insert(kind, host);
    states()
        .lock()
        .unwrap()
        .get_mut(&(g as usize))
        .unwrap()
        .classes
        .as_mut()
        .unwrap()
        .types
        .insert(kind, c);
}
unsafe fn method(g: *mut c_void, kind: Kind, name: &str, arity: u32, f: HostFn) {
    let proto = states().lock().unwrap()[&(g as usize)]
        .classes
        .as_ref()
        .unwrap()
        .types[&kind]
        .proto;
    define(g, proto, name, make_fn(g, name, arity, f), 2);
}
unsafe fn constants(g: *mut c_void, name: &str, values: &[(&str, u32)]) {
    let o = Val::new(g, new_object(g, UNDEF));
    for &(n, v) in values {
        define(g, o.v, n, enc_f64(v as f64), 1 | 2 | 4);
    }
    define(g, glue::rjs_global(g), name, o.v, 1 | 2);
}
pub(crate) fn forget(gone: crate::context::Gone) {
    canvas::forget(gone);
    let s = states().lock().unwrap().remove(&gone.key);
    if let Some(s) = s {
        for r in s.clients.values() {
            if let Some(c) = r.client.upgrade() {
                c.destroy();
            }
        }
        if let Some(c) = s.classes {
            unsafe {
                for host in c.hosts.into_values() {
                    capi::JSClassRelease(host as capi::JSClassRef);
                }
            }
        }
    }
}
