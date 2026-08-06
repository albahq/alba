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
//! [`load_project`]'s job, which also validates the resulting dependency
//! graph (unknown `needs` targets, `needs`-cycles) via [`validate_graph`]
//! before returning it. [`execution_subgraph`] extracts the transitive
//! closure of a target beam, for whichever engine schedules and runs it.

mod error;
mod eval;
mod files;
mod graph;
mod loader;
mod model;

pub use error::CoreError;
pub use eval::{Scope, eval_expr, render_template, suggest};
pub use files::{expand_globs, outputs_satisfied};
pub use graph::{execution_subgraph, validate_graph};
pub use loader::{LoadError, SourceMap, load_project};
pub use model::{Beam, BeamId, ExecutorKind, OptionValue, Project, SourceId, Value};

#[doc(hidden)]
pub use eval::load_str;
