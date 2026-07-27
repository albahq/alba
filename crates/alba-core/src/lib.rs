//! Core domain model for beams and Beamfiles.
//!
//! `alba-core` takes the AST `alba-syntax` produces and evaluates it into
//! the typed [`Project`] model the execution engine schedules: resolving
//! `let` bindings and `{expr}` interpolation, checking the small type
//! system (strings and booleans), and running the built-in `env()`/`glob()`
//! functions. See [`render_template`] and [`eval_expr`] for the
//! load-time-vs-schedule-time rendering split between a beam's fields.
//!
//! Multi-file loading (`import "path" as alias`, namespaced beam ids) is
//! [`load_project`]'s job. Dependency-graph validation (unknown `needs`
//! targets, `needs`-cycles) and subgraph extraction are layered on top in
//! later work — this crate builds the model, it doesn't validate the
//! graph.

mod error;
mod eval;
mod loader;
mod model;

pub use error::CoreError;
pub use eval::{Scope, eval_expr, render_template};
pub use loader::{LoadError, SourceMap, load_project};
pub use model::{Beam, BeamId, ExecutorKind, Project, SourceId, Value};

#[doc(hidden)]
pub use eval::load_str;
