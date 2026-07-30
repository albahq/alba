//! Clap derive types for the `alba` binary's command line.
//!
//! Kept deliberately thin: `Cli` holds the flags that apply regardless of
//! subcommand (today, only `--file`), `Command` is the subcommand enum, and
//! `RunFlags` groups everything that shapes a run. Nothing here executes
//! anything — `main.rs` is the only place a parsed `Cli` is acted on.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Parsed command line for the `alba` binary.
#[derive(Debug, Parser)]
#[command(name = "alba", version, about = "A task runner for Beamfiles")]
pub struct Cli {
    /// Path to the Beamfile to load. Defaults to `./Beamfile` in the
    /// current directory when omitted.
    #[arg(long, global = true, value_name = "PATH")]
    pub file: Option<PathBuf>,

    /// The subcommand to run. `None` is bare `alba`: runs the declared
    /// `default` beam, or lists beams when there is none.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// A subcommand of the `alba` binary.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Load and validate the Beamfile without running anything.
    Check,
    /// Run a beam and everything it needs.
    Run {
        /// The beam to run. Defaults to the Beamfile's `default` beam.
        #[arg(value_name = "BEAM")]
        beam: Option<String>,
        /// Positional arguments bound, in order, to the parameters the
        /// beam declares (`beam deploy(target)` takes one).
        #[arg(value_name = "PARAM")]
        params: Vec<String>,
        #[command(flatten)]
        flags: RunFlags,
    },
    /// Manage the project's cache.
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
}

/// A subcommand of `alba cache`. `clean` is deliberately the only one for
/// now; the subcommand level exists so `alba cache status` can join it
/// without breaking the CLI's shape.
#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Remove every cache entry for this project.
    Clean,
}

/// Everything that shapes a run, as opposed to what the Beamfile declares.
///
/// A struct of its own (rather than fields on `Command::Run`) because bare
/// `alba` runs the declared `default` beam through the same code path and
/// needs a value for all of it — which is exactly `RunFlags::default()`.
#[derive(Debug, Default, Args)]
pub struct RunFlags {
    /// How many beams may run at once
    ///
    /// At least 1. Defaults to the machine's available parallelism.
    // `0` is rejected rather than accepted and reinterpreted: a run with no
    // slots at all is not a run, and quietly reading it as `1` would turn a
    // typo into a silently sequential build.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
    pub jobs: Option<u64>,

    /// Keep going after a beam fails, instead of cancelling the beams that
    /// have not started
    #[arg(long)]
    pub keep_going: bool,

    /// Ignore the cache when reading: run every beam, and rewrite the
    /// cache entries of the ones that succeed
    #[arg(long)]
    pub force: bool,

    /// How text output is laid out
    ///
    /// Defaults to `interleaved` on a terminal and `grouped` otherwise.
    /// Ignored with `--log-format json`.
    #[arg(long, value_name = "STYLE", value_enum)]
    pub output: Option<OutputStyle>,

    /// What stdout carries
    ///
    /// Either human-readable text, laid out by `--output`, or one JSON
    /// object per event and per line.
    #[arg(long, value_name = "FORMAT", value_enum, default_value_t = LogFormat::Text)]
    pub log_format: LogFormat,

    /// Keep running: re-run the beam whenever the files its subgraph
    /// declares as `inputs` change. Ctrl-C ends the session.
    #[arg(long)]
    pub watch: bool,

    /// Force the interactive interface on, even where it would default off
    ///
    /// It still needs a real terminal to draw on: down a pipe this is
    /// refused rather than honoured.
    #[arg(long, conflicts_with = "no_ui")]
    pub ui: bool,

    /// Force the headless output, even on a terminal
    #[arg(long)]
    pub no_ui: bool,
}

/// How the text renderers lay a run out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputStyle {
    /// Every line as it happens, prefixed with the beam it came from.
    Interleaved,
    /// Each beam's output held back and printed as one block when it ends.
    Grouped,
}

/// What stdout carries during a run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    /// Human-readable, laid out by `--output`.
    #[default]
    Text,
    /// One JSON object per event, one per line.
    Json,
}
