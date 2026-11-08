//! Bounded GPU -> CPU presentation readback. No JSC values or callbacks.
//!
//! Copy commands are submitted on the caller thread, before a canvas can expire
//! its texture. One worker per physical Context maps at most three staging
//! leases; consumers keep their previous frame when all leases are busy.

use super::backend::{Client, Context};
use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{mpsc, Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

const SLOTS: usize = 3;
const TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct Frame {
    pub width: u32,
    pub height: u32,
    /// Premultiplied RGBA8, or RGB with alpha forced to 255 in opaque mode.
    pub rgba: Vec<u8>,
}

type Completion = Box<dyn FnOnce(Result<Frame, String>) + Send>;

#[derive(Clone)]
pub(super) struct Reader(Arc<Service>);
struct Service {
    context: Weak<Context>,
    sender: OnceLock<Result<mpsc::Sender<Job>, String>>,
    pool: Arc<Mutex<Pool>>,
}
#[derive(Default)]
struct Slot {
    buffer: Option<Arc<wgpu::Buffer>>,
    bytes: u64,
    busy: bool,
}
struct Pool {
    slots: [Slot; SLOTS],
    waiters: HashSet<crate::context::ContextId>,
}
impl Default for Pool {
    fn default() -> Self {
        Self {
            slots: std::array::from_fn(|_| Slot::default()),
            waiters: HashSet::new(),
        }
    }
}

/// A reservation exists before allocation, so all failure paths release the
/// slot. Only this lease may access its staging buffer until it is dropped.
struct Lease {
    pool: Arc<Mutex<Pool>>,
    index: usize,
    buffer: Option<Arc<wgpu::Buffer>>,
    reusable: bool,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let waiters = {
            let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
            let slot = &mut pool.slots[self.index];
            if !self.reusable {
                slot.buffer = None;
                slot.bytes = 0;
            }
            slot.busy = false;
            std::mem::take(&mut pool.waiters)
        };
        // Wakers can acquire other runtime locks. Never call them while the
        // staging pool is locked or a deferred surface is being registered.
        for id in waiters {
            crate::web::wake(id);
        }
    }
}
impl Lease {
    fn reserve(pool: &Arc<Mutex<Pool>>, bytes: u64) -> Option<Self> {
        let mut state = pool.lock().unwrap_or_else(|e| e.into_inner());
        let index = state.slots.iter().position(|s| !s.busy)?;
        let slot = &mut state.slots[index];
        slot.busy = true;
        let buffer = (slot.bytes == bytes).then(|| slot.buffer.clone()).flatten();
        Some(Self {
            pool: pool.clone(),
            index,
            buffer,
            reusable: true,
        })
    }

    fn allocate(&mut self, client: &Arc<Client>, bytes: u64) -> Result<Arc<wgpu::Buffer>, String> {
        if let Some(buffer) = &self.buffer {
            return Ok(buffer.clone());
        }
        let buffer = staging(client, bytes)?;
        {
            let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
            let slot = &mut pool.slots[self.index];
            slot.buffer = Some(buffer.clone());
            slot.bytes = bytes;
        }
        self.buffer = Some(buffer.clone());
        Ok(buffer)
    }
}

#[derive(Clone, Copy)]
struct Layout {
    width: u32,
    height: u32,
    stride: u32,
    bytes: u64,
    bgra: bool,
    opaque: bool,
}
impl Layout {
    fn new(
        client: &Client,
        texture: &wgpu::Texture,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        opaque: bool,
    ) -> Result<Self, String> {
        client.check()?;
        let bgra = match format {
            wgpu::TextureFormat::Bgra8Unorm => true,
            wgpu::TextureFormat::Rgba8Unorm => false,
            _ => return Err("Canvas readback supports rgba8unorm and bgra8unorm".into()),
        };
        if width == 0 || height == 0 {
            return Err("Canvas readback dimensions must be nonzero".into());
        }
        if width > texture.width()
            || height > texture.height()
            || texture.dimension() != wgpu::TextureDimension::D2
            || texture.sample_count() != 1
            || texture.format() != format
            || !texture.usage().contains(wgpu::TextureUsages::COPY_SRC)
        {
            return Err("Canvas readback texture dimensions, format or usage are invalid".into());
        }
        let stride = width
            .checked_mul(4)
            .and_then(|n| n.checked_add(255))
            .map(|n| n & !255)
            .ok_or("Canvas readback row size overflow")?;
        let bytes = u64::from(stride)
            .checked_mul(u64::from(height))
            .ok_or("Canvas readback buffer size overflow")?;
        if bytes > client.limits.max_buffer_size
            || bytes > client.context.device.limits().max_buffer_size
            || bytes > usize::MAX as u64
        {
            return Err("Canvas readback exceeds maxBufferSize".into());
        }
        Ok(Self {
            width,
            height,
            stride,
            bytes,
            bgra,
            opaque,
        })
    }
}

fn services() -> &'static Mutex<HashMap<usize, Weak<Service>>> {
    static SERVICES: OnceLock<Mutex<HashMap<usize, Weak<Service>>>> = OnceLock::new();
    SERVICES.get_or_init(Default::default)
}

impl Reader {
    pub(super) fn new(client: &Arc<Client>) -> Self {
        let key = Arc::as_ptr(&client.context) as usize;
        let mut registry = services().lock().unwrap_or_else(|e| e.into_inner());
        registry.retain(|_, service| service.strong_count() != 0);
        if let Some(service) = registry.get(&key).and_then(Weak::upgrade) {
            if service
                .context
                .upgrade()
                .is_some_and(|c| Arc::ptr_eq(&c, &client.context))
            {
                return Self(service);
            }
        }
        // Merely configuring a canvas allocates no thread or staging buffer.
        // The first accepted frame starts the worker through this OnceLock.
        let service = Arc::new(Service {
            context: Arc::downgrade(&client.context),
            sender: OnceLock::new(),
            pool: Arc::new(Mutex::new(Pool::default())),
        });
        registry.insert(key, Arc::downgrade(&service));
        Self(service)
    }

    /// Arm a retry after backpressure. Checking capacity and registering the
    /// waiter under the same lock closes the race with a just-released lease.
    pub(super) fn wake_when_available(&self, id: crate::context::ContextId) {
        let ready = {
            let mut pool = self.0.pool.lock().unwrap_or_else(|e| e.into_inner());
            if pool.slots.iter().any(|slot| !slot.busy) {
                true
            } else {
                pool.waiters.insert(id);
                false
            }
        };
        if ready {
            crate::web::wake(id);
        }
    }

    /// Returns false only for backpressure; the completion is not called then.
    /// An accepted request calls its completion exactly once, on the worker.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn request(
        &self,
        client: &Arc<Client>,
        texture: Arc<wgpu::Texture>,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        opaque: bool,
        done: Completion,
    ) -> Result<bool, String> {
        let service = self.0.clone();
        catch_unwind(AssertUnwindSafe(|| {
            if !service
                .context
                .upgrade()
                .is_some_and(|c| Arc::ptr_eq(&c, &client.context))
            {
                return Err("Canvas readback reader belongs to another GPU context".into());
            }
            let layout = Layout::new(client, &texture, format, width, height, opaque)?;
            let sender = service
                .sender
                .get_or_init(|| {
                    let (sender, receiver) = mpsc::channel();
                    std::thread::Builder::new()
                        .name("redjs-gpu-readback".into())
                        .spawn(move || worker(receiver))
                        .map(|_| sender)
                        .map_err(|error| format!("Cannot start canvas readback worker: {error}"))
                })
                .as_ref()
                .map_err(Clone::clone)?;

            let Some(mut lease) = Lease::reserve(&service.pool, layout.bytes) else {
                return Ok(false);
            };
            let buffer = lease.allocate(client, layout.bytes)?;
            let submission = submit_copy(client, &texture, &buffer, layout)?;
            let job = Job {
                client: client.clone(),
                _texture: texture,
                _service: service.clone(),
                lease,
                layout,
                submission,
                done: Some(done),
            };
            // Pool reservations bound the channel to at most three jobs even
            // though mpsc itself has no capacity (and can never block JS).
            sender
                .send(job)
                .map_err(|_| "Canvas readback worker stopped".to_string())?;
            Ok(true)
        }))
        .unwrap_or_else(|_| Err("Native canvas readback preparation failed".into()))
    }
}

struct Job {
    client: Arc<Client>,
    // Preserve the source's outer handle through the in-flight copy as well.
    _texture: Arc<wgpu::Texture>,
    // Keep this Context's service discoverable while a job is in flight, even
    // if a canvas is reconfigured. The receiver owns no Service while idle;
    // after its final finite job drops, the last sender closes and it exits.
    _service: Arc<Service>,
    lease: Lease,
    layout: Layout,
    submission: wgpu::SubmissionIndex,
    done: Option<Completion>,
}
fn worker(receiver: mpsc::Receiver<Job>) {
    for mut job in receiver {
        let result = catch_unwind(AssertUnwindSafe(|| {
            map_frame(
                &job.client,
                job.lease.buffer.as_ref().unwrap(),
                job.layout,
                job.submission.clone(),
            )
        }))
        .unwrap_or_else(|_| Err("Native canvas readback failed".into()));
        if result.is_err() {
            job.lease.reusable = false;
        }
        let done = job.done.take();
        drop(job); // Unmap/release the lease before a completion can request again.
        if let Some(done) = done {
            let _ = catch_unwind(AssertUnwindSafe(|| done(result)));
        }
    }
}

/// Use our own nested scopes, inside Client.checked, to return only readback
/// errors. Draining Client.take_errors would steal errors from other JS calls.
fn gpu<T>(client: &Arc<Client>, operation: impl FnOnce() -> T) -> Result<T, String> {
    client.checked(|| {
        let device = &client.context.device;
        let internal = device.push_error_scope(wgpu::ErrorFilter::Internal);
        let oom = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let value = operation();
        let errors: Vec<_> = [
            pollster::block_on(validation.pop()),
            pollster::block_on(oom.pop()),
            pollster::block_on(internal.pop()),
        ]
        .into_iter()
        .flatten()
        .map(|e| e.to_string())
        .collect();
        if errors.is_empty() {
            Ok(value)
        } else {
            Err(errors.join("; "))
        }
    })
}

fn staging(client: &Arc<Client>, bytes: u64) -> Result<Arc<wgpu::Buffer>, String> {
    gpu(client, || {
        Arc::new(
            client
                .context
                .device
                .create_buffer(&wgpu::BufferDescriptor {
                    label: Some("redjs canvas readback"),
                    size: bytes,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                }),
        )
    })
}
fn submit_copy(
    client: &Arc<Client>,
    texture: &wgpu::Texture,
    buffer: &wgpu::Buffer,
    layout: Layout,
) -> Result<wgpu::SubmissionIndex, String> {
    client.check()?;
    gpu(client, || {
        let mut encoder =
            client
                .context
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("redjs canvas presentation copy"),
                });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(layout.stride),
                    rows_per_image: Some(layout.height),
                },
            },
            wgpu::Extent3d {
                width: layout.width,
                height: layout.height,
                depth_or_array_layers: 1,
            },
        );
        client.context.queue.submit([encoder.finish()])
    })
}

struct Unmap {
    buffer: Arc<wgpu::Buffer>,
    client: Arc<Client>,
    armed: bool,
}
impl Unmap {
    fn finish(&mut self) -> Result<(), String> {
        gpu(&self.client, || self.buffer.unmap())?;
        self.armed = false;
        Ok(())
    }
}
impl Drop for Unmap {
    fn drop(&mut self) {
        if self.armed {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                gpu(&self.client, || self.buffer.unmap())
            }));
        }
    }
}
fn map_frame(
    client: &Arc<Client>,
    buffer: &Arc<wgpu::Buffer>,
    layout: Layout,
    submission: wgpu::SubmissionIndex,
) -> Result<Frame, String> {
    client.check()?;
    let mut unmap = Unmap {
        buffer: buffer.clone(),
        client: client.clone(),
        armed: true,
    };
    let (sender, receiver) = mpsc::channel();
    gpu(client, || {
        buffer.map_async(wgpu::MapMode::Read, 0..layout.bytes, move |result| {
            let _ = sender.send(result.map_err(|e| e.to_string()));
        })
    })?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        client.check()?;
        match receiver.try_recv() {
            Ok(result) => {
                result?;
                break;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err("Canvas readback map callback was lost".into())
            }
            Err(mpsc::TryRecvError::Empty) => (),
        }
        if Instant::now() >= deadline {
            return Err("Canvas readback timed out".into());
        }
        match client.context.device.poll(wgpu::PollType::Wait {
            submission_index: Some(submission.clone()),
            timeout: Some(Duration::from_millis(10)),
        }) {
            Ok(_) | Err(wgpu::PollError::Timeout) => (),
            Err(error) => return Err(error.to_string()),
        }
    }
    let view = buffer
        .get_mapped_range(0..layout.bytes)
        .map_err(|e| e.to_string())?;
    let rgba = tight_rgba(&view, layout)?;
    drop(view); // Never unmap while a native mapped view still exists.
    unmap.finish()?;
    Ok(Frame {
        width: layout.width,
        height: layout.height,
        rgba,
    })
}

fn tight_rgba(mapped: &[u8], layout: Layout) -> Result<Vec<u8>, String> {
    let row = (layout.width as usize)
        .checked_mul(4)
        .ok_or("Canvas row overflow")?;
    let size = row
        .checked_mul(layout.height as usize)
        .ok_or("Canvas pixel size overflow")?;
    if mapped.len() < layout.bytes as usize {
        return Err("Canvas readback mapping is too short".into());
    }
    let mut rgba = Vec::with_capacity(size);
    for y in 0..layout.height as usize {
        let start = y * layout.stride as usize;
        rgba.extend_from_slice(&mapped[start..start + row]);
    }
    if layout.bgra || layout.opaque {
        for pixel in rgba.chunks_exact_mut(4) {
            if layout.bgra {
                pixel.swap(0, 2);
            }
            if layout.opaque {
                pixel[3] = 255;
            }
        }
    }
    Ok(rgba)
}

/// Explicit synchronous exports may wait for GPU completion; ordinary frame
/// presentation must use Reader and never block the JS thread on a map.
#[allow(clippy::too_many_arguments)]
pub(super) fn snapshot_sync(
    client: &Arc<Client>,
    texture: Arc<wgpu::Texture>,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
    opaque: bool,
) -> Result<Frame, String> {
    catch_unwind(AssertUnwindSafe(|| {
        let layout = Layout::new(client, &texture, format, width, height, opaque)?;
        let buffer = staging(client, layout.bytes)?;
        let submission = submit_copy(client, &texture, &buffer, layout)?;
        map_frame(client, &buffer, layout, submission)
    }))
    .unwrap_or_else(|_| Err("Native canvas snapshot failed".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn padded_bgra_rows_become_tight_rgba_without_losing_premultiplication() {
        let layout = Layout {
            width: 2,
            height: 2,
            stride: 256,
            bytes: 512,
            bgra: true,
            opaque: false,
        };
        let mut bytes = vec![123; 512];
        bytes[..8].copy_from_slice(&[30, 20, 10, 40, 90, 80, 70, 255]);
        bytes[256..264].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            tight_rgba(&bytes, layout).unwrap(),
            [10, 20, 30, 40, 70, 80, 90, 255, 3, 2, 1, 4, 7, 6, 5, 8]
        );
    }
    #[test]
    fn opaque_readback_replaces_alpha_and_short_mapping_is_rejected() {
        let layout = Layout {
            width: 1,
            height: 1,
            stride: 256,
            bytes: 256,
            bgra: false,
            opaque: true,
        };
        let mut bytes = vec![0; 256];
        bytes[..4].copy_from_slice(&[11, 22, 33, 7]);
        assert_eq!(tight_rgba(&bytes, layout).unwrap(), [11, 22, 33, 255]);
        assert!(tight_rgba(&bytes[..255], layout).is_err());
    }
    #[test]
    fn pool_release_wakes_waiters_and_late_registration_cannot_miss_capacity() {
        let ctx = crate::api::Context::new();
        let id = crate::context::id_of(ctx.raw()).unwrap();
        let waker = crate::web::waker(id);
        let pool = Arc::new(Mutex::new(Pool::default()));
        let reader = Reader(Arc::new(Service {
            context: Weak::new(),
            sender: OnceLock::new(),
            pool: pool.clone(),
        }));
        let mut leases: Vec<_> = (0..SLOTS)
            .map(|_| Lease::reserve(&pool, 256).unwrap())
            .collect();
        reader.wake_when_available(id);
        reader.wake_when_available(id);
        assert_eq!(
            pool.lock().unwrap().waiters.len(),
            1,
            "one waiter per context"
        );
        assert!(
            !waker.wait(Duration::from_millis(1)),
            "a full pool must not busy-wake"
        );
        drop(leases.pop());
        assert!(
            waker.wait(Duration::from_millis(50)),
            "capacity release must wake the waiting VM"
        );
        assert!(pool.lock().unwrap().waiters.is_empty());
        // Simulate a release between request(false) and retry registration.
        reader.wake_when_available(id);
        assert!(
            waker.wait(Duration::from_millis(50)),
            "late registration must observe free capacity"
        );
        crate::context::forget(ctx.raw());
    }
}
