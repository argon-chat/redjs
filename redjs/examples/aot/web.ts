declare var __SK_AOT_HOST__: any;
const g: any = __SK_AOT_HOST__;
const value = { answer: 42, child: { text: "native" }, list: [1, 2, 3] };
g.__sk_aot_plain = value;
g.__sk_aot_array = value.list;
g.__sk_aot_map = new Map([["answer", 42]]);
g.__sk_aot_date = new Date(1234);
g.__sk_aot_error = new Error("native");
g.__sk_aot_callback = (n: number): number => n === 0 ? 0 : 1 + g.__sk_aot_callback(n - 1);
const copied = Object.assign({}, { value: "ok" });
g.__sk_aot_builtins = JSON.stringify([Array.isArray(value.list), copied.value, g.__sk_aot_map.get("answer"), new Set([1,1,2]).size, /a+/.test("aaa"), "ABC".toLowerCase(), g.__sk_aot_date.getTime(), g.__sk_aot_callback(6)]);
try { const clone = g.structuredClone(value); g.__sk_aot_clone = clone.answer === 42 && clone.child.text === "native" && clone !== value ? "ok" : "bad"; }
catch (error) { g.__sk_aot_clone = String(error); }
g.__sk_aot_url = new g.URL("https://example.invalid/path?x=1").hostname;
const target = new g.EventTarget();
let events = 0;
target.addEventListener("probe", () => events++);
target.dispatchEvent(new g.Event("probe"));
g.__sk_aot_event = events;
Promise.resolve(6).then((n: number) => { g.__sk_aot_async = g.__sk_aot_probe(n); });
g.setTimeout(() => { g.__sk_aot_answer = g.__sk_aot_probe(6); }, 0);