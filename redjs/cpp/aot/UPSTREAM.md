# Supplemental WebKit headers

The AOT prebuilt includes its generated builtin table sources but omits these
private headers they include. These are unmodified copies from
`oven-sh/WebKit@4d45755fed4aeef19e95beb90d3d0c884a2812bd`:

| Local file | Upstream source | Git blob SHA-1 |
|---|---|---|
| config.h | [Source/JavaScriptCore/config.h](https://github.com/oven-sh/WebKit/blob/4d45755fed4aeef19e95beb90d3d0c884a2812bd/Source/JavaScriptCore/config.h) | `27f1675c8544d4d23f5eab80e2db28d7a4b8babb` |
| BuiltinExecutables.h | [Source/JavaScriptCore/builtins/BuiltinExecutables.h](https://github.com/oven-sh/WebKit/blob/4d45755fed4aeef19e95beb90d3d0c884a2812bd/Source/JavaScriptCore/builtins/BuiltinExecutables.h) | `b079b54e3782f6b69418e0c223e96f8769d4791f` |

Their original copyright and license notices are retained in each file.
The generated table sources themselves are compiled directly from the
hash-verified prebuilt archive; no substitute builtin table is maintained here.
When updating JSC, review these supplemental headers against the new pin as well.
