//! Turns spanned parser/lexer errors into rendered, Rust-quality
//! diagnostics: the source line, a caret under the exact span, and help
//! text.
//!
//! [`Diagnostic`] implements [`miette::Diagnostic`] so it can be handed to
//! any of miette's report handlers. [`render_diagnostic`] wraps that up
//! with a fixed, platform-independent [`miette::GraphicalReportHandler`]
//! configuration, since a rendered report is otherwise sensitive to color
//! support, terminal width, and unicode support, which vary across
//! machines and CI runners.

use miette::{Diagnostic as MietteDiagnostic, GraphicalReportHandler, GraphicalTheme};
use miette::{LabeledSpan, NamedSource, SourceCode, SourceSpan};
use std::fmt;

use crate::parser::ParseError;
use crate::token::Span;

/// A renderable diagnostic: a message, an optional label on the offending
/// span, and optional help text. Built from a [`ParseError`] via
/// [`ParseError::into_diagnostic`].
///
/// Deliberately does not carry the source text or a display path: those are
/// supplied separately to [`render_diagnostic`], matching how a caller
/// (typically a CLI reading a file) already has both on hand and shouldn't
/// need to clone the source into every error it produces.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    message: String,
    span: Option<SourceSpan>,
    help: Option<String>,
}

impl Diagnostic {
    /// Creates a diagnostic from a message, an optional byte-offset span
    /// into the source, and optional help text.
    ///
    /// This is the constructor other crates use to lift their own errors
    /// into something [`render_diagnostic`] can render, without this crate
    /// exposing `Diagnostic`'s fields (deliberately private — see the
    /// struct's doc comment). `alba-core`'s `CoreError::into_diagnostic` is
    /// the first such caller: `CoreError` already carries a message, an
    /// optional [`Span`], and optional help in the same shape a
    /// [`ParseError`] does, so it goes through this constructor rather than
    /// duplicating `Diagnostic`'s internals or `alba-syntax` growing a
    /// dependency on `alba-core`'s error type.
    ///
    /// `span` is `None` for a failure that has no place in the source to
    /// point at — a beam named on the command line that does not exist, for
    /// instance. Such a diagnostic renders as the message and its help
    /// alone: asserting a source position that is not where the mistake is
    /// misleads a reader more than showing none does.
    pub fn new(message: impl Into<String>, span: Option<Span>, help: Option<String>) -> Self {
        Self {
            message: message.into(),
            span: span.map(to_source_span),
            help,
        }
    }
}

/// Converts our `[start, end)` byte-offset [`Span`] into miette's
/// offset-plus-length [`SourceSpan`]. Zero-width spans (for example the
/// lexer's `Eof` token, or a parse error pointing at end of input) are
/// preserved as zero-length spans rather than special-cased: miette renders
/// those as a single caret at that offset instead of an underlined range,
/// which is exactly the behavior we want, and never panics on them.
fn to_source_span(span: Span) -> SourceSpan {
    SourceSpan::new(span.start.into(), span.end.saturating_sub(span.start))
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Diagnostic {}

impl MietteDiagnostic for Diagnostic {
    fn help<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        self.help
            .as_ref()
            .map(|help| Box::new(help) as Box<dyn fmt::Display + 'a>)
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        let span = self.span?;
        Some(Box::new(std::iter::once(LabeledSpan::underline(span))))
    }
}

impl ParseError {
    /// Converts this parse error into a renderable [`Diagnostic`].
    ///
    /// Lex errors are already surfaced as [`ParseError`] by
    /// [`crate::parser::tokenize`] (message and span preserved, no help),
    /// so this single conversion covers both lexing and parsing failures in
    /// practice.
    pub fn into_diagnostic(self) -> Diagnostic {
        Diagnostic::new(self.message, Some(self.span), self.help)
    }
}

/// Renders `err` against `source` (identified as `path` in the output)
/// using miette's graphical report handler, producing the source line, a
/// caret under the exact span, and the help text if present.
///
/// The handler is fixed to unicode box-drawing, no color, and an explicit
/// 80-column width, so the rendered string is byte-identical regardless of
/// the terminal, OS, or CI runner it's produced on (see the module doc
/// comment).
pub fn render_diagnostic(source: &str, path: &str, err: &Diagnostic) -> String {
    let report = WithSource {
        source: NamedSource::new(path, source.to_string()),
        inner: err,
    };
    let mut rendered = String::new();
    handler()
        .render_report(&mut rendered, &report)
        .expect("rendering a diagnostic into a String cannot fail");
    rendered
}

/// The single, deterministic [`GraphicalReportHandler`] configuration used
/// by [`render_diagnostic`]. Unicode (not ASCII) box-drawing, no color, and
/// a fixed width: none of those depend on the environment the code runs in,
/// which is what makes the insta snapshots in `tests/diagnostics.rs` stable
/// across macOS, Linux, and Windows.
fn handler() -> GraphicalReportHandler {
    GraphicalReportHandler::new_themed(GraphicalTheme::unicode_nocolor()).with_width(80)
}

/// Pairs a [`Diagnostic`] with the source text it refers to, so the pair
/// together implements [`miette::Diagnostic`] with `source_code()`
/// present. Kept private and constructed only inside [`render_diagnostic`]:
/// `Diagnostic` itself deliberately stays source-free (see its doc
/// comment), so something has to carry the two together at render time.
struct WithSource<'a> {
    source: NamedSource<String>,
    inner: &'a Diagnostic,
}

impl fmt::Debug for WithSource<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.inner, f)
    }
}

impl fmt::Display for WithSource<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.inner, f)
    }
}

impl std::error::Error for WithSource<'_> {}

impl MietteDiagnostic for WithSource<'_> {
    fn help<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        self.inner.help()
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        self.inner.labels()
    }

    fn source_code(&self) -> Option<&dyn SourceCode> {
        Some(&self.source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    /// `Eof`'s zero-width span must render without panicking and without
    /// producing nothing at all. Exercised through the real parser (a beam
    /// block left open, so parsing runs off the end of input) rather than
    /// building a synthetic zero-width `Diagnostic` by hand.
    #[test]
    fn renders_zero_width_eof_span_without_panicking() {
        let src = "beam x {";
        let err = parse(src).unwrap_err();
        assert_eq!(err.span.start, err.span.end, "expected a zero-width span");

        let rendered = render_diagnostic(src, "Beamfile", &err.into_diagnostic());

        assert!(!rendered.is_empty());
        assert!(rendered.contains("Beamfile"));
    }

    /// A diagnostic with no span renders as a plain sentence: the message
    /// and its help, with no source excerpt and no caret pointing at a
    /// place that has nothing to do with the failure.
    #[test]
    fn renders_a_diagnostic_without_a_span_as_a_plain_sentence() {
        let diagnostic = Diagnostic::new(
            "unknown beam `biuld`",
            None,
            Some("did you mean `build`?".to_string()),
        );

        let rendered = render_diagnostic("beam build { run \"x\" }", "Beamfile", &diagnostic);

        assert!(rendered.contains("unknown beam `biuld`"));
        assert!(rendered.contains("did you mean `build`?"));
        assert!(
            !rendered.contains("Beamfile:"),
            "no source position may be asserted, got: {rendered}"
        );
    }
}
