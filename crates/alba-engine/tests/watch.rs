//! Behaviour tests for the watch session loop, driven through
//! [`alba_engine::watch`] with a scripted watcher and the `FakeExecutor` —
//! no real file watcher, no real process, so restart and trigger
//! semantics are asserted deterministically. Real files and directories
//! *are* used: the watched set resolves `inputs` on disk.
//!
//! Timing: every wait on an event is bounded at five seconds, so a hung
//! loop fails the test rather than the CI job's timeout, and the latency
//! assertions use margins an order of magnitude above what they measure,
//! so a loaded machine cannot flip them while a real regression still
//! does.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alba_core::{BeamId, load_project};
use alba_engine::{
    CacheOptions, Executors, RunEvent, RunOptions, SessionError, WatchBatch, WatchExit, Watcher,
    watch,
};
use alba_executors::{FakeBehavior, FakeExecutor};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

/// A watcher whose batches the test sends by hand: closing the sender is
/// how a test stages a watcher that died.
struct ScriptedWatcher {
    batches: UnboundedReceiver<WatchBatch>,
}

#[async_trait::async_trait]
impl Watcher for ScriptedWatcher {
    async fn next_batch(&mut self) -> Option<WatchBatch> {
        self.batches.recv().await
    }
}

/// A live watch session over a real temporary project, plus the handles
/// that drive and observe it.
struct Session {
    dir: tempfile::TempDir,
    executor: Arc<FakeExecutor>,
    batches: UnboundedSender<WatchBatch>,
    events: UnboundedReceiver<RunEvent>,
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<WatchExit>,
}

impl Session {
    /// The next event, or a panic after five seconds — a hung loop must
    /// fail the test, not the CI job's timeout.
    async fn event(&mut self) -> RunEvent {
        tokio::time::timeout(Duration::from_secs(5), self.events.recv())
            .await
            .expect("no event within 5s")
            .expect("event channel closed unexpectedly")
    }

    /// Drains until an event satisfies `matches`, returning it.
    async fn event_matching(&mut self, matches: impl Fn(&RunEvent) -> bool) -> RunEvent {
        loop {
            let event = self.event().await;
            if matches(&event) {
                return event;
            }
        }
    }

    fn touch(&self, relative: &str, content: &str) -> PathBuf {
        let path = self.dir.path().join(relative);
        std::fs::write(&path, content).unwrap();
        path
    }

    fn send(&self, paths: Vec<PathBuf>) {
        self.batches.send(WatchBatch::Paths(paths)).unwrap();
    }

    async fn finish(self) -> WatchExit {
        self.cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), self.handle)
            .await
            .expect("the session must end after cancellation")
            .expect("the session task must not panic")
    }
}

fn start(beamfile: &str, target: &str, executor: FakeExecutor, force: bool) -> Session {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join("docs")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
    // Every watched beam needs at least one resolved input file: a beam
    // whose patterns match nothing is not cacheable, and would re-execute
    // on every run whatever the cache decided.
    std::fs::write(dir.path().join("docs/index.md"), "# docs").unwrap();
    std::fs::write(dir.path().join("Beamfile"), beamfile).unwrap();

    let beamfile_path = dir.path().join("Beamfile");
    let (project, sources) = load_project(&beamfile_path).unwrap();
    let executor = Arc::new(executor);
    let (batches_tx, batches_rx) = unbounded_channel();
    let (events_tx, events_rx) = unbounded_channel();
    let cancel = CancellationToken::new();

    let options = RunOptions {
        jobs: 4,
        keep_going: false,
        params: Vec::new(),
        cache: Some(CacheOptions {
            dir: dir.path().join(".alba").join("cache"),
            force,
        }),
    };
    let handle = tokio::spawn({
        let executor = Arc::clone(&executor);
        let cancel = cancel.clone();
        let target = BeamId(target.to_string());
        async move {
            watch(
                &beamfile_path,
                project,
                sources,
                target,
                options,
                Executors::uniform(executor),
                events_tx,
                cancel,
                Box::new(ScriptedWatcher {
                    batches: batches_rx,
                }),
                &mut |_: &SessionError| {},
            )
            .await
        }
    });

    Session {
        dir,
        executor,
        batches: batches_tx,
        events: events_rx,
        cancel,
        handle,
    }
}

const TWO_BEAMS: &str = "beam build { inputs [\"src/**/*.rs\"] run \"echo compile\" }\n\
     beam docs { inputs [\"docs/**\"] needs [build] run \"echo docs\" }\n";

fn is_waiting(event: &RunEvent) -> bool {
    matches!(event, RunEvent::WatchWaiting { .. })
}

fn is_triggered(event: &RunEvent) -> bool {
    matches!(event, RunEvent::WatchTriggered { .. })
}

fn is_run_finished(event: &RunEvent) -> bool {
    matches!(event, RunEvent::RunFinished { .. })
}

fn is_beam_started(event: &RunEvent) -> bool {
    matches!(event, RunEvent::BeamStarted { .. })
}

/// The session's first act is a plain run, and only then does it wait.
#[tokio::test]
async fn runs_once_then_waits() {
    let mut session = start(TWO_BEAMS, "docs", FakeExecutor::new(), false);

    session.event_matching(is_run_finished).await;
    session.event_matching(is_waiting).await;
    assert_eq!(session.executor.calls().len(), 2); // build + docs

    session.finish().await;
}

/// A relevant change triggers a new run, naming only the paths the
/// session watches.
#[tokio::test]
async fn a_relevant_change_reruns_only_the_affected_beams() {
    let mut session = start(TWO_BEAMS, "docs", FakeExecutor::new(), false);
    session.event_matching(is_waiting).await;

    let changed = session.touch("src/main.rs", "fn main() { changed(); }");
    session.send(vec![changed]);

    let triggered = session.event_matching(is_triggered).await;
    let RunEvent::WatchTriggered { paths } = triggered else {
        unreachable!()
    };
    assert_eq!(paths, vec!["src/main.rs".to_string()]);

    session.event_matching(is_run_finished).await;
    session.event_matching(is_waiting).await;
    // `build` reruns because its own input changed; `docs` follows
    // because its dependency's fingerprint is part of its own. What this
    // guarantees: the change reached `build` a second time, and the
    // session stopped there — a loop that re-triggered itself would show
    // a third run.
    let builds = session
        .executor
        .calls()
        .iter()
        .filter(|call| call.command.contains("compile"))
        .count();
    assert_eq!(builds, 2);
    assert_eq!(session.executor.calls().len(), 4);

    session.finish().await;
}

/// Deleting a watched input triggers a run and still names the file
/// relative to the project root — a deleted path cannot be resolved on
/// disk, so the display must not depend on resolving it.
#[tokio::test]
async fn a_deleted_input_is_still_named_relative_to_the_root() {
    let mut session = start(TWO_BEAMS, "docs", FakeExecutor::new(), false);
    session.event_matching(is_waiting).await;

    let deleted = session.dir.path().join("src/main.rs");
    std::fs::remove_file(&deleted).unwrap();
    session.send(vec![deleted]);

    let RunEvent::WatchTriggered { paths } = session.event_matching(is_triggered).await else {
        unreachable!()
    };
    assert_eq!(paths, vec!["src/main.rs".to_string()]);

    session.finish().await;
}

/// Irrelevant paths do not wake the session: after an ignored batch, the
/// next relevant one is still the *first* trigger.
#[tokio::test]
async fn an_irrelevant_change_keeps_waiting() {
    let mut session = start(TWO_BEAMS, "docs", FakeExecutor::new(), false);
    session.event_matching(is_waiting).await;

    let unrelated = session.touch("notes.txt", "not an input");
    session.send(vec![unrelated]);
    let changed = session.touch("src/main.rs", "fn main() { changed(); }");
    session.send(vec![changed]);

    let RunEvent::WatchTriggered { paths } = session.event_matching(is_triggered).await else {
        unreachable!()
    };
    assert_eq!(paths, vec!["src/main.rs".to_string()]);

    session.finish().await;
}

/// A change landing mid-run cancels the run; the cancelled summary is
/// followed by the trigger and a fresh run. Current-thread runtime so
/// the scripted delay reliably keeps the run in flight when the batch
/// arrives.
#[tokio::test]
async fn a_mid_run_change_cancels_and_restarts() {
    let executor = FakeExecutor::new().on(
        "compile",
        FakeBehavior {
            exit_code: 0,
            delay: Duration::from_secs(30),
            output_lines: Vec::new(),
        },
    );
    let mut session = start(TWO_BEAMS, "docs", executor, false);

    session.event_matching(is_beam_started).await;
    let changed = session.touch("src/main.rs", "fn main() { changed(); }");
    session.send(vec![changed]);

    // The interrupted run ends (its long beam cancelled), the trigger is
    // announced after its summary, and a new run starts — well before the
    // scripted 30s delay could have elapsed on its own.
    let started = Instant::now();
    let RunEvent::RunFinished { summary } = session.event_matching(is_run_finished).await else {
        unreachable!()
    };
    assert!(!summary.cancelled.is_empty());
    session.event_matching(is_triggered).await;
    session.event_matching(is_beam_started).await;
    assert!(started.elapsed() < Duration::from_secs(10));

    session.finish().await;
}

/// Cancelling the session token ends the loop with `Interrupted`.
#[tokio::test]
async fn cancellation_ends_the_session() {
    let mut session = start(TWO_BEAMS, "docs", FakeExecutor::new(), false);
    session.event_matching(is_waiting).await;
    assert!(matches!(session.finish().await, WatchExit::Interrupted));
}

/// A watcher that dies takes the session with it — silently watching
/// nothing would look exactly like a healthy idle session.
#[tokio::test]
async fn a_closed_watcher_ends_the_session() {
    let mut session = start(TWO_BEAMS, "docs", FakeExecutor::new(), false);
    session.event_matching(is_waiting).await;

    drop(std::mem::replace(
        &mut session.batches,
        unbounded_channel().0,
    ));

    let exit = tokio::time::timeout(Duration::from_secs(5), session.handle)
        .await
        .expect("the session must end when the watcher closes")
        .expect("the session task must not panic");
    assert!(matches!(exit, WatchExit::WatcherClosed));
}

/// A rescan (overflow) triggers a run with no named paths.
#[tokio::test]
async fn a_rescan_triggers_a_run_with_no_paths() {
    let mut session = start(TWO_BEAMS, "docs", FakeExecutor::new(), false);
    session.event_matching(is_waiting).await;

    session.batches.send(WatchBatch::Rescan).unwrap();

    let RunEvent::WatchTriggered { paths } = session.event_matching(is_triggered).await else {
        unreachable!()
    };
    assert!(paths.is_empty());

    session.finish().await;
}

/// `--force` empties the cache's read side for the *initial* run only: a
/// triggered run with unchanged inputs comes back cached.
#[tokio::test]
async fn force_applies_to_the_initial_run_only() {
    let mut session = start(TWO_BEAMS, "docs", FakeExecutor::new(), true);
    session.event_matching(is_waiting).await;
    let first_calls = session.executor.calls().len();
    assert_eq!(first_calls, 2);

    // Rewrite an input with identical content: relevant (it changed on
    // disk), but fingerprint-identical — cached unless force leaked.
    let changed = session.touch("src/main.rs", "fn main() {}");
    session.send(vec![changed]);
    session.event_matching(is_triggered).await;
    let RunEvent::RunFinished { summary } = session.event_matching(is_run_finished).await else {
        unreachable!()
    };
    assert_eq!(summary.cached.len(), 2);
    assert_eq!(session.executor.calls().len(), first_calls);

    session.finish().await;
}

/// The latency guard: from batch delivery to the triggered event must be
/// imperceptible. The bound is wide (2s vs the spec's tens of ms) so a
/// loaded CI machine cannot flip it; a regression to seconds still fails.
#[tokio::test]
async fn trigger_latency_stays_imperceptible() {
    let mut session = start(TWO_BEAMS, "docs", FakeExecutor::new(), false);
    session.event_matching(is_waiting).await;

    let changed = session.touch("src/main.rs", "fn main() { changed(); }");
    let sent_at = Instant::now();
    session.send(vec![changed]);
    session.event_matching(is_triggered).await;
    assert!(sent_at.elapsed() < Duration::from_secs(2));

    session.finish().await;
}
