//! `icu-root` with the full package handed over before the first context: every locale is back.

use redjs::api::Context;

#[test]
fn full_data_wins_over_the_linked_root() {
    let data = aligned(&std::fs::read(env!("REDJS_ICU_FULL_DATA")).unwrap());
    assert_eq!(redjs::icu::package().as_deref(), Some("icudt79l"));
    assert!(redjs::icu::set_common_data(&data[1..]).is_err());
    redjs::icu::set_common_data(data).unwrap();
    let ctx = Context::new();
    assert!(redjs::icu::set_common_data(data).is_err());
    let eval = |src: &str| {
        let v = ctx.eval(src).unwrap_or_else(|e| panic!("{src}: {e:?}"));
        ctx.to_rust_string(v).unwrap()
    };
    let date = "new Date(Date.UTC(2024, 0, 15, 12))";
    assert_eq!(
        eval(&format!(
            "new Intl.DateTimeFormat('de', {{ month: 'long', timeZone: 'UTC' }}).format({date})"
        )),
        "Januar"
    );
    assert_eq!(eval("(1234.5).toLocaleString('de')"), "1.234,5");
    assert_eq!(
        eval("['b', 'a', 'ä'].sort(new Intl.Collator('sv').compare).join('')"),
        "abä"
    );
    assert_eq!(
        eval("new Intl.DisplayNames('en', { type: 'region' }).of('DE')"),
        "Germany"
    );
}

fn aligned(bytes: &[u8]) -> &'static [u8] {
    let words: &'static mut [u128] =
        Box::leak(vec![0u128; bytes.len().div_ceil(16)].into_boxed_slice());
    // SAFETY: the leaked u128 buffer is at least bytes.len() bytes long and lives forever.
    let out =
        unsafe { core::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), bytes.len()) };
    out.copy_from_slice(bytes);
    out
}
