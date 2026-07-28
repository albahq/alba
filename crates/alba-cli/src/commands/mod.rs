//! Subcommand implementations.
//!
//! Loading the Beamfile — and rendering a failure as a diagnostic — happens
//! exactly once, in `main.rs`, before any command runs: every command here
//! is handed an already-loaded [`alba_core::Project`] and only formats its
//! own success output. This keeps the "how do we load and report a load
//! failure" logic in one place shared by `check`, bare listing, and (once
//! Task 12 adds it) `run`.

pub mod check;
pub mod list;
