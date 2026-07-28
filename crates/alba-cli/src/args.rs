//! Clap derive types for the `alba` binary's command line.
//!
//! Kept deliberately thin: `Cli` holds the flags that apply regardless of
//! subcommand (today, only `--file`), and `Command` is the subcommand enum
//! Task 12 adds `Run` to. Nothing here executes anything — `main.rs` is
//! the only place a parsed `Cli` is acted on.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Parsed command line for the `alba` binary.
#[derive(Debug, Parser)]
#[command(name = "alba", version, about = "A task runner for Beamfiles")]
pub struct Cli {
    /// Path to the Beamfile to load. Defaults to `./Beamfile` in the
    /// current directory when omitted.
    #[arg(long, global = true, value_name = "PATH")]
    pub file: Option<PathBuf>,

    /// The subcommand to run. `None` is bare `alba`: lists beams, or (once
    /// Task 12 wires it) runs the declared `default`.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// A subcommand of the `alba` binary. `Run` (with its beam id, positional
/// arguments, `--jobs`, `--keep-going`, `--output`, `--log-format`) is
/// Task 12's addition; this task only needs `Check`.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Load and validate the Beamfile without running anything.
    Check,
}
