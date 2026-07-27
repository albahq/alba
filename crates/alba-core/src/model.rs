//! The typed project model that `alba-core` evaluates a Beamfile's AST
//! into: [`Project`], its [`Beam`]s, and the small value types they carry.
//!
//! Scope boundary: a [`Beam`]'s `needs` list is converted straight from
//! the AST's `BeamRef`s (namespaced by [`crate::loader::load_project`]
//! when it came from an `import`) without validating that the referenced
//! beam actually exists — graph validation and subgraph extraction are
//! Task 8's job, not this crate's model.

use std::path::PathBuf;

use alba_syntax::{Span, StringTemplate};

use crate::eval::Scope;

/// Identifies which source file a [`Beam`] or [`crate::CoreError`] came
/// from, for pointing diagnostics at the right source text. Assigned by
/// [`crate::loader::load_project`] (the root file is always `SourceId(0)`,
/// each import gets the next one in load order) and resolvable back to a
/// path and source text via [`crate::loader::SourceMap::get`].
/// `load_str`'s single-file loading always produces `SourceId(0)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SourceId(pub usize);

/// A fully namespaced beam identifier, e.g. `"api:build"` for a beam
/// imported under the alias `api`, `"api:db:migrate"` for one imported
/// transitively through a chain of aliases, or `"build"` for a local one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BeamId(pub String);

/// How a beam's commands run.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutorKind {
    /// The default: the embedded shell (or, later, a configured system
    /// shell). Selecting a real shell implementation is Task 10's concern.
    Shell,
    /// `executor docker { image "..." }`. Task 10 rejects this at run
    /// time until the docker executor exists; this crate only carries the
    /// image name through the model.
    Docker { image: String },
}

/// A value produced by evaluating an [`alba_syntax::Expr`]: either of the
/// DSL's two runtime types.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Bool(bool),
}

impl Value {
    /// A short, human-readable name for this value's type, for error
    /// messages ("expected a boolean, found a string").
    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Value::Str(_) => "a string",
            Value::Bool(_) => "a boolean",
        }
    }

    /// This value as it appears when interpolated into a template:
    /// strings verbatim, booleans as `true`/`false`.
    pub(crate) fn as_display(&self) -> &str {
        match self {
            Value::Str(s) => s.as_str(),
            Value::Bool(true) => "true",
            Value::Bool(false) => "false",
        }
    }
}

/// A single beam: the unit of work produced by evaluating a `beam { ... }`
/// declaration.
///
/// Load-time vs. schedule-time rendering: `description`, `inputs`,
/// `outputs`, `cwd`, and the executor's options are rendered once, at load
/// time, into plain values — referencing a beam parameter there is a load
/// error. `run` and `env` are kept as [`StringTemplate`]s and rendered at
/// schedule time (via [`crate::render_template`]), once the beam's
/// parameters are bound into a [`Scope`] built from [`Beam::scope`] via
/// [`Scope::with_params`].
#[derive(Debug, Clone)]
pub struct Beam {
    pub id: BeamId,
    pub description: Option<String>,
    pub needs: Vec<BeamId>,
    pub params: Vec<String>,
    /// Evaluated but inert in this crate: the cache (out of scope here) is
    /// the only intended consumer.
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub run: Vec<StringTemplate>,
    pub env: Vec<(String, StringTemplate)>,
    pub cwd: Option<String>,
    pub executor: ExecutorKind,
    pub allow_failure: bool,
    /// The directory of the defining Beamfile, used as the `cwd` base.
    pub dir: PathBuf,
    pub span: Span,
    /// Which source file this beam was defined in, for diagnostics.
    pub source: SourceId,
    /// The defining file's evaluated file-level `let` bindings. The
    /// engine composes this with the beam's own parameter values at
    /// schedule time — `beam.scope.with_params(&beam.params, &args)` —
    /// before rendering `run`/`env`.
    pub scope: Scope,
}

/// A fully evaluated Beamfile (single-file loading only; see the module
/// doc comment): every beam it declares, and which one runs by default.
#[derive(Debug, Clone)]
pub struct Project {
    pub beams: Vec<Beam>,
    pub default: Option<BeamId>,
}
