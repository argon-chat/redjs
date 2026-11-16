# Supplemental WebKit headers

The AOT prebuilt includes its generated builtin table sources but omits these
private headers they include. These are unmodified copies from
`oven-sh/WebKit@3bc94211496460feda299ef85eef710eb67fafb3`:

| Local file | Upstream source | Git blob SHA-1 |
|---|---|---|
| config.h | [Source/JavaScriptCore/config.h](https://github.com/oven-sh/WebKit/blob/3bc94211496460feda299ef85eef710eb67fafb3/Source/JavaScriptCore/config.h) | `27f1675c8544d4d23f5eab80e2db28d7a4b8babb` |
| BuiltinExecutables.h | [Source/JavaScriptCore/builtins/BuiltinExecutables.h](https://github.com/oven-sh/WebKit/blob/3bc94211496460feda299ef85eef710eb67fafb3/Source/JavaScriptCore/builtins/BuiltinExecutables.h) | `53153d6e9294f6e355b7e304dcf5f672c17f6965` |

Their original copyright and license notices are retained in each file.
The generated table sources themselves are compiled directly from the
hash-verified prebuilt archive; no substitute builtin table is maintained here.
When updating JSC, review these supplemental headers against the new pin as well.
