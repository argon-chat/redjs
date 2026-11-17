//! `icu-root` with no data handed over: Intl works on the root locale and nothing throws.

use redjs::api::Context;

#[test]
fn root_locale_only() {
    assert_eq!(redjs::icu::LINKED, redjs::icu::Linked::Root);
    let ctx = Context::new();
    let eval = |src: &str| {
        let v = ctx.eval(src).unwrap_or_else(|e| panic!("{src}: {e:?}"));
        ctx.to_rust_string(v).unwrap()
    };
    let date = "new Date(Date.UTC(2024, 0, 15, 12))";
    assert_eq!(
        eval(&format!(
            "new Intl.DateTimeFormat('de', {{ month: 'long', timeZone: 'UTC' }}).format({date})"
        )),
        "M01"
    );
    assert_eq!(eval("(1234.5).toLocaleString('de')"), "1,234.5");
    assert_eq!(
        eval("['b', 'a', 'ä'].sort(new Intl.Collator('sv').compare).join('')"),
        "aäb"
    );
    assert_eq!(
        eval("new Intl.DisplayNames('en', { type: 'region' }).of('DE')"),
        "DE"
    );
    assert_eq!(eval("[...new Intl.Segmenter('ja', { granularity: 'word' }).segment('日本語です')].length > 0"), "true");
    assert_eq!(eval("'ǅ'.normalize('NFKC')"), "Dž");
}
