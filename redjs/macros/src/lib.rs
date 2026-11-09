//! JSC uses SysV on Windows x64 and the platform C ABI on Unix.
//! Emit target cfgs rather than inspecting the proc-macro host: cross builds
//! must select the ABI of the application, not the compiler's machine.

use proc_macro::{Ident, Literal, TokenStream, TokenTree};

#[proc_macro_attribute]
pub fn host_fn(arguments: TokenStream, item: TokenStream) -> TokenStream {
    if !arguments.is_empty() {
        return "compile_error!(\"host_fn takes no arguments\");"
            .parse()
            .unwrap();
    }
    fn with_abi(item: TokenStream, abi: &str) -> TokenStream {
        let mut output = TokenStream::new();
        let mut inserted = false;
        for token in item {
            if !inserted && matches!(&token, TokenTree::Ident(name) if name.to_string() == "fn") {
                output.extend([
                    TokenTree::Ident(Ident::new("extern", token.span())),
                    TokenTree::Literal(Literal::string(abi)),
                ]);
                inserted = true;
            }
            output.extend([token]);
        }
        if !inserted {
            return "compile_error!(\"host_fn requires a function\");"
                .parse()
                .unwrap();
        }
        output
    }
    let mut output: TokenStream = "#[cfg(all(target_os = \"windows\", target_arch = \"x86_64\"))]"
        .parse()
        .unwrap();
    output.extend(with_abi(item.clone(), "sysv64"));
    output.extend(
        "#[cfg(not(all(target_os = \"windows\", target_arch = \"x86_64\")))]"
            .parse::<TokenStream>()
            .unwrap(),
    );
    output.extend(with_abi(item, "C"));
    output
}
