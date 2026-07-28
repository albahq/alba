//! The shell's single error type for lexing and parsing, and its
//! plain-text rendering (message, source excerpt, caret underline,
//! optional help). Rendering is here rather than in the CLI because the
//! executor turns a parse failure into beam output lines and must not
//! depend on CLI rendering.

use crate::token::Span;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct ShellParseError {
    pub message: String,
    pub span: Span,
    pub suggestion: Option<String>,
}

impl ShellParseError {
    /// Renders the diagnostic against the source the span points into:
    ///
    /// ```text
    /// error: background jobs (`&`) are not supported
    ///   │ watch & build
    ///   │       ^
    ///   = help: the engine already parallelizes beams; ...
    /// ```
    ///
    /// Multi-line sources (a `run` string with embedded newlines) excerpt
    /// only the line containing the span start.
    pub fn render(&self, source: &str) -> String {
        let line_start = source[..self.span.start.min(source.len())]
            .rfind('\n')
            .map_or(0, |i| i + 1);
        let line_end = source[line_start..]
            .find('\n')
            .map_or(source.len(), |i| line_start + i);
        let line = &source[line_start..line_end];
        let caret_offset = self.span.start.saturating_sub(line_start);
        let caret_len = (self.span.end.min(line_end).saturating_sub(self.span.start)).max(1);

        let mut out = format!("error: {}\n", self.message);
        out.push_str(&format!("  \u{2502} {line}\n"));
        out.push_str(&format!(
            "  \u{2502} {}{}\n",
            " ".repeat(caret_offset),
            "^".repeat(caret_len)
        ));
        if let Some(help) = &self.suggestion {
            out.push_str(&format!("  = help: {help}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::Span;

    #[test]
    fn renders_message_caret_and_suggestion() {
        let error = ShellParseError {
            message: "background jobs (`&`) are not supported".into(),
            span: Span { start: 6, end: 7 },
            suggestion: Some(
                "the engine already parallelizes beams; run one command per `run` entry".into(),
            ),
        };
        insta::assert_snapshot!(error.render("watch & build"));
    }

    #[test]
    fn renders_without_suggestion() {
        let error = ShellParseError {
            message: "unclosed single quote".into(),
            span: Span { start: 5, end: 6 },
            suggestion: None,
        };
        insta::assert_snapshot!(error.render("echo 'oops"));
    }
}
