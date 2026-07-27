//! [`CoreError`]: the single error type produced while evaluating a
//! Beamfile's AST into a [`crate::Project`].

use alba_syntax::Span;

use crate::model::SourceId;

/// The `SourceId` every error and beam produced by this crate's
/// single-file loading (`load_str`) carries. Multi-file loading (Task 7's
/// real `load_project` entry point) will assign distinct ids per imported
/// file instead of this constant — and because [`CoreError::new`] is the
/// single place that reaches for it, that change is a one-line edit here
/// rather than a search-and-replace across every error site.
pub(crate) const ROOT_SOURCE_ID: SourceId = SourceId(0);

/// An error produced while evaluating a Beamfile: an unknown variable, a
/// type mismatch, an unresolvable built-in call, and so on. Always carries
/// a source span (and, when a nearby valid name exists, help text) so it
/// can be rendered the same way `alba_syntax::Diagnostic` renders parse
/// errors.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct CoreError {
    pub message: String,
    pub span: Span,
    pub help: Option<String>,
    pub source_id: SourceId,
}

impl CoreError {
    /// A new error with no help text, at this crate's current single-file
    /// `SourceId`. The one place every `eval.rs` error-construction site
    /// goes through, instead of repeating the `CoreError { .. }` literal
    /// (including its `source_id`) at each of them.
    pub(crate) fn new(message: impl Into<String>, span: Span) -> Self {
        Self {
            message: message.into(),
            span,
            help: None,
            source_id: ROOT_SOURCE_ID,
        }
    }

    /// Attaches help text (chainable with [`CoreError::new`]).
    pub(crate) fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }
}
