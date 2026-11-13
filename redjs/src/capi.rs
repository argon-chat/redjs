//! The subset of JSC's stable C API we use directly (everything else goes
//! through `cpp/glue.cpp`). On 64-bit JSC, `JSValueRef`/`JSObjectRef` are
//! bit-identical to `EncodedJSValue` for cells, and `JSContextRef` is the
//! `JSGlobalObject*` — so our `u64` values and the glue's context pointer
//! cast straight into the C API (see APICast.h).

use core::ffi::c_void;

pub type JSContextRef = *mut c_void;
pub type JSGlobalContextRef = *mut c_void;
pub type JSValueRef = *const c_void;
pub type JSObjectRef = *mut c_void;
pub type JSClassRef = *mut c_void;
pub type JSStringRef = *mut c_void;
pub type JSPropertyNameAccumulatorRef = *mut c_void;

/// `void (*)(void* bytes, void* deallocatorContext)` — plain C convention,
/// safe for C# `[UnmanagedCallersOnly]` delegates.
pub type JSTypedArrayBytesDeallocator = Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>;

pub type JSObjectFinalizeCallback = Option<unsafe extern "C" fn(JSObjectRef)>;

/// C API typed-array type ids (JSValueRef.h). Note: no Float16 here.
pub const K_JS_TYPED_ARRAY_TYPE_INT8: u32 = 0;
pub const K_JS_TYPED_ARRAY_TYPE_INT16: u32 = 1;
pub const K_JS_TYPED_ARRAY_TYPE_INT32: u32 = 2;
pub const K_JS_TYPED_ARRAY_TYPE_UINT8: u32 = 3;
pub const K_JS_TYPED_ARRAY_TYPE_UINT8_CLAMPED: u32 = 4;
pub const K_JS_TYPED_ARRAY_TYPE_UINT16: u32 = 5;
pub const K_JS_TYPED_ARRAY_TYPE_UINT32: u32 = 6;
pub const K_JS_TYPED_ARRAY_TYPE_FLOAT32: u32 = 7;
pub const K_JS_TYPED_ARRAY_TYPE_FLOAT64: u32 = 8;
pub const K_JS_TYPED_ARRAY_TYPE_NONE: u32 = 10;
pub const K_JS_TYPED_ARRAY_TYPE_BIGINT64: u32 = 11;
pub const K_JS_TYPED_ARRAY_TYPE_BIGUINT64: u32 = 12;

/// Maps our stable `RjsTaKind` (ffi.rs / glue.cpp) to the C API enum.
/// Returns `K_JS_TYPED_ARRAY_TYPE_NONE` for unsupported kinds (DataView, f16).
pub fn ta_kind_to_capi(kind: u32) -> u32 {
    match kind {
        1 => K_JS_TYPED_ARRAY_TYPE_INT8,
        2 => K_JS_TYPED_ARRAY_TYPE_UINT8,
        3 => K_JS_TYPED_ARRAY_TYPE_UINT8_CLAMPED,
        4 => K_JS_TYPED_ARRAY_TYPE_INT16,
        5 => K_JS_TYPED_ARRAY_TYPE_UINT16,
        6 => K_JS_TYPED_ARRAY_TYPE_INT32,
        7 => K_JS_TYPED_ARRAY_TYPE_UINT32,
        8 => K_JS_TYPED_ARRAY_TYPE_FLOAT32,
        9 => K_JS_TYPED_ARRAY_TYPE_FLOAT64,
        10 => K_JS_TYPED_ARRAY_TYPE_BIGINT64,
        11 => K_JS_TYPED_ARRAY_TYPE_BIGUINT64,
        _ => K_JS_TYPED_ARRAY_TYPE_NONE,
    }
}

/// JSClassDefinition (JSObjectRef.h). We populate `class_name`, `finalize` and, for named-property classes,
/// the property callbacks; layout must match the header exactly.
#[repr(C)]
pub struct JSClassDefinition {
    pub version: i32,
    pub attributes: u32, // kJSClassAttributeNone = 0
    pub class_name: *const core::ffi::c_char,
    pub parent_class: JSClassRef,
    pub static_values: *const c_void,
    pub static_functions: *const c_void,
    pub initialize: *const c_void,
    pub finalize: JSObjectFinalizeCallback,
    pub has_property: *const c_void,
    pub get_property: *const c_void,
    pub set_property: *const c_void,
    pub delete_property: *const c_void,
    pub get_property_names: *const c_void,
    pub call_as_function: *const c_void,
    pub call_as_constructor: *const c_void,
    pub has_instance: *const c_void,
    pub convert_to_type: *const c_void,
}

impl JSClassDefinition {
    pub const fn empty() -> Self {
        Self {
            version: 0,
            attributes: 0,
            class_name: core::ptr::null(),
            parent_class: core::ptr::null_mut(),
            static_values: core::ptr::null(),
            static_functions: core::ptr::null(),
            initialize: core::ptr::null(),
            finalize: None,
            has_property: core::ptr::null(),
            get_property: core::ptr::null(),
            set_property: core::ptr::null(),
            delete_property: core::ptr::null(),
            get_property_names: core::ptr::null(),
            call_as_function: core::ptr::null(),
            call_as_constructor: core::ptr::null(),
            has_instance: core::ptr::null(),
            convert_to_type: core::ptr::null(),
        }
    }
}

extern "C" {
    pub fn JSGlobalContextCreate(global_object_class: JSClassRef) -> JSGlobalContextRef;
    pub fn JSGlobalContextRelease(ctx: JSGlobalContextRef);
    pub fn JSContextGetGlobalObject(ctx: JSContextRef) -> JSObjectRef;

    pub fn JSValueProtect(ctx: JSContextRef, value: JSValueRef);
    pub fn JSValueUnprotect(ctx: JSContextRef, value: JSValueRef);

    pub fn JSObjectMakeDeferredPromise(
        ctx: JSContextRef,
        resolve: *mut JSObjectRef,
        reject: *mut JSObjectRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;

    pub fn JSObjectMakeTypedArray(
        ctx: JSContextRef,
        array_type: u32,
        length: usize,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    pub fn JSObjectMakeTypedArrayWithBytesNoCopy(
        ctx: JSContextRef,
        array_type: u32,
        bytes: *mut c_void,
        byte_length: usize,
        deallocator: JSTypedArrayBytesDeallocator,
        deallocator_context: *mut c_void,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    pub fn JSObjectMakeTypedArrayWithArrayBufferAndOffset(
        ctx: JSContextRef,
        array_type: u32,
        buffer: JSObjectRef,
        byte_offset: usize,
        length: usize,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    pub fn JSObjectMakeArrayBufferWithBytesNoCopy(
        ctx: JSContextRef,
        bytes: *mut c_void,
        byte_length: usize,
        deallocator: JSTypedArrayBytesDeallocator,
        deallocator_context: *mut c_void,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;

    /// `JSContextRefPrivate.h` — the unhandled-promise-rejection hook, the
    /// context-level equivalent of `window.onunhandledrejection`. `function`
    /// receives `(promise, reason)`. Present and functional in this prebuilt
    /// (verified against the shipped export table); the slot is null until set,
    /// which is exactly why rejections used to vanish. See `errors.rs`.
    pub fn JSGlobalContextSetUnhandledRejectionCallback(
        ctx: JSGlobalContextRef,
        function: JSObjectRef,
        exception: *mut JSValueRef,
    );

    pub fn JSClassCreate(definition: *const JSClassDefinition) -> JSClassRef;
    pub fn JSClassRelease(js_class: JSClassRef);
    pub fn JSObjectMake(ctx: JSContextRef, js_class: JSClassRef, data: *mut c_void) -> JSObjectRef;
    pub fn JSObjectGetPrivate(object: JSObjectRef) -> *mut c_void;
    pub fn JSObjectSetPrivate(object: JSObjectRef, data: *mut c_void) -> bool;
    pub fn JSObjectSetPrototype(ctx: JSContextRef, object: JSObjectRef, value: JSValueRef);
    /// The object's [[Prototype]] slot as stored (no Proxy trap runs).
    pub fn JSObjectGetPrototype(ctx: JSContextRef, object: JSObjectRef) -> JSValueRef;
    /// Brand check: is `value` an object created from `js_class`?
    pub fn JSValueIsObjectOfClass(ctx: JSContextRef, value: JSValueRef, js_class: JSClassRef) -> bool;
    pub fn JSStringCreateWithCharacters(chars: *const u16, len: usize) -> JSStringRef;
    pub fn JSStringRelease(string: JSStringRef);
    pub fn JSStringGetLength(string: JSStringRef) -> usize;
    pub fn JSStringGetCharactersPtr(string: JSStringRef) -> *const u16;
    pub fn JSPropertyNameAccumulatorAddName(accumulator: JSPropertyNameAccumulatorRef, name: JSStringRef);
}
