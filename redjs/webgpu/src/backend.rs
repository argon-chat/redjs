//! Native GPU ownership, shared-device injection and per-client isolation.
//!
//! This layer contains no JSC values. An embedder can supply its renderer's
//! device through `GpuHost`; a JavaScript client's `destroy()` never destroys
//! that physical device or replaces the embedder's error/loss callbacks.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

#[derive(Clone, Copy, Debug, Default)]
pub struct AdapterOptions {
    pub power_preference: wgpu::PowerPreference,
    pub force_fallback_adapter: bool,
}

#[derive(Clone)]
pub struct Adapter {
    pub raw: Arc<wgpu::Adapter>,
    pub shared: Option<Arc<Context>>,
}

/// A loss notification owned by the host rather than by any one JS client.
#[derive(Clone, Debug)]
pub struct DeviceLoss {
    pub reason: &'static str,
    pub message: String,
}

/// The physical device and queue, potentially also owned by GPUI.
///
/// Construct with `from_shared` when importing a renderer's existing device.
/// That constructor does not change either of wgpu's process/device callbacks.
/// Forward a renderer-owned loss callback into `notify_lost` so all logical
/// clients observe the same device generation becoming unusable.
pub struct Context {
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    pub adapter: Arc<wgpu::Adapter>,
    loss: Arc<Mutex<Option<DeviceLoss>>>,
    // An independently negotiated device still belongs to its host generation.
    parent: Option<Arc<Context>>,
    // Declared last so a host generation outlives the wgpu wrappers above.
    _owner: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

impl Context {
    pub fn from_shared(
        adapter: Arc<wgpu::Adapter>,
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
    ) -> Self {
        Self {
            adapter,
            device,
            queue,
            loss: Arc::new(Mutex::new(None)),
            parent: None,
            _owner: None,
        }
    }

    /// Retains the renderer generation whose physical device backs these
    /// wrappers. Logical clients may outlive the window which requested them.
    pub fn from_shared_with_owner(
        adapter: Arc<wgpu::Adapter>,
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        owner: Arc<dyn std::any::Any + Send + Sync>,
    ) -> Self {
        Self {
            _owner: Some(owner),
            ..Self::from_shared(adapter, device, queue)
        }
    }

    pub fn notify_lost(&self, reason: &'static str, message: impl Into<String>) {
        let mut loss = self.loss.lock().unwrap();
        if loss.is_none() {
            *loss = Some(DeviceLoss {
                reason,
                message: message.into(),
            });
        }
    }

    pub fn loss(&self) -> Option<DeviceLoss> {
        self.loss
            .lock()
            .unwrap()
            .clone()
            .or_else(|| self.parent.as_ref()?.loss())
    }
}

pub type AdapterFuture =
    Pin<Box<dyn Future<Output = Result<Option<Adapter>, String>> + Send + 'static>>;

pub trait GpuHost: Send + Sync {
    /// A shared adapter must pair `raw` with the adapter used to create its
    /// `shared` context. A host controls feature/limit negotiation on that
    /// physical device before handing it to redjs.
    fn request_adapter(&self, options: AdapterOptions) -> AdapterFuture;
}

fn host_slot() -> &'static RwLock<Option<Arc<dyn GpuHost>>> {
    static HOST: OnceLock<RwLock<Option<Arc<dyn GpuHost>>>> = OnceLock::new();
    HOST.get_or_init(|| RwLock::new(None))
}

/// Changes the provider for future requests. Existing adapters/devices retain
/// their original provider's resources and continue to work.
pub fn set_host(host: Option<Arc<dyn GpuHost>>) {
    *host_slot().write().unwrap() = host;
}

pub async fn request_adapter(options: AdapterOptions) -> Result<Option<Adapter>, String> {
    let host = host_slot().read().unwrap().clone();
    if let Some(host) = host {
        return host.request_adapter(options).await;
    }
    // No work is done at JS-global installation time. Windows deliberately
    // selects D3D12, the backend that can eventually share GPUI's device;
    // D3D11 is not a wgpu backend.
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = if cfg!(target_os = "windows") {
        wgpu::Backends::DX12
    } else {
        wgpu::Backends::PRIMARY
    };
    descriptor.flags |= wgpu::InstanceFlags::AUTOMATIC_TIMESTAMP_NORMALIZATION;
    let instance = wgpu::Instance::new(descriptor);
    match instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: options.power_preference,
            force_fallback_adapter: options.force_fallback_adapter,
            compatible_surface: None,
            ..Default::default()
        })
        .await
    {
        Ok(raw) => Ok(Some(Adapter {
            raw: Arc::new(raw),
            shared: None,
        })),
        // The WebGPU result for an unavailable matching adapter is null.
        Err(_) => Ok(None),
    }
}

/// Physical capabilities a shared client may lack: redjs validates every JS
/// path to them against the client's own features and limits.
pub const VIRTUALIZED_FEATURES: wgpu::Features = wgpu::Features::TIMESTAMP_QUERY
    .union(wgpu::Features::IMMEDIATES)
    .union(wgpu::Features::EXTERNAL_TEXTURE);

/// Capabilities the renderer can share with default JS clients. Hosts creating a shared device
/// should request these, on an instance with
/// `InstanceFlags::AUTOMATIC_TIMESTAMP_NORMALIZATION`.
pub fn shareable_capabilities(adapter: &wgpu::Adapter) -> (wgpu::Features, wgpu::Limits) {
    let features = adapter.features() & VIRTUALIZED_FEATURES;
    let mut limits = wgpu::Limits::default();
    if features.contains(wgpu::Features::IMMEDIATES) {
        limits.max_immediate_size = adapter.limits().max_immediate_size;
    }
    (features, limits)
}

impl Adapter {
    pub fn features(&self) -> wgpu::Features {
        match &self.shared {
            // WebGPU resolves timestamps in nanoseconds; an unnormalized
            // device period cannot honour that.
            Some(c) if c.queue.get_timestamp_period() != 1.0 => {
                self.raw.features() - wgpu::Features::TIMESTAMP_QUERY
            }
            _ => self.raw.features(),
        }
    }

    pub fn limits(&self) -> wgpu::Limits {
        self.raw.limits()
    }

    pub async fn request_device(
        &self,
        features: wgpu::Features,
        limits: wgpu::Limits,
    ) -> Result<Arc<Context>, String> {
        let available_features = self.features();
        if !available_features.contains(features) {
            return Err(format!(
                "Unsupported requiredFeatures: {:?}",
                features - available_features
            ));
        }
        let available_limits = self.limits();
        if !limits.check_limits(&available_limits) {
            let mut failures = Vec::new();
            limits.check_limits_with_fail_fn(
                &available_limits,
                false,
                |name, requested, allowed| {
                    failures.push(format!(
                        "{name}: requested {requested}, available {allowed}"
                    ));
                },
            );
            return Err(format!(
                "Unsupported requiredLimits: {}",
                failures.join(", ")
            ));
        }
        let mut native_features =
            features | (available_features & wgpu::Features::EXTERNAL_TEXTURE);
        if limits.max_immediate_size > 0 {
            native_features |= wgpu::Features::IMMEDIATES;
        }
        if let Some(context) = &self.shared {
            let physical = context.device.limits();
            let comparable = wgpu::Limits {
                max_immediate_size: physical.max_immediate_size,
                ..limits.clone()
            };
            if context.adapter.as_ref() != self.raw.as_ref() {
                return Err("Shared GPU context belongs to a different adapter".into());
            }
            if let Some(loss) = context.loss() {
                return Err(format!("Shared GPU device is lost: {}", loss.message));
            }
            // A different feature set or limit gets its own wgpu validation
            // domain. This also gates WGSL extensions on the requesting device.
            let enabled = context.device.features();
            if enabled.contains(native_features)
                && VIRTUALIZED_FEATURES.contains(enabled - native_features)
                && comparable == physical
                && limits.max_immediate_size <= physical.max_immediate_size
            {
                return Ok(context.clone());
            }
        }
        let (device, queue) = self
            .raw
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("redjs WebGPU"),
                required_features: native_features,
                required_limits: limits,
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                ..Default::default()
            })
            .await
            .map_err(|e| e.to_string())?;
        let context = Arc::new(Context {
            parent: self.shared.clone(),
            ..Context::from_shared(self.raw.clone(), Arc::new(device), Arc::new(queue))
        });
        // This is our standalone device: there is no embedder callback to
        // replace. Shared-device hosts forward their own callback instead.
        let loss = context.loss.clone();
        context
            .device
            .set_device_lost_callback(move |reason, message| {
                let mut state = loss.lock().unwrap();
                if state.is_none() {
                    *state = Some(DeviceLoss {
                        reason: if reason == wgpu::DeviceLostReason::Destroyed {
                            "destroyed"
                        } else {
                            "unknown"
                        },
                        message,
                    });
                }
            });
        Ok(context)
    }
}

/// Runs `operation` inside this thread's own error scopes and returns the
/// first error it raised, instead of recording it on a client.
pub fn scoped<T>(device: &wgpu::Device, operation: impl FnOnce() -> T) -> (T, Option<wgpu::Error>) {
    let internal = device.push_error_scope(wgpu::ErrorFilter::Internal);
    let oom = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
    let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let result = operation();
    let error = [
        pollster::block_on(validation.pop()),
        pollster::block_on(oom.pop()),
        pollster::block_on(internal.pop()),
    ]
    .into_iter()
    .flatten()
    .next();
    (result, error)
}

/// One step of waiting for `device`'s callbacks: a pause, growing from none to 1 ms, then a
/// non-blocking poll. Never a timed `PollType::Wait`: wgpu-core panics when such a wait times
/// out and another thread's poll meanwhile retires every submission in flight.
pub(crate) fn pump(
    device: &wgpu::Device,
    pause: &mut std::time::Duration,
) -> Result<wgpu::PollStatus, wgpu::PollError> {
    std::thread::sleep(*pause);
    *pause = (*pause * 2).clamp(
        std::time::Duration::from_micros(50),
        std::time::Duration::from_millis(1),
    );
    device.poll(wgpu::PollType::Poll)
}

#[derive(Clone, Debug)]
pub struct GpuError {
    /// WebGPU filter spelling: validation / out-of-memory / internal.
    pub kind: &'static str,
    pub message: String,
}

#[derive(Default)]
struct Resources {
    buffers: Vec<Weak<wgpu::Buffer>>,
    textures: Vec<Weak<wgpu::Texture>>,
}

/// One logical JavaScript GPUDevice. Resource brands use this id even when
/// two clients share an identical physical device.
pub struct Client {
    pub context: Arc<Context>,
    pub id: u64,
    pub features: wgpu::Features,
    pub limits: wgpu::Limits,
    pub destroyed: AtomicBool,
    resources: Mutex<Resources>,
    pub(crate) video_blits:
        Mutex<std::collections::HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>>,
    errors: Mutex<Vec<GpuError>>,
}

impl Client {
    pub fn new(context: Arc<Context>) -> Arc<Self> {
        let features = context.device.features();
        let limits = context.device.limits();
        Self::new_with_capabilities(context, features, limits)
    }

    pub fn new_with_capabilities(
        context: Arc<Context>,
        features: wgpu::Features,
        limits: wgpu::Limits,
    ) -> Arc<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Arc::new(Self {
            context,
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            features,
            limits,
            destroyed: AtomicBool::new(false),
            resources: Mutex::new(Resources::default()),
            video_blits: Mutex::default(),
            errors: Mutex::new(Vec::new()),
        })
    }

    pub fn check(&self) -> Result<(), String> {
        if self.destroyed.load(Ordering::Acquire) {
            return Err("GPUDevice has been destroyed".into());
        }
        if let Some(loss) = self.context.loss() {
            return Err(format!("GPUDevice is lost: {}", loss.message));
        }
        Ok(())
    }

    pub fn track_buffer(&self, buffer: &Arc<wgpu::Buffer>) {
        let mut resources = self.resources.lock().unwrap();
        if self.destroyed.load(Ordering::Acquire) {
            drop(resources);
            buffer.destroy();
        } else {
            resources.buffers.retain(|b| b.strong_count() != 0);
            resources.buffers.push(Arc::downgrade(buffer));
        }
    }

    pub fn track_texture(&self, texture: &Arc<wgpu::Texture>) {
        let mut resources = self.resources.lock().unwrap();
        if self.destroyed.load(Ordering::Acquire) {
            drop(resources);
            texture.destroy();
        } else {
            resources.textures.retain(|t| t.strong_count() != 0);
            resources.textures.push(Arc::downgrade(texture));
        }
    }

    /// Transfers an expired canvas buffer to a host presentation owner. Its JS
    /// wrapper is already invalid; physical destruction now follows that owner.
    pub(super) fn release_presented_texture(&self, texture: &Arc<wgpu::Texture>) -> bool {
        let mut resources = self.resources.lock().unwrap();
        if self.destroyed.load(Ordering::Acquire) {
            return false;
        }
        resources.textures.retain(|weak| {
            weak.upgrade()
                .is_some_and(|raw| !Arc::ptr_eq(&raw, texture))
        });
        true
    }

    /// Explicit logical destruction. Never calls wgpu::Device::destroy.
    pub fn destroy(&self) {
        let resources = {
            let mut resources = self.resources.lock().unwrap();
            if self.destroyed.swap(true, Ordering::AcqRel) {
                return;
            }
            std::mem::take(&mut *resources)
        };
        for buffer in resources.buffers.into_iter().filter_map(|b| b.upgrade()) {
            buffer.destroy();
        }
        for texture in resources.textures.into_iter().filter_map(|t| t.upgrade()) {
            texture.destroy();
        }
    }

    pub fn record_error(&self, kind: &'static str, message: impl Into<String>) {
        self.errors.lock().unwrap().push(GpuError {
            kind,
            message: message.into(),
        });
    }

    pub fn has_errors(&self) -> bool {
        !self.errors.lock().unwrap().is_empty()
    }

    pub fn take_errors(&self) -> Vec<GpuError> {
        std::mem::take(&mut *self.errors.lock().unwrap())
    }

    /// Every native operation gets its own short-lived, thread-local scopes.
    /// It cannot steal GPUI's errors or another JS client's scope. The native
    /// wgpu-core backend's pop futures are already-ready (no GPU queue wait).
    /// Logical JS error scopes are managed by the binding above this layer.
    pub fn checked<T>(&self, operation: impl FnOnce() -> T) -> T {
        let device = &self.context.device;
        let internal = device.push_error_scope(wgpu::ErrorFilter::Internal);
        let oom = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let result = operation();
        for error in [
            pollster::block_on(validation.pop()),
            pollster::block_on(oom.pop()),
            pollster::block_on(internal.pop()),
        ]
        .into_iter()
        .flatten()
        {
            let kind = match &error {
                wgpu::Error::Validation { .. } => "validation",
                wgpu::Error::OutOfMemory { .. } => "out-of-memory",
                wgpu::Error::Internal { .. } => "internal",
            };
            self.record_error(kind, error.to_string());
        }
        result
    }
}
