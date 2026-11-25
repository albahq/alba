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
use std::ops::Range;

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

/// Number of context lines shown before and after the faulty line.
const CONTEXT_LINES: usize = 2;

/// A structured, source-aware view of a [`Diagnostic`], for front ends that
/// draw the report themselves instead of using [`render_diagnostic`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticReport {
    pub message: String,
    pub path: String,
    /// 1-based line of the span start.
    pub line: usize,
    /// 1-based column of the span start, counted in chars.
    pub column: usize,
    /// The faulty line with up to two lines of context on each side.
    pub snippet: Vec<SnippetLine>,
    /// Char range of the span on the faulty line, clipped to that line.
    pub caret: Range<usize>,
    pub help: Option<String>,
}

/// One source line of a [`DiagnosticReport`] snippet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetLine {
    /// 1-based line number.
    pub number: usize,
    pub text: String,
    pub faulty: bool,
}

/// Maps a byte `offset` to a 1-based `(line, column)`, the column counted
/// in chars. An offset past the end maps to the last position; one inside
/// a multi-byte char maps to that char.
pub fn line_col(source: &str, offset: usize) -> (usize, usize) {
    let mut end = offset.min(source.len());
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    let before = &source[..end];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    (line, before[line_start..].chars().count() + 1)
}

impl Diagnostic {
    /// Builds the structured report of this diagnostic against `source`
    /// (identified as `path`), or `None` when it has no span to point at.
    pub fn report(&self, source: &str, path: &str) -> Option<DiagnosticReport> {
        let span = self.span?;
        let start = span.offset().min(source.len());
        let end = (span.offset() + span.len()).min(source.len()).max(start);
        let (line, column) = line_col(source, start);

        let lines: Vec<&str> = source
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .collect();
        let faulty_index = line - 1;
        let first = faulty_index.saturating_sub(CONTEXT_LINES);
        let last = (faulty_index + CONTEXT_LINES).min(lines.len() - 1);
        let snippet = (first..=last)
            .map(|i| SnippetLine {
                number: i + 1,
                text: lines[i].to_string(),
                faulty: i == faulty_index,
            })
            .collect();

        let faulty_text = lines[faulty_index];
        let caret_start = column - 1;
        let span_chars = source[start..end].chars().count();
        let caret_end = (caret_start + span_chars).min(faulty_text.chars().count());

        Some(DiagnosticReport {
            message: self.message.clone(),
            path: path.to_string(),
            line,
            column,
            snippet,
            caret: caret_start..caret_end.max(caret_start),
            help: self.help.clone(),
        })
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

    #[test]
    fn line_col_counts_from_one() {
        let source = "beam a {\n  needs [b]\n}\n";
        assert_eq!(line_col(source, 0), (1, 1));
        assert_eq!(line_col(source, 11), (2, 3));
    }

    #[test]
    fn line_col_counts_columns_in_chars_and_clamps_past_the_end() {
        let source = "é→x\nab";
        assert_eq!(line_col(source, "é→".len()), (1, 3));
        assert_eq!(line_col(source, 999), (2, 3));
    }

    #[test]
    fn a_report_points_at_the_faulty_span_with_context() {
        let source =
            "beam check {\n  description \"x\"\n  needs [fmt, lint, tset]\n  run \"echo\"\n}\n";
        let start = source.find("tset").unwrap();
        let d = Diagnostic::new(
            "unknown beam `tset`",
            Some(Span {
                start,
                end: start + 4,
            }),
            Some("did you mean `test`?".into()),
        );
        let r = d.report(source, "Beamfile").unwrap();
        assert_eq!((r.line, r.column), (3, 21));
        assert_eq!(r.path, "Beamfile");
        assert_eq!(r.message, "unknown beam `tset`");
        assert_eq!(
            r.snippet.iter().map(|l| l.number).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        assert!(r.snippet[2].faulty);
        assert_eq!(r.snippet[2].text, "  needs [fmt, lint, tset]");
        assert_eq!(r.caret, 20..24);
        assert_eq!(r.help.as_deref(), Some("did you mean `test`?"));
    }

    #[test]
    fn a_report_clips_the_snippet_to_the_file_and_the_caret_to_the_line() {
        let source = "one\ntwo\nthree";
        let d = Diagnostic::new("m", Some(Span { start: 1, end: 100 }), None);
        let r = d.report(source, "Beamfile").unwrap();
        assert_eq!(
            r.snippet.iter().map(|l| l.number).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(r.caret, 1..3);
        assert_eq!(r.help, None);
    }

    #[test]
    fn a_report_for_a_span_at_the_end_of_input_does_not_panic() {
        let source = "beam x {";
        let d = Diagnostic::new(
            "m",
            Some(Span {
                start: source.len(),
                end: source.len(),
            }),
            None,
        );
        let r = d.report(source, "Beamfile").unwrap();
        assert_eq!((r.line, r.column), (1, 9));
        assert_eq!(r.caret, 8..8);
    }

    #[test]
    fn a_diagnostic_without_a_span_has_no_report() {
        assert!(
            Diagnostic::new("boom", None, None)
                .report("", "Beamfile")
                .is_none()
        );
    }
}
