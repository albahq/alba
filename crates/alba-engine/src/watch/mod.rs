//! Watch mode: run the target, then re-run it whenever the files its
//! subgraph declares as `inputs` change.
//!
//! What lives here is the vocabulary of watching — the [`Watcher`] stream
//! and the batches it delivers, the ways a session can end, the trouble it
//! survives, and where a session's paths are rooted and how they are shown.
//! The loop itself lives in [`crate::session`], of which [`watch`] is one
//! configuration: always watching, driven by nobody.

mod notify;
pub(crate) mod set;

use std::path::{Path, PathBuf};

use alba_core::{BeamId, Project, SourceMap};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::EngineError;
use crate::event::RunEvent;
use crate::scheduler::{Executors, RunOptions};
use crate::session::session;
pub use notify::NotifyWatcher;

/// One delivery from a file watcher.
pub enum WatchBatch {
    /// The paths the debouncer coalesced into this batch.
    Paths(Vec<PathBuf>),
    /// The watcher lost track (queue overflow): something changed, but
    /// it cannot say what. Treated as a trigger with no named paths —
    /// the cache absorbs the imprecision.
    Rescan,
}

/// A stream of debounced change batches. The indirection exists for the
/// engine's own tests, which script batches instead of touching a real
/// file system.
#[async_trait::async_trait]
pub trait Watcher: Send {
    /// The next batch; `None` when the watcher died for good.
    ///
    /// Must be cancel-safe: the session polls it in a `select!` against
    /// the running build and drops the future when the run finishes
    /// first, so a batch that was already taken from the underlying
    /// stream would be lost with it.
    async fn next_batch(&mut self) -> Option<WatchBatch>;
}

/// Why a session ended. Sessions have no failure exit — mid-session
/// trouble is reported and survived — so this is the complete list.
pub enum WatchExit {
    /// The caller's token fired: the user is done.
    Interrupted,
    /// The watcher's stream ended. The session cannot honestly continue:
    /// idling while watching nothing would look exactly like a healthy
    /// quiet session.
    WatcherClosed,
}

/// Mid-session trouble, handed to the caller's `on_error` for rendering.
///
/// Both variants carry the sources their spans index, because a session
/// outlives the project it started on: by the time an error is reported,
/// any number of reloads may have replaced the map the caller was holding
/// when it started the session. Rendering against that stale map would
/// draw the caret on text the error was never about.
pub enum SessionError {
    /// A Beamfile stopped loading. [`alba_core::LoadError`] already pairs
    /// the failure with every file read before it happened.
    Load(alba_core::LoadError),
    /// A run could not be carried out: an unknown target, or a beam the
    /// scheduler refuses. Reported rather than fatal — the fix is one
    /// Beamfile save away.
    Run {
        error: EngineError,
        /// The session's sources as of this failure. An
        /// [`EngineError::Core`] carries a span and a
        /// [`alba_core::SourceId`] that only this map resolves — the
        /// scheduler stamps each one with the id of the file the offending
        /// beam was declared in, precisely so the caret lands there.
        sources: SourceMap,
    },
}

/// Runs `target` and keeps re-running it as long as `watcher` reports
/// relevant changes, until `cancel` fires or the watcher dies.
///
/// A [`crate::session`] nobody drives: watching is on for its whole life,
/// and no command channel can retarget or stop it. Everything this
/// function's behaviour amounts to is documented there.
#[allow(clippy::too_many_arguments)]
pub async fn watch(
    beamfile: &Path,
    project: Project,
    sources: SourceMap,
    target: BeamId,
    options: RunOptions,
    executors: Executors,
    events: UnboundedSender<RunEvent>,
    cancel: CancellationToken,
    watcher: Box<dyn Watcher>,
    render_error: &mut (dyn FnMut(&SessionError) -> String + Send),
) -> WatchExit {
    session(
        beamfile,
        project,
        sources,
        target,
        options,
        executors,
        events,
        cancel,
        watcher,
        render_error,
    )
    .await
}

/// The absolute directory holding `beamfile`: the project root for the
/// root Beamfile, and its own directory for an imported one.
///
/// An empty parent means the current directory, and must be spelled that
/// way rather than passed on: a bare `Beamfile` (what `alba run` resolves
/// to without `--file`) has one, `notify` rejects an empty path outright,
/// and `strip_prefix("")` succeeds on any path at all — so an empty root
/// would leave every displayed path absolute with nothing to signal it.
/// The one rule, applied both to the roots put under watch and to the root
/// that paths are displayed against, so the two cannot drift apart.
///
/// Lexical: `std::path::absolute` never requires the directory to exist,
/// and never resolves symlinks — settling those is [`relative_to`]'s job,
/// on the paths a watcher actually reports.
pub fn beamfile_dir(beamfile: &Path) -> PathBuf {
    let dir = beamfile
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf())
}

/// Root-relative display strings with `/` separators, deduplicated,
/// sorted for stable output. A path outside the root (an import's input
/// in a sibling directory) displays as-is.
pub(crate) fn display_paths(root: &Path, paths: &[PathBuf]) -> Vec<String> {
    let canonical_root = root.canonicalize();
    let mut display: Vec<String> = paths
        .iter()
        .map(|path| {
            relative_to(root, canonical_root.as_deref().ok(), path)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    display.sort();
    display.dedup();
    display
}

/// `path` seen from the project root, or `path` itself when it lies
/// outside it.
///
/// The paths are compared as they came before anything is resolved,
/// because `canonicalize` is `realpath(3)` and fails outright on a file
/// that no longer exists — and a deletion (an `rm`, a `git checkout`, the
/// first half of a rename) is an ordinary watch event, whose path
/// `WatchSet` still classifies as an input from its snapshot. Resolving
/// only settles a disagreement about symlinks between what the watcher
/// reports and how the root was spelled: the root's canonical form works
/// on a deleted file too, its own does not and is the last attempt.
fn relative_to(root: &Path, canonical_root: Option<&Path>, path: &Path) -> PathBuf {
    if let Ok(relative) = path.strip_prefix(root) {
        return relative.to_path_buf();
    }
    if let Some(base) = canonical_root
        && let Ok(relative) = path.strip_prefix(base)
    {
        return relative.to_path_buf();
    }
    if let Some(base) = canonical_root
        && let Ok(canonical) = path.canonicalize()
        && let Ok(relative) = canonical.strip_prefix(base)
    {
        return relative.to_path_buf();
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The nominal invocation: `alba run --watch` without `--file` resolves
    /// to a bare `Beamfile`, and the paths a watcher reports are absolute.
    /// Rooting the session anywhere else — the empty path, most of all —
    /// leaves those paths absolute in the status line and in the
    /// `watch_triggered` event, which both promise root-relative ones.
    #[test]
    fn a_bare_beamfile_roots_the_session_at_the_current_directory() {
        let reported = std::env::current_dir().unwrap().join("src/input.txt");

        let root = beamfile_dir(Path::new("Beamfile"));

        assert_eq!(display_paths(&root, &[reported]), vec!["src/input.txt"]);
    }

    /// A Beamfile named through a directory roots the session there, not in
    /// the directory Alba happens to have been started from.
    #[test]
    fn a_named_beamfile_roots_the_session_at_its_own_directory() {
        let dir = tempfile::tempdir().unwrap();
        let reported = dir.path().join("src/input.txt");

        let root = beamfile_dir(&dir.path().join("Beamfile"));

        assert_eq!(display_paths(&root, &[reported]), vec!["src/input.txt"]);
    }

    /// The rule the root computation exists to satisfy: an empty root
    /// shortens nothing, because `strip_prefix("")` succeeds and hands back
    /// the whole path. Every branch below it is unreachable once the first
    /// one matches, so a session rooted at the empty path displays absolute
    /// paths with no error anywhere to say so.
    #[test]
    fn an_empty_root_shortens_nothing() {
        let path = Path::new("/project/src/input.txt");

        assert_eq!(relative_to(Path::new(""), None, path), path);
    }

    /// A relative root works lexically against a path spelled the same way
    /// — no filesystem access, nothing to resolve.
    #[test]
    fn a_relative_root_shortens_a_path_spelled_from_it() {
        assert_eq!(
            relative_to(
                Path::new("project"),
                None,
                Path::new("project/src/input.txt")
            ),
            Path::new("src/input.txt")
        );
    }

    /// An input reached through an import in a sibling tree is not under
    /// the root, and is shown as it came rather than as a `../..` chain.
    #[test]
    fn a_path_outside_the_root_is_left_as_it_came() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let path = elsewhere.path().join("shared/input.txt");

        assert_eq!(
            relative_to(
                root.path(),
                root.path().canonicalize().ok().as_deref(),
                &path
            ),
            path
        );
    }

    /// A deletion is an ordinary watch event, and `canonicalize` fails on a
    /// path that no longer exists — so shortening must not depend on it.
    #[test]
    fn a_path_that_no_longer_exists_is_still_shortened() {
        let root = tempfile::tempdir().unwrap();
        let deleted = root.path().join("src/deleted.txt");
        assert!(!deleted.exists(), "precondition");

        assert_eq!(
            relative_to(
                root.path(),
                root.path().canonicalize().ok().as_deref(),
                &deleted
            ),
            Path::new("src/deleted.txt")
        );
    }

    /// The root's canonical form settles a disagreement about symlinks
    /// between how the root was spelled and what the watcher reports
    /// (macOS reports `/private/var/...` for `/var/...`).
    #[test]
    fn a_root_spelled_through_a_symlink_still_shortens_what_the_watcher_reports() {
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();
        let reported = canonical.join("src/input.txt");

        assert_eq!(
            relative_to(root.path(), Some(&canonical), &reported),
            Path::new("src/input.txt")
        );
    }
}
