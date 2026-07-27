//! Core domain model for beams and Beamfiles.
//!
//! `alba-core` takes the AST `alba-syntax` produces and evaluates it into
//! the typed [`Project`] model the execution engine schedules: resolving
//! `let` bindings and `{expr}` interpolation, checking the small type
//! system (strings and booleans), and running the built-in `env()`/`glob()`
//! functions. See [`render_template`] and [`eval_expr`] for the
//! load-time-vs-schedule-time rendering split between a beam's fields.
//!
//! Scope boundary: this crate currently only loads a single Beamfile.
//! `import`/namespace resolution and dependency-graph validation are
//! layered on top in later work.

mod error;
mod eval;
mod model;

pub use error::CoreError;
pub use eval::{Scope, eval_expr, render_template};
pub use model::{Beam, BeamId, ExecutorKind, Project, SourceId, Value};

#[doc(hidden)]
pub use eval::load_str;
