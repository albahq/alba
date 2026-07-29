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

use alba_core::{BeamId, ExecutorKind, load_str};
use alba_engine::{BeamStatus, CacheOptions, Executors, RunEvent, RunOptions, RunSummary, run};
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

    /// Every stderr line this run emitted, live or replayed — where the
    /// cache tells the user why a beam is running uncached.
    fn stderr(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|event| match event {
                RunEvent::BeamOutput { line, .. }
                    if line.stream == alba_executors::Stream::Stderr =>
                {
                    Some(line.text.clone())
                }
                _ => None,
            })
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
    run_prepared(source, target, dir, options, executor, |_| {}).await
}

/// [`run_once`] with a hook to amend the loaded project, for a scenario
/// that cannot be spelled in a Beamfile any more — loading now rejects a
/// pattern that will not compile, so reaching the engine with one takes a
/// [`alba_core::Project`] built by hand, as any library consumer may.
async fn run_prepared(
    source: &str,
    target: &str,
    dir: &Path,
    options: RunOptions,
    executor: FakeExecutor,
    prepare: impl FnOnce(&mut alba_core::Project),
) -> Outcome {
    let mut project = load_str(source).expect("the test Beamfile must load");
    for beam in &mut project.beams {
        beam.dir = dir.to_path_buf();
    }
    prepare(&mut project);
    let executor = Arc::new(executor);
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

    let summary = run(
        &project,
        &BeamId(target.to_string()),
        options,
        Executors::uniform(executor.clone()),
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

/// A cached beam's manifest was computed under one executor; replaying it
/// after the beam switched to the other must not happen, because the two
/// executors can run the very same command differently (the whole reason
/// `system_shell` exists as an opt-out). `unchanged` is the control: it
/// proves the cache really does hit here absent the switch, so the
/// `switched` assertion is not vacuous.
#[tokio::test]
async fn switching_executor_kind_invalidates_the_cache() {
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
    let unchanged = run_once(
        GEN,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;
    let switched = run_prepared(
        GEN,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
        |project| project.beams[0].executor = ExecutorKind::SystemShell,
    )
    .await;

    assert!(
        unchanged.executed().is_empty(),
        "precondition: an unchanged rerun must still hit"
    );
    assert_eq!(
        switched.executed(),
        vec!["generate"],
        "switching a beam's executor must invalidate its cache entry rather than replay it"
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

/// The four tests below share one shape: a beam that declares `inputs`
/// but, for four different ordinary reasons, resolved none of the files
/// the author meant. Each once produced a permanently cached beam — the
/// fingerprint of an empty file list is a constant, so the first run
/// succeeded, wrote a manifest, and every later run replayed it however
/// much the sources changed. A beam whose `inputs` resolve to nothing is
/// not cacheable for that run, and says so.
const SRC: &str = r#"
beam gen {
  inputs ["src/**/*.rs"]
  run "generate"
}
"#;

/// One pattern that will not compile must not silence the ones that do:
/// the globset is built per pattern, so `src/**/*.rs` still resolves.
#[tokio::test]
async fn a_pattern_that_will_not_compile_does_not_silence_its_siblings() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/a.rs", "v1");
    let broken = |project: &mut alba_core::Project| {
        project.beams[0].inputs = vec!["src/**/*.rs".to_string(), "a[b".to_string()];
    };

    run_prepared(
        SRC,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
        broken,
    )
    .await;
    write(dir.path(), "src/a.rs", "v2");
    let second = run_prepared(
        SRC,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
        broken,
    )
    .await;

    assert_eq!(
        second.executed().len(),
        1,
        "an uncompilable sibling must not make every pattern match nothing"
    );
}

/// An input directory the project's own `.gitignore` excludes resolves to
/// nothing: glob expansion is deliberately `.gitignore`-aware.
#[tokio::test]
async fn an_ignored_input_directory_is_not_cached_silently() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), ".gitignore", "src/\n");
    write(dir.path(), "src/a.rs", "v1");

    run_once(
        SRC,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;
    write(dir.path(), "src/a.rs", "v2");
    let second = run_once(
        SRC,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;

    assert_eq!(
        second.executed().len(),
        1,
        "an ignored input must not cache"
    );
    assert!(second.summary.cached.is_empty());
}

/// A misspelled path matches nothing, and the beam is told so where the
/// user is already looking rather than caching forever in silence.
#[tokio::test]
async fn a_misspelled_input_path_is_not_cached_silently() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/a.rs", "v1");
    const MISSPELLED: &str = r#"
beam gen {
  inputs ["scr/**/*.rs"]
  run "generate"
}
"#;

    run_once(
        MISSPELLED,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;
    write(dir.path(), "src/a.rs", "v2");
    let second = run_once(
        MISSPELLED,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;

    assert_eq!(second.executed().len(), 1);
    assert!(
        second
            .stderr()
            .iter()
            .any(|line| line.starts_with("cache:") && line.contains("running without cache")),
        "a beam whose inputs matched nothing must say so, got {:?}",
        second.stderr()
    );
}

/// An input reached through a symbolic link is a real input: it is
/// followed and hashed by content, so an unchanged run still hits and a
/// change to the link's target still reruns.
#[cfg(unix)]
#[tokio::test]
async fn an_input_reached_through_a_symbolic_link_is_hashed() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "real/a.rs", "v1");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::os::unix::fs::symlink("../real/a.rs", dir.path().join("src/a.rs")).unwrap();

    run_once(
        SRC,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;
    let unchanged = run_once(
        SRC,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;
    write(dir.path(), "real/a.rs", "v2");
    let changed = run_once(
        SRC,
        "gen",
        dir.path(),
        options(dir.path(), false),
        FakeExecutor::new(),
    )
    .await;

    assert_eq!(
        ids(&unchanged.summary.cached),
        vec!["gen"],
        "the linked file is a real input, so an unchanged run hits"
    );
    assert_eq!(
        changed.executed().len(),
        1,
        "a change behind the link must rerun"
    );
}

/// An input that exists but cannot be read is the other way the cache
/// degrades rather than fails: the beam runs, uncached, and says why.
///
/// Unix only — `chmod` is what makes a file unreadable here — and skipped
/// where the mode turns out not to be enforced, which is the case for
/// root and would make the whole scenario vacuous. Probed rather than
/// deduced from the user id: the question is whether the read fails, and
/// that is exactly what the probe asks.
#[cfg(unix)]
#[tokio::test]
async fn an_unreadable_input_runs_the_beam_uncached_with_a_notice() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");
    let input = dir.path().join("data.txt");
    std::fs::set_permissions(&input, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&input).is_ok() {
        return;
    }

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

    assert_eq!(ids(&first.summary.succeeded), vec!["gen"]);
    assert!(
        first
            .stderr()
            .iter()
            .any(|line| line.starts_with("cache: cannot hash input `data.txt`")),
        "an unhashable input must say so, got {:?}",
        first.stderr()
    );
    assert_eq!(
        second.executed().len(),
        1,
        "an unhashable input leaves nothing to hit on"
    );
}
