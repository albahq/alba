//! [`CoreError`]: the single error type produced while evaluating a
//! Beamfile's AST into a [`crate::Project`].

use std::cell::Cell;

use alba_syntax::Span;

use crate::model::SourceId;

/// The `SourceId` this crate's single-file loading (`load_str`) always
/// assigns to its one file, and the id `loader::load_project` always
/// assigns to the root Beamfile (the first file registered in a
/// [`crate::loader::SourceMap`] is always id 0).
pub(crate) const ROOT_SOURCE_ID: SourceId = SourceId(0);

thread_local! {
    /// The `SourceId` of whichever file is currently being evaluated on
    /// this thread. [`CoreError::new`] and `Beam`'s construction
    /// (`current_source_id`) both read this instead of taking an explicit
    /// parameter, so that stamping the right id is a change made in one
    /// place ([`SourceIdScope::enter`]) rather than a parameter threaded
    /// through every one of `eval.rs`'s error-construction sites.
    ///
    /// Only ever set by a [`SourceIdScope`], which is `pub(crate)` — so
    /// outside this crate (in particular, the future engine calling the
    /// publicly re-exported [`crate::eval_expr`]/[`crate::render_template`]
    /// at schedule time, after loading has finished) this defaults to
    /// [`ROOT_SOURCE_ID`] regardless of which file the value being
    /// rendered actually came from. A schedule-time caller that knows the
    /// real answer (typically `Beam::source`) must correct it explicitly
    /// with [`CoreError::with_source_id`] — see that method's doc comment.
    static CURRENT_SOURCE: Cell<SourceId> = const { Cell::new(ROOT_SOURCE_ID) };
}

/// The `SourceId` every [`CoreError`] and `Beam` built right now will be
/// stamped with. See [`SourceIdScope`].
pub(crate) fn current_source_id() -> SourceId {
    CURRENT_SOURCE.with(Cell::get)
}

/// RAII guard that sets this thread's "currently loading" [`SourceId`] to
/// `id` for its lifetime, restoring the previous value when it drops
/// (including on an early return via `?`, or a panic unwind).
///
/// The loader holds one of these per file it evaluates. Because imports
/// are loaded depth-first and a recursive call's guard only drops once
/// that file (and everything it imports) has been fully evaluated, nested
/// guards naturally restore the importing file's id afterward — plain
/// stack discipline, no separate stack type needed. `load_str`'s
/// single-file front door also enters one (pinned to [`ROOT_SOURCE_ID`])
/// so its behavior doesn't depend on whatever a previous call on the same
/// thread left behind.
pub(crate) struct SourceIdScope {
    previous: SourceId,
}

impl SourceIdScope {
    pub(crate) fn enter(id: SourceId) -> Self {
        let previous = CURRENT_SOURCE.with(|cell| cell.replace(id));
        Self { previous }
    }
}

impl Drop for SourceIdScope {
    fn drop(&mut self) {
        CURRENT_SOURCE.with(|cell| cell.set(self.previous));
    }
}

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
    /// A new error with no help text, stamped with whichever file is
    /// currently being loaded (see [`SourceIdScope`]). The one place every
    /// error-construction site in this crate goes through, instead of
    /// repeating the `CoreError { .. }` literal (including its
    /// `source_id`) at each of them.
    pub(crate) fn new(message: impl Into<String>, span: Span) -> Self {
        Self {
            message: message.into(),
            span,
            help: None,
            source_id: current_source_id(),
        }
    }

    /// Attaches help text (chainable with [`CoreError::new`]).
    pub(crate) fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// Overrides this error's `source_id`. Public (unlike [`CoreError::new`]
    /// and [`CoreError::with_help`]) because it's meant for callers outside
    /// this crate: [`crate::eval_expr`] and [`crate::render_template`] are
    /// re-exported so the engine can render a beam's `run`/`env` templates
    /// at schedule time, but by then loading has finished and there is no
    /// active [`SourceIdScope`] (that type is `pub(crate)`-only, scoped to
    /// this crate's own loading) — every error they build defaults to
    /// [`ROOT_SOURCE_ID`] regardless of which file the beam actually came
    /// from. A schedule-time caller that has the beam on hand should
    /// correct that explicitly, e.g. `render_template(&tpl,
    /// &scope).map_err(|e| e.with_source_id(beam.source))`, rather than
    /// reaching into the (also-public) `source_id` field directly — this
    /// method is the one, documented, intentional place that's meant to
    /// happen, as opposed to an ad hoc patch scattered at whichever call
    /// site happens to need it.
    pub fn with_source_id(mut self, source_id: SourceId) -> Self {
        self.source_id = source_id;
        self
    }
}
