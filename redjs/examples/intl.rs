//! ICU/Intl probe.
fn main() {
    unsafe {
        redjs::glue::rjs_init(core::ptr::null());
        let g = redjs::ffi::redjs_context_create();
        for src in [
            "typeof Intl",
            "(() => { try { return new Date(0).toLocaleTimeString(); } catch (e) { return 'ERR: ' + e.message; } })()",
            "(() => { try { return new Intl.DateTimeFormat().format(new Date(0)); } catch (e) { return 'ERR: ' + e.message; } })()",
            "(() => { try { return new Intl.NumberFormat().format(1234.5); } catch (e) { return 'ERR: ' + e.message; } })()",
            "(() => { try { return Intl.DateTimeFormat.supportedLocalesOf(['en-US']).join(); } catch (e) { return 'ERR: ' + e.message; } })()",
        ] {
            let mut out = 0u64;
            let rc = redjs::glue::rjs_eval8(g, src.as_ptr(), src.len(), core::ptr::null(), 0, &mut out);
            let mut s = 0u64;
            redjs::glue::rjs_to_string(g, out, &mut s);
            let mut info = redjs::ffi::RjsStr::default();
            redjs::glue::rjs_string_info(g, s, &mut info);
            let txt = if info.is8bit != 0 {
                core::slice::from_raw_parts(info.ptr as *const u8, info.len as usize).iter().map(|&b| b as char).collect::<String>()
            } else {
                String::from_utf16_lossy(core::slice::from_raw_parts(info.ptr as *const u16, info.len as usize))
            };
            println!("rc={rc} {src} => {txt}");
        }
    }
}
