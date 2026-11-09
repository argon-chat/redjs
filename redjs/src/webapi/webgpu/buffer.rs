use super::*;

#[derive(Clone)]
struct Range {
    offset: u64,
    size: u64,
    ab: u64,
}
#[derive(Default)]
struct Mapping {
    status: u8,
    mode: u32,
    start: u64,
    end: u64,
    epoch: u64,
    ranges: Vec<Range>,
}
pub(super) struct Buffer {
    pub raw: Arc<wgpu::Buffer>,
    pub size: u64,
    pub usage: wgpu::BufferUsages,
    pub destroyed: AtomicBool,
    pub valid: bool,
    mapping: Mutex<Mapping>,
}
// Constructed before spawning: dropping an unpolled future (thread creation
// failure or cancelled work) must not leave a native map pending forever.
struct MapCleanup {
    buffer: Arc<Buffer>,
    epoch: u64,
    armed: bool,
}
impl Drop for MapCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut m = self
            .buffer
            .mapping
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if m.epoch == self.epoch {
            m.status = 0;
            m.epoch = m.epoch.wrapping_add(1);
            self.buffer.raw.unmap();
        }
    }
}
unsafe fn get_buffer(g: *mut c_void, cf: *mut c_void) -> Result<(Arc<Client>, Arc<Buffer>), ()> {
    let h = this(g, cf, Kind::Buffer)?;
    let c = h.client.unwrap();
    let Resource::Buffer(b) = h.resource else {
        unreachable!()
    };
    Ok((c, b))
}
unsafe fn ranges_slot(g: *mut c_void) -> *mut c_void {
    states().lock().unwrap()[&(g as usize)]
        .classes
        .as_ref()
        .unwrap()
        .s_ranges as *mut c_void
}
#[crate::host_fn]
unsafe fn create(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Device)?;
        let c = client(g, &h)?;
        let d = Val::new(g, cf_arg(cf, 0));
        let size = num(g, field(g, d.v, "size")?.v, 0)?;
        let bits = uint(g, field(g, d.v, "usage")?.v, 0)?;
        let mut raw_bits = wgpu::BufferUsages::empty().bits();
        raw_bits.0[0] = bits;
        let Some(usage) = wgpu::BufferUsages::from_bits(raw_bits) else {
            invalid(g, "Invalid GPUBufferUsage flags")?;
            unreachable!()
        };
        let mapped = boolean(g, field(g, d.v, "mappedAtCreation")?.v);
        let label = text(g, field(g, d.v, "label")?.v, "")?;
        let within_limits = size <= c.limits.max_buffer_size;
        if !within_limits {
            validation(g, &c, "Buffer exceeds this logical device's maxBufferSize");
        }
        let raw = Arc::new(c.checked(|| {
            c.context.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(&label),
                size: if within_limits { size } else { 0 },
                usage: if within_limits {
                    usage
                } else {
                    wgpu::BufferUsages::empty()
                },
                mapped_at_creation: mapped && within_limits,
            })
        }));
        let errors = c.take_errors();
        let valid = errors.is_empty() && within_limits;
        route_errors(g, c.id, errors);
        c.track_buffer(&raw);
        let b = Arc::new(Buffer {
            raw,
            size,
            usage,
            destroyed: AtomicBool::new(false),
            valid,
            mapping: Mutex::new(Mapping {
                status: if mapped && valid { 2 } else { 0 },
                mode: 2,
                start: 0,
                end: size,
                ..Default::default()
            }),
        });
        let o = Val::new(
            g,
            wrap(g, Kind::Buffer, Some(c.clone()), Resource::Buffer(b)),
        );
        track_buffer_object(g, &c, o.v);
        let ranges = new_array(g);
        glue::rjs_define(g, o.v, ranges_slot(g), ranges, 2);
        Ok(o.v)
    })
}
#[crate::host_fn]
unsafe fn size(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || Ok(enc_f64(get_buffer(g, cf)?.1.size as f64)))
}
#[crate::host_fn]
unsafe fn usage(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        Ok(enc_f64(get_buffer(g, cf)?.1.usage.bits().0[0] as f64))
    })
}
#[crate::host_fn]
unsafe fn map_state(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (_, b) = get_buffer(g, cf)?;
        let status = b.mapping.lock().unwrap().status;
        Ok(js_string(
            g,
            match status {
                1 => "pending",
                2 => "mapped",
                _ => "unmapped",
            },
        ))
    })
}
#[crate::host_fn]
unsafe fn map_async(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, b) = get_buffer(g, cf)?;
        let mode = uint(g, cf_arg(cf, 0), 0)?;
        let offset = num(g, cf_arg(cf, 1), 0)?;
        let size = num(g, cf_arg(cf, 2), b.size.saturating_sub(offset))?;
        let end = offset.checked_add(size);
        let native_mode = match mode {
            1 => wgpu::MapMode::Read,
            2 => wgpu::MapMode::Write,
            _ => {
                return Ok(super::super::domexception::rejected(
                    g,
                    "OperationError",
                    "Invalid GPUMapMode",
                ))
            }
        };
        let usage = if mode == 1 {
            wgpu::BufferUsages::MAP_READ
        } else {
            wgpu::BufferUsages::MAP_WRITE
        };
        if c.check().is_err()
            || !b.valid
            || b.destroyed.load(Ordering::Acquire)
            || !b.usage.contains(usage)
            || offset % 8 != 0
            || size % 4 != 0
            || end.is_none_or(|e| e > b.size)
        {
            validation(g, &c, "Invalid buffer mapping range, state, or usage");
            return Ok(super::super::domexception::rejected(
                g,
                "OperationError",
                "Invalid buffer mapping range, state, or usage",
            ));
        }
        let epoch = {
            let mut m = b.mapping.lock().unwrap();
            if m.status != 0 {
                drop(m);
                return Ok(super::super::domexception::rejected(
                    g,
                    "OperationError",
                    "GPUBuffer is already mapped or mapping",
                ));
            }
            m.epoch = m.epoch.wrapping_add(1);
            m.status = 1;
            m.mode = mode;
            m.start = offset;
            m.end = end.unwrap();
            m.epoch
        };
        let (tx, rx) = std::sync::mpsc::channel();
        checked(g, &c, || {
            b.raw
                .map_async(native_mode, offset..end.unwrap(), move |result| {
                    let _ = tx.send(result.map_err(|e| e.to_string()));
                })
        });
        let cb = b.clone();
        let cc = c.clone();
        let cleanup = MapCleanup {
            buffer: b.clone(),
            epoch,
            armed: true,
        };
        Ok(async_op(
            g,
            vec![cf_this(cf)],
            async move {
                let mut cleanup = cleanup;
                let result = (|| {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    loop {
                        if cb.mapping.lock().unwrap().epoch != epoch {
                            return Err("Buffer mapping was aborted".into());
                        }
                        if let Ok(result) = rx.try_recv() {
                            result?;
                            return Ok((cb.clone(), epoch));
                        }
                        if std::time::Instant::now() >= deadline {
                            return Err("GPU buffer mapping timed out".into());
                        }
                        let _ = cc.context.device.poll(wgpu::PollType::Wait {
                            submission_index: None,
                            timeout: Some(std::time::Duration::from_millis(10)),
                        });
                        cc.check()?;
                    }
                })();
                if result.is_err() {
                    let mut m = cb.mapping.lock().unwrap();
                    if m.epoch == epoch {
                        m.status = 0;
                        m.epoch = m.epoch.wrapping_add(1);
                        cb.raw.unmap();
                    }
                }
                if result.is_ok() {
                    cleanup.armed = false;
                }
                result
            },
            |g, (b, epoch)| {
                let mut m = b.mapping.lock().unwrap();
                if m.epoch != epoch || m.status != 1 {
                    drop(m);
                    return super::super::domexception::rejected(
                        g,
                        "AbortError",
                        "Buffer mapping was aborted",
                    );
                }
                m.status = 2;
                UNDEF
            },
        ))
    })
}
#[crate::host_fn]
unsafe fn get_range(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (c, b) = get_buffer(g, cf)?;
        let offset = num(g, cf_arg(cf, 0), 0)?;
        let size = num(g, cf_arg(cf, 1), b.size.saturating_sub(offset))?;
        let end = offset.checked_add(size);
        let epoch = {
            let m = b.mapping.lock().unwrap();
            if m.status != 2
                || c.check().is_err()
                || b.destroyed.load(Ordering::Acquire)
                || offset % 8 != 0
                || size % 4 != 0
                || offset < m.start
                || end.is_none_or(|e| e > m.end)
                || m.ranges
                    .iter()
                    .any(|r| offset < r.offset + r.size && end.unwrap() > r.offset)
            {
                drop(m);
                return Ok(super::super::domexception::throw(
                    g,
                    "OperationError",
                    "Invalid or overlapping getMappedRange",
                ));
            }
            m.epoch
        };
        let bytes = if size == 0 {
            Vec::new()
        } else {
            match b.raw.get_mapped_range(offset..end.unwrap()) {
                Ok(view) => view.to_vec(),
                Err(e) => {
                    return Ok(super::super::domexception::throw(
                        g,
                        "OperationError",
                        &e.to_string(),
                    ))
                }
            }
        };
        let view = Val::new(g, make_u8(g, &bytes));
        let mut backing = UNDEF;
        if glue::rjs_view_buffer(g, view.v, &mut backing) != 0 {
            invalid(g, "Cannot obtain mapped ArrayBuffer")?;
        }
        let ab = Val::new(g, backing);
        glue::rjs_define(g, ab.v, ranges_slot(g), cf_this(cf), 1 | 2);
        let edges = Val::new(g, get_a(g, cf_this(cf), ranges_slot(g)));
        let n = array_len(g, edges.v);
        set_i(g, edges.v, n, ab.v);
        let mut m = b.mapping.lock().unwrap();
        if m.epoch != epoch {
            drop(m);
            glue::rjs_arraybuffer_detach(g, ab.v);
            return Ok(super::super::domexception::throw(
                g,
                "OperationError",
                "Buffer was unmapped during allocation",
            ));
        }
        m.ranges.push(Range {
            offset,
            size,
            ab: ab.v,
        });
        Ok(ab.v)
    })
}
/// Detach the JSC-owned shadow ranges before releasing the native mapping.
/// No pointer into mapped GPU memory survives an allocation or GC callback.
unsafe fn unmap_inner(g: *mut c_void, obj: u64, b: &Buffer, write: bool) {
    let (mode, ranges) = {
        let mut m = b.mapping.lock().unwrap();
        m.epoch = m.epoch.wrapping_add(1);
        m.status = 0;
        (m.mode, std::mem::take(&mut m.ranges))
    };
    for r in ranges {
        if write && mode == 2 && r.size != 0 && b.valid && !b.destroyed.load(Ordering::Acquire) {
            let bytes = bufferish(g, r.ab).unwrap_or(&[]);
            if bytes.len() == r.size as usize {
                if let Ok(mut view) = b.raw.get_mapped_range_mut(r.offset..r.offset + r.size) {
                    view.copy_from_slice(bytes);
                }
            }
        }
        glue::rjs_arraybuffer_detach(g, r.ab);
    }
    if b.valid {
        b.raw.unmap();
    }
    let edges = new_array(g);
    glue::rjs_define(g, obj, ranges_slot(g), edges, 2);
}
#[crate::host_fn]
unsafe fn unmap(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (_, b) = get_buffer(g, cf)?;
        unmap_inner(g, cf_this(cf), &b, true);
        Ok(UNDEF)
    })
}
#[crate::host_fn]
unsafe fn destroy(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let (_, b) = get_buffer(g, cf)?;
        unmap_inner(g, cf_this(cf), &b, false);
        b.destroyed.store(true, Ordering::Release);
        b.raw.destroy();
        Ok(UNDEF)
    })
}
/// Copy a BufferSource after all observable conversions. Typed-array offsets
/// are in elements; DataView and ArrayBuffer offsets are in bytes.
pub(super) unsafe fn copy_source(
    g: *mut c_void,
    value: u64,
    offset: u64,
    size: Option<u64>,
) -> Result<Vec<u8>, ()> {
    let mut ta = crate::ffi::RjsTaInfo::default();
    let mut ab = crate::ffi::RjsAbInfo::default();
    let (ptr, len, units, detached) = if glue::rjs_typedarray_info(g, value, &mut ta) == 0 {
        (
            ta.data,
            ta.byte_length as u64,
            match ta.kind {
                4 | 5 | 13 => 2,
                6 | 7 | 8 => 4,
                9 | 10 | 11 => 8,
                _ => 1,
            },
            ta.detached != 0,
        )
    } else if glue::rjs_arraybuffer_info(g, value, &mut ab) == 0 {
        (ab.data, ab.byte_length, 1, ab.detached != 0)
    } else {
        invalid(g, "WebGPU data must be a BufferSource")?;
        unreachable!()
    };
    if ta.shared != 0 || ab.shared != 0 {
        super::super::domexception::throw(
            g,
            "NotSupportedError",
            "Shared WebGPU uploads require an atomic native copy path",
        );
        return Err(());
    }
    let Some(start) = offset.checked_mul(units) else {
        invalid(g, "BufferSource offset overflow")?;
        unreachable!()
    };
    let size = match size {
        Some(s) => s.checked_mul(units),
        None => len.checked_sub(start),
    };
    let Some(size) = size else {
        invalid(g, "BufferSource size overflow")?;
        unreachable!()
    };
    if detached || start > len || size > len - start || (ptr.is_null() && size != 0) {
        super::super::domexception::throw(
            g,
            "OperationError",
            "BufferSource range is out of bounds or detached",
        );
        return Err(());
    }
    if size == 0 {
        Ok(Vec::new())
    } else {
        Ok(
            core::slice::from_raw_parts((ptr as *const u8).add(start as usize), size as usize)
                .to_vec(),
        )
    }
}
#[crate::host_fn]
unsafe fn write(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Queue)?;
        let c = client(g, &h)?;
        let b = owned(g, cf_arg(cf, 0), Kind::Buffer, &c)?;
        let Resource::Buffer(b) = b.resource else {
            unreachable!()
        };
        let offset = num(g, cf_arg(cf, 1), 0)?;
        let data = Val::new(g, cf_arg(cf, 2));
        let start = num(g, cf_arg(cf, 3), 0)?;
        let size = if cf_arg(cf, 4) == UNDEF {
            None
        } else {
            Some(num(g, cf_arg(cf, 4), 0)?)
        };
        let bytes = copy_source(g, data.v, start, size)?;
        if offset % 4 != 0
            || bytes.len() % 4 != 0
            || offset > b.size
            || bytes.len() as u64 > b.size - offset
            || b.destroyed.load(Ordering::Acquire)
            || b.mapping.lock().unwrap().status != 0
        {
            validation(g, &c, "Invalid writeBuffer range or buffer state");
            return Ok(UNDEF);
        }
        checked(g, &c, || {
            c.context.queue.write_buffer(&b.raw, offset, &bytes)
        });
        Ok(UNDEF)
    })
}
pub(super) unsafe fn install(g: *mut c_void) {
    class(
        g,
        Kind::Buffer,
        "GPUBuffer",
        &[
            ("mapAsync", 1, map_async),
            ("getMappedRange", 0, get_range),
            ("unmap", 0, unmap),
            ("destroy", 0, destroy),
        ],
        &[("size", size), ("usage", usage), ("mapState", map_state)],
    );
    method(g, Kind::Device, "createBuffer", 1, create);
    method(g, Kind::Queue, "writeBuffer", 3, write);
    constants(
        g,
        "GPUBufferUsage",
        &[
            ("MAP_READ", 1),
            ("MAP_WRITE", 2),
            ("COPY_SRC", 4),
            ("COPY_DST", 8),
            ("INDEX", 16),
            ("VERTEX", 32),
            ("UNIFORM", 64),
            ("STORAGE", 128),
            ("INDIRECT", 256),
            ("QUERY_RESOLVE", 512),
        ],
    );
    constants(g, "GPUMapMode", &[("READ", 1), ("WRITE", 2)]);
}

pub(super) unsafe fn release_on_device_destroy(g: *mut c_void, obj: u64) {
    if let Ok(h) = handle(g, obj, Kind::Buffer) {
        if let Resource::Buffer(b) = h.resource {
            unmap_inner(g, obj, &b, false);
            b.destroyed.store(true, Ordering::Release);
        }
    }
}
