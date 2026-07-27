//! [`CoreError`]: the single error type produced while evaluating a
//! Beamfile's AST into a [`crate::Project`].

use alba_syntax::Span;

use crate::model::SourceId;

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
