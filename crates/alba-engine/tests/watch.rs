//! Behaviour tests for the session loop, driven through
//! [`alba_engine::watch`] and [`alba_engine::session`] with a scripted
//! watcher and the `FakeExecutor` — no real file watcher, no real process,
//! so restart, trigger and command semantics are asserted
//! deterministically. Real files and directories *are* used: the watched
//! set resolves `inputs` on disk.
//!
//! Timing: every wait on an event is bounded at five seconds, so a hung
//! loop fails the test rather than the CI job's timeout, and the latency
//! assertions use margins an order of magnitude above what they measure,
//! so a loaded machine cannot flip them while a real regression still
//! does.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alba_core::{BeamId, load_project};
use alba_engine::{
    CacheOptions, EngineError, Executors, RunEvent, RunOptions, Selection, SessionCommand,
    SessionError, WatchBatch, WatchExit, Watcher, session, watch,
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

    /// Whether an event satisfying `matches` arrives within `window`.
    /// Bounded on both answers: the negative one is a real assertion here
    /// ("nothing happened"), and waiting for it must not be able to hang.
    async fn saw_within(&mut self, window: Duration, matches: impl Fn(&RunEvent) -> bool) -> bool {
        let deadline = Instant::now() + window;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, self.events.recv()).await {
                Ok(Some(event)) if matches(&event) => return true,
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return false,
            }
        }
    }

    async fn finish(self) -> WatchExit {
        self.cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), self.handle)
            .await
            .expect("the session must end after cancellation")
            .expect("the session task must not panic")
    }

    /// Waits for a session that ends on its own — a command, a driver that
    /// left — with the caller's token never fired. The other handles stay
    /// alive until the wait is over, so nothing the session watches or
    /// reads closes under it.
    async fn ended(self) -> WatchExit {
        tokio::time::timeout(Duration::from_secs(5), self.handle)
            .await
            .expect("the session must end on its own")
            .expect("the session task must not panic")
    }
}

/// Session-error kinds observed, in order. The engine's error types are
/// not `Clone`, so the log keeps a tag per error rather than the error.
#[derive(Debug, PartialEq)]
enum ObservedError {
    Load,
    Run,
}

fn start(beamfile: &str, target: &str, executor: FakeExecutor, force: bool) -> Session {
    start_reporting_to(beamfile, target, executor, force, |_| String::new())
}

/// A session whose `render_error` reports are recorded, so a test can
/// assert what the caller was told and in which order.
fn start_with_error_log(
    beamfile: &str,
    target: &str,
    executor: FakeExecutor,
) -> (Session, Arc<Mutex<Vec<ObservedError>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    let session = start_reporting_to(beamfile, target, executor, false, move |error| {
        sink.lock().unwrap().push(match error {
            SessionError::Load(_) => ObservedError::Load,
            SessionError::Run { .. } => ObservedError::Run,
        });
        String::new()
    });
    (session, log)
}

fn start_reporting_to(
    beamfile: &str,
    target: &str,
    executor: FakeExecutor,
    force: bool,
    render_error: impl FnMut(&SessionError) -> String + Send + 'static,
) -> Session {
    spawn(beamfile, target, executor, force, true, None, render_error)
}

/// A session a driver can talk to: the same scripted project, plus the
/// command channel it reads and the watch state it starts in.
fn start_commandable(
    beamfile: &str,
    target: &str,
    executor: FakeExecutor,
    watch_enabled: bool,
) -> (Session, UnboundedSender<SessionCommand>) {
    let (commands, receiver) = unbounded_channel();
    let session = spawn(
        beamfile,
        target,
        executor,
        false,
        watch_enabled,
        Some(receiver),
        |_| String::new(),
    );
    (session, commands)
}

fn spawn(
    beamfile: &str,
    target: &str,
    executor: FakeExecutor,
    force: bool,
    watch_enabled: bool,
    commands: Option<UnboundedReceiver<SessionCommand>>,
    mut render_error: impl FnMut(&SessionError) -> String + Send + 'static,
) -> Session {
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
            let watcher = Box::new(ScriptedWatcher {
                batches: batches_rx,
            });
            match commands {
                // A session nobody drives is started through `watch`
                // itself, so the wrapper the CLI calls is what the whole
                // watch half of this suite exercises.
                None if watch_enabled => {
                    watch(
                        &beamfile_path,
                        project,
                        sources,
                        Selection::Beam(target),
                        options,
                        Executors::uniform(executor),
                        events_tx,
                        cancel,
                        watcher,
                        &mut render_error,
                    )
                    .await
                }
                commands => {
                    session(
                        &beamfile_path,
                        project,
                        sources,
                        Selection::Beam(target),
                        options,
                        Executors::uniform(executor),
                        events_tx,
                        commands,
                        watch_enabled,
                        cancel,
                        watcher,
                        &mut render_error,
                    )
                    .await
                }
            }
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

const ONE_BEAM: &str = "beam build { inputs [\"src/**/*.rs\"] run \"echo compile\" }\n";

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

fn is_run_started(event: &RunEvent) -> bool {
    matches!(event, RunEvent::RunStarted { .. })
}

fn is_broken(event: &RunEvent) -> bool {
    matches!(event, RunEvent::ProjectBroken { .. })
}

fn run_beam(id: &str, force: bool) -> SessionCommand {
    SessionCommand::RunBeam {
        id: BeamId(id.to_string()),
        force,
    }
}

/// The beam a run was started for.
fn started_target(event: RunEvent) -> String {
    let RunEvent::RunStarted { targets, .. } = event else {
        unreachable!("not a RunStarted event")
    };
    targets[0].0.clone()
}

/// The summary a run ended on.
fn finished_summary(event: RunEvent) -> alba_engine::RunSummary {
    let RunEvent::RunFinished { summary } = event else {
        unreachable!("not a RunFinished event")
    };
    summary
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

/// Editing the Beamfile mid-session is picked up without a restart: the
/// next run executes the *new* command.
#[tokio::test]
async fn a_beamfile_change_reloads_and_reruns() {
    let mut session = start(
        "beam build { inputs [\"src/**/*.rs\"] run \"echo compile-v1\" }\n",
        "build",
        FakeExecutor::new(),
        false,
    );
    session.event_matching(is_waiting).await;

    let beamfile = session.touch(
        "Beamfile",
        "beam build { inputs [\"src/**/*.rs\"] run \"echo compile-v2\" }\n",
    );
    session.send(vec![beamfile]);

    // One trigger announces the whole cycle: the reload and the run that
    // follows it are the answer to that one change, so nothing further is
    // announced before the session settles back into waiting.
    session.event_matching(is_triggered).await;
    let mut further_triggers = 0;
    loop {
        let event = session.event().await;
        further_triggers += usize::from(is_triggered(&event));
        if is_waiting(&event) {
            break;
        }
    }
    assert_eq!(further_triggers, 0);

    let commands: Vec<String> = session
        .executor
        .calls()
        .iter()
        .map(|call| call.command.clone())
        .collect();
    assert!(commands.iter().any(|c| c.contains("compile-v1")));
    assert!(commands.iter().any(|c| c.contains("compile-v2")));

    session.finish().await;
}

/// A Beamfile that stops parsing is reported, nothing executes, and the
/// session resumes as soon as it parses again.
#[tokio::test]
async fn a_broken_beamfile_reports_waits_and_recovers() {
    let (mut session, errors) = start_with_error_log(
        "beam build { inputs [\"src/**/*.rs\"] run \"echo compile\" }\n",
        "build",
        FakeExecutor::new(),
    );
    session.event_matching(is_waiting).await;
    let calls_before = session.executor.calls().len();

    let beamfile = session.touch("Beamfile", "beam build { this does not parse");
    session.send(vec![beamfile.clone()]);
    session.event_matching(is_triggered).await;

    // The load failure is reported; no run happens on a broken project.
    // Give the loop a moment to (wrongly) start one before asserting.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(*errors.lock().unwrap(), vec![ObservedError::Load]);
    assert_eq!(session.executor.calls().len(), calls_before);

    // The fix arrives: reload succeeds and the session runs again. The
    // recovery announces itself — the earlier trigger belonged to the
    // breaking edit, and the error report closed that cycle.
    session.touch(
        "Beamfile",
        "beam build { inputs [\"src/**/*.rs\"] run \"echo compile-fixed\" }\n",
    );
    session.send(vec![beamfile]);
    let RunEvent::WatchTriggered { paths } = session.event_matching(is_triggered).await else {
        unreachable!()
    };
    assert!(paths.is_empty());
    session.event_matching(is_run_finished).await;
    assert!(
        session
            .executor
            .calls()
            .iter()
            .any(|call| call.command.contains("compile-fixed"))
    );

    session.finish().await;
}

/// A broken reload is not only rendered to the caller: it travels the
/// event stream, so a consumer that never sees stderr (the TUI) still
/// learns the session parked and why. The `render_error` callback returns
/// a fixed, recognisable string, so the assertion proves the emitted
/// diagnostic is the callback's answer, not something the engine invented.
#[tokio::test]
async fn a_failed_reload_emits_project_broken() {
    let mut session = start_reporting_to(ONE_BEAM, "build", FakeExecutor::new(), false, |_| {
        "rendered by the callback".to_string()
    });
    session.event_matching(is_waiting).await;

    let beamfile = session.touch("Beamfile", "beam build { this does not parse");
    session.send(vec![beamfile]);
    session.event_matching(is_triggered).await;

    let event = session
        .event_matching(|event| matches!(event, RunEvent::ProjectBroken { .. }))
        .await;
    let RunEvent::ProjectBroken { diagnostic } = event else {
        unreachable!()
    };
    assert_eq!(diagnostic, "rendered by the callback");

    session.finish().await;
}

/// A parked session says so on the event channel. Until it does, the last
/// event on the stream is the trigger that means "running", and a consumer
/// — a JSON reader, a TUI — believes a session that is in fact stopped is
/// executing, for as long as the Beamfile stays broken.
#[tokio::test]
async fn a_parked_session_announces_that_it_is_waiting() {
    let (mut session, _errors) = start_with_error_log(ONE_BEAM, "build", FakeExecutor::new());
    session.event_matching(is_waiting).await;

    let beamfile = session.touch("Beamfile", "beam build { this does not parse");
    session.send(vec![beamfile]);
    session.event_matching(is_triggered).await;

    let RunEvent::WatchWaiting { files } = session.event_matching(is_waiting).await else {
        unreachable!()
    };
    // Nothing is resolved while the project will not load.
    assert_eq!(files, 0);

    session.finish().await;
}

/// A parked session reports once per change, not once per batch. Anything
/// writing inside the watched tree — an editor's temporary file, an LSP, a
/// build writing `target/` — would otherwise reprint the whole rendered
/// diagnostic, scrolling away the message the user is trying to read.
#[tokio::test]
async fn a_parked_session_does_not_repeat_an_unchanged_diagnostic() {
    let (mut session, errors) = start_with_error_log(ONE_BEAM, "build", FakeExecutor::new());
    session.event_matching(is_waiting).await;

    let beamfile = session.touch("Beamfile", "beam build { this does not parse");
    session.send(vec![beamfile]);
    session.event_matching(is_triggered).await;
    session.event_matching(is_waiting).await;

    for index in 0..5 {
        let noise = session.touch(&format!("notes{index}.txt"), "not an input");
        session.send(vec![noise]);
    }
    // Long enough for five reports to have landed if they were coming.
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(*errors.lock().unwrap(), vec![ObservedError::Load]);

    session.finish().await;
}

/// An edit that leaves the project broken *differently* is a new answer to
/// a new question, and is reported.
#[tokio::test]
async fn a_parked_session_reports_a_diagnostic_that_changed() {
    let (mut session, errors) = start_with_error_log(ONE_BEAM, "build", FakeExecutor::new());
    session.event_matching(is_waiting).await;

    let beamfile = session.touch("Beamfile", "beam build { this does not parse");
    session.send(vec![beamfile.clone()]);
    session.event_matching(is_waiting).await;

    session.touch("Beamfile", "beam build { neither does this");
    session.send(vec![beamfile]);
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        *errors.lock().unwrap(),
        vec![ObservedError::Load, ObservedError::Load]
    );

    session.finish().await;
}

/// The property no flood-suppression may cost: the fix can arrive through
/// a file the session never loaded — an `import` added while the project
/// was broken, whose target is only created afterwards — and the session
/// must still notice it.
#[tokio::test]
async fn a_parked_session_recovers_through_a_file_it_never_knew() {
    let (mut session, _errors) = start_with_error_log(ONE_BEAM, "build", FakeExecutor::new());
    session.event_matching(is_waiting).await;

    // The import names a file that does not exist yet: the project stops
    // loading, and the session parks.
    let beamfile = session.touch(
        "Beamfile",
        "import \"extra/Extra.beam\" as extra\n\
         beam build { inputs [\"src/**/*.rs\"] needs [extra:more] run \"echo compile\" }\n",
    );
    session.send(vec![beamfile]);
    session.event_matching(is_waiting).await;

    // Only the missing file is created, and only its path is reported.
    std::fs::create_dir_all(session.dir.path().join("extra")).unwrap();
    let created = session.touch("extra/Extra.beam", "beam more { run \"echo more\" }\n");
    session.send(vec![created]);

    session.event_matching(is_triggered).await;
    session.event_matching(is_run_finished).await;
    assert!(
        session
            .executor
            .calls()
            .iter()
            .any(|call| call.command.contains("more"))
    );

    session.finish().await;
}

/// A target that vanishes on reload (renamed beam) is a run error, not a
/// crash: reported, and the session waits for the next Beamfile change.
#[tokio::test]
async fn a_renamed_target_reports_and_recovers_on_the_next_edit() {
    let (mut session, errors) = start_with_error_log(
        "beam build { inputs [\"src/**/*.rs\"] run \"echo compile\" }\n",
        "build",
        FakeExecutor::new(),
    );
    session.event_matching(is_waiting).await;

    let beamfile = session.touch(
        "Beamfile",
        "beam renamed { inputs [\"src/**/*.rs\"] run \"echo compile\" }\n",
    );
    session.send(vec![beamfile.clone()]);
    session.event_matching(is_triggered).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(errors.lock().unwrap().contains(&ObservedError::Run));

    // Recovery from an unbuildable watched set announces itself the same
    // way recovery from an unparsable Beamfile does.
    session.touch(
        "Beamfile",
        "beam build { inputs [\"src/**/*.rs\"] run \"echo compile-back\" }\n",
    );
    session.send(vec![beamfile]);
    let RunEvent::WatchTriggered { paths } = session.event_matching(is_triggered).await else {
        unreachable!()
    };
    assert!(paths.is_empty());
    session.event_matching(is_run_finished).await;

    session.finish().await;
}

/// A run error reported after a reload carries the *reloaded* sources.
///
/// What this protects: an [`EngineError::Core`] carries a span and a source
/// id that index the project the session is running now, not the one the
/// caller loaded before it started. Hand back the startup map and the
/// caller draws the caret on text the error was never about — on a file
/// that has since been edited, or on no file at all when the reload
/// registered an import the old map never knew.
#[tokio::test]
async fn a_run_error_after_a_reload_carries_the_reloaded_sources() {
    let reported = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&reported);
    let mut session = start_reporting_to(
        "beam build { inputs [\"src/**/*.rs\"] run \"echo compile\" }\n",
        "build",
        FakeExecutor::new(),
        false,
        move |error| {
            if let SessionError::Run {
                error: EngineError::Core(core),
                sources,
            } = error
            {
                // Exactly what the CLI's renderer does to place the caret.
                let text = sources
                    .get(core.source_id)
                    .map_or_else(String::new, |(_, source)| source.to_string());
                sink.lock().unwrap().push(text);
            }
            String::new()
        },
    );
    session.event_matching(is_waiting).await;

    // Renaming the target is the cheapest way to make the *next* run fail
    // after a successful reload: the reload works, the target is gone.
    let beamfile = session.touch(
        "Beamfile",
        "beam renamed { inputs [\"src/**/*.rs\"] run \"echo compile\" }\n",
    );
    session.send(vec![beamfile]);
    session.event_matching(is_triggered).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let reported = reported.lock().unwrap().clone();
    assert!(
        !reported.is_empty(),
        "the vanished target must be reported as a run error"
    );
    assert!(
        reported.iter().all(|text| text.contains("renamed")),
        "the error must resolve against the reloaded Beamfile, got: {reported:?}"
    );

    session.finish().await;
}

/// The one test that exercises the real file watcher: a write on disk
/// must come through as a batch naming the file. Everything else in this
/// suite scripts batches; this proves the scripting matches reality on
/// each platform.
#[tokio::test]
async fn notify_watcher_reports_a_real_write() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("watched.txt"), "before").unwrap();

    let mut watcher =
        alba_engine::NotifyWatcher::new(&[dir.path().to_path_buf()]).expect("watcher must start");

    // Give the OS watcher a moment to arm before the write, then write.
    tokio::time::sleep(Duration::from_millis(250)).await;
    std::fs::write(dir.path().join("watched.txt"), "after").unwrap();

    let batch = tokio::time::timeout(Duration::from_secs(10), watcher.next_batch())
        .await
        .expect("a batch must arrive within 10s")
        .expect("the watcher must not close");
    match batch {
        WatchBatch::Paths(paths) => assert!(
            paths.iter().any(|p| p.ends_with("watched.txt")),
            "batch must name the written file, got {paths:?}"
        ),
        WatchBatch::Rescan => {} // an overflow still reports a change; acceptable
    }
}

// ---- commands ------------------------------------------------------------

/// The commandable session's whole point: a `RunBeam` triggers a run
/// exactly like a file change does, of the beam the driver named.
#[tokio::test]
async fn a_run_beam_command_runs_that_beam() {
    let (mut session, commands) = start_commandable(TWO_BEAMS, "build", FakeExecutor::new(), false);
    session.event_matching(is_run_finished).await;

    commands.send(run_beam("docs", false)).unwrap();

    let started = session.event_matching(is_run_started).await;
    assert_eq!(started_target(started), "docs");

    session.finish().await;
}

/// `RunBeam` retargets the session, so what a later watch trigger re-runs
/// is the beam the driver asked for and not the one the session started
/// on. `build` alone watches `src/**`; only a session retargeted at `docs`
/// watches `docs/**` as well.
#[tokio::test]
async fn a_run_beam_command_retargets_the_watch() {
    let (mut session, commands) = start_commandable(TWO_BEAMS, "build", FakeExecutor::new(), true);
    session.event_matching(is_waiting).await;

    commands.send(run_beam("docs", false)).unwrap();
    session.event_matching(is_waiting).await;

    let changed = session.touch("docs/index.md", "# docs, edited");
    session.send(vec![changed]);

    session.event_matching(is_triggered).await;
    let started = session.event_matching(is_run_started).await;
    assert_eq!(started_target(started), "docs");

    session.finish().await;
}

/// Cancelling ends the run, not the session: the next command still works.
#[tokio::test]
async fn a_cancel_run_command_leaves_the_session_alive() {
    let executor = FakeExecutor::new().on(
        "compile",
        FakeBehavior {
            exit_code: 0,
            delay: Duration::from_secs(30),
            output_lines: Vec::new(),
        },
    );
    let (mut session, commands) = start_commandable(TWO_BEAMS, "docs", executor, false);
    session.event_matching(is_beam_started).await;

    commands.send(SessionCommand::CancelRun).unwrap();

    let started = Instant::now();
    let summary = finished_summary(session.event_matching(is_run_finished).await);
    assert!(!summary.cancelled.is_empty(), "the slow beam was cancelled");
    assert!(started.elapsed() < Duration::from_secs(10));

    commands.send(run_beam("docs", false)).unwrap();
    session.event_matching(is_run_started).await;

    session.finish().await;
}

/// With watching off a relevant batch triggers nothing; turning it on
/// mid-session makes the next batch trigger, and says so on the stream.
#[tokio::test]
async fn a_set_watch_command_gates_the_watchers_batches() {
    let (mut session, commands) = start_commandable(TWO_BEAMS, "docs", FakeExecutor::new(), false);
    session.event_matching(is_run_finished).await;

    let changed = session.touch("src/main.rs", "fn main() { changed(); }");
    session.send(vec![changed.clone()]);
    assert!(
        !session
            .saw_within(Duration::from_millis(200), is_triggered)
            .await,
        "a batch must trigger nothing while watching is off"
    );

    commands.send(SessionCommand::SetWatch(true)).unwrap();
    // Turning watching on re-announces the wait, so a consumer sees the
    // toggle take effect rather than having to infer it.
    session.event_matching(is_waiting).await;

    session.send(vec![changed]);
    session.event_matching(is_triggered).await;
    session.event_matching(is_run_started).await;

    session.finish().await;
}

/// `Shutdown` ends the session promptly even with a run in flight, without
/// the caller's token ever firing.
#[tokio::test]
async fn a_shutdown_command_ends_a_running_session() {
    let executor = FakeExecutor::new().on(
        "compile",
        FakeBehavior {
            exit_code: 0,
            delay: Duration::from_secs(30),
            output_lines: Vec::new(),
        },
    );
    let (mut session, commands) = start_commandable(TWO_BEAMS, "docs", executor, true);
    session.event_matching(is_beam_started).await;

    commands.send(SessionCommand::Shutdown).unwrap();

    assert!(matches!(session.ended().await, WatchExit::Interrupted));
}

/// A parked session answers commands too: a driver must be able to quit a
/// session that is executing nothing while the Beamfile will not load.
#[tokio::test]
async fn a_shutdown_command_ends_a_parked_session() {
    let (mut session, commands) = start_commandable(ONE_BEAM, "build", FakeExecutor::new(), true);
    session.event_matching(is_waiting).await;

    let beamfile = session.touch("Beamfile", "beam build { this does not parse");
    session.send(vec![beamfile]);
    session.event_matching(is_broken).await;

    commands.send(SessionCommand::Shutdown).unwrap();

    assert!(matches!(session.ended().await, WatchExit::Interrupted));
}

/// A dropped sender means the driver is gone; the session must not hang on
/// a channel no one will ever write to again.
#[tokio::test]
async fn a_dropped_command_channel_ends_the_session() {
    let (mut session, commands) = start_commandable(TWO_BEAMS, "docs", FakeExecutor::new(), false);
    session.event_matching(is_run_finished).await;

    drop(commands);

    assert!(matches!(session.ended().await, WatchExit::Interrupted));
}

/// `force` is the CLI's `--force` scoped to one command: it empties the
/// cache's read side for that run and for no other.
#[tokio::test]
async fn a_forced_run_beam_command_ignores_the_cache() {
    let build = vec![BeamId("build".to_string())];
    let (mut session, commands) = start_commandable(ONE_BEAM, "build", FakeExecutor::new(), false);
    let summary = finished_summary(session.event_matching(is_run_finished).await);
    assert_eq!(summary.succeeded, build);

    commands.send(run_beam("build", false)).unwrap();
    let summary = finished_summary(session.event_matching(is_run_finished).await);
    assert_eq!(summary.cached, build, "nothing changed: the rerun is a hit");

    commands.send(run_beam("build", true)).unwrap();
    let summary = finished_summary(session.event_matching(is_run_finished).await);
    assert_eq!(summary.succeeded, build, "force empties the read side");

    commands.send(run_beam("build", false)).unwrap();
    let summary = finished_summary(session.event_matching(is_run_finished).await);
    assert_eq!(summary.cached, build, "and only for that one run");

    session.finish().await;
}

/// A `RunBeam` naming a beam the parked project *does* declare unparks the
/// session: the load is retried on the spot, with no file change to wait
/// for. The path out of a park that a renamed beam caused, for a driver
/// that can see the new name.
#[tokio::test]
async fn a_run_beam_command_unparks_a_session_onto_the_renamed_beam() {
    let (mut session, commands) = start_commandable(ONE_BEAM, "build", FakeExecutor::new(), true);
    session.event_matching(is_waiting).await;

    // The project still loads; it is the watched set that cannot be built,
    // because the target beam is gone.
    let beamfile = session.touch(
        "Beamfile",
        "beam renamed { inputs [\"src/**/*.rs\"] run \"echo compile-renamed\" }\n",
    );
    session.send(vec![beamfile]);
    session.event_matching(is_broken).await;

    commands.send(run_beam("renamed", false)).unwrap();

    let started = session.event_matching(is_run_started).await;
    assert_eq!(started_target(started), "renamed");
    session.event_matching(is_run_finished).await;
    assert!(
        session
            .executor
            .calls()
            .iter()
            .any(|call| call.command.contains("compile-renamed"))
    );

    session.finish().await;
}

/// `--affected` in a session: the reference is fixed, the targets are
/// recomputed for every run, and a beam that was not affected at startup
/// joins once its input changes.
#[tokio::test]
async fn an_affected_session_recomputes_its_targets_on_every_run() {
    fn git(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.email", "alba@example.com"]);
    git(dir.path(), &["config", "user.name", "Alba"]);
    git(dir.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::create_dir_all(dir.path().join("docs")).unwrap();
    std::fs::write(dir.path().join("docs/index.md"), "v1").unwrap();
    std::fs::write(
        dir.path().join("Beamfile"),
        "beam docs { inputs [\"docs/**\"] run \"docs\" }\nbeam free { run \"free\" }\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);

    let beamfile_path = dir.path().join("Beamfile");
    let (project, sources) = load_project(&beamfile_path).unwrap();
    let executor = Arc::new(FakeExecutor::new());
    let (batches_tx, batches_rx) = unbounded_channel();
    let (events_tx, events_rx) = unbounded_channel();
    let cancel = CancellationToken::new();
    let options = RunOptions {
        jobs: 4,
        keep_going: false,
        params: Vec::new(),
        cache: None,
    };
    let handle = tokio::spawn({
        let executor = Arc::clone(&executor);
        let cancel = cancel.clone();
        async move {
            let watcher = Box::new(ScriptedWatcher {
                batches: batches_rx,
            });
            watch(
                &beamfile_path,
                project,
                sources,
                Selection::Affected {
                    reference: "HEAD".to_string(),
                    within: None,
                },
                options,
                Executors::uniform(executor),
                events_tx,
                cancel,
                watcher,
                &mut |_| String::new(),
            )
            .await
        }
    });
    let mut session = Session {
        dir,
        executor,
        batches: batches_tx,
        events: events_rx,
        cancel,
        handle,
    };

    let RunEvent::RunStarted { targets, .. } = session.event_matching(is_run_started).await else {
        unreachable!()
    };
    assert!(targets.is_empty(), "nothing has changed yet");
    session
        .event_matching(|event| matches!(event, RunEvent::WatchWaiting { .. }))
        .await;

    let touched = session.touch("docs/index.md", "v2");
    session.send(vec![touched]);
    let RunEvent::RunStarted { targets, .. } = session.event_matching(is_run_started).await else {
        unreachable!()
    };
    assert_eq!(
        targets.iter().map(|id| id.0.as_str()).collect::<Vec<_>>(),
        ["docs"]
    );
    session.finish().await;
}
