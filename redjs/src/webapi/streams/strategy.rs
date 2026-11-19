use super::binding::*;

const COUNT: &str = "CountQueuingStrategy";
const BYTES: &str = "ByteLengthQueuingStrategy";

unsafe fn construct(cx: Env, cf: *mut c_void, name: &str) -> JsResult {
    let init = cx.dictionary(cf_arg(cf, 0))?;
    let mark = cx.prop(init, "highWaterMark")?;
    if mark == UNDEF {
        return Err(cx.type_error("highWaterMark is required"));
    }
    let data = cx.record();
    cx.put(data, "highWaterMark", enc_f64(cx.number(mark)?));
    cx.instance(name, cf_new_target(cf), data)
}

stream_fn!(new_count(cx, cf) throws {
    construct(cx, cf, COUNT)
});
stream_fn!(new_bytes(cx, cf) throws {
    construct(cx, cf, BYTES)
});
stream_fn!(count_mark(cx, cf) throws {
    Ok(cx.field(cx.data(cf_this(cf), COUNT)?, "highWaterMark"))
});
stream_fn!(bytes_mark(cx, cf) throws {
    Ok(cx.field(cx.data(cf_this(cf), BYTES)?, "highWaterMark"))
});
stream_fn!(count_get_size(cx, cf) throws {
    cx.data(cf_this(cf), COUNT)?;
    Ok(cx.slot(cx.ctor(COUNT)))
});
stream_fn!(bytes_get_size(cx, cf) throws {
    cx.data(cf_this(cf), BYTES)?;
    Ok(cx.slot(cx.ctor(BYTES)))
});
stream_fn!(count_size(_cx, _cf) throws {
    Ok(enc_i32(1))
});
stream_fn!(bytes_size(cx, cf) throws {
    cx.prop(cf_arg(cf, 0), "byteLength")
});

pub(super) unsafe fn install(cx: Env) {
    let classes: [(&str, HostFn, HostFn, HostFn, HostFn, u32); 2] = [
        (COUNT, new_count, count_mark, count_get_size, count_size, 0),
        (BYTES, new_bytes, bytes_mark, bytes_get_size, bytes_size, 1),
    ];
    for (name, new, hwm, size, algorithm, arity) in classes {
        let class = cx.register(ClassSpec {
            name,
            arity: 1,
            construct: Some(new),
            parent_proto: 0,
            methods: &[],
            getters: &[("highWaterMark", hwm), ("size", size)],
            accessors: &[],
        });
        cx.attach(class.ctor, make_fn(cx.g, "size", arity, algorithm));
    }
}
