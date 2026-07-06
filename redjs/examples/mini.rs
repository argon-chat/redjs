//! Stepwise bring-up probe: run with `cargo run --example mini` to see the
//! exact stage where the engine dies (fastfail prints nothing otherwise).

use std::io::Write;

fn step(s: &str) {
    println!("[mini] {s}");
    std::io::stdout().flush().unwrap();
}

fn main() {
    if std::env::var_os("MINI_THREAD").is_some() {
        step("spawning worker thread (test-harness simulation)...");
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(run)
            .unwrap()
            .join()
            .unwrap();
        step("worker joined");
        return;
    }
    run();
}

fn run() {
    step("start");
    unsafe {
        step("rjs_init...");
        let rc = redjs::glue::rjs_init(core::ptr::null());
        step(&format!("rjs_init rc={rc}"));

        step("context create...");
        let g = redjs::ffi::redjs_context_create();
        step(&format!("context = {g:p}"));

        step("layout...");
        let mut l = redjs::glue::RjsLayout::default();
        redjs::glue::rjs_layout(&mut l);
        step(&format!(
            "layout: abi={} argc@{} this@{} args@{} undef={:#x} numtag={:#x}",
            l.abi_version, l.cf_argc_offset, l.cf_this_offset, l.cf_args_offset, l.value_undefined, l.tag_number
        ));

        step("eval 6*7...");
        let mut out = 0u64;
        let rc = redjs::glue::rjs_eval8(g, "6*7".as_ptr(), 3, core::ptr::null(), 0, &mut out);
        step(&format!("eval rc={rc} out={out:#x}"));

        step("eval [1,2,3]...");
        let mut js_arr = 0u64;
        let src = "[1,2,3]";
        let rc = redjs::glue::rjs_eval8(g, src.as_ptr(), src.len(), core::ptr::null(), 0, &mut js_arr);
        step(&format!("eval arr rc={rc}"));

        step("set_index on eval'd array...");
        let rc = redjs::glue::rjs_set_index(g, js_arr, 1, l.value_true);
        step(&format!("set_index(js arr) rc={rc}"));

        step("rjs_array_create(3)...");
        let mut arr = 0u64;
        let rc = redjs::glue::rjs_array_create(g, 3, &mut arr);
        step(&format!("array_create rc={rc} kind={}", redjs::glue::rjs_kind(g, arr)));

        step("get_index on fresh array...");
        let mut item = 0u64;
        let rc = redjs::glue::rjs_get_index(g, arr, 0, &mut item);
        step(&format!("get_index rc={rc} item={item:#x}"));

        step("set_index on fresh array...");
        let rc = redjs::glue::rjs_set_index(g, arr, 0, l.value_true);
        step(&format!("set_index rc={rc}"));

        step("done");
    }
}
