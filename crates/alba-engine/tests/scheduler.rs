//! Behaviour tests for the parallel scheduler, driven end to end through
//! [`alba_engine::run`] against `alba_executors`' `FakeExecutor` — no real
//! process is ever spawned, so the scheduling semantics (dependency order,
//! the parallelism bound, fail-fast, `keep_going`, `allow_failure`,
//! cancellation, and event ordering) are asserted deterministically.
//!
//! Timing: the scripted delays are chosen with wide margins (tens of
//! milliseconds where an ordering must hold, hundreds where a beam must
//! still be running when another finishes) so a loaded machine does not
//! flip an assertion. Tests whose assertions depend on *which* beam takes
//! the single available permit first deliberately run on the
//! current-thread runtime, where tasks are polled in spawn order; the ones
//! asserting real concurrency run on a multi-threaded one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alba_core::{BeamId, load_str};
use alba_engine::{BeamStatus, EngineError, RunEvent, RunOptions, RunSummary, run};
use alba_executors::{
    CommandSpec, ExecContext, ExecError, ExecResult, Executor, FakeBehavior, FakeExecutor,
    OutputLine, Stream,
};
use tokio_util::sync::CancellationToken;

/// Everything one `run` produced: its result and every event it emitted,
/// drained after the run returned (all senders are dropped by then, so
/// nothing can still be in flight).
struct Outcome {
    result: Result<RunSummary, EngineError>,
    events: Vec<RunEvent>,
}

impl Outcome {
    fn summary(&self) -> &RunSummary {
        self.result.as_ref().expect("the run must not fail")
    }

    fn error(&self) -> &EngineError {
        self.result.as_ref().expect_err("the run must fail")
    }
}

fn options(jobs: usize, keep_going: bool) -> RunOptions {
    RunOptions {
        jobs,
        keep_going,
        params: Vec::new(),
        cache: None,
    }
}

fn behavior(exit_code: i32, delay_ms: u64) -> FakeBehavior {
    FakeBehavior {
        exit_code,
        delay: Duration::from_millis(delay_ms),
        output_lines: Vec::new(),
    }
}

fn stdout(text: &str) -> OutputLine {
    OutputLine {
        stream: Stream::Stdout,
        text: text.to_string(),
    }
}

async fn run_target(
    source: &str,
    target: &str,
    options: RunOptions,
    executor: Arc<dyn Executor>,
) -> Outcome {
    run_target_with_cancel(source, target, options, executor, CancellationToken::new()).await
}

async fn run_target_with_cancel(
    source: &str,
    target: &str,
    options: RunOptions,
    executor: Arc<dyn Executor>,
    cancel: CancellationToken,
) -> Outcome {
    let project = load_str(source).expect("the test Beamfile must load");
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

    let result = run(
        &project,
        &BeamId(target.to_string()),
        options,
        executor,
        events_tx,
        cancel,
    )
    .await;

    let mut events = Vec::new();
    while let Ok(event) = events_rx.try_recv() {
        events.push(event);
    }
    Outcome { result, events }
}

fn ids(beams: &[BeamId]) -> Vec<&str> {
    beams.iter().map(|id| id.0.as_str()).collect()
}

fn commands(executor: &FakeExecutor) -> Vec<String> {
    executor
        .calls()
        .into_iter()
        .map(|call| call.command)
        .collect()
}

fn position(commands: &[String], command: &str) -> usize {
    commands
        .iter()
        .position(|candidate| candidate == command)
        .unwrap_or_else(|| panic!("`{command}` was never executed, ran: {commands:?}"))
}

fn positions(events: &[RunEvent], predicate: impl Fn(&RunEvent) -> bool) -> Vec<usize> {
    events
        .iter()
        .enumerate()
        .filter(|(_, event)| predicate(event))
        .map(|(index, _)| index)
        .collect()
}

fn only(indexes: Vec<usize>, what: &str) -> usize {
    assert_eq!(indexes.len(), 1, "expected exactly one {what}");
    indexes[0]
}

/// Dependency order, and that independent beams are not accidentally
/// serialized. The `jobs` bound itself is pinned by
/// [`parallelism_never_exceeds_the_jobs_limit`]: this diamond can never
/// exceed two concurrent beams by its own shape, so its `running_peak`
/// assertion would survive the semaphore being removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn respects_dependency_order_and_runs_independent_beams_concurrently() {
    const SOURCE: &str = r#"
beam a { run "step a" }
beam b { needs [a] run "step b" }
beam c { needs [a] run "step c" }
beam d { needs [b, c] run "step d" }
"#;

    let executor = Arc::new(FakeExecutor::new().on("step", behavior(0, 50)));
    let outcome = run_target(SOURCE, "d", options(2, false), executor.clone()).await;

    let commands = commands(&executor);
    assert_eq!(
        commands.len(),
        4,
        "every beam runs exactly once: {commands:?}"
    );
    let (a, b) = (position(&commands, "step a"), position(&commands, "step b"));
    let (c, d) = (position(&commands, "step c"), position(&commands, "step d"));
    assert!(
        a < b && a < c,
        "`a` runs before its dependents: {commands:?}"
    );
    assert!(b < d && c < d, "`d` runs last: {commands:?}");

    assert_eq!(
        executor.running_peak(),
        2,
        "`b` and `c` are independent and must overlap under jobs=2"
    );

    let summary = outcome.summary();
    assert_eq!(ids(&summary.succeeded), ["a", "b", "c", "d"]);
    assert_eq!(summary.exit_code(), 0);
}

/// `--jobs N` really bounds how many beams run at once. Four mutually
/// independent beams would all overlap if nothing held them back, so the
/// peak is the bound itself and not an artifact of the graph's shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallelism_never_exceeds_the_jobs_limit() {
    const SOURCE: &str = r#"
beam one { run "step one" }
beam two { run "step two" }
beam three { run "step three" }
beam four { run "step four" }
beam all { needs [one, two, three, four] run "step all" }
"#;

    let executor = Arc::new(FakeExecutor::new().on("step", behavior(0, 50)));
    let outcome = run_target(SOURCE, "all", options(2, false), executor.clone()).await;
    assert_eq!(outcome.summary().cancelled.len(), 0);
    assert_eq!(
        executor.running_peak(),
        2,
        "four independent beams under jobs=2 must never exceed two at once"
    );

    let executor = Arc::new(FakeExecutor::new().on("step", behavior(0, 20)));
    let outcome = run_target(SOURCE, "all", options(1, false), executor.clone()).await;
    assert_eq!(outcome.summary().succeeded.len(), 5);
    assert_eq!(
        executor.running_peak(),
        1,
        "jobs=1 must serialize the run completely"
    );
}

/// Fail-fast cancels every beam that has not started yet, but a beam that
/// already holds a permit keeps running to completion. Note the
/// counter-intuitive consequence the rule implies and this test pins:
/// `d` needs only `c`, which succeeds, yet `d` is still cancelled — `b`'s
/// failure cancels *all* not-yet-started beams, not just its own
/// dependents.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fail_fast_cancels_pending_but_lets_running_finish() {
    const SOURCE: &str = r#"
beam b { run "fail b" }
beam c { run "slow c" }
beam d { needs [c] run "step d" }
beam all { needs [b, d] run "step all" }
"#;

    let executor = Arc::new(
        FakeExecutor::new()
            .on("fail b", behavior(1, 50))
            .on("slow c", behavior(0, 300)),
    );
    let outcome = run_target(SOURCE, "all", options(2, false), executor.clone()).await;

    let summary = outcome.summary();
    assert_eq!(ids(&summary.failed), ["b"]);
    assert_eq!(
        ids(&summary.succeeded),
        ["c"],
        "`c` was already running when `b` failed and must finish"
    );
    assert_eq!(ids(&summary.cancelled), ["d", "all"]);
    assert_eq!(summary.exit_code(), 1);

    // `b` and `c` start concurrently, so the order they were recorded in
    // is not meaningful here — only that nothing else ever started.
    let mut commands = commands(&executor);
    commands.sort();
    assert_eq!(
        commands,
        ["fail b", "slow c"],
        "no beam starts after the failure"
    );
}

/// With `keep_going`, a failure no longer cancels the beams that have not
/// started: `d` is independent of `b` and still runs. What `keep_going`
/// does *not* change is dependency propagation — `e` needs the failed `b`
/// and `all` needs the cancelled `e`, so both are cancelled anyway.
///
/// Runs on the current-thread runtime with `jobs = 1` on purpose: tasks are
/// polled in spawn order (declaration order) and the semaphore is fair, so
/// `b` provably takes the only permit first and `d` is still pending when
/// `b` fails — which is exactly the situation `keep_going` governs.
#[tokio::test]
async fn keep_going_runs_independent_beams() {
    const SOURCE: &str = r#"
beam b { run "fail b" }
beam d { run "step d" }
beam e { needs [b] run "step e" }
beam all { needs [d, e] run "step all" }
"#;

    let executor = Arc::new(
        FakeExecutor::new()
            .on("fail b", behavior(1, 10))
            .on("step d", behavior(0, 10)),
    );
    let outcome = run_target(SOURCE, "all", options(1, true), executor.clone()).await;

    let summary = outcome.summary();
    assert_eq!(ids(&summary.failed), ["b"]);
    assert_eq!(ids(&summary.succeeded), ["d"]);
    assert_eq!(ids(&summary.cancelled), ["e", "all"]);
    assert_eq!(summary.exit_code(), 1);

    assert_eq!(
        commands(&executor),
        ["fail b", "step d"],
        "`d` was still pending when `b` failed, and ran anyway"
    );
}

#[tokio::test]
async fn allow_failure_does_not_stop_the_run() {
    const SOURCE: &str = r#"
beam lint { allow_failure true run "run lint" }
beam build { needs [lint] run "run build" }
"#;

    let executor = Arc::new(FakeExecutor::new().on("run lint", behavior(1, 0)));
    let outcome = run_target(SOURCE, "build", options(2, false), executor.clone()).await;

    let summary = outcome.summary();
    assert_eq!(ids(&summary.failed_allowed), ["lint"]);
    assert_eq!(ids(&summary.succeeded), ["build"]);
    assert!(summary.failed.is_empty());
    assert!(summary.cancelled.is_empty());
    assert_eq!(
        summary.exit_code(),
        0,
        "an allowed failure does not affect the exit code"
    );

    assert_eq!(commands(&executor), ["run lint", "run build"]);
}

/// A beam's commands run one after another and stop at the first failure:
/// a beam declaring `["build", "deploy"]` must not deploy what did not
/// build.
#[tokio::test]
async fn a_failing_command_skips_the_rest_of_its_beam() {
    const SOURCE: &str = r#"
beam release { run ["fail first", "never second"] }
"#;

    let executor = Arc::new(FakeExecutor::new().on("fail first", behavior(3, 0)));
    let outcome = run_target(SOURCE, "release", options(2, false), executor.clone()).await;

    assert_eq!(
        commands(&executor),
        ["fail first"],
        "the second command must never run"
    );
    let summary = outcome.summary();
    assert_eq!(ids(&summary.failed), ["release"]);
    assert_eq!(summary.exit_code(), 1);
}

/// The same rule under `allow_failure`: the beam's outcome is forgiving,
/// its remaining commands are not still run.
#[tokio::test]
async fn a_failing_command_skips_the_rest_even_when_failure_is_allowed() {
    const SOURCE: &str = r#"
beam lint { allow_failure true run ["fail first", "never second"] }
"#;

    let executor = Arc::new(FakeExecutor::new().on("fail first", behavior(1, 0)));
    let outcome = run_target(SOURCE, "lint", options(2, false), executor.clone()).await;

    assert_eq!(commands(&executor), ["fail first"]);
    assert_eq!(ids(&outcome.summary().failed_allowed), ["lint"]);
}

/// A `run` template that cannot be rendered at schedule time — here an
/// environment variable that is deliberately deferred to that moment and
/// turns out to be unset — is that beam's failure, not the run's. It gets
/// the same treatment as a command that cannot be spawned: a started
/// event, the message as stderr output, a failed status, and a run that
/// keeps going for everything that does not depend on it.
#[tokio::test]
async fn a_template_that_cannot_render_fails_only_its_own_beam() {
    const SOURCE: &str = r#"
beam broken { run "echo {env('ALBA_MISSING_VAR_XYZ')}" }
beam other { run "step other" }
beam all { needs [broken, other] run "step all" }
"#;

    let executor = Arc::new(FakeExecutor::new());
    let outcome = run_target(SOURCE, "all", options(2, true), executor.clone()).await;

    let summary = outcome.summary();
    assert_eq!(ids(&summary.failed), ["broken"]);
    assert_eq!(
        ids(&summary.succeeded),
        ["other"],
        "a beam whose template fails must not stop the rest of the run"
    );
    assert_eq!(ids(&summary.cancelled), ["all"]);
    assert_eq!(summary.exit_code(), 1);

    let events = &outcome.events;
    let started = only(
        positions(
            events,
            |event| matches!(event, RunEvent::BeamStarted { id } if id.0 == "broken"),
        ),
        "`broken` started event",
    );
    let reported = only(
        positions(events, |event| {
            matches!(event, RunEvent::BeamOutput { id, line, .. }
                if id.0 == "broken"
                    && line.stream == Stream::Stderr
                    && line.text.contains("ALBA_MISSING_VAR_XYZ"))
        }),
        "render failure reported as stderr output",
    );
    let finished = only(
        positions(
            events,
            |event| matches!(event, RunEvent::BeamFinished { id, .. } if id.0 == "broken"),
        ),
        "`broken` finished event",
    );
    assert!(started < reported && reported < finished);

    let last = events.last().expect("the run must emit events");
    assert!(
        matches!(last, RunEvent::RunFinished { .. }),
        "the run must still end with its summary, got {last:?}"
    );
}

/// The forgiving half of the same rule: a beam that declares
/// `allow_failure` and whose template cannot render lands in
/// `failed_allowed`, so its dependents still run.
#[tokio::test]
async fn a_template_that_cannot_render_honours_allow_failure() {
    const SOURCE: &str = r#"
beam lint { allow_failure true run "echo {env('ALBA_MISSING_VAR_XYZ')}" }
beam build { needs [lint] run "step build" }
"#;

    let executor = Arc::new(FakeExecutor::new());
    let outcome = run_target(SOURCE, "build", options(2, false), executor.clone()).await;

    let summary = outcome.summary();
    assert_eq!(ids(&summary.failed_allowed), ["lint"]);
    assert_eq!(ids(&summary.succeeded), ["build"]);
    assert_eq!(summary.exit_code(), 0);
}

/// Without `--keep-going`, a template failure stops the beams that have
/// not started, exactly like any other failure — rather than abandoning
/// the run and discarding what already succeeded.
#[tokio::test]
async fn a_template_that_cannot_render_triggers_fail_fast() {
    const SOURCE: &str = r#"
beam broken { run "echo {env('ALBA_MISSING_VAR_XYZ')}" }
beam after { needs [broken] run "step after" }
"#;

    let executor = Arc::new(FakeExecutor::new());
    let outcome = run_target(SOURCE, "after", options(1, false), executor.clone()).await;

    let summary = outcome.summary();
    assert_eq!(ids(&summary.failed), ["broken"]);
    assert_eq!(ids(&summary.cancelled), ["after"]);
    assert!(commands(&executor).is_empty());
}

/// An `Executor` that cannot spawn a command at all — a missing shell, a
/// `cwd` that does not exist. Scriptable behaviour `FakeExecutor` does not
/// offer, so this test drives the case directly through the trait.
struct SpawnFailureExecutor;

#[async_trait::async_trait]
impl Executor for SpawnFailureExecutor {
    async fn execute(&self, cmd: CommandSpec, _ctx: ExecContext) -> Result<ExecResult, ExecError> {
        if cmd.command.contains("broken") {
            return Err(ExecError {
                message: format!("failed to spawn `{}`", cmd.command),
            });
        }
        Ok(ExecResult { exit_code: 0 })
    }
}

/// A command that never produced an exit code is that beam's failure, not
/// the run's: it is reported like any other failure, its message reaches
/// the user as stderr output, and `--keep-going` still runs what does not
/// depend on it.
#[tokio::test]
async fn a_command_that_cannot_be_spawned_fails_only_its_own_beam() {
    const SOURCE: &str = r#"
beam broken { run "broken command" }
beam other { run "step other" }
beam all { needs [broken, other] run "step all" }
"#;

    let outcome = run_target(
        SOURCE,
        "all",
        options(2, true),
        Arc::new(SpawnFailureExecutor),
    )
    .await;

    let summary = outcome.summary();
    assert_eq!(ids(&summary.failed), ["broken"]);
    assert_eq!(
        ids(&summary.succeeded),
        ["other"],
        "a beam that cannot spawn must not stop the rest of the run"
    );
    assert_eq!(ids(&summary.cancelled), ["all"]);
    assert_eq!(summary.exit_code(), 1);

    let events = &outcome.events;
    let reported = only(
        positions(events, |event| {
            matches!(event, RunEvent::BeamOutput { id, line, .. }
                if id.0 == "broken"
                    && line.stream == Stream::Stderr
                    && line.text.contains("failed to spawn `broken command`"))
        }),
        "spawn failure reported as stderr output",
    );
    let finished = only(
        positions(
            events,
            |event| matches!(event, RunEvent::BeamFinished { id, .. } if id.0 == "broken"),
        ),
        "`broken` finished event",
    );
    assert!(
        reported < finished,
        "the failure message must reach the user before the beam finishes"
    );
}

/// Commands run in their Beamfile's directory, which `cwd` overrides —
/// relatively to that directory, or absolutely.
#[tokio::test]
async fn commands_run_in_the_beams_directory_unless_cwd_overrides_it() {
    // `load_str` has no file on disk, so every beam's directory is `.`.
    #[cfg(windows)]
    const ABSOLUTE: &str = "C:/opt/alba";
    #[cfg(not(windows))]
    const ABSOLUTE: &str = "/opt/alba";

    let source = format!(
        r#"
beam plain {{ run "step plain" }}
beam relative {{ cwd "sub/dir" run "step relative" }}
beam absolute {{ cwd "{ABSOLUTE}" run "step absolute" }}
beam all {{ needs [plain, relative, absolute] run "step all" }}
"#
    );

    let executor = Arc::new(FakeExecutor::new());
    let outcome = run_target(&source, "all", options(4, false), executor.clone()).await;
    assert_eq!(outcome.summary().succeeded.len(), 4);

    let cwd_of = |command: &str| -> PathBuf {
        executor
            .calls()
            .into_iter()
            .find(|call| call.command == command)
            .unwrap_or_else(|| panic!("`{command}` was never executed"))
            .cwd
    };
    assert_eq!(cwd_of("step plain"), Path::new("."));
    assert_eq!(cwd_of("step relative"), Path::new(".").join("sub/dir"));
    assert_eq!(
        cwd_of("step absolute"),
        Path::new(ABSOLUTE),
        "an absolute `cwd` replaces the Beamfile's directory"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_token_cancels_everything() {
    const SOURCE: &str = r#"
beam a { run "slow a" }
beam b { needs [a] run "slow b" }
"#;

    let executor = Arc::new(FakeExecutor::new().on("slow", behavior(0, 30_000)));
    let cancel = CancellationToken::new();

    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        trigger.cancel();
    });

    let started = Instant::now();
    let outcome =
        run_target_with_cancel(SOURCE, "b", options(2, false), executor.clone(), cancel).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(10),
        "a cancelled run must return promptly, took {elapsed:?}"
    );

    let summary = outcome.summary();
    assert_eq!(ids(&summary.cancelled), ["a", "b"]);
    assert!(summary.failed.is_empty(), "cancellation is not a failure");
}

/// Per beam, `BeamStarted` precedes every `BeamOutput`, which precedes
/// `BeamFinished`; `RunFinished` is the very last event. `a` runs two
/// commands to pin the rule that a multi-command `run` still emits exactly
/// one started/finished pair.
#[tokio::test]
async fn events_are_emitted_in_order_per_beam() {
    const SOURCE: &str = r#"
beam a { run ["step a1", "step a2"] }
beam b { needs [a] run "step b" }
"#;

    let executor = Arc::new(
        FakeExecutor::new()
            .on("a1", {
                let mut behavior = behavior(0, 0);
                behavior.output_lines = vec![stdout("a1 out")];
                behavior
            })
            .on("a2", {
                let mut behavior = behavior(0, 0);
                behavior.output_lines = vec![stdout("a2 out")];
                behavior
            })
            .on("step b", {
                let mut behavior = behavior(0, 0);
                behavior.output_lines = vec![stdout("b out")];
                behavior
            }),
    );
    let outcome = run_target(SOURCE, "b", options(2, false), executor.clone()).await;
    let events = &outcome.events;

    for beam in ["a", "b"] {
        let started = only(
            positions(
                events,
                |e| matches!(e, RunEvent::BeamStarted { id } if id.0 == beam),
            ),
            &format!("`{beam}` started event"),
        );
        let finished = positions(
            events,
            |e| matches!(e, RunEvent::BeamFinished { id, status, .. } if id.0 == beam && *status == BeamStatus::Succeeded),
        );
        let finished = only(finished, &format!("`{beam}` finished event"));
        let output = positions(
            events,
            |e| matches!(e, RunEvent::BeamOutput { id, .. } if id.0 == beam),
        );
        assert!(!output.is_empty(), "`{beam}` emitted no output");
        for index in output {
            assert!(
                started < index && index < finished,
                "`{beam}`: started {started}, output {index}, finished {finished}"
            );
        }
    }

    let a_finished = only(
        positions(
            events,
            |e| matches!(e, RunEvent::BeamFinished { id, .. } if id.0 == "a"),
        ),
        "`a` finished event",
    );
    let b_started = only(
        positions(
            events,
            |e| matches!(e, RunEvent::BeamStarted { id } if id.0 == "b"),
        ),
        "`b` started event",
    );
    assert!(a_finished < b_started, "`b` starts after `a` finished");

    let run_finished = only(
        positions(events, |e| matches!(e, RunEvent::RunFinished { .. })),
        "run finished event",
    );
    assert_eq!(
        run_finished,
        events.len() - 1,
        "`RunFinished` must be the last event"
    );
    let RunEvent::RunFinished { summary } = &events[run_finished] else {
        unreachable!()
    };
    assert_eq!(ids(&summary.succeeded), ["a", "b"]);
    assert_eq!(ids(&outcome.summary().succeeded), ["a", "b"]);
}

#[tokio::test]
async fn docker_executor_is_rejected_before_anything_runs() {
    const SOURCE: &str = r#"
beam build { run "step build" }
beam deploy {
  needs [build]
  executor docker { image "deployer:latest" }
  run "step deploy"
}
"#;

    let executor = Arc::new(FakeExecutor::new());
    let outcome = run_target(SOURCE, "deploy", options(2, false), executor.clone()).await;

    assert_eq!(
        outcome.error().to_string(),
        "docker executor is not yet supported"
    );
    assert!(
        commands(&executor).is_empty(),
        "the rejection happens before any beam starts"
    );
    assert!(outcome.events.is_empty(), "no event is emitted either");
}

#[tokio::test]
async fn target_parameters_are_bound_into_commands_and_env() {
    const SOURCE: &str = r#"
beam deploy(target) {
  env { DEPLOY_TARGET = target }
  run "deploy {target}"
}
"#;

    let executor = Arc::new(FakeExecutor::new());
    let options = RunOptions {
        jobs: 2,
        keep_going: false,
        params: vec!["staging".to_string()],
        cache: None,
    };
    let outcome = run_target(SOURCE, "deploy", options, executor.clone()).await;

    assert_eq!(ids(&outcome.summary().succeeded), ["deploy"]);
    let calls = executor.calls();
    assert_eq!(calls[0].command, "deploy staging");
    assert_eq!(
        calls[0].env,
        [("DEPLOY_TARGET".to_string(), "staging".to_string())]
    );
}

#[tokio::test]
async fn parameter_count_must_match_the_target() {
    const NO_PARAMS: &str = r#"beam build { run "step build" }"#;
    const ONE_PARAM: &str = r#"beam deploy(target) { run "deploy {target}" }"#;

    let too_many = run_target(
        NO_PARAMS,
        "build",
        RunOptions {
            jobs: 1,
            keep_going: false,
            params: vec!["extra".to_string()],
            cache: None,
        },
        Arc::new(FakeExecutor::new()),
    )
    .await;
    assert_eq!(
        too_many.error().to_string(),
        "beam `build` takes no parameters, got 1"
    );

    let missing = run_target(
        ONE_PARAM,
        "deploy",
        options(1, false),
        Arc::new(FakeExecutor::new()),
    )
    .await;
    assert_eq!(
        missing.error().to_string(),
        "beam `deploy` expects 1 parameter (`target`), got 0"
    );
}

#[tokio::test]
async fn only_the_target_beam_may_declare_parameters() {
    const SOURCE: &str = r#"
beam helper(name) { run "help {name}" }
beam build { needs [helper] run "step build" }
"#;

    let executor = Arc::new(FakeExecutor::new());
    let outcome = run_target(SOURCE, "build", options(2, false), executor.clone()).await;

    assert_eq!(
        outcome.error().to_string(),
        "beam `helper` declares parameters, but only the target beam can be given any"
    );
    assert!(commands(&executor).is_empty());
}
