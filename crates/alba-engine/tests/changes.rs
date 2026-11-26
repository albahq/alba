//! What `git.changes()` answers at run time: each beam's mode against the
//! run's change set, and the `unchanged` status of a beam with nothing to
//! process. Driven through [`alba_engine::run`] with the embedded shell, in
//! real temporary repositories.
//!
//! The repositories live in `tempfile::tempdir()`, which on macOS sits
//! under `/var/folders/...`, a symbolic link to `/private/var/...`. Every
//! incremental list asserted non-empty here therefore also proves that the
//! changed paths and the beams' directories are built in the same form.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use alba_core::{BeamId, ChangeMode, FullReason, Project, SourceMap, load_project};
use alba_engine::{
    BeamStatus, Executors, RunChanges, RunEvent, RunOptions, RunSummary, Targets, resolve_changes,
    run,
};
use alba_executors::{EmbeddedShellExecutor, Stream};
use tokio_util::sync::CancellationToken;

const PHPCS: &str = r#"
git { base 'main' }

beam phpcs {
  inputs ["src/**/*.php", "phpcs.xml"]
  run "echo {git.changes('src/**/*.php')}"
}
"#;

fn write(path: PathBuf, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
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

/// A repository on `main` holding `beamfile` and a few PHP sources, all
/// committed.
fn repository(beamfile: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.email", "alba@example.com"]);
    git(dir.path(), &["config", "user.name", "Alba"]);
    git(dir.path(), &["config", "commit.gpgsign", "false"]);
    // The machine's global excludes would otherwise apply: one that
    // ignores `Beamfile` leaves it uncommitted.
    git(
        dir.path(),
        &["config", "core.excludesFile", ".git/no-global-excludes"],
    );
    write(dir.path().join("Beamfile"), beamfile);
    write(dir.path().join("phpcs.xml"), "<ruleset/>");
    write(dir.path().join("composer.lock"), "{}");
    write(dir.path().join("src/A.php"), "<?php // a");
    write(dir.path().join("src/Billing/B.php"), "<?php // b");
    write(dir.path().join("docs/index.md"), "docs");
    commit(dir.path(), "init");
    dir
}

fn commit(dir: &Path, message: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
}

fn branch(dir: &Path, name: &str) {
    git(dir, &["checkout", "-q", "-b", name]);
}

fn options(changes: Option<RunChanges>, unresolved_base: bool, sources: SourceMap) -> RunOptions {
    RunOptions {
        jobs: 4,
        keep_going: false,
        params: Vec::new(),
        cache: None,
        extra_env: Vec::new(),
        ambient_env: Vec::new(),
        definitions: Default::default(),
        changes,
        unresolved_base,
        sources,
    }
}

struct Outcome {
    summary: RunSummary,
    events: Vec<RunEvent>,
}

impl Outcome {
    /// What `beam` printed on stdout, line by line.
    fn stdout(&self, beam: &str) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|event| match event {
                RunEvent::BeamOutput { id, line, .. }
                    if id.0 == beam && line.stream == Stream::Stdout =>
                {
                    Some(line.text.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// The mode `beam` announced when it started; `None` inside when it
    /// started without one, a panic when it never started.
    fn mode(&self, beam: &str) -> Option<ChangeMode> {
        self.events
            .iter()
            .find_map(|event| match event {
                RunEvent::BeamStarted { id, mode } if id.0 == beam => Some(mode.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("`{beam}` never started"))
    }

    fn status(&self, beam: &str) -> BeamStatus {
        self.events
            .iter()
            .find_map(|event| match event {
                RunEvent::BeamFinished { id, status, .. } if id.0 == beam => Some(status.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("`{beam}` never finished"))
    }

    fn base(&self) -> Option<(String, String)> {
        self.events
            .iter()
            .find_map(|event| match event {
                RunEvent::RunStarted { base, .. } => Some(base.clone()),
                _ => None,
            })
            .expect("every run starts")
    }
}

fn load(dir: &Path) -> (Project, SourceMap) {
    load_project(&dir.join("Beamfile")).expect("the test Beamfile must load")
}

/// Runs `target` with the change set against the project's own `base`.
async fn run_incremental(dir: &Path, target: &str) -> Outcome {
    let (project, sources) = load(dir);
    let reference = project
        .git
        .base
        .clone()
        .expect("the Beamfile declares a base");
    let changes = resolve_changes(dir, &reference).expect("the base resolves");
    run_with(&project, target, options(Some(changes), false, sources)).await
}

async fn run_with(project: &Project, target: &str, options: RunOptions) -> Outcome {
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let summary = run(
        project,
        &Targets::beam(BeamId(target.to_string())),
        options,
        Executors::uniform(Arc::new(EmbeddedShellExecutor)),
        events_tx,
        CancellationToken::new(),
    )
    .await
    .expect("the run must not fail");
    let mut events = Vec::new();
    while let Ok(event) = events_rx.try_recv() {
        events.push(event);
    }
    Outcome { summary, events }
}

fn ids(bucket: &[BeamId]) -> Vec<&str> {
    bucket.iter().map(|id| id.0.as_str()).collect()
}

fn head_sha(dir: &Path, reference: &str) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", reference])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[tokio::test]
async fn only_the_changed_files_reach_the_command() {
    let dir = repository(PHPCS);
    branch(dir.path(), "feature");
    write(dir.path().join("src/A.php"), "<?php // a2");
    commit(dir.path(), "change a");
    // An untracked source counts; a tracked change outside every input
    // does not.
    write(dir.path().join("src/New.php"), "<?php // new");
    write(dir.path().join("docs/index.md"), "docs 2");

    let outcome = run_incremental(dir.path(), "phpcs").await;

    assert_eq!(outcome.stdout("phpcs"), ["src/A.php src/New.php"]);
    assert_eq!(outcome.mode("phpcs"), Some(ChangeMode::Incremental));
    assert_eq!(outcome.status("phpcs"), BeamStatus::Succeeded);
    assert_eq!(
        outcome.base(),
        Some(("main".to_string(), head_sha(dir.path(), "main")))
    );
}

#[tokio::test]
async fn a_config_input_change_runs_full() {
    let dir = repository(PHPCS);
    branch(dir.path(), "feature");
    write(dir.path().join("src/A.php"), "<?php // a2");
    write(dir.path().join("phpcs.xml"), "<ruleset name='strict'/>");
    commit(dir.path(), "stricter rules");

    let outcome = run_incremental(dir.path(), "phpcs").await;

    assert_eq!(outcome.stdout("phpcs"), ["src"]);
    assert_eq!(
        outcome.mode("phpcs"),
        Some(ChangeMode::Full(FullReason::InputOutsideChanges(
            "phpcs.xml".to_string()
        )))
    );
}

#[tokio::test]
async fn a_beamfile_change_runs_full() {
    let dir = repository(PHPCS);
    branch(dir.path(), "feature");
    write(
        dir.path().join("Beamfile"),
        &format!("{PHPCS}\n# tweaked\n"),
    );
    write(dir.path().join("src/A.php"), "<?php // a2");
    commit(dir.path(), "tweak the Beamfile");

    let outcome = run_incremental(dir.path(), "phpcs").await;

    assert_eq!(outcome.stdout("phpcs"), ["src"]);
    assert_eq!(
        outcome.mode("phpcs"),
        Some(ChangeMode::Full(FullReason::BeamfileChanged))
    );
}

const WITH_INSTALL: &str = r#"
git { base 'main' }

beam install {
  inputs ["composer.lock"]
  run "echo install"
}

beam phpcs {
  needs [install]
  inputs ["src/**/*.php", "phpcs.xml"]
  run "echo {git.changes('src/**/*.php')}"
}
"#;

#[tokio::test]
async fn an_affected_dependency_runs_full() {
    let dir = repository(WITH_INSTALL);
    branch(dir.path(), "feature");
    write(dir.path().join("composer.lock"), "{\"tool\": 2}");
    write(dir.path().join("src/A.php"), "<?php // a2");
    commit(dir.path(), "upgrade the tool");

    let outcome = run_incremental(dir.path(), "phpcs").await;

    assert_eq!(outcome.stdout("phpcs"), ["src"]);
    assert_eq!(
        outcome.mode("phpcs"),
        Some(ChangeMode::Full(FullReason::DependencyAffected(BeamId(
            "install".to_string()
        ))))
    );
}

const WITH_CHAIN: &str = r#"
git { base 'main' }

beam install {
  inputs ["composer.lock"]
  run "echo install"
}

beam b {
  needs [install]
  run "echo b"
}

beam a {
  needs [b]
  inputs ["src/**/*.php"]
  run "echo {git.changes('src/**/*.php')}"
}
"#;

/// Affected through `needs` is transitive: the reason names the direct
/// dependency the change reaches `a` through.
#[tokio::test]
async fn a_dependency_affected_through_a_chain_runs_full() {
    let dir = repository(WITH_CHAIN);
    branch(dir.path(), "feature");
    write(dir.path().join("composer.lock"), "{\"tool\": 2}");
    write(dir.path().join("src/A.php"), "<?php // a2");
    commit(dir.path(), "upgrade the tool");

    let outcome = run_incremental(dir.path(), "a").await;

    assert_eq!(outcome.stdout("a"), ["src"]);
    assert_eq!(
        outcome.mode("a"),
        Some(ChangeMode::Full(FullReason::DependencyAffected(BeamId(
            "b".to_string()
        ))))
    );
}

const WITH_INCREMENTAL_DEPENDENCY: &str = r#"
git { base 'main' }

beam gen {
  inputs ["schema/**/*.json"]
  run "echo {git.changes('schema/**/*.json')}"
}

beam phpcs {
  needs [gen]
  inputs ["src/**/*.php"]
  run "echo {git.changes('src/**/*.php')}"
}
"#;

/// A dependency that is itself incremental on its own changed sources
/// still makes its dependent run full: any doubt means full mode.
#[tokio::test]
async fn an_incremental_dependency_affected_by_its_sources_runs_its_dependent_full() {
    let dir = repository(WITH_INCREMENTAL_DEPENDENCY);
    write(dir.path().join("schema/a.json"), "{}");
    commit(dir.path(), "schema");
    branch(dir.path(), "feature");
    write(dir.path().join("schema/a.json"), "{\"v\": 2}");
    write(dir.path().join("src/A.php"), "<?php // a2");
    commit(dir.path(), "change both");

    let outcome = run_incremental(dir.path(), "phpcs").await;

    assert_eq!(outcome.mode("gen"), Some(ChangeMode::Incremental));
    assert_eq!(outcome.stdout("gen"), ["schema/a.json"]);
    assert_eq!(outcome.stdout("phpcs"), ["src"]);
    assert_eq!(
        outcome.mode("phpcs"),
        Some(ChangeMode::Full(FullReason::DependencyAffected(BeamId(
            "gen".to_string()
        ))))
    );
}

#[tokio::test]
async fn the_target_branch_on_a_clean_tree_runs_full() {
    let dir = repository(PHPCS);

    let outcome = run_incremental(dir.path(), "phpcs").await;

    assert_eq!(outcome.stdout("phpcs"), ["src"]);
    assert_eq!(
        outcome.mode("phpcs"),
        Some(ChangeMode::Full(FullReason::OnTargetBranch))
    );
}

#[tokio::test]
async fn the_target_branch_with_uncommitted_work_stays_incremental() {
    let dir = repository(PHPCS);
    write(dir.path().join("src/Billing/B.php"), "<?php // b2");

    let outcome = run_incremental(dir.path(), "phpcs").await;

    assert_eq!(outcome.stdout("phpcs"), ["src/Billing/B.php"]);
    assert_eq!(outcome.mode("phpcs"), Some(ChangeMode::Incremental));
}

#[tokio::test]
async fn a_fresh_branch_with_uncommitted_work_stays_incremental() {
    let dir = repository(PHPCS);
    branch(dir.path(), "feature");
    write(dir.path().join("src/A.php"), "<?php // a2");

    let outcome = run_incremental(dir.path(), "phpcs").await;

    assert_eq!(outcome.stdout("phpcs"), ["src/A.php"]);
    assert_eq!(outcome.mode("phpcs"), Some(ChangeMode::Incremental));
}

const WITH_REPORT: &str = r#"
git { base 'main' }

beam phpcs {
  inputs ["src/**/*.php", "phpcs.xml"]
  run "echo {git.changes('src/**/*.php')}"
}

beam report {
  needs [phpcs]
  run "echo report"
}
"#;

#[tokio::test]
async fn a_beam_with_no_changed_file_ends_unchanged() {
    let dir = repository(WITH_REPORT);
    branch(dir.path(), "feature");
    write(dir.path().join("docs/index.md"), "docs 2");
    commit(dir.path(), "docs only");

    let outcome = run_incremental(dir.path(), "report").await;

    assert_eq!(outcome.status("phpcs"), BeamStatus::Unchanged);
    assert!(
        outcome.stdout("phpcs").is_empty(),
        "an unchanged beam runs nothing"
    );
    assert!(
        !outcome
            .events
            .iter()
            .any(|event| matches!(event, RunEvent::BeamStarted { id, .. } if id.0 == "phpcs")),
        "an unchanged beam never starts"
    );
    assert_eq!(outcome.status("report"), BeamStatus::Succeeded);
    assert_eq!(outcome.stdout("report"), ["report"]);
    assert_eq!(ids(&outcome.summary.unchanged), ["phpcs"]);
    assert_eq!(ids(&outcome.summary.succeeded), ["report"]);
    assert_eq!(outcome.summary.exit_code(), 0);
}

#[tokio::test]
async fn a_beam_with_only_deleted_files_ends_unchanged() {
    let dir = repository(PHPCS);
    branch(dir.path(), "feature");
    git(dir.path(), &["rm", "-q", "src/A.php"]);
    commit(dir.path(), "drop a");

    let outcome = run_incremental(dir.path(), "phpcs").await;

    assert_eq!(outcome.status("phpcs"), BeamStatus::Unchanged);
    assert_eq!(ids(&outcome.summary.unchanged), ["phpcs"]);
}

const WITH_PLAIN: &str = r#"
git { base 'main' }

beam plain {
  inputs ["src/**/*.php"]
  run "echo plain"
}
"#;

#[tokio::test]
async fn a_beam_without_git_changes_runs_as_before() {
    let dir = repository(WITH_PLAIN);
    branch(dir.path(), "feature");
    write(dir.path().join("docs/index.md"), "docs 2");
    commit(dir.path(), "docs only");

    let outcome = run_incremental(dir.path(), "plain").await;

    assert_eq!(outcome.mode("plain"), None);
    assert_eq!(outcome.stdout("plain"), ["plain"]);
    assert_eq!(outcome.status("plain"), BeamStatus::Succeeded);
}

#[tokio::test]
async fn no_reference_runs_full() {
    let dir = repository(PHPCS);
    branch(dir.path(), "feature");
    write(dir.path().join("src/A.php"), "<?php // a2");
    let (project, sources) = load(dir.path());

    let outcome = run_with(&project, "phpcs", options(None, false, sources)).await;

    assert_eq!(outcome.stdout("phpcs"), ["src"]);
    assert_eq!(
        outcome.mode("phpcs"),
        Some(ChangeMode::Full(FullReason::NoReference))
    );
    assert_eq!(outcome.base(), None);
}

/// A shallow clone in CI: the declared base is fetched, but not deep
/// enough to reach the merge base. The warning is the caller's to print;
/// the engine runs every beam full.
#[tokio::test]
async fn unresolved_base_runs_full_with_a_warning() {
    // `main` moves on after `feature` forks, so a depth-1 fetch of `main`
    // stops short of the fork point.
    let origin = repository(PHPCS);
    branch(origin.path(), "feature");
    write(origin.path().join("src/A.php"), "<?php // a2");
    commit(origin.path(), "feature work");
    git(origin.path(), &["checkout", "-q", "main"]);
    write(origin.path().join("docs/index.md"), "docs 2");
    commit(origin.path(), "main moves on");

    let parent = tempfile::tempdir().unwrap();
    // `file://`: a plain path makes git ignore `--depth`.
    let url = format!("file://{}", origin.path().display());
    git(
        parent.path(),
        &[
            "clone", "-q", "--depth", "1", "--branch", "feature", &url, "clone",
        ],
    );
    let clone = parent.path().join("clone");
    // A shallow clone tracks one branch: the refspec writes the reference.
    git(
        &clone,
        &[
            "fetch",
            "-q",
            "--depth",
            "1",
            "origin",
            "main:refs/remotes/origin/main",
        ],
    );

    let error = resolve_changes(&clone, "origin/main")
        .expect_err("a merge base the shallow clone does not hold cannot resolve");
    let message = error.to_string();
    assert!(
        message.contains("origin/main"),
        "names the reference: {message}"
    );
    assert!(message.contains("shallow"), "says why: {message}");

    let (project, sources) = load(&clone);
    let outcome = run_with(&project, "phpcs", options(None, true, sources)).await;

    assert_eq!(outcome.stdout("phpcs"), ["src"]);
    assert_eq!(
        outcome.mode("phpcs"),
        Some(ChangeMode::Full(FullReason::ReferenceUnresolved))
    );
    assert_eq!(outcome.status("phpcs"), BeamStatus::Succeeded);
}

/// A reference git does not know at all is unresolved the same way.
#[tokio::test]
async fn an_unknown_reference_cannot_resolve() {
    let dir = repository(PHPCS);

    let error = resolve_changes(dir.path(), "origin/develop")
        .expect_err("a reference the repository does not have cannot resolve");

    assert!(
        error.to_string().contains("origin/develop"),
        "names the reference: {error}"
    );
}
