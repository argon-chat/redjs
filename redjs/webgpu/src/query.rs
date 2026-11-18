//! Query sets: occlusion and timestamp queries, pass timestamp writes and resolution.
use super::*;

pub(super) struct QuerySet {
    pub raw: wgpu::QuerySet,
    pub ty: wgpu::QueryType,
    pub count: u32,
    pub destroyed: AtomicBool,
}

/// A pass's `timestampWrites`, already checked against the WebGPU rules
/// wgpu does not enforce itself.
pub(super) struct TimestampWrites {
    pub set: Arc<QuerySet>,
    pub beginning: Option<u32>,
    pub end: Option<u32>,
}

pub(super) unsafe fn timestamp_writes(
    g: *mut c_void,
    descriptor: u64,
    c: &Arc<Client>,
) -> Result<Option<TimestampWrites>, ()> {
    let writes = field(g, descriptor, "timestampWrites")?;
    if writes.v == UNDEF {
        return Ok(None);
    }
    let set = field(g, writes.v, "querySet")?;
    if set.v == UNDEF {
        invalid(g, "GPUPassTimestampWrites.querySet is required")?;
    }
    let h = owned(g, set.v, Kind::QuerySet, c)?;
    let Resource::QuerySet(set) = h.resource else {
        unreachable!()
    };
    let index = |name| -> Result<Option<u32>, ()> {
        let v = field(g, writes.v, name)?;
        if v.v == UNDEF {
            Ok(None)
        } else {
            Ok(Some(uint(g, v.v, 0)?))
        }
    };
    let beginning = index("beginningOfPassWriteIndex")?;
    let end = index("endOfPassWriteIndex")?;
    if !matches!(set.ty, wgpu::QueryType::Timestamp)
        || (beginning.is_none() && end.is_none())
        || (beginning.is_some() && beginning == end)
    {
        validation(
            g,
            c,
            "timestampWrites needs a timestamp query set and distinct write indices",
        );
    }
    Ok(Some(TimestampWrites {
        set,
        beginning,
        end,
    }))
}

pub(super) unsafe fn occlusion_set(
    g: *mut c_void,
    descriptor: u64,
    c: &Arc<Client>,
) -> Result<Option<Arc<QuerySet>>, ()> {
    let set = field(g, descriptor, "occlusionQuerySet")?;
    if set.v == UNDEF {
        return Ok(None);
    }
    let h = owned(g, set.v, Kind::QuerySet, c)?;
    let Resource::QuerySet(set) = h.resource else {
        unreachable!()
    };
    Ok(Some(set))
}

#[redjs::host_fn]
unsafe fn create(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Device)?;
        let c = client(g, &h)?;
        let d = Val::new(g, cf_arg(cf, 0));
        let label = text(g, field(g, d.v, "label")?.v, "")?;
        let ty = field(g, d.v, "type")?;
        if ty.v == UNDEF {
            invalid(g, "GPUQuerySetDescriptor.type is required")?;
        }
        let ty = match text(g, ty.v, "")?.as_str() {
            "occlusion" => wgpu::QueryType::Occlusion,
            "timestamp" => wgpu::QueryType::Timestamp,
            _ => {
                invalid(g, "Invalid GPUQueryType")?;
                unreachable!()
            }
        };
        let count = field(g, d.v, "count")?;
        if count.v == UNDEF {
            invalid(g, "GPUQuerySetDescriptor.count is required")?;
        }
        let count = uint(g, count.v, 0)?;
        if matches!(ty, wgpu::QueryType::Timestamp)
            && !c.features.contains(wgpu::Features::TIMESTAMP_QUERY)
        {
            invalid(
                g,
                "Timestamp query sets need the 'timestamp-query' feature on this device",
            )?;
        }
        let raw = checked(g, &c, || {
            c.context
                .device
                .create_query_set(&wgpu::QuerySetDescriptor {
                    label: Some(&label),
                    ty,
                    count,
                })
        });
        Ok(wrap_labeled(
            g,
            Kind::QuerySet,
            Some(c),
            Resource::QuerySet(Arc::new(QuerySet {
                raw,
                ty,
                count,
                destroyed: AtomicBool::new(false),
            })),
            &label,
        ))
    })
}

unsafe fn query_this(g: *mut c_void, cf: *mut c_void) -> Result<Arc<QuerySet>, ()> {
    let h = this(g, cf, Kind::QuerySet)?;
    let Resource::QuerySet(set) = h.resource else {
        unreachable!()
    };
    Ok(set)
}

#[redjs::host_fn]
unsafe fn query_type(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let set = query_this(g, cf)?;
        Ok(js_string(
            g,
            match set.ty {
                wgpu::QueryType::Timestamp => "timestamp",
                _ => "occlusion",
            },
        ))
    })
}

#[redjs::host_fn]
unsafe fn query_count(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || Ok(enc_f64(query_this(g, cf)?.count as f64)))
}

#[redjs::host_fn]
unsafe fn destroy(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        query_this(g, cf)?.destroyed.store(true, Ordering::Release);
        Ok(UNDEF)
    })
}

#[redjs::host_fn]
unsafe fn resolve(g: *mut c_void, cf: *mut c_void) -> u64 {
    run(g, || {
        let h = this(g, cf, Kind::Encoder)?;
        let c = client(g, &h)?;
        let Resource::Encoder(encoder) = h.resource else {
            unreachable!()
        };
        let set = owned(g, cf_arg(cf, 0), Kind::QuerySet, &c)?;
        let Resource::QuerySet(set) = set.resource else {
            unreachable!()
        };
        let first = uint(g, cf_arg(cf, 1), 0)?;
        let count = uint(g, cf_arg(cf, 2), 0)?;
        let destination = owned(g, cf_arg(cf, 3), Kind::Buffer, &c)?;
        let Resource::Buffer(destination) = destination.resource else {
            unreachable!()
        };
        let offset = num(g, cf_arg(cf, 4), 0)?;
        let Some(end) = first.checked_add(count) else {
            validation(g, &c, "resolveQuerySet range overflows");
            return Ok(UNDEF);
        };
        compute::retain_encoder(
            &encoder,
            &[
                Resource::QuerySet(set.clone()),
                Resource::Buffer(destination.clone()),
            ],
        );
        compute::with_encoder(g, &c, &encoder, |e| {
            e.resolve_query_set(&set.raw, first..end, &destination.raw, offset)
        })?;
        Ok(UNDEF)
    })
}

pub(super) unsafe fn install(g: *mut c_void) {
    class(
        g,
        Kind::QuerySet,
        "GPUQuerySet",
        &[("destroy", 0, destroy)],
        &[("type", query_type), ("count", query_count)],
    );
    method(g, Kind::Device, "createQuerySet", 1, create);
    method(g, Kind::Encoder, "resolveQuerySet", 5, resolve);
}
