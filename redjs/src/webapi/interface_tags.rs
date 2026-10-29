//! Every interface prototype carries `Symbol.toStringTag` (WebIDL §3.7.3).
//!
//! `Object.prototype.toString` is how code classifies objects. Vue's
//! `reactive()` reads it to decide what it may wrap: `"Object"` and `"Array"`
//! are proxied, every other tag is left raw. A platform object without a tag
//! reads `[object Object]`, gets wrapped, and then its native accessors refuse
//! the Proxy as a foreign `this` — the Argon client's Settings → Audio crashed
//! that way on `MediaDeviceInfo.deviceId` (research/reports/m26/
//! client-sweep-1.md, finding 6). `define_class` now tags every prototype it
//! builds; this test walks the global object so a class defined some other
//! way (a JS shim in bootstrap.js, a hand-built prototype) cannot regress it
//! silently.

#[cfg(test)]
mod tests {
    use crate::webapi::testing::*;

    /// The interfaces a page constructs or receives, with the tag a browser
    /// gives each. The audit below covers everything on the global; this list
    /// pins the ones the finding was about, by name.
    const PINNED: &[&str] = &[
        "Blob", "File", "FileList", "Headers", "Request", "Response", "FormData", "URL",
        "URLSearchParams", "AbortController", "AbortSignal", "Event", "EventTarget",
        "DOMException", "TextEncoder", "TextDecoder", "WebSocket", "XMLHttpRequest",
        "ImageBitmap", "ImageData", "Path2D", "DOMMatrix", "Navigator",
    ];

    /// The audit: every global function with an own `prototype` object whose
    /// name is capitalised, minus ECMAScript's own constructors (whose
    /// prototypes have no tag in the language either). Returns the names
    /// whose prototype tag is missing or different from the name.
    const AUDIT: &str = r#"(() => {
        const es = new Set(["Object", "Function", "Array", "Number", "Boolean", "String", "RegExp",
          "Date", "Error", "EvalError", "RangeError", "ReferenceError", "SyntaxError", "TypeError",
          "URIError", "AggregateError", "SuppressedError", "Proxy", "SharedArrayBuffer"]);
        // ECMAScript's typed arrays answer through a getter on
        // %TypedArray%.prototype, which is undefined on the prototypes themselves.
        const typedArrayProto = Object.getPrototypeOf(Int8Array.prototype);
        const bad = [];
        for (const name of Object.getOwnPropertyNames(globalThis)) {
          if (!/^[A-Z]/.test(name) || es.has(name)) continue;
          let v;
          try { v = globalThis[name]; } catch { continue; }
          if (typeof v !== "function") continue;
          const d = Object.getOwnPropertyDescriptor(v, "prototype");
          if (!d || !d.value || typeof d.value !== "object") continue;
          if (Object.getPrototypeOf(d.value) === typedArrayProto) continue;
          // A second name for another interface (`Image` is HTMLImageElement's
          // legacy factory, `Node` our Element) keeps that interface's tag.
          if (d.value.constructor !== v) continue;
          if (v.name !== name && d.value[Symbol.toStringTag] === v.name) continue;
          if (d.value[Symbol.toStringTag] !== name) bad.push(name + "=" + String(d.value[Symbol.toStringTag]));
        }
        return bad.sort().join(",");
    })()"#;

    #[test]
    fn every_interface_prototype_is_tagged_with_its_name() {
        let c = ctx();
        let bad = s(&c, AUDIT);
        let unexpected: Vec<&str> = bad.split(',').filter(|e| !e.is_empty()).collect();
        assert!(unexpected.is_empty(), "interface prototypes without their own @@toStringTag: {unexpected:?}");
    }

    #[test]
    fn the_pinned_interfaces_classify_as_themselves() {
        let c = ctx();
        for name in PINNED {
            let got = s(
                &c,
                &format!(
                    "typeof {name} === 'function' ? Object.prototype.toString.call(Object.create({name}.prototype)) : 'absent'"
                ),
            );
            assert_eq!(got, format!("[object {name}]"), "{name}");
        }
    }

    /// The tag has WebIDL's attributes: readable, not writable, not
    /// enumerable, configurable.
    #[test]
    fn the_tag_is_a_non_writable_non_enumerable_configurable_data_property() {
        let c = ctx();
        ok(
            &c,
            "(() => { const d = Object.getOwnPropertyDescriptor(Blob.prototype, Symbol.toStringTag); \
             return d.value === 'Blob' && !d.writable && !d.enumerable && d.configurable; })()",
        );
    }

    /// What Vue's `reactive()` decides from it (`toRawType` in @vue/shared):
    /// a tagged platform object is not wrapped.
    #[test]
    fn vue_would_leave_a_platform_object_raw() {
        let c = ctx();
        ok(
            &c,
            "(() => { const raw = (v) => Object.prototype.toString.call(v).slice(8, -1); \
             const proxied = (v) => raw(v) === 'Object' || raw(v) === 'Array'; \
             return !proxied(new Blob([])) && !proxied(new Headers()) && !proxied(new URL('https://a.b/')) \
               && proxied({}) && proxied([]); })()",
        );
    }

    #[cfg(feature = "audio")]
    #[test]
    fn a_media_device_info_is_not_a_plain_object() {
        let c = ctx();
        ok(&c, "MediaDeviceInfo.prototype[Symbol.toStringTag] === 'MediaDeviceInfo'");
        ok(&c, "AudioContext.prototype[Symbol.toStringTag] === 'AudioContext'");
        ok(&c, "GainNode.prototype[Symbol.toStringTag] === 'GainNode'");
    }
}
