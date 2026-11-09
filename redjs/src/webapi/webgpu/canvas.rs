//! GPUCanvasContext with a native host presenter and bounded readback fallback.
//!
//! JSC owns logical contexts; expired drawing buffers can move into a native
//! frame owner without retaining CPU pixels. Readback completions are fenced
//! by context/resize epochs, and no JSC value crosses onto a GPU worker.
use super::*;
use std::collections::HashSet;
use std::sync::{Condvar, Weak};
use std::time::{Duration, Instant};

/// A published canvas buffer: completed CPU pixels or an ordered native frame.
#[derive(Debug)]
pub struct PresentedFrame {
    /// Bitmap dimensions, zero for a constant-black marker.
    pub width: u32,
    pub height: u32,
    /// Tightly packed premultiplied RGBA8; empty for a constant-black marker.
    pub rgba: Vec<u8>,
    /// Paint black at `present_size(key)` without allocating a bitmap.
    pub opaque_blank: bool,
    /// Optional host-owned GPU frame. `rgba` is empty until an explicit snapshot.
    pub native: Option<Arc<NativePresentation>>,
}

/// A borrowed canvas buffer. A presenter must submit its reads before returning.
pub struct NativeSource<'a> {
    pub client: &'a Arc<Client>,
    pub texture: &'a Arc<wgpu::Texture>,
    pub format: wgpu::TextureFormat,
    pub width: u32,
    pub height: u32,
    pub opaque: bool,
}

/// Native texture format is straight-alpha BGRA8 on the renderer's own device.
/// The pointer is borrowed only for the synchronous copy callback.
///
/// # Safety
/// Implementors must keep a valid ID3D11Texture2D alive and acquired throughout
/// the callback, then release/flush it before subsequent wgpu queue work.
/// The callback may panic; cleanup must still run. No JSC callback is allowed.
pub unsafe trait NativeFrame: std::fmt::Debug + Send + Sync {
    fn copy_d3d11(&self, copy: &mut dyn FnMut(*mut c_void) -> bool) -> Result<bool, String>;

    /// Exposes a BGRA8 D3D12 resource in RENDER_TARGET state and a readiness
    /// fence signalled on the producer queue. The consumer waits on its GPU
    /// queue, restores the state after copying, and retains this frame until
    /// its own copy fence completes. No JS callback is allowed here.
    fn copy_d3d12(
        &self,
        _copy: &mut dyn FnMut(*mut c_void, *mut c_void, u64) -> bool,
    ) -> Result<bool, String> {
        Ok(false)
    }
}

pub trait NativePresenter: Send + Sync {
    /// None means unsupported; the bounded asynchronous readback is the fallback.
    fn prepare(&self, source: NativeSource<'_>) -> Result<Option<Arc<dyn NativeFrame>>, String>;
}
fn presenter_slot() -> &'static std::sync::RwLock<Option<Arc<dyn NativePresenter>>> {
    static PRESENTER: OnceLock<std::sync::RwLock<Option<Arc<dyn NativePresenter>>>> =
        OnceLock::new();
    PRESENTER.get_or_init(Default::default)
}
pub fn set_presenter(presenter: Option<Arc<dyn NativePresenter>>) {
    *presenter_slot().write().unwrap() = presenter;
}

pub struct NativePresentation {
    pub frame: Arc<dyn NativeFrame>,
    // The expired JS texture is no longer in its client's destroy list. This
    // owner retains exact original pixels for export and destroys it on drop,
    // even if JS keeps the expired texture wrapper alive indefinitely.
    source: Arc<wgpu::Texture>,
    snapshot_client: Arc<Client>,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
    opaque: bool,
}
impl std::fmt::Debug for NativePresentation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativePresentation")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}
impl Drop for NativePresentation {
    fn drop(&mut self) {
        self.source.destroy();
    }
}
impl NativePresentation {
    /// Explicit readback only; normal display visits `frame` instead.
    pub fn snapshot(&self) -> Result<Arc<PresentedFrame>, String> {
        SNAPSHOTS.fetch_add(1, Ordering::Relaxed);
        let f = readback::snapshot_sync(
            &self.snapshot_client,
            self.source.clone(),
            self.format,
            self.width,
            self.height,
            self.opaque,
        )?;
        Ok(Arc::new(PresentedFrame {
            width: f.width,
            height: f.height,
            rgba: f.rgba,
            opaque_blank: false,
            native: None,
        }))
    }
}
static NATIVE_FRAMES: AtomicU64 = AtomicU64::new(0);
static READBACK_FRAMES: AtomicU64 = AtomicU64::new(0);
static SNAPSHOTS: AtomicU64 = AtomicU64::new(0);
/// (native display frames, asynchronous display readbacks, explicit snapshots).
pub fn presentation_stats() -> (u64, u64, u64) {
    (
        NATIVE_FRAMES.load(Ordering::Relaxed),
        READBACK_FRAMES.load(Ordering::Relaxed),
        SNAPSHOTS.load(Ordering::Relaxed),
    )
}
#[derive(Clone)]
struct Configuration {
    client: Arc<Client>,
    format: wgpu::TextureFormat,
    format_name: String,
    usage: wgpu::TextureUsages,
    views: Vec<wgpu::TextureFormat>,
    view_names: Vec<String>,
    opaque: bool,
}
struct State {
    width: u32,
    height: u32,
    epoch: u64,
    serial: u64,
    config: Option<Configuration>,
    current: Option<Arc<texture::Texture>>,
    deferred: Option<(u64, Arc<texture::Texture>)>,
    reader: Option<readback::Reader>,
    pending: HashSet<u64>,
    last_serial: u64,
    last_error: Option<String>,
    generation: u64,
    last: Arc<PresentedFrame>,
    enabled: bool,
}
pub(super) struct Surface {
    key: Option<u64>,
    context: crate::context::ContextId,
    state: Mutex<State>,
    changed: Condvar,
}
struct Record {
    global: usize,
    surface: Weak<Surface>,
}
fn all() -> &'static Mutex<Vec<Record>> {
    static A: OnceLock<Mutex<Vec<Record>>> = OnceLock::new();
    A.get_or_init(Default::default)
}
fn shown() -> &'static Mutex<HashMap<u64, Weak<Surface>>> {
    static A: OnceLock<Mutex<HashMap<u64, Weak<Surface>>>> = OnceLock::new();
    A.get_or_init(Default::default)
}
static CHANGED: AtomicBool = AtomicBool::new(false);
fn generation() -> u64 {
    static N: AtomicU64 = AtomicU64::new(1);
    (1 << 63) | N.fetch_add(1, Ordering::Relaxed)
}
fn clear(opaque: bool) -> Arc<PresentedFrame> {
    Arc::new(PresentedFrame {
        width: 0,
        height: 0,
        rgba: Vec::new(),
        opaque_blank: opaque,
        native: None,
    })
}
fn notify(s: &Surface, st: &State) {
    s.changed.notify_all();
    if st.enabled && s.key.is_some() {
        CHANGED.store(true, Ordering::Release);
        crate::web::wake(s.context);
    }
}
fn discard(t: Option<Arc<texture::Texture>>) {
    if let Some(t) = t {
        t.destroyed.store(true, Ordering::Release);
        t.raw.destroy();
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        let st = self.state.get_mut().unwrap_or_else(|e| e.into_inner());
        discard(st.current.take());
        discard(st.deferred.take().map(|(_, t)| t));
        if let Some(key) = self.key {
            let mut m = shown().lock().unwrap();
            if m.get(&key)
                .is_some_and(|w| std::ptr::eq(w.as_ptr(), self as *const Self))
            {
                m.remove(&key);
                CHANGED.store(true, Ordering::Release);
            }
        }
    }
}
fn reset(s: &Surface, config: Option<Configuration>, dimensions: Option<(u32, u32)>) {
    let mut st = s.state.lock().unwrap();
    discard(st.current.take());
    discard(st.deferred.take().map(|(_, t)| t));
    st.epoch = st.epoch.wrapping_add(1);
    st.pending.clear();
    let (w, h) = dimensions.unwrap_or((st.width, st.height));
    let opaque = config.as_ref().is_some_and(|c| c.opaque) && w > 0 && h > 0;
    st.config = config;
    st.reader = None;
    st.last_error = None;
    st.last = clear(opaque);
    st.generation = generation();
    if let Some((w, h)) = dimensions {
        st.width = w;
        st.height = h;
    }
    notify(s, &st);
}

/// Published frames only. Native hosts enforce queue ordering during import.
pub fn present(key: u64, since: u64) -> Option<(u64, Arc<PresentedFrame>)> {
    let s = shown().lock().unwrap().get(&key)?.upgrade()?;
    let st = s.state.lock().unwrap();
    (st.enabled && st.generation != since).then(|| (st.generation, st.last.clone()))
}
pub fn present_size(key: u64) -> Option<(u32, u32)> {
    let s = shown().lock().unwrap().get(&key)?.upgrade()?;
    let st = s.state.lock().unwrap();
    st.enabled.then_some((st.width, st.height))
}
pub fn take_presented_changed() -> bool {
    CHANGED.swap(false, Ordering::AcqRel)
}
/// Keep the weak slot so a retained DOM canvas can be inserted again.
pub fn unregister_present(key: u64) {
    let s = shown().lock().unwrap().get(&key).and_then(Weak::upgrade);
    if let Some(s) = s {
        let mut st = s.state.lock().unwrap();
        if st.enabled {
            st.enabled = false;
            CHANGED.store(true, Ordering::Release);
        }
    }
}
pub fn register_present(key: u64) {
    let s = shown().lock().unwrap().get(&key).and_then(Weak::upgrade);
    if let Some(s) = s {
        let mut st = s.state.lock().unwrap();
        if !st.enabled {
            st.enabled = true;
            CHANGED.store(true, Ordering::Release);
        }
    }
}

pub(crate) unsafe fn create(g: *mut c_void, owner: u64, w: u32, h: u32, key: Option<u64>) -> u64 {
    let context = crate::context::id_of(g).unwrap();
    let _ = crate::web::waker(context);
    let surface = Arc::new(Surface {
        key,
        context,
        state: Mutex::new(State {
            width: w,
            height: h,
            epoch: 0,
            serial: 0,
            config: None,
            current: None,
            deferred: None,
            reader: None,
            pending: HashSet::new(),
            last_serial: 0,
            last_error: None,
            generation: generation(),
            last: clear(false),
            enabled: key.is_some(),
        }),
        changed: Condvar::new(),
    });
    all().lock().unwrap().push(Record {
        global: g as usize,
        surface: Arc::downgrade(&surface),
    });
    if let Some(k) = key {
        shown().lock().unwrap().insert(k, Arc::downgrade(&surface));
        CHANGED.store(true, Ordering::Release);
    }
    let o = Val::new(g, wrap(g, Kind::Canvas, None, Resource::Canvas(surface)));
    let slot = slots(g).0;
    glue::rjs_define(g, o.v, slot, owner, 1 | 2);
    o.v
}
fn slots(g: *mut c_void) -> (*mut c_void, *mut c_void, *mut c_void) {
    let m = states().lock().unwrap();
    let c = m[&(g as usize)].classes.as_ref().unwrap();
    (
        c.s_gpu_canvas as *mut c_void,
        c.s_gpu_device as *mut c_void,
        c.s_gpu_texture as *mut c_void,
    )
}
unsafe fn surface(g: *mut c_void, v: u64) -> Option<Arc<Surface>> {
    let cls = {
        let m = states().lock().unwrap();
        *m.get(&(g as usize))?
            .classes
            .as_ref()?
            .hosts
            .get(&Kind::Canvas)? as capi::JSClassRef
    };
    let h = host_data::<Handle>(g, cls, v)?;
    if let Resource::Canvas(s) = &h.resource {
        Some(s.clone())
    } else {
        None
    }
}
#[cfg(feature = "canvas")]
pub(crate) unsafe fn is_context(g: *mut c_void, v: u64) -> bool {
    surface(g, v).is_some()
}
unsafe fn this_surface(g: *mut c_void, cf: *mut c_void) -> Result<Arc<Surface>, ()> {
    let h = this(g, cf, Kind::Canvas)?;
    let Resource::Canvas(s) = h.resource else {
        unreachable!()
    };
    Ok(s)
}
pub(crate) unsafe fn resize(g: *mut c_void, ctx: u64, w: u32, h: u32) -> Result<(), ()> {
    let Some(s) = surface(g, ctx) else {
        return invalid(g, "Not a GPUCanvasContext");
    };
    let config = s.state.lock().unwrap().config.clone();
    reset(&s, config, Some((w, h)));
    glue::rjs_define(g, ctx, slots(g).2, UNDEF, 1 | 2);
    Ok(())
}
#[crate::host_fn]
unsafe fn get_canvas(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        this_surface(g, cf)?;
        Ok(get_a(g, cf_this(cf), slots(g).0))
    })
}
#[crate::host_fn]
unsafe fn configure(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let s = this_surface(g, cf)?;
        let d = Val::new(g, cf_arg(cf, 0));
        let device = field(g, d.v, "device")?;
        let h = handle(g, device.v, Kind::Device)?;
        let c = client(g, &h)?;
        let name = text(g, field(g, d.v, "format")?.v, "")?;
        let format = match name.as_str() {
            "rgba8unorm" => wgpu::TextureFormat::Rgba8Unorm,
            "bgra8unorm" => wgpu::TextureFormat::Bgra8Unorm,
            _ => {
                return Ok(super::super::domexception::throw(
                    g,
                    "NotSupportedError",
                    "Canvas presentation currently supports rgba8unorm and bgra8unorm",
                ))
            }
        };
        let bits = uint(g, field(g, d.v, "usage")?.v, 16)?;
        let Some(usage) = wgpu::TextureUsages::from_bits(bits) else {
            invalid(g, "Invalid GPUCanvasConfiguration usage flags")?;
            unreachable!()
        };
        if usage.is_empty() {
            invalid(g, "Canvas texture usage cannot be empty")?;
        }
        if !c
            .context
            .adapter
            .get_texture_format_features(format)
            .allowed_usages
            .contains(usage)
        {
            invalid(g, "Canvas format does not support requested usage")?;
        }
        if format == wgpu::TextureFormat::Bgra8Unorm
            && usage.contains(wgpu::TextureUsages::STORAGE_BINDING)
            && !c.features.contains(wgpu::Features::BGRA8UNORM_STORAGE)
        {
            invalid(g, "bgra8unorm-storage was not enabled")?;
        }
        let mut views = Vec::new();
        let mut names = Vec::new();
        for v in seq(g, field(g, d.v, "viewFormats")?.v)? {
            let name = text(g, v.v, "")?;
            let f = texture::parse_format(g, v.v)?;
            if f.remove_srgb_suffix() != format {
                invalid(
                    g,
                    "Canvas view format is incompatible with its texture format",
                )?;
            }
            views.push(f);
            names.push(name);
        }
        if text(g, field(g, d.v, "colorSpace")?.v, "srgb")? != "srgb" {
            return Ok(super::super::domexception::throw(
                g,
                "NotSupportedError",
                "Only sRGB canvas presentation is supported",
            ));
        }
        let tone = field(g, d.v, "toneMapping")?;
        if text(g, field(g, tone.v, "mode")?.v, "standard")? != "standard" {
            return Ok(super::super::domexception::throw(
                g,
                "NotSupportedError",
                "Extended tone mapping is not supported",
            ));
        }
        let alpha = text(g, field(g, d.v, "alphaMode")?.v, "opaque")?;
        if !["opaque", "premultiplied"].contains(&alpha.as_str()) {
            invalid(g, "Invalid GPUCanvasAlphaMode")?;
        }
        if let Err(e) = c.check() {
            return Ok(super::super::domexception::throw(
                g,
                "InvalidStateError",
                &e,
            ));
        }
        reset(
            &s,
            Some(Configuration {
                client: c,
                format,
                format_name: name,
                usage,
                views,
                view_names: names,
                opaque: alpha == "opaque",
            }),
            None,
        );
        glue::rjs_define(g, cf_this(cf), slots(g).1, device.v, 1 | 2);
        glue::rjs_define(g, cf_this(cf), slots(g).2, UNDEF, 1 | 2);
        Ok(UNDEF)
    })
}
#[crate::host_fn]
unsafe fn unconfigure(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let s = this_surface(g, cf)?;
        reset(&s, None, None);
        glue::rjs_define(g, cf_this(cf), slots(g).1, UNDEF, 1 | 2);
        glue::rjs_define(g, cf_this(cf), slots(g).2, UNDEF, 1 | 2);
        Ok(UNDEF)
    })
}
#[crate::host_fn]
unsafe fn configuration(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let s = this_surface(g, cf)?;
        let config = s.state.lock().unwrap().config.clone();
        let Some(c) = config else { return Ok(NULL) };
        let o = Val::new(g, new_object(g, UNDEF));
        define(g, o.v, "device", get_a(g, cf_this(cf), slots(g).1), 0);
        define(g, o.v, "format", js_string(g, &c.format_name), 0);
        define(g, o.v, "usage", enc_f64(c.usage.bits() as f64), 0);
        define(
            g,
            o.v,
            "alphaMode",
            js_string(g, if c.opaque { "opaque" } else { "premultiplied" }),
            0,
        );
        define(g, o.v, "colorSpace", js_string(g, "srgb"), 0);
        let a = Val::new(g, new_array(g));
        for (i, n) in c.view_names.iter().enumerate() {
            let v = js_string(g, n);
            set_i(g, a.v, i as u32, v);
        }
        define(g, o.v, "viewFormats", a.v, 0);
        let tone = Val::new(g, new_object(g, UNDEF));
        define(g, tone.v, "mode", js_string(g, "standard"), 0);
        define(g, o.v, "toneMapping", tone.v, 0);
        Ok(o.v)
    })
}
#[crate::host_fn]
unsafe fn current_texture(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let s = this_surface(g, cf)?;
        let (c, w, h, existing, epoch) = {
            let st = s.state.lock().unwrap();
            let Some(c) = st.config.clone() else {
                return Ok(super::super::domexception::throw(
                    g,
                    "InvalidStateError",
                    "GPUCanvasContext is not configured",
                ));
            };
            (c, st.width, st.height, st.current.clone(), st.epoch)
        };
        if let Err(e) = c.client.check() {
            return Ok(super::super::domexception::throw(
                g,
                "InvalidStateError",
                &e,
            ));
        }
        if existing.is_some() {
            return Ok(get_a(g, cf_this(cf), slots(g).2));
        }
        if w == 0
            || h == 0
            || w > c.client.limits.max_texture_dimension_2d
            || h > c.client.limits.max_texture_dimension_2d
        {
            return Ok(super::super::domexception::throw(
                g,
                "OperationError",
                "Canvas dimensions are zero or exceed the device limit",
            ));
        }
        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        let raw = Arc::new(checked(g, &c.client, || {
            c.client
                .context
                .device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("GPUCanvasContext drawing buffer"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: c.format,
                    usage: c.usage
                        | wgpu::TextureUsages::COPY_SRC
                        | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &c.views,
                })
        }));
        c.client.track_texture(&raw);
        let t = Arc::new(texture::Texture {
            raw,
            size,
            usage: c.usage,
            destroyed: AtomicBool::new(false),
            mip_levels: 1,
            samples: 1,
            dimension: "2d".into(),
            format_name: c.format_name.clone(),
            canvas: true,
        });
        {
            let mut st = s.state.lock().unwrap();
            if st.epoch != epoch {
                discard(Some(t));
                return Ok(super::super::domexception::throw(
                    g,
                    "InvalidStateError",
                    "Canvas was reconfigured",
                ));
            }
            discard(st.deferred.take().map(|(_, t)| t));
            st.serial = st.serial.wrapping_add(1);
            st.current = Some(t.clone());
        }
        let o = Val::new(
            g,
            wrap(g, Kind::Texture, Some(c.client), Resource::Texture(t)),
        );
        glue::rjs_define(g, o.v, slots(g).0, cf_this(cf), 1 | 2);
        glue::rjs_define(g, cf_this(cf), slots(g).2, o.v, 1 | 2);
        Ok(o.v)
    })
}

fn blank(w: u32, h: u32, opaque: bool) -> Result<Arc<PresentedFrame>, String> {
    let n = (w as usize)
        .checked_mul(h as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or("Canvas size overflow")?;
    if n > 256 * 1024 * 1024 {
        return Err("Canvas snapshot exceeds the 256 MiB limit".into());
    }
    let mut rgba = vec![0; n];
    if opaque {
        for p in rgba.chunks_exact_mut(4) {
            p[3] = 255;
        }
    }
    Ok(Arc::new(PresentedFrame {
        width: w,
        height: h,
        rgba,
        opaque_blank: false,
        native: None,
    }))
}
fn snapshot(s: &Surface) -> Result<Arc<PresentedFrame>, String> {
    let mut st = s.state.lock().unwrap();
    let c = st.config.clone();
    let native = st
        .current
        .clone()
        .or_else(|| st.deferred.as_ref().map(|(_, t)| t.clone()));
    if let (Some(c), Some(t)) = (c.clone(), native) {
        if c.client.check().is_ok() {
            let (w, h) = (st.width, st.height);
            drop(st);
            SNAPSHOTS.fetch_add(1, Ordering::Relaxed);
            let f = readback::snapshot_sync(&c.client, t.raw.clone(), c.format, w, h, c.opaque)?;
            return Ok(Arc::new(PresentedFrame {
                width: f.width,
                height: f.height,
                rgba: f.rgba,
                opaque_blank: false,
                native: None,
            }));
        }
    }
    let valid = c.as_ref().is_some_and(|c| c.client.check().is_ok());
    let deadline = Instant::now() + Duration::from_secs(30);
    while valid && !st.pending.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("Canvas snapshot timed out".into());
        }
        let (next, timeout) = s.changed.wait_timeout(st, remaining).unwrap();
        st = next;
        if timeout.timed_out() && !st.pending.is_empty() {
            return Err("Canvas snapshot timed out".into());
        }
    }
    if st.last.width > 0 && st.last.height > 0 {
        let last = st.last.clone();
        drop(st);
        return if let Some(native) = &last.native {
            native.snapshot()
        } else {
            Ok(last)
        };
    }
    if valid {
        if let Some(e) = &st.last_error {
            return Err(e.clone());
        }
    }
    blank(st.width, st.height, c.as_ref().is_some_and(|c| c.opaque))
}
fn straight(frame: &PresentedFrame) -> Vec<u8> {
    let mut rgba = frame.rgba.clone();
    for p in rgba.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a == 0 {
            p[..3].fill(0);
        } else if a != 255 {
            for c in &mut p[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    rgba
}
pub(crate) unsafe fn pixels(g: *mut c_void, v: u64) -> Option<Result<(Vec<u8>, u32, u32), String>> {
    let s = surface(g, v)?;
    Some(snapshot(&s).map(|f| (straight(&f), f.width, f.height)))
}
/// Used by native 2D draws without round-tripping premultiplied pixels.
#[cfg(feature = "canvas")]
pub(crate) unsafe fn premul_pixels(
    g: *mut c_void,
    v: u64,
) -> Option<Result<(Vec<u8>, u32, u32), String>> {
    let s = surface(g, v)?;
    Some(snapshot(&s).map(|f| (f.rgba.clone(), f.width, f.height)))
}
fn capture(s: &Arc<Surface>) {
    let (t, serial, epoch, c, w, h) = {
        let mut st = s.state.lock().unwrap();
        if !st.enabled {
            return;
        }
        let Some(c) = st.config.clone() else {
            return;
        };
        let active = st
            .current
            .take()
            .map(|t| (st.serial, t))
            .or_else(|| st.deferred.take());
        let Some((serial, t)) = active else {
            return;
        };
        t.destroyed.store(true, Ordering::Release);
        if c.client.check().is_err() {
            discard(Some(t));
            return;
        }
        (t, serial, st.epoch, c, st.width, st.height)
    };
    if s.key.is_some() {
        let presenter = presenter_slot().read().unwrap().clone();
        if let Some(presenter) = presenter {
            let result = c.client.checked(|| {
                presenter.prepare(NativeSource {
                    client: &c.client,
                    texture: &t.raw,
                    format: c.format,
                    width: w,
                    height: h,
                    opaque: c.opaque,
                })
            });
            match result {
                Ok(Some(frame)) if c.client.release_presented_texture(&t.raw) => {
                    let native = Arc::new(NativePresentation {
                        frame,
                        source: t.raw.clone(),
                        snapshot_client: Client::new(c.client.context.clone()),
                        format: c.format,
                        width: w,
                        height: h,
                        opaque: c.opaque,
                    });
                    let mut st = s.state.lock().unwrap();
                    if st.epoch == epoch && serial >= st.last_serial {
                        // Older fallback readbacks cannot improve this native frame.
                        // Drop their lease and do not make exports wait for them.
                        st.reader = None;
                        st.pending.clear();
                        st.last_serial = serial;
                        st.last_error = None;
                        st.last = Arc::new(PresentedFrame {
                            width: w,
                            height: h,
                            rgba: Vec::new(),
                            opaque_blank: false,
                            native: Some(native),
                        });
                        st.generation = generation();
                        NATIVE_FRAMES.fetch_add(1, Ordering::Relaxed);
                        notify(s, &st);
                    }
                    return;
                }
                Err(e) => {
                    static WARNED: AtomicBool = AtomicBool::new(false);
                    if !WARNED.swap(true, Ordering::Relaxed) {
                        eprintln!(
                            "[webgpu] Native presenter failed; using asynchronous readback: {e}"
                        );
                    }
                }
                _ => {}
            }
        }
    }
    let reader = {
        let mut st = s.state.lock().unwrap();
        if st.epoch != epoch {
            discard(Some(t));
            return;
        }
        st.pending.insert(serial);
        st.reader
            .get_or_insert_with(|| readback::Reader::new(&c.client))
            .clone()
    };
    let weak = Arc::downgrade(s);
    let result = reader.request(
        &c.client,
        t.raw.clone(),
        c.format,
        w,
        h,
        c.opaque,
        Box::new(move |result| {
            if let Some(s) = weak.upgrade() {
                let mut st = s.state.lock().unwrap();
                if st.epoch != epoch {
                    return;
                }
                st.pending.remove(&serial);
                match result {
                    Ok(f) if serial >= st.last_serial => {
                        st.last_serial = serial;
                        st.last_error = None;
                        st.last = Arc::new(PresentedFrame {
                            width: f.width,
                            height: f.height,
                            rgba: f.rgba,
                            opaque_blank: false,
                            native: None,
                        });
                        st.generation = generation();
                    }
                    Err(e) if serial >= st.last_serial => st.last_error = Some(e),
                    _ => {}
                }
                notify(&s, &st);
            }
        }),
    );
    match result {
        Ok(true) => {
            if s.key.is_some() {
                READBACK_FRAMES.fetch_add(1, Ordering::Relaxed);
            }
            t.raw.destroy();
        }
        Ok(false) => {
            let mut st = s.state.lock().unwrap();
            if st.epoch == epoch {
                st.pending.remove(&serial);
                st.deferred = Some((serial, t));
                s.changed.notify_all();
                drop(st);
                reader.wake_when_available(s.context);
            } else {
                discard(Some(t));
            }
        }
        Err(e) => {
            let mut st = s.state.lock().unwrap();
            if st.epoch == epoch {
                st.pending.remove(&serial);
                st.last_error = Some(e);
                notify(s, &st);
            }
            discard(Some(t));
        }
    }
}
/// Present only at the end of the host pump, after the whole rAF batch and its
/// microtasks. No canvas texture expires between two callbacks in one frame.
pub(crate) unsafe fn pump(g: *mut c_void) {
    if glue::rjs_in_js(g) != 0 {
        return;
    }
    let surfaces: Vec<_> = {
        let mut a = all().lock().unwrap();
        a.retain(|r| r.surface.strong_count() > 0);
        a.iter()
            .filter(|r| r.global == g as usize)
            .filter_map(|r| r.surface.upgrade())
            .collect()
    };
    for s in surfaces {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| capture(&s)));
    }
}
// Staging buffers belong to the shared native reader, not to a JS device.
// Release this canvas's reader lease promptly when its logical client is lost.
pub(super) fn device_destroyed(id: u64) {
    let surfaces: Vec<_> = all()
        .lock()
        .unwrap()
        .iter()
        .filter_map(|r| r.surface.upgrade())
        .collect();
    for s in surfaces {
        let mut st = s.state.lock().unwrap();
        if st.config.as_ref().is_some_and(|c| c.client.id == id) {
            discard(st.current.take());
            discard(st.deferred.take().map(|(_, t)| t));
            st.reader = None;
            st.epoch = st.epoch.wrapping_add(1);
            st.pending.clear();
            st.last_error = None;
            if st.last.native.is_some() {
                st.last = clear(st.config.as_ref().is_some_and(|c| c.opaque));
                st.generation = generation();
                notify(&s, &st);
            }
            s.changed.notify_all();
        }
    }
}

pub(crate) fn forget(gone: crate::context::Gone) {
    let surfaces: Vec<_> = {
        let mut a = all().lock().unwrap();
        let surfaces = a
            .iter()
            .filter(|r| r.global == gone.key)
            .filter_map(|r| r.surface.upgrade())
            .collect();
        a.retain(|r| r.global != gone.key);
        surfaces
    };
    for s in surfaces {
        reset(&s, None, None);
        if let Some(key) = s.key {
            unregister_present(key);
        }
    }
}

#[crate::host_fn]
unsafe fn h_context(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let owner = cf_arg(cf, 0);
        if !is_cell(owner) {
            invalid(g, "Canvas owner must be an object")?;
        }
        let w = uint(g, cf_arg(cf, 1), 0)?;
        let h = uint(g, cf_arg(cf, 2), 0)?;
        let key = num(g, cf_arg(cf, 3), 0)?;
        Ok(create(g, owner, w, h, (key != 0).then_some(key)))
    })
}
#[crate::host_fn]
unsafe fn h_resize(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let w = uint(g, cf_arg(cf, 1), 0)?;
        let h = uint(g, cf_arg(cf, 2), 0)?;
        resize(g, cf_arg(cf, 0), w, h)?;
        Ok(UNDEF)
    })
}
#[cfg(feature = "canvas")]
unsafe fn capture_args(
    g: *mut c_void,
    cf: *mut c_void,
) -> Result<(Arc<PresentedFrame>, String, Option<f64>), ()> {
    let Some(s) = surface(g, cf_arg(cf, 0)) else {
        invalid(g, "Not a GPUCanvasContext")?;
        unreachable!()
    };
    let ty = text(g, cf_arg(cf, 1), "image/png")?;
    let q = if cf_arg(cf, 2) == UNDEF {
        None
    } else {
        Some(to_number(g, cf_arg(cf, 2))?)
    };
    let f = snapshot(&s).map_err(|e| {
        super::super::domexception::throw(g, "OperationError", &e);
    })?;
    Ok((f, ty, q))
}
#[crate::host_fn]
unsafe fn h_data_url(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        #[cfg(feature = "canvas")]
        {
            let (f, ty, q) = capture_args(g, cf)?;
            let rgba = straight(&f);
            return Ok(js_string(
                g,
                &super::super::canvas2d::encode::data_url(&rgba, f.width, f.height, &ty, q),
            ));
        }
        #[cfg(not(feature = "canvas"))]
        {
            let _ = cf;
            Ok(super::super::domexception::throw(
                g,
                "NotSupportedError",
                "Image encoding requires the canvas feature",
            ))
        }
    })
}
#[crate::host_fn]
unsafe fn h_blob(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        #[cfg(feature = "canvas")]
        {
            let (f, ty, q) = capture_args(g, cf)?;
            if f.width == 0 || f.height == 0 {
                return Ok(NULL);
            }
            let (mime, data) =
                super::super::canvas2d::encode::encode(&straight(&f), f.width, f.height, &ty, q);
            return Ok(super::super::blob::create(
                g,
                super::super::util::state(g).blob.proto,
                data,
                mime,
            ));
        }
        #[cfg(not(feature = "canvas"))]
        {
            let _ = cf;
            Ok(super::super::domexception::throw(
                g,
                "NotSupportedError",
                "Image encoding requires the canvas feature",
            ))
        }
    })
}
#[cfg(feature = "canvas")]
pub(crate) unsafe fn convert_to_blob(g: *mut c_void, ctx: u64, options: u64) -> u64 {
    run(g, || {
        let Some(s) = surface(g, ctx) else {
            invalid(g, "Not a GPUCanvasContext")?;
            unreachable!()
        };
        let o = Val::new(g, options);
        let ty = text(g, field(g, o.v, "type")?.v, "image/png")?;
        let v = field(g, o.v, "quality")?;
        let q = if v.v == UNDEF {
            None
        } else {
            Some(to_number(g, v.v)?)
        };
        let (w, h) = {
            let st = s.state.lock().unwrap();
            (st.width, st.height)
        };
        if w == 0 || h == 0 {
            return Ok(super::super::domexception::rejected(
                g,
                "IndexSizeError",
                "Canvas has no pixels",
            ));
        }
        let f = snapshot(&s).map_err(|e| {
            super::super::domexception::throw(g, "OperationError", &e);
        })?;
        Ok(async_op(
            g,
            vec![ctx],
            async move {
                Ok(super::super::canvas2d::encode::encode(
                    &straight(&f),
                    f.width,
                    f.height,
                    &ty,
                    q,
                ))
            },
            |g, (mime, bytes)| {
                super::super::blob::create(g, super::super::util::state(g).blob.proto, bytes, mime)
            },
        ))
    })
}
#[cfg(feature = "canvas")]
pub(crate) unsafe fn transfer(g: *mut c_void, ctx: u64) -> u64 {
    run(g, || {
        let Some(s) = surface(g, ctx) else {
            invalid(g, "Not a GPUCanvasContext")?;
            unreachable!()
        };
        let f = snapshot(&s).map_err(|e| {
            super::super::domexception::throw(g, "OperationError", &e);
        })?;
        let config = s.state.lock().unwrap().config.clone();
        reset(&s, config, None);
        glue::rjs_define(g, ctx, slots(g).2, UNDEF, 1 | 2);
        Ok(super::super::image::create_bitmap(
            g,
            super::super::image::BitmapData::new(f.width, f.height, straight(&f)),
        ))
    })
}
pub(super) unsafe fn install(g: *mut c_void) {
    class(
        g,
        Kind::Canvas,
        "GPUCanvasContext",
        &[
            ("configure", 1, configure),
            ("unconfigure", 0, unconfigure),
            ("getConfiguration", 0, configuration),
            ("getCurrentTexture", 0, current_texture),
        ],
        &[("canvas", get_canvas)],
    );
    let global = glue::rjs_global(g);
    for (name, arity, f) in [
        ("__rjs_webgpu_context", 4, h_context as HostFn),
        ("__rjs_webgpu_resize", 3, h_resize),
        ("__rjs_webgpu_data_url", 3, h_data_url),
        ("__rjs_webgpu_blob", 3, h_blob),
    ] {
        define(g, global, name, make_fn(g, name, arity, f), 2);
    }
}
