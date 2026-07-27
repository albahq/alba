//! Snapshot tests for rendered diagnostics: source line, caret under the
//! exact span, and help text.

use alba_syntax::{parse, render_diagnostic};

#[test]
fn renders_unknown_field_with_caret_and_help() {
    let src = "beam x {\n  descriptoin \"typo\"\n}";
    let err = parse(src).unwrap_err();
    let rendered = render_diagnostic(src, "Beamfile", &err.into_diagnostic());
    insta::assert_snapshot!(rendered);
}

#[test]
fn renders_unterminated_string() {
    let src = "beam x {\n  run \"oops\n}";
    let err = parse(src).unwrap_err();
    let rendered = render_diagnostic(src, "Beamfile", &err.into_diagnostic());
    insta::assert_snapshot!(rendered);
}
