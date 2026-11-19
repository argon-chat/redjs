pub(super) use super::util::*;
pub(super) use crate::glue;
pub(super) use core::ffi::c_void;

pub(super) type JsResult = Result<u64, u64>;

#[derive(Clone, Copy)]
pub(super) struct Env {
    pub g: *mut c_void,
    slot_key: *mut c_void,
    pub interfaces: u64,
}

impl Env {
    pub unsafe fn new(g: *mut c_void, slot_key: *mut c_void, interfaces: u64) -> Self {
        Self {
            g,
            slot_key,
            interfaces,
        }
    }

    pub unsafe fn record(self) -> u64 {
        new_object(self.g, NULL)
    }

    pub unsafe fn put(self, o: u64, key: &str, value: u64) {
        define(self.g, o, key, value, 0);
    }

    pub unsafe fn field(self, o: u64, key: &str) -> u64 {
        get(self.g, o, key)
    }

    pub unsafe fn prop(self, o: u64, name: &str) -> JsResult {
        let mut value = UNDEF;
        if glue::rjs_get(self.g, o, key(self.g, name), &mut value) == 0 {
            Ok(value)
        } else {
            Err(value)
        }
    }

    pub unsafe fn slot(self, o: u64) -> u64 {
        glue::rjs_private_slot(self.g, o, self.slot_key)
    }

    pub unsafe fn attach(self, o: u64, state: u64) {
        glue::rjs_define(self.g, o, self.slot_key, state, 0);
    }

    pub unsafe fn register(self, spec: ClassSpec<'_>) -> Class {
        let class = define_class(self.g, &spec);
        self.put(self.interfaces, spec.name, class.ctor);
        class
    }

    pub unsafe fn ctor(self, name: &str) -> u64 {
        self.field(self.interfaces, name)
    }

    pub unsafe fn symbol(self, obj: u64, which: u32) -> JsResult {
        let atom = glue::rjs_atom_wellknown(self.g, which);
        let mut value = UNDEF;
        let status = glue::rjs_get(self.g, obj, atom, &mut value);
        glue::rjs_atom_free(self.g, atom);
        if status == 0 {
            Ok(value)
        } else {
            Err(value)
        }
    }

    pub unsafe fn instance(self, name: &str, new_target: u64, data: u64) -> JsResult {
        let ctor = self.ctor(name);
        let proto = if new_target == UNDEF {
            UNDEF
        } else {
            self.prop(new_target, "prototype")?
        };
        let proto = if glue::rjs_kind(self.g, proto) >= 8 {
            proto
        } else {
            self.field(ctor, "prototype")
        };
        let obj = new_object(self.g, proto);
        self.put(data, "brand", ctor);
        self.attach(obj, data);
        Ok(obj)
    }

    pub unsafe fn try_data(self, obj: u64, name: &str) -> Option<u64> {
        let data = self.slot(obj);
        if data != UNDEF && self.field(data, "brand") == self.ctor(name) {
            return Some(data);
        }
        None
    }

    pub unsafe fn data(self, obj: u64, name: &str) -> JsResult {
        self.try_data(obj, name)
            .ok_or_else(|| self.type_error(&format!("This operation requires a {name}")))
    }

    pub unsafe fn error(self, kind: u32, message: &str) -> u64 {
        let message: Vec<_> = message.encode_utf16().collect();
        let mut value = UNDEF;
        glue::rjs_error_create(self.g, kind, message.as_ptr(), message.len(), &mut value);
        value
    }

    pub unsafe fn type_error(self, message: &str) -> u64 {
        self.error(1, message)
    }

    pub unsafe fn number(self, value: u64) -> Result<f64, u64> {
        if let Some(n) = dec_f64(value) {
            return Ok(n);
        }
        let (mut number, mut error) = (0., UNDEF);
        if glue::rjs_to_number_x(self.g, value, &mut number, &mut error) == 0 {
            Ok(number)
        } else {
            Err(error)
        }
    }

    pub unsafe fn method(self, obj: u64, key: &str) -> JsResult {
        let f = self.prop(obj, key)?;
        if f != UNDEF && glue::rjs_kind(self.g, f) != 9 {
            return Err(self.type_error(&format!("{key} must be a function")));
        }
        Ok(f)
    }

    pub unsafe fn invoke(self, f: u64, receiver: u64, args: &[u64]) -> JsResult {
        if f == UNDEF {
            Ok(UNDEF)
        } else {
            call(self.g, f, receiver, args)
        }
    }

    pub unsafe fn throws(self, result: JsResult) -> u64 {
        match result {
            Ok(v) => v,
            Err(e) => {
                glue::rjs_throw(self.g, e);
                UNDEF
            }
        }
    }

    pub unsafe fn promises(self, result: JsResult) -> u64 {
        match result {
            Ok(v) => v,
            Err(e) => self.rejected(e),
        }
    }

    pub unsafe fn pending(self) -> u64 {
        glue::rjs_promise_pending(self.g)
    }

    pub unsafe fn handled(self, p: u64) -> u64 {
        glue::rjs_promise_mark_handled(self.g, p);
        p
    }

    pub unsafe fn settle(self, promise: u64, result: JsResult) {
        let (value, rejected) = match result {
            Ok(v) => (v, false),
            Err(e) => (e, true),
        };
        glue::rjs_promise_settle(self.g, promise, value, rejected);
    }

    pub unsafe fn resolved(self, value: u64) -> u64 {
        let p = self.pending();
        self.settle(p, Ok(value));
        p
    }

    pub unsafe fn rejected(self, error: u64) -> u64 {
        let p = self.pending();
        self.settle(p, Err(error));
        p
    }

    pub unsafe fn callback(self, f: HostFn, data: u64) -> u64 {
        let fun = make_fn(self.g, "", 1, f);
        self.attach(fun, data);
        fun
    }

    pub unsafe fn capture(self, frame: *mut c_void) -> u64 {
        self.slot(crate::api::HostArgs::new(frame).callee().0)
    }

    pub unsafe fn construct(self, ctor: u64, args: &[u64]) -> JsResult {
        let mut value = UNDEF;
        if glue::rjs_construct(self.g, ctor, args.as_ptr(), args.len() as u32, &mut value) == 0 {
            Ok(value)
        } else {
            Err(value)
        }
    }

    pub unsafe fn string(self, value: u64) -> JsResult {
        let mut text = UNDEF;
        if glue::rjs_to_string(self.g, value, &mut text) == 0 {
            Ok(text)
        } else {
            Err(text)
        }
    }

    pub unsafe fn then(
        self,
        value: u64,
        data: u64,
        ok: Option<HostFn>,
        err: Option<HostFn>,
    ) -> u64 {
        let ok = ok.map_or(UNDEF, |f| self.callback(f, data));
        let err = err.map_or(UNDEF, |f| self.callback(f, data));
        glue::rjs_promise_chain(self.g, value, ok, err)
    }

    pub unsafe fn dictionary(self, obj: u64) -> JsResult {
        if matches!(obj, UNDEF | NULL) {
            Ok(self.record())
        } else if glue::rjs_kind(self.g, obj) >= 8 {
            Ok(obj)
        } else {
            Err(self.type_error("Expected an options object"))
        }
    }

    pub unsafe fn hwm(self, value: u64, fallback: f64) -> Result<f64, u64> {
        let n = if value == UNDEF {
            fallback
        } else {
            self.number(value)?
        };
        if n.is_nan() || n < 0. {
            Err(self.error(2, "highWaterMark must be non-negative"))
        } else {
            Ok(n)
        }
    }

    // Queue cells and user values live in the GC heap, including cycles through a stream or a callback.
    pub unsafe fn push(self, queue: u64, value: u64) {
        let node = self.record();
        self.put(node, "value", value);
        let tail = self.field(queue, "tail");
        if tail == UNDEF {
            self.put(queue, "head", node);
        } else {
            self.put(tail, "next", node);
        }
        self.put(queue, "tail", node);
    }

    pub unsafe fn pop(self, queue: u64) -> Option<u64> {
        let head = self.field(queue, "head");
        if head == UNDEF {
            return None;
        }
        let next = self.field(head, "next");
        self.put(queue, "head", next);
        if next == UNDEF {
            self.put(queue, "tail", UNDEF);
        }
        Some(self.field(head, "value"))
    }

    pub unsafe fn iter_result(self, value: u64, done: bool) -> u64 {
        let result = new_object(self.g, UNDEF);
        self.put(result, "value", value);
        self.put(result, "done", enc_bool(done));
        result
    }
}

macro_rules! native_callback {
    ($env:path, $name:ident($cx:ident, $cf:ident) $mode:ident $body:block) => {
        #[crate::host_fn]
        unsafe fn $name(g: *mut c_void, $cf: *mut c_void) -> u64 {
            let $cx = $env(g);
            let result: JsResult = (|| $body)();
            $cx.$mode(result)
        }
    };
}
