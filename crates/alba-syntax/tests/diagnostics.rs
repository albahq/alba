//! Snapshot tests for rendered diagnostics: source line, caret under the
//! exact span, and help text.

use alba_syntax::{Diagnostic, Span, parse, render_diagnostic};

/// `Diagnostic::new` is the constructor other crates (starting with
/// `alba-core`'s `CoreError::into_diagnostic`) use to lift their own
/// spanned errors into something `render_diagnostic` can render, without
/// this crate exposing `Diagnostic`'s private fields. Exercised here, from
/// outside the crate, so a regression that makes `new` private again fails
/// to compile instead of silently only being caught inside `alba-core`.
#[test]
fn diagnostic_new_is_a_public_constructor_for_external_crates() {
    let diagnostic = Diagnostic::new(
        "custom error",
        Some(Span::new(1, 4)),
        Some("try this".into()),
    );
    let rendered = render_diagnostic("a boom b", "Beamfile", &diagnostic);

    assert!(rendered.contains("custom error"));
    assert!(rendered.contains("try this"));
}

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
