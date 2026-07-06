//! Conformance suite for the redjs ABI — every feature the player needs from
//! the JS engine, exercised the way the generated C# bindings will use it.
//!
//! One #[test] entry point (JSC contexts are process-lifetime; cargo test
//! threads + many VMs are fine, but a single ordered run keeps output and
//! teardown deterministic).

use core::ffi::c_void;
use redjs::api::{layout, Context, Error, HostArgs, TaKind, Value};
use redjs::ffi;

fn sec(name: &str) {
    use std::io::Write;
    eprintln!("[conformance] {name}");
    let _ = std::io::stderr().flush();
}

#[test]
fn conformance() {
    let ctx = Context::new();

    sec("eval_basics");
    eval_basics(&ctx);
    sec("value_encoding");
    value_encoding(&ctx);
    sec("strings");
    strings(&ctx);
    sec("properties_objects_arrays");
    properties_objects_arrays(&ctx);
    sec("host_functions");
    host_functions(&ctx);
    sec("callframe_layout_contract");
    callframe_layout_contract(&ctx);
    sec("calls_and_constructors");
    calls_and_constructors(&ctx);
    sec("exceptions");
    exceptions(&ctx);
    sec("typed_arrays");
    typed_arrays(&ctx);
    sec("external_buffers");
    external_buffers(&ctx);
    sec("promises_and_microtasks");
    promises_and_microtasks(&ctx);
    sec("host_objects");
    host_objects(&ctx);
    sec("gc_and_stats");
    gc_and_stats(&ctx);
    sec("host_function_throw");
    host_function_throw(&ctx);

    println!("redjs conformance: all sections passed");
}

fn eval_basics(ctx: &Context) {
    let v = ctx.eval("6 * 7").unwrap();
    assert_eq!(v.as_number(), Some(42.0));

    let v = ctx.eval("(function f(){ return 'hi ' + 'there'; })()").unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "hi there");

    // Named source shows up in stacks.
    let err = ctx.eval_named("throw new Error('boom')", "game/main.js").unwrap_err();
    let Error::Exception(e) = err else { panic!("expected exception") };
    let (msg, stack) = ctx.error_info(e);
    assert_eq!(msg, "boom");
    assert!(stack.contains("game/main.js"), "stack should name the source: {stack}");
}

fn value_encoding(ctx: &Context) {
    // Round-trip int32/double/bool/null/undefined through JS and decode with
    // pure-bit logic (the zero-FFI path C# uses).
    let v = ctx.eval("123").unwrap();
    assert!(v.is_int32());
    assert_eq!(v.as_number(), Some(123.0));

    let v = ctx.eval("1.5").unwrap();
    assert!(!v.is_int32() && v.is_number());
    assert_eq!(v.as_number(), Some(1.5));

    let v = ctx.eval("-1").unwrap();
    assert_eq!(v.as_number(), Some(-1.0));

    assert_eq!(ctx.eval("true").unwrap().as_bool(), Some(true));
    assert_eq!(ctx.eval("false").unwrap().as_bool(), Some(false));
    assert!(ctx.eval("null").unwrap().is_null());
    assert!(ctx.eval("undefined").unwrap().is_undefined());

    // Encode side: pass values INTO JS.
    let sum = ctx
        .call(ctx.eval("(function(a,b){ return a+b; })").unwrap(), Value::undefined(), &[
            Value::int32(40),
            Value::double(2.5),
        ])
        .unwrap();
    assert_eq!(sum.as_number(), Some(42.5));

    // kind() classification.
    assert_eq!(ctx.kind(ctx.eval("undefined").unwrap()), 0);
    assert_eq!(ctx.kind(ctx.eval("null").unwrap()), 1);
    assert_eq!(ctx.kind(ctx.eval("true").unwrap()), 2);
    assert_eq!(ctx.kind(ctx.eval("7").unwrap()), 3);
    assert_eq!(ctx.kind(ctx.eval("7.5").unwrap()), 4);
    assert_eq!(ctx.kind(ctx.eval("'s'").unwrap()), 5);
    assert_eq!(ctx.kind(ctx.eval("Symbol('x')").unwrap()), 6);
    assert_eq!(ctx.kind(ctx.eval("10n").unwrap()), 7);
    assert_eq!(ctx.kind(ctx.eval("({})").unwrap()), 8);
    assert_eq!(ctx.kind(ctx.eval("(()=>1)").unwrap()), 9);
    assert_eq!(ctx.kind(ctx.eval("[1,2]").unwrap()), 10);
    assert_eq!(ctx.kind(ctx.eval("new Float32Array(4)").unwrap()), 11);
    assert_eq!(ctx.kind(ctx.eval("new ArrayBuffer(4)").unwrap()), 12);
    assert_eq!(ctx.kind(ctx.eval("Promise.resolve(1)").unwrap()), 13);
}

fn strings(ctx: &Context) {
    // Latin-1 internal representation.
    let v = ctx.string("hello");
    assert_eq!(ctx.read_string(v).unwrap(), "hello");

    // Non-Latin-1 (UTF-16 internal) + round trip.
    let v = ctx.string("привет 🎮");
    assert_eq!(ctx.read_string(v).unwrap(), "привет 🎮");

    // Rope resolution: concatenation builds ropes; string_info must resolve.
    let v = ctx.eval("('a'.repeat(1000) + 'b'.repeat(1000))").unwrap();
    let s = ctx.read_string(v).unwrap();
    assert_eq!(s.len(), 2000);
    assert!(s.starts_with("aaa") && s.ends_with("bbb"));

    // ToString coercion.
    assert_eq!(ctx.to_rust_string(Value::int32(42)).unwrap(), "42");
}

fn properties_objects_arrays(ctx: &Context) {
    sec("  object()");
    let obj = ctx.object();
    sec("  atoms");
    let a_x = ctx.atom("x");
    let a_y = ctx.atom("y");

    sec("  set");
    ctx.set(obj, &a_x, Value::int32(10)).unwrap();
    sec("  define");
    ctx.define(obj, &a_y, ctx.string("val"), 1 /* readonly */).unwrap();

    sec("  get");
    assert_eq!(ctx.get(obj, &a_x).unwrap().as_number(), Some(10.0));
    assert_eq!(ctx.to_rust_string(ctx.get(obj, &a_y).unwrap()).unwrap(), "val");

    // Expose to JS through the global and read from script.
    sec("  global_set");
    ctx.global_set("testObj", obj).unwrap();
    let v = ctx.eval("testObj.x + testObj.y").unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "10val");

    // Getter through get (spec semantics).
    sec("  getter");
    let g = ctx.eval("({ get p() { return 55; } })").unwrap();
    assert_eq!(ctx.get(g, &ctx.atom("p")).unwrap().as_number(), Some(55.0));

    // Arrays.
    sec("  array()");
    let arr = ctx.array(3).unwrap();
    sec("  set_index");
    ctx.set_index(arr, 0, Value::int32(1)).unwrap();
    ctx.set_index(arr, 1, Value::int32(2)).unwrap();
    ctx.set_index(arr, 2, Value::int32(3)).unwrap();
    assert_eq!(ctx.array_length(arr).unwrap(), 3);
    assert_eq!(ctx.get_index(arr, 1).unwrap().as_number(), Some(2.0));

    // Descriptor-style reads (the GPUDevice.createX(descriptor) pattern).
    let desc = ctx.eval("({ size: 256, usage: 0x28, label: 'vbuf', origin: [1,2,3] })").unwrap();
    assert_eq!(ctx.get(desc, &ctx.atom("size")).unwrap().as_number(), Some(256.0));
    assert_eq!(ctx.get(desc, &ctx.atom("usage")).unwrap().as_number(), Some(40.0));
    let origin = ctx.get(desc, &ctx.atom("origin")).unwrap();
    assert_eq!(ctx.get_index(origin, 2).unwrap().as_number(), Some(3.0));
    // Missing property → undefined (ClearScript's IsUndefined pattern).
    assert!(ctx.get(desc, &ctx.atom("mipLevel")).unwrap().is_undefined());
}

fn host_functions(ctx: &Context) {
    // Rust closure via trampoline pool (kind DATA).
    let f = ctx
        .function("hostAdd", 2, move |_g, cf| {
            let args = unsafe { HostArgs::new(cf) };
            let a = args.arg(0).as_number().unwrap_or(0.0);
            let b = args.arg(1).as_number().unwrap_or(0.0);
            Value::double(a + b).0
        })
        .unwrap();
    ctx.global_set("hostAdd", f).unwrap();

    let v = ctx.eval("hostAdd(20, 22.5)").unwrap();
    assert_eq!(v.as_number(), Some(42.5));

    // Hot loop through the trampoline — sanity that the SysV→MS transition
    // holds under JIT tiers (LLInt → DFG/FTL happens within these counts).
    let v = ctx.eval("let s=0; for (let i=0;i<200000;i++) s = hostAdd(s, 1); s").unwrap();
    assert_eq!(v.as_number(), Some(200000.0));

    // Direct sysv64 host function — the zero-overhead Rust path.
    unsafe extern "sysv64" fn native_mul(_g: *mut c_void, cf: *mut c_void) -> u64 {
        let args = HostArgs::new(cf);
        let a = args.arg(0).as_number().unwrap_or(0.0);
        let b = args.arg(1).as_number().unwrap_or(0.0);
        Value::double(a * b).0
    }
    let f = unsafe { ctx.function_sysv("nativeMul", 2, native_mul as *const c_void) }.unwrap();
    ctx.global_set("nativeMul", f).unwrap();
    assert_eq!(ctx.eval("nativeMul(6, 7)").unwrap().as_number(), Some(42.0));

    // MS-ABI function registered through the C ABI exactly like C# will be.
    unsafe extern "C" fn ms_abi_sub(_g: *mut c_void, cf: *mut c_void) -> u64 {
        let args = HostArgs::new(cf);
        let a = args.arg(0).as_number().unwrap_or(0.0);
        let b = args.arg(1).as_number().unwrap_or(0.0);
        Value::double(a - b).0
    }
    let mut out = 0u64;
    let name: Vec<u16> = "msSub".encode_utf16().collect();
    let status = unsafe {
        ffi::redjs_function_create(ctx.raw(), name.as_ptr(), name.len(), 2, ms_abi_sub as *const c_void, &mut out)
    };
    assert_eq!(status, 0);
    ctx.global_set("msSub", Value(out)).unwrap();
    assert_eq!(ctx.eval("msSub(50, 8)").unwrap().as_number(), Some(42.0));

    // with_data variant (instance-bound host fn pattern).
    unsafe extern "C" fn ms_abi_data(_g: *mut c_void, _cf: *mut c_void, data: *mut c_void) -> u64 {
        Value::int32(data as usize as i32).0
    }
    let mut out = 0u64;
    let name: Vec<u16> = "msData".encode_utf16().collect();
    let status = unsafe {
        ffi::redjs_function_create_with_data(
            ctx.raw(), name.as_ptr(), name.len(), 0, ms_abi_data as *const c_void, 777usize as *mut c_void, &mut out,
        )
    };
    assert_eq!(status, 0);
    ctx.global_set("msData", Value(out)).unwrap();
    assert_eq!(ctx.eval("msData()").unwrap().as_number(), Some(777.0));

    let mut used = 0u32;
    let mut total = 0u32;
    unsafe { ffi::redjs_host_slots(&mut used, &mut total) };
    assert!(used >= 3 && total >= 1024, "slots {used}/{total}");
}

fn callframe_layout_contract(ctx: &Context) {
    // Validates the exported RjsLayout offsets by reading a CallFrame raw —
    // exactly what the C# codegen does. If this passes, C# decoding works.
    let l = layout();
    assert_eq!(l.abi_version, 1);
    // JSVALUE64 constants are architectural; pin them so a header change screams.
    assert_eq!(l.tag_number, 0xfffe_0000_0000_0000);
    assert_eq!(l.value_undefined, 0xa);
    assert_eq!(l.value_null, 0x2);
    assert_eq!(l.value_true, 0x7);
    assert_eq!(l.value_false, 0x6);
    // CallFrame: [callerFrame, returnPC, codeBlock, callee, argc, this, args...]
    assert_eq!(l.cf_callee_offset, 24);
    assert_eq!(l.cf_argc_offset, 32);
    assert_eq!(l.cf_this_offset, 40);
    assert_eq!(l.cf_args_offset, 48);

    let f = ctx
        .function("probe", 3, move |_g, cf| {
            let args = unsafe { HostArgs::new(cf) };
            // argc excludes `this`; JS may pass fewer args than arity.
            let argc = args.argc();
            let a0 = args.arg(0).as_number().unwrap_or(f64::NAN);
            let a2 = args.arg(2);
            let a2n = a2.as_number().unwrap_or(-1.0);
            let this_is_obj = args.this().is_cell();
            let ok = argc == 3 && a0 == 11.0 && a2n == 33.0 && this_is_obj;
            Value::boolean(ok).0
        })
        .unwrap();
    ctx.global_set("probe", f).unwrap();
    assert_eq!(ctx.eval("probe.call({o:1}, 11, 22, 33)").unwrap().as_bool(), Some(true));

    // Under-application: arg(1) must read as undefined, argc == 1.
    let f = ctx
        .function("probe2", 2, move |_g, cf| {
            let args = unsafe { HostArgs::new(cf) };
            Value::boolean(args.argc() == 1 && args.arg(1).is_undefined()).0
        })
        .unwrap();
    ctx.global_set("probe2", f).unwrap();
    assert_eq!(ctx.eval("probe2(5)").unwrap().as_bool(), Some(true));
}

fn calls_and_constructors(ctx: &Context) {
    // rust→js call with `this`.
    let obj = ctx.eval("({ n: 40, add(x) { return this.n + x; } })").unwrap();
    let add = ctx.get(obj, &ctx.atom("add")).unwrap();
    let r = ctx.call(add, obj, &[Value::int32(2)]).unwrap();
    assert_eq!(r.as_number(), Some(42.0));

    // construct.
    let ctor = ctx.eval("(class Point { constructor(x, y) { this.x = x; this.y = y; } })").unwrap();
    let p = ctx.construct(ctor, &[Value::int32(3), Value::int32(4)]).unwrap();
    assert_eq!(ctx.get(p, &ctx.atom("y")).unwrap().as_number(), Some(4.0));

    // js exception propagates through call.
    let thrower = ctx.eval("(function(){ throw new TypeError('nope'); })").unwrap();
    let Err(Error::Exception(e)) = ctx.call(thrower, Value::undefined(), &[]) else {
        panic!("expected exception");
    };
    let (msg, _) = ctx.error_info(e);
    assert_eq!(msg, "nope");
}

fn exceptions(ctx: &Context) {
    // Syntax error at eval.
    let Err(Error::Exception(e)) = ctx.eval("function {") else { panic!() };
    let (msg, _) = ctx.error_info(e);
    assert!(!msg.is_empty());

    // Error created host-side, thrown into JS, observed by JS.
    let f = {
        let err_fn = move |g: *mut c_void, _cf: *mut c_void| -> u64 {
            unsafe {
                let msg: Vec<u16> = "host says no".encode_utf16().collect();
                let mut err = 0u64;
                ffi::redjs_error_create(g, 1, msg.as_ptr(), msg.len(), &mut err);
                ffi::redjs_throw(g, err);
                Value::undefined().0
            }
        };
        ctx.function("hostThrow", 0, err_fn).unwrap()
    };
    ctx.global_set("hostThrow", f).unwrap();
    let v = ctx
        .eval("try { hostThrow(); 'no-throw' } catch (e) { e instanceof TypeError ? e.message : 'wrong-type' }")
        .unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "host says no");
}

fn typed_arrays(ctx: &Context) {
    // JS-created typed array introspected from the host (writeBuffer path).
    let v = ctx.eval("(() => { const a = new Float32Array(8); for (let i=0;i<8;i++) a[i]=i*1.5; return a; })()").unwrap();
    let info = ctx.typedarray_info(v).unwrap();
    assert_eq!(info.kind, TaKind::F32 as u32);
    assert_eq!(info.length, 8);
    assert_eq!(info.byte_length, 32);
    assert_eq!(info.detached, 0);
    let floats = unsafe { core::slice::from_raw_parts(info.data as *const f32, info.length as usize) };
    assert_eq!(floats[4], 6.0);

    // Subarray keeps byteOffset.
    let v = ctx.eval("new Uint16Array(new ArrayBuffer(64), 8, 12)").unwrap();
    let info = ctx.typedarray_info(v).unwrap();
    assert_eq!((info.kind, info.length, info.byte_offset), (TaKind::U16 as u32, 12, 8));

    // Host-created typed array visible to JS.
    let ta = ctx.typedarray(TaKind::U8, 4).unwrap();
    let info = ctx.typedarray_info(ta).unwrap();
    unsafe { core::slice::from_raw_parts_mut(info.data as *mut u8, 4).copy_from_slice(&[1, 2, 3, 4]) };
    ctx.global_set("hostBytes", ta).unwrap();
    assert_eq!(ctx.eval("hostBytes[0] + hostBytes[3]").unwrap().as_number(), Some(5.0));

    // ArrayBuffer info.
    let ab = ctx.eval("new ArrayBuffer(128)").unwrap();
    let abi = ctx.arraybuffer_info(ab).unwrap();
    assert_eq!(abi.byte_length, 128);
    assert_eq!(abi.detached, 0);
}

fn external_buffers(ctx: &Context) {
    // The getMappedRange killer feature: JS ArrayBuffer VIEWING host memory,
    // zero copies; JS writes land in the host buffer directly.
    let mut host_mem = vec![0u8; 64].into_boxed_slice();
    let ptr = host_mem.as_mut_ptr() as *mut c_void;

    let ab = unsafe { ctx.arraybuffer_external(ptr, 64, None, core::ptr::null_mut()) }.unwrap();
    ctx.global_set("mapped", ab).unwrap();
    ctx.eval("new Uint8Array(mapped).set([7, 8, 9], 4)").unwrap();
    assert_eq!(&host_mem[4..7], &[7, 8, 9], "JS write must hit host memory directly");

    // unmap(): detach → JS views die, host memory stays ours.
    ctx.arraybuffer_detach(ab).unwrap();
    let v = ctx.eval("mapped.byteLength").unwrap();
    assert_eq!(v.as_number(), Some(0.0));
    drop(host_mem); // safe: buffer detached

    // External buffer deallocator, deterministic path: detach must release
    // the contents and fire the deallocator (this is the unmap() semantics).
    static DEALLOC_HITS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    unsafe extern "C" fn dealloc(bytes: *mut c_void, _ctx: *mut c_void) {
        DEALLOC_HITS.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        drop(Box::from_raw(core::slice::from_raw_parts_mut(bytes as *mut u8, 16)));
    }
    let leaked = Box::into_raw(vec![0u8; 16].into_boxed_slice()) as *mut u8;
    let ab = unsafe {
        ctx.arraybuffer_external(leaked as *mut c_void, 16, Some(dealloc), core::ptr::null_mut())
    }
    .unwrap();
    let abi = ctx.arraybuffer_info(ab).unwrap();
    assert_eq!(abi.byte_length, 16);
    ctx.arraybuffer_detach(ab).unwrap();
    assert_eq!(
        DEALLOC_HITS.load(core::sync::atomic::Ordering::SeqCst),
        1,
        "detach must fire the external deallocator"
    );

    // GC path: an unreferenced external typed array must eventually fire the
    // deallocator once no stack slot roots it (conservative scan).
    make_dead_external_ta(ctx, dealloc);
    clobber_stack();
    for _ in 0..8 {
        ctx.gc();
        if DEALLOC_HITS.load(core::sync::atomic::Ordering::SeqCst) >= 2 {
            break;
        }
    }
    assert!(
        DEALLOC_HITS.load(core::sync::atomic::Ordering::SeqCst) >= 2,
        "external buffer deallocator should run on GC"
    );
}

#[inline(never)]
fn make_dead_external_ta(
    ctx: &Context,
    dealloc: unsafe extern "C" fn(*mut c_void, *mut c_void),
) {
    let leaked = Box::into_raw(vec![0u8; 16].into_boxed_slice()) as *mut u8;
    let ta = unsafe {
        ctx.typedarray_external(TaKind::U8, leaked as *mut c_void, 16, Some(dealloc), core::ptr::null_mut())
    }
    .unwrap();
    let info = ctx.typedarray_info(ta).unwrap();
    assert_eq!(info.byte_length, 16);
}

/// Overwrites a chunk of stack so dead frames' JSValue bits stop being seen
/// by the conservative scan.
#[inline(never)]
fn clobber_stack() {
    let mut buf = [0u8; 64 * 1024];
    std::hint::black_box(&mut buf[..]);
    for b in buf.iter_mut() {
        *b = 0;
    }
    std::hint::black_box(&mut buf[..]);
}

fn promises_and_microtasks(ctx: &Context) {
    // Host-created deferred promise: resolve from the host, JS observes via
    // .then after a microtask drain (the Task→Promise seam).
    let (promise, resolve, _reject) = ctx.promise().unwrap();
    ctx.global_set("hostPromise", promise).unwrap();
    ctx.eval("globalThis.observed = 'pending'; hostPromise.then(v => { observed = 'got:' + v; })").unwrap();

    ctx.call(resolve, Value::undefined(), &[Value::int32(99)]).unwrap();
    ctx.drain_microtasks();
    let v = ctx.eval("observed").unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "got:99");

    // Promise state introspection (both directions of the bridge).
    let (st, res) = ctx.promise_info(promise).unwrap();
    assert_eq!(st, 1);
    assert_eq!(res.as_number(), Some(99.0));

    // JS async function → host observes settlement by polling promise_info
    // (how C# converts a JS promise into a Task without callbacks).
    let p = ctx.eval("(async () => { let x = await Promise.resolve(20); return x + 22; })()").unwrap();
    ctx.protect(p);
    ctx.drain_microtasks();
    let (st, res) = ctx.promise_info(p).unwrap();
    assert_eq!((st, res.as_number()), (1, Some(42.0)));
    ctx.unprotect(p);

    // Rejection.
    let p = ctx.eval("Promise.reject(new Error('rejected!'))").unwrap();
    ctx.drain_microtasks();
    let (st, res) = ctx.promise_info(p).unwrap();
    assert_eq!(st, 2);
    let (msg, _) = ctx.error_info(res);
    assert_eq!(msg, "rejected!");
}

fn host_objects(ctx: &Context) {
    static FINALIZED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    unsafe extern "C" fn on_finalize(data: *mut c_void) {
        FINALIZED.fetch_add(data as usize, core::sync::atomic::Ordering::SeqCst);
    }
    unsafe { ffi::redjs_set_host_finalizer(on_finalize as *const c_void) };

    let cls = unsafe { ffi::redjs_hostclass_create(c"GPUBuffer".as_ptr()) };
    assert!(!cls.is_null());

    // Handle 0xBEEF travels JS and comes back identical (the wrapper pattern:
    // JS object carries a host handle; methods read it back in one call).
    let mut obj = 0u64;
    let status = unsafe { ffi::redjs_hostobject_create(ctx.raw(), cls, 0xBEEF as *mut c_void, 0, &mut obj) };
    assert_eq!(status, 0);
    ctx.global_set("gpuBuf", Value(obj)).unwrap();
    let round = ctx.eval("(() => gpuBuf)()").unwrap();
    assert_eq!(unsafe { ffi::redjs_hostobject_data(round.0) } as usize, 0xBEEF);

    // Not a host object → null.
    assert!(unsafe { ffi::redjs_hostobject_data(ctx.object().0) }.is_null());

    // Attach a prototype with a host method that reads the handle from `this`.
    let proto = ctx.object();
    let getter = ctx
        .function("handle", 0, move |_g, cf| {
            let args = unsafe { HostArgs::new(cf) };
            let h = unsafe { ffi::redjs_hostobject_data(args.this().0) };
            Value::int32(h as usize as i32).0
        })
        .unwrap();
    ctx.define(proto, &ctx.atom("handle"), getter, 0).unwrap();
    let mut obj2 = 0u64;
    unsafe { ffi::redjs_hostobject_create(ctx.raw(), cls, 0x1234 as *mut c_void, proto.0, &mut obj2) };
    ctx.global_set("gpuBuf2", Value(obj2)).unwrap();
    assert_eq!(ctx.eval("gpuBuf2.handle()").unwrap().as_number(), Some(0x1234 as f64));

    // Finalizer: create unreferenced host objects, force GC. Conservative
    // stack scanning may keep a straggler alive, so require ≥3 of 4.
    let before = FINALIZED.load(core::sync::atomic::Ordering::SeqCst);
    make_dead_host_objects(ctx, cls);
    clobber_stack();
    for _ in 0..8 {
        ctx.gc();
        if FINALIZED.load(core::sync::atomic::Ordering::SeqCst) >= before + 75 {
            break;
        }
    }
    assert!(
        FINALIZED.load(core::sync::atomic::Ordering::SeqCst) >= before + 75,
        "host finalizer should have run for the dead objects"
    );
}

#[inline(never)]
fn make_dead_host_objects(ctx: &Context, cls: *mut c_void) {
    for _ in 0..4 {
        let mut tmp = 0u64;
        unsafe { ffi::redjs_hostobject_create(ctx.raw(), cls, 25 as *mut c_void, 0, &mut tmp) };
        std::hint::black_box(&mut tmp);
    }
}

fn gc_and_stats(ctx: &Context) {
    let s0 = ctx.heap_stats();
    assert!(s0.heap_capacity > 0);

    // protect keeps a value across GC.
    let v = ctx.string("survives");
    ctx.protect(v);
    ctx.eval("for (let i = 0; i < 1000; i++) ({ junk: 'x'.repeat(100) })").unwrap();
    ctx.gc();
    assert_eq!(ctx.read_string(v).unwrap(), "survives");
    ctx.unprotect(v);

    let s1 = ctx.heap_stats();
    assert!(s1.object_count > 0);

    unsafe { ffi::redjs_report_extra_memory(ctx.raw(), 1024 * 1024) };
}

fn host_function_throw(ctx: &Context) {
    // Host fn returning after redjs_throw → JS sees the exception; the VM
    // exception state must not leak into subsequent evals.
    let v = ctx.eval("(() => { try { hostThrow(); return 'bad'; } catch { return 'caught'; } })()").unwrap();
    assert_eq!(ctx.to_rust_string(v).unwrap(), "caught");
    assert_eq!(ctx.eval("1 + 1").unwrap().as_number(), Some(2.0));

    let mut pending = 0u64;
    assert_eq!(unsafe { ffi::redjs_exception_pending(ctx.raw(), &mut pending) }, 0);
}
