//! Behaviour tests for the skip-only cache, driven through
//! [`alba_engine::run`] with a `FakeExecutor` and a real temporary
//! directory for both the input files and the `.alba/cache` state.
//!
//! `load_str` gives every beam `dir == "."`; each test rewrites the beams'
//! `dir` to its own temporary directory so glob expansion and the cache
//! never touch the process's working directory.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use alba_core::{BeamId, load_str};
use alba_engine::{BeamStatus, CacheOptions, RunEvent, RunOptions, RunSummary, run};
use alba_executors::{FakeBehavior, FakeExecutor};
use tokio_util::sync::CancellationToken;

struct Outcome {
    summary: RunSummary,
    events: Vec<RunEvent>,
    executor: Arc<FakeExecutor>,
}

impl Outcome {
    fn executed(&self) -> Vec<String> {
        self.executor
            .calls()
            .into_iter()
            .map(|c| c.command)
            .collect()
    }
}

fn options(dir: &Path, force: bool) -> RunOptions {
    RunOptions {
        jobs: 4,
        keep_going: false,
        params: Vec::new(),
        cache: Some(CacheOptions {
            dir: dir.join(".alba").join("cache"),
            force,
        }),
    }
}

fn write(dir: &Path, name: &str, content: &str) {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn ids(bucket: &[BeamId]) -> Vec<&str> {
    bucket.iter().map(|id| id.0.as_str()).collect()
}

/// One run with a fresh executor, against a shared project dir (and thus
/// a shared cache). `params` are the target's positional arguments.
async fn run_once(
    source: &str,
    target: &str,
    dir: &Path,
    options: RunOptions,
    executor: FakeExecutor,
) -> Outcome {
    let mut project = load_str(source).expect("the test Beamfile must load");
    for beam in &mut project.beams {
        beam.dir = dir.to_path_buf();
    }
    let executor = Arc::new(executor);
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

    let summary = run(
        &project,
        &BeamId(target.to_string()),
        options,
        executor.clone(),
        events_tx,
        CancellationToken::new(),
    )
    .await
    .expect("the run must not fail");

    let mut events = Vec::new();
    while let Ok(event) = events_rx.try_recv() {
        events.push(event);
    }
    Outcome {
        summary,
        events,
        executor,
    }
}

const GEN: &str = r#"
beam gen {
  inputs ["data.txt"]
  run "generate"
}
"#;

#[tokio::test]
async fn a_second_unchanged_run_is_cached() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");

    let first = run_once(
        GEN,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;
    let second = run_once(
        GEN,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;

    assert_eq!(first.executed().len(), 1);
    assert!(
        second.executed().is_empty(),
        "the second run must not execute"
    );
    assert_eq!(ids(&second.summary.cached), vec!["gen"]);
    assert!(
        second
            .events
            .iter()
            .any(|event| matches!(event, RunEvent::BeamCached { id } if id.0 == "gen"))
    );
    assert!(
        !second
            .events
            .iter()
            .any(|event| matches!(event, RunEvent::BeamStarted { id } if id.0 == "gen"))
    );
}

#[tokio::test]
async fn a_changed_input_file_reruns() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");

    run_once(
        GEN,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;
    write(dir.path(), "data.txt", "v2");
    let second = run_once(
        GEN,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;

    assert_eq!(second.executed().len(), 1);
    assert!(second.summary.cached.is_empty());
}

#[tokio::test]
async fn a_changed_command_reruns() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");
    let changed = GEN.replace("generate", "generate --verbose");

    run_once(
        GEN,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;
    let second = run_once(
        &changed,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;

    assert_eq!(second.executed(), vec!["generate --verbose"]);
}

#[tokio::test]
async fn a_changed_env_block_reruns() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");
    let with_env = |value: &str| {
        format!(
            "beam gen {{\n  inputs [\"data.txt\"]\n  env {{ MODE = \"{value}\" }}\n  run \"generate\"\n}}\n"
        )
    };

    run_once(
        &with_env("debug"),
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;
    let second = run_once(
        &with_env("release"),
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;

    assert_eq!(second.executed().len(), 1);
}

#[tokio::test]
async fn changed_arguments_rerun() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");
    const PARAM: &str = r#"
beam deploy(target) {
  inputs ["data.txt"]
  run "deploy {target}"
}
"#;
    let with_args = |args: &[&str]| {
        let mut options = options(dir.path(), false);
        options.params = args.iter().map(|a| a.to_string()).collect();
        options
    };

    run_once(
        PARAM,
        "deploy",
        dir.path(),
        with_args(&["staging"]),
        FakeExecutor::new(),
    )
    .await;
    let same = run_once(
        PARAM,
        "deploy",
        dir.path(),
        with_args(&["staging"]),
        FakeExecutor::new(),
    )
    .await;
    let different = run_once(
        PARAM,
        "deploy",
        dir.path(),
        with_args(&["production"]),
        FakeExecutor::new(),
    )
    .await;

    assert!(same.executed().is_empty(), "same arguments must hit");
    assert_eq!(different.executed(), vec!["deploy production"]);
}

#[tokio::test]
async fn a_beam_without_inputs_always_runs() {
    let dir = tempfile::tempdir().unwrap();
    const NO_INPUTS: &str = r#"beam gen { run "generate" }"#;

    run_once(
        NO_INPUTS,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;
    let second = run_once(
        NO_INPUTS,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;

    assert_eq!(second.executed().len(), 1);
    assert!(second.summary.cached.is_empty());
}

#[tokio::test]
async fn no_cache_configuration_disables_caching() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");
    let disabled = || RunOptions {
        jobs: 4,
        keep_going: false,
        params: Vec::new(),
        cache: None,
    };

    run_once(GEN, "gen", dir.path(), disabled(), FakeExecutor::new()).await;
    let second = run_once(GEN, "gen", dir.path(), disabled(), FakeExecutor::new()).await;

    assert_eq!(second.executed().len(), 1);
}

#[tokio::test]
async fn missing_outputs_force_a_rerun() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");
    const WITH_OUTPUT: &str = r#"
beam gen {
  inputs ["data.txt"]
  outputs ["out.txt"]
  run "generate"
}
"#;
    // FakeExecutor touches no files, so the "produced" output is created
    // by hand: present for the second run, deleted before the third.
    write(dir.path(), "out.txt", "produced");

    run_once(
        WITH_OUTPUT,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;
    let with_output = run_once(
        WITH_OUTPUT,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;
    std::fs::remove_file(dir.path().join("out.txt")).unwrap();
    let without_output = run_once(
        WITH_OUTPUT,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;

    assert!(with_output.executed().is_empty(), "outputs present: hit");
    assert_eq!(without_output.executed().len(), 1, "outputs missing: rerun");
}

#[tokio::test]
async fn a_failed_beam_writes_no_manifest() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");

    let failing = FakeExecutor::new().on(
        "generate",
        FakeBehavior {
            exit_code: 1,
            ..Default::default()
        },
    );
    let first = run_once(GEN, "gen", dir.path(), options(dir.path(), false), failing).await;
    let second = run_once(
        GEN,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;

    assert_eq!(ids(&first.summary.failed), vec!["gen"]);
    assert_eq!(second.executed().len(), 1, "a failure must not have cached");
}

#[tokio::test]
async fn an_allowed_failure_writes_no_manifest_either() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");
    const ALLOWED: &str = r#"
beam gen {
  allow_failure true
  inputs ["data.txt"]
  run "generate"
}
"#;

    let failing = FakeExecutor::new().on(
        "generate",
        FakeBehavior {
            exit_code: 1,
            ..Default::default()
        },
    );
    run_once(
        ALLOWED,
        "gen",
        dir.path(),
        options(dir.path(), false),
        failing,
    )
    .await;
    let second = run_once(
        ALLOWED,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;

    assert_eq!(second.executed().len(), 1);
}

#[tokio::test]
async fn force_reruns_and_rewrites_the_manifest() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");

    run_once(
        GEN,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;
    let forced = run_once(
        GEN,
        "gen",
        dir.path(),
        options(dir.path(), true),
        FakeExecutor::new(),
    )
    .await;
    let after = run_once(
        GEN,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;

    assert_eq!(forced.executed().len(), 1, "--force must execute");
    assert!(forced.summary.cached.is_empty());
    assert!(
        after.executed().is_empty(),
        "the forced run must have rewritten the entry"
    );
}

#[tokio::test]
async fn a_really_changed_dependency_invalidates_its_dependents() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "codegen.txt", "v1");
    write(dir.path(), "app.txt", "v1");
    const CHAIN: &str = r#"
beam codegen {
  inputs ["codegen.txt"]
  run "generate"
}

beam build {
  needs [codegen]
  inputs ["app.txt"]
  run "compile"
}
"#;

    run_once(
        CHAIN,
        "build",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;
    let unchanged = run_once(
        CHAIN,
        "build",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;
    write(dir.path(), "codegen.txt", "v2");
    let changed = run_once(
        CHAIN,
        "build",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;

    assert!(
        unchanged.executed().is_empty(),
        "nothing changed: both cached"
    );
    assert_eq!(ids(&unchanged.summary.cached), vec!["codegen", "build"]);
    assert_eq!(
        changed.executed(),
        vec!["generate", "compile"],
        "a changed dependency fingerprint must cascade to its dependents"
    );
}

#[tokio::test]
async fn a_non_cacheable_dependency_does_not_poison_its_dependents() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "app.txt", "v1");
    const MIXED: &str = r#"
beam always {
  run "prepare"
}

beam build {
  needs [always]
  inputs ["app.txt"]
  run "compile"
}
"#;

    run_once(
        MIXED,
        "build",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;
    let second = run_once(
        MIXED,
        "build",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;

    assert_eq!(
        second.executed(),
        vec!["prepare"],
        "the input-less beam still runs"
    );
    assert_eq!(ids(&second.summary.cached), vec!["build"]);
}

#[tokio::test]
async fn a_corrupted_cache_entry_is_a_miss() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");

    run_once(
        GEN,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;
    for entry in std::fs::read_dir(dir.path().join(".alba/cache")).unwrap() {
        std::fs::write(entry.unwrap().path(), "garbage").unwrap();
    }
    let second = run_once(
        GEN,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;

    assert_eq!(second.executed().len(), 1);
}

#[tokio::test]
async fn the_cached_duration_is_the_original_runs() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");

    let slow = FakeExecutor::new().on(
        "generate",
        FakeBehavior {
            delay: Duration::from_millis(150),
            ..Default::default()
        },
    );
    run_once(GEN, "gen", dir.path(), options(dir.path(), false), slow).await;
    let second = run_once(
        GEN,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;

    let duration = second.events.iter().find_map(|event| match event {
        RunEvent::BeamFinished {
            status: BeamStatus::Cached,
            duration,
            ..
        } => Some(*duration),
        _ => None,
    });
    assert!(
        duration.expect("a cached BeamFinished must exist") >= Duration::from_millis(100),
        "the reported duration must be the original run's, not the replay's"
    );
}

#[tokio::test]
async fn a_changed_cwd_reruns() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");
    let with_cwd = |where_in: &str| {
        format!(
            "beam gen {{\n  cwd \"{where_in}\"\n  inputs [\"data.txt\"]\n  run \"generate\"\n}}\n"
        )
    };

    run_once(
        &with_cwd("build"),
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;
    let same = run_once(
        &with_cwd("build"),
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;
    let different = run_once(
        &with_cwd("dist"),
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;

    assert!(same.executed().is_empty(), "the same cwd must hit");
    assert_eq!(
        different.executed().len(),
        1,
        "a beam that now runs somewhere else must rerun"
    );
}

/// A beam holding a valid manifest must still report `Cancelled` once
/// fail-fast has stopped the run: a hit is not a licence to keep walking a
/// subgraph the run has abandoned.
///
/// `build` is gated behind `slow` so the assertion does not depend on
/// which of two independent beams its task happens to be polled first —
/// the cache decision is taken as soon as a beam's dependencies are
/// satisfied, so an ungated sibling would race `broken`'s failure. The
/// control run in the middle proves the manifest really is a hit, without
/// which the last assertion would hold vacuously.
#[tokio::test]
async fn a_pending_hit_is_cancelled_when_the_run_is_already_stopping() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "app.txt", "v1");
    const SOURCE: &str = r#"
beam broken {
  run "explode"
}

beam slow {
  run "wait"
}

beam build {
  needs [slow]
  inputs ["app.txt"]
  run "compile"
}

beam all {
  needs [broken, build]
  run "finish"
}
"#;
    let gated = || {
        FakeExecutor::new().on(
            "wait",
            FakeBehavior {
                delay: Duration::from_millis(200),
                ..Default::default()
            },
        )
    };

    run_once(
        SOURCE,
        "all",
        dir.path(),
        options(dir.path(), false),
        gated(),
    )
    .await;
    let control = run_once(
        SOURCE,
        "all",
        dir.path(),
        options(dir.path(), false),
        gated(),
    )
    .await;
    let stopping = run_once(
        SOURCE,
        "all",
        dir.path(),
        options(dir.path(), false),
        gated().on(
            "explode",
            FakeBehavior {
                exit_code: 1,
                ..Default::default()
            },
        ),
    )
    .await;

    assert_eq!(ids(&control.summary.cached), vec!["build"]);
    assert_eq!(ids(&stopping.summary.failed), vec!["broken"]);
    assert!(
        stopping.summary.cached.is_empty(),
        "a stopping run must report no hit"
    );
    assert_eq!(ids(&stopping.summary.cancelled), vec!["build", "all"]);
    assert!(
        !stopping
            .events
            .iter()
            .any(|event| matches!(event, RunEvent::BeamCached { .. }))
    );
}

#[tokio::test]
async fn a_hit_replays_the_stored_output_lines() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");

    let talkative = FakeExecutor::new().on(
        "generate",
        FakeBehavior {
            output_lines: vec![
                alba_executors::OutputLine {
                    stream: alba_executors::Stream::Stdout,
                    text: "generated 12 files".to_string(),
                },
                alba_executors::OutputLine {
                    stream: alba_executors::Stream::Stderr,
                    text: "warning: deprecated".to_string(),
                },
            ],
            ..Default::default()
        },
    );
    run_once(
        GEN,
        "gen",
        dir.path(),
        options(dir.path(), false),
        talkative,
    )
    .await;
    let second = run_once(
        GEN,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;

    let replayed: Vec<(&str, bool)> = second
        .events
        .iter()
        .filter_map(|event| match event {
            RunEvent::BeamOutput { line, replayed, .. } => Some((line.text.as_str(), *replayed)),
            _ => None,
        })
        .collect();
    assert_eq!(
        replayed,
        vec![("generated 12 files", true), ("warning: deprecated", true)],
        "stored lines must replay, in order, marked as replayed"
    );
}

#[tokio::test]
async fn live_output_is_not_marked_replayed() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");

    let talkative = FakeExecutor::new().on(
        "generate",
        FakeBehavior {
            output_lines: vec![alba_executors::OutputLine {
                stream: alba_executors::Stream::Stdout,
                text: "generated".to_string(),
            }],
            ..Default::default()
        },
    );
    let first = run_once(
        GEN,
        "gen",
        dir.path(),
        options(dir.path(), false),
        talkative,
    )
    .await;

    assert!(first.events.iter().any(|event| matches!(
        event,
        RunEvent::BeamOutput {
            replayed: false,
            ..
        }
    )));
    assert!(
        !first
            .events
            .iter()
            .any(|event| matches!(event, RunEvent::BeamOutput { replayed: true, .. }))
    );
}

/// The replay order contract: `BeamCached`, then every replayed line,
/// then `BeamFinished` — mirroring a live beam's started/output/finished.
#[tokio::test]
async fn replayed_lines_sit_between_cached_and_finished() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");

    let talkative = FakeExecutor::new().on(
        "generate",
        FakeBehavior {
            output_lines: vec![alba_executors::OutputLine {
                stream: alba_executors::Stream::Stdout,
                text: "generated".to_string(),
            }],
            ..Default::default()
        },
    );
    run_once(
        GEN,
        "gen",
        dir.path(),
        options(dir.path(), false),
        talkative,
    )
    .await;
    let second = run_once(
        GEN,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;

    let index = |predicate: &dyn Fn(&RunEvent) -> bool| {
        second
            .events
            .iter()
            .position(predicate)
            .expect("event must exist")
    };
    let cached = index(&|e| matches!(e, RunEvent::BeamCached { .. }));
    let output = index(&|e| matches!(e, RunEvent::BeamOutput { replayed: true, .. }));
    let finished = index(&|e| matches!(e, RunEvent::BeamFinished { .. }));
    assert!(cached < output && output < finished);
}
