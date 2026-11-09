"use strict";
// Feed this exact source to the pinned frontend, then aot-image compile.
// Host Web APIs deliberately cross the opaque embedding boundary.
const g = __VN_AOT_HOST__;
const value = { answer: 42, child: { text: "native" }, list: [1, 2, 3] };
g.__vn_aot_plain = value;
g.__vn_aot_array = value.list;
g.__vn_aot_map = new Map([["answer", 42]]);
g.__vn_aot_date = new Date(1234);
g.__vn_aot_error = new Error("native");
g.__vn_aot_callback = (n) => n === 0 ? 0 : 1 + g.__vn_aot_callback(n - 1);
const copied = Object.assign({}, { value: "ok" });
g.__vn_aot_builtins = JSON.stringify([Array.isArray(value.list), copied.value, g.__vn_aot_map.get("answer"), new Set([1, 1, 2]).size, /a+/.test("aaa"), "ABC".toLowerCase(), g.__vn_aot_date.getTime(), g.__vn_aot_callback(6)]);
try {
  const clone = g.structuredClone(value);
  g.__vn_aot_clone = clone.answer === 42 && clone.child.text === "native" && clone !== value ? "ok" : "bad";
} catch (error) { g.__vn_aot_clone = String(error); }
g.__vn_aot_url = new g.URL("https://example.invalid/path?x=1").hostname;
const target = new g.EventTarget();
let events = 0;
target.addEventListener("probe", () => events++);
target.dispatchEvent(new g.Event("probe"));
g.__vn_aot_event = events;
Promise.resolve(6).then((n) => { g.__vn_aot_async = g.__vn_aot_probe(n); });
g.setTimeout(() => { g.__vn_aot_answer = g.__vn_aot_probe(6); }, 0);

// A direct `Math.abs = ...` can be refused by the frontend itself. This write
// must execute dynamically through an opaque host object in compiled code.
// Math.abs is AlwaysLocked (table id 4108), independent of image use counts.
const opaqueMath = g.Math;
const originalAbs = opaqueMath.abs;
let rejected = false;
let refusal = "";
try { opaqueMath.abs = (n) => n; }
catch (error) { rejected = error instanceof TypeError; refusal = String(error); }
const unchanged = opaqueMath.abs === originalAbs;
if (!unchanged) opaqueMath.abs = originalAbs;
const restored = opaqueMath.abs === originalAbs && opaqueMath.abs(-7) === 7;
g.__vn_aot_math_guard = rejected && unchanged && restored ? "native-locked" :
  !rejected && !unchanged && restored ? "jit-writable" : "bad";
g.__vn_aot_math_refusal = refusal;
