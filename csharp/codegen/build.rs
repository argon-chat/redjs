// Generates the C# P/Invoke bindings for the redjs C ABI:
//
//   ../../redjs/src/ffi.rs  --csbindgen-->  ../RedJs/NativeMethods.g.cs
//
// Run with:  cargo build
//
// ffi.rs is deliberately self-contained (all repr(C) structs + only
// pointer/primitive types in signatures) so csbindgen can mirror it 1:1.
// The hand-written layer (JsValue/JsContext/JsCallFrame) lives next to the
// generated file and is NOT generated.
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let ffi = manifest.join("../../redjs/src/ffi.rs");
    let cs_out = manifest.join("../RedJs/NativeMethods.g.cs");

    println!("cargo:rerun-if-changed={}", ffi.display());

    csbindgen::Builder::default()
        .input_extern_file(&ffi)
        .csharp_dll_name("redjs")
        .csharp_namespace("RedJs.Native")
        .csharp_class_name("NativeMethods")
        .csharp_entry_point_prefix("")
        .csharp_method_prefix("")
        .generate_csharp_file(&cs_out)
        .expect("csbindgen failed to generate C#");
}
