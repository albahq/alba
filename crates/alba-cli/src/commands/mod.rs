//! Subcommand implementations.
//!
//! Loading the Beamfile — and rendering a failure as a diagnostic — happens
//! exactly once, in `main.rs`, before any command runs: every command here
//! is handed an already-loaded [`alba_core::Project`] and only formats its
//! own success output. This keeps the "how do we load and report a load
//! failure" logic in one place shared by `check`, bare listing, and `run`.
//!
//! `run` is the one command that can still fail after loading succeeded (a
//! target that does not exist, an executor Alba does not implement yet), so
//! it is also handed the `SourceMap` it needs to render such a failure the
//! same way `main.rs` renders a load failure.
//!
//! `cache` is the one exception to "already loaded": it only needs the
//! Beamfile's path to locate the project's cache directory, never its
//! content, so `main.rs` dispatches it before loading — a Beamfile that
//! does not even parse must not block cleaning the cache sitting next to
//! it.
//!
//! `plugin` goes further still: `alba plugin check` needs no Beamfile at
//! all, not even to locate a directory, since it drives a binary through
//! the protocol directly. It is dispatched the same way, before loading.
//!
//! `affected` lists the beams a git reference's diff touches, and never
//! runs anything — it is the dry run of `alba run --affected`, meant for
//! CI to gate on.

pub mod affected;
pub mod cache;
pub mod check;
/// `alba hook <name> [args]`: dispatched after loading, like every command
/// in this list except `cache` and `plugin` — but see `main.rs`, which
/// answers `0` before loading is even attempted when there is no Beamfile
/// at all, since an undeclared hook (the common case, git knows every
/// hook by name whether or not a Beamfile declares it) must stay silent.
pub mod hook;
pub mod hooks;
pub mod list;
pub mod plugin;
pub mod run;
