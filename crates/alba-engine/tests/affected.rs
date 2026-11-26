//! Affected selection: the pure computation over a loaded project (no git
//! involved, paths are handed in) and `select` against a real repository.

use std::path::{Path, PathBuf};
use std::process::Command;

use alba_core::{BeamId, load_project};
use alba_engine::{Selection, affected, select};

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

const BEAMFILE: &str = r#"
import "api/Beamfile" as api
beam codegen { inputs ["schema/**"] run "gen" }
beam build { needs [codegen] inputs ["src/**/*.rs"] run "build" }
beam test { needs [build] run "test" }
beam docs { inputs ["docs/**"] run "docs" }
beam deploy(target) { inputs ["deploy/**"] run "deploy {target}" }
beam free { run "free" }
"#;

const API_BEAMFILE: &str = r#"
beam lint { inputs ["**/*.rs"] run "lint" }
"#;

/// A committed repository holding the two Beamfiles and one file per
/// input pattern.
fn repository() -> tempfile::TempDir {
    let mut files = vec![("Beamfile", BEAMFILE), ("api/Beamfile", API_BEAMFILE)];
    for file in [
        "schema/a.json",
        "src/lib.rs",
        "docs/index.md",
        "deploy/x",
        "api/src/lib.rs",
    ] {
        files.push((file, "v1"));
    }
    repository_with(&files)
}

/// A repository holding `files`, all committed.
fn repository_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.email", "alba@example.com"]);
    git(dir.path(), &["config", "user.name", "Alba"]);
    git(dir.path(), &["config", "commit.gpgsign", "false"]);
    // The machine's global excludes would otherwise apply: one that
    // ignores `Beamfile` leaves it uncommitted and every assertion about it
    // answering for that machine instead of for the code under test.
    git(
        dir.path(),
        &["config", "core.excludesFile", ".git/no-global-excludes"],
    );
    for (file, content) in files {
        write(dir.path().join(file), content);
    }
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

fn names(ids: &[BeamId]) -> Vec<&str> {
    ids.iter().map(|id| id.0.as_str()).collect()
}

fn affected_after(dir: &tempfile::TempDir, touch: &[&str]) -> Vec<BeamId> {
    for file in touch {
        let path = dir.path().join(file);
        // A touched Beamfile must stay valid syntax: a plain overwrite
        // would corrupt the declarations `affected` is meant to attribute
        // the change to, so a change to one is appended as a comment.
        if path.file_name().is_some_and(|name| name == "Beamfile") {
            let mut content = std::fs::read_to_string(&path).unwrap();
            content.push_str("\n# v2\n");
            write(path, &content);
        } else {
            write(path, "v2");
        }
    }
    let beamfile = dir.path().join("Beamfile");
    let (project, sources) = load_project(&beamfile).unwrap();
    affected(&project, &sources, dir.path(), "HEAD").unwrap()
}

#[test]
fn a_changed_input_affects_its_beam_and_every_dependent() {
    let dir = repository();
    assert_eq!(
        names(&affected_after(&dir, &["schema/a.json"])),
        ["codegen", "build", "test"]
    );
}

#[test]
fn a_beam_without_inputs_is_never_directly_affected() {
    let dir = repository();
    assert!(affected_after(&dir, &[]).is_empty());
    assert_eq!(
        names(&affected_after(&dir, &["src/lib.rs"])),
        ["build", "test"]
    );
}

#[test]
fn a_deleted_input_counts_as_changed() {
    let dir = repository();
    std::fs::remove_file(dir.path().join("docs/index.md")).unwrap();
    assert_eq!(names(&affected_after(&dir, &[])), ["docs"]);
}

#[test]
fn an_imported_beam_matches_paths_under_its_own_directory() {
    let dir = repository();
    assert_eq!(
        names(&affected_after(&dir, &["api/src/lib.rs"])),
        ["api:lint"]
    );
    // Root `src/` is not under `api/`, so `api:lint`'s `**/*.rs` misses it.
    let dir = repository();
    assert!(!names(&affected_after(&dir, &["src/lib.rs"])).contains(&"api:lint"));
}

/// An imported beam's input may lie above its own Beamfile, such as a
/// `Dockerfile` at the project root: changing that file affects the beam,
/// while a file of the same name under the import's directory does not.
#[test]
fn an_input_above_its_beamfile_affects_the_beam_declaring_it() {
    let dir = repository_with(&[
        (
            "Beamfile",
            "import \"contexts/shared/Beamfile\" as shared\n",
        ),
        (
            "contexts/shared/Beamfile",
            "beam test { inputs [\"../../Dockerfile\"] run \"test\" }\n",
        ),
        ("Dockerfile", "v1"),
        ("contexts/shared/Dockerfile", "v1"),
    ]);
    assert!(affected_after(&dir, &["contexts/shared/Dockerfile"]).is_empty());
    assert_eq!(
        names(&affected_after(&dir, &["Dockerfile"])),
        ["shared:test"]
    );
}

#[test]
fn a_changed_beamfile_affects_every_beam_it_declares() {
    let dir = repository();
    let affected = affected_after(&dir, &["api/Beamfile"]);
    assert_eq!(names(&affected), ["api:lint"]);
    let dir = repository();
    let affected = affected_after(&dir, &["Beamfile"]);
    assert_eq!(
        names(&affected),
        ["codegen", "build", "test", "docs", "deploy", "free"]
    );
}

#[test]
fn select_within_a_beam_is_that_beam_or_nothing() {
    let dir = repository();
    write(dir.path().join("schema/a.json"), "v2");
    let (project, sources) = load_project(&dir.path().join("Beamfile")).unwrap();
    let within = |beam: &str| Selection::Affected {
        reference: "HEAD".to_string(),
        within: Some(BeamId(beam.to_string())),
    };
    let targets = select(&project, &sources, dir.path(), &within("test")).unwrap();
    assert_eq!(names(&targets.beams), ["test"]);
    assert_eq!(targets.affected_by.as_deref(), Some("HEAD"));
    let targets = select(&project, &sources, dir.path(), &within("docs")).unwrap();
    assert!(targets.beams.is_empty());
    assert!(select(&project, &sources, dir.path(), &within("nope")).is_err());
}

#[test]
fn select_without_a_beam_targets_every_affected_beam_but_parameterized_ones() {
    let dir = repository();
    write(dir.path().join("deploy/x"), "v2");
    write(dir.path().join("docs/index.md"), "v2");
    let (project, sources) = load_project(&dir.path().join("Beamfile")).unwrap();
    let selection = Selection::Affected {
        reference: "HEAD".to_string(),
        within: None,
    };
    let targets = select(&project, &sources, dir.path(), &selection).unwrap();
    assert_eq!(names(&targets.beams), ["docs"]);
    // The listing still names `deploy`; only running skips it.
    assert_eq!(
        names(&affected(&project, &sources, dir.path(), "HEAD").unwrap()),
        ["docs", "deploy"]
    );
}

/// No `git init` here, on purpose: `Selection::Beam` must never touch git
/// at all, so this asserts it by making any git access fail rather than
/// by watching that it happens not to. A regression that made the `Beam`
/// path call `changed_files` would fail loudly here instead of passing
/// unnoticed inside a real repository.
#[test]
fn select_a_plain_beam_is_the_old_behaviour() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path().join("Beamfile"), "beam free { run \"free\" }\n");
    let (project, sources) = load_project(&dir.path().join("Beamfile")).unwrap();
    let targets = select(
        &project,
        &sources,
        dir.path(),
        &Selection::Beam(BeamId("free".into())),
    )
    .unwrap();
    assert_eq!(names(&targets.beams), ["free"]);
    assert!(targets.affected_by.is_none());
    assert!(
        select(
            &project,
            &sources,
            dir.path(),
            &Selection::Beam(BeamId("nope".into()))
        )
        .is_err()
    );
}

#[test]
fn an_unknown_reference_is_reported_as_a_git_error() {
    let dir = repository();
    let (project, sources) = load_project(&dir.path().join("Beamfile")).unwrap();
    let err = affected(&project, &sources, dir.path(), "no-such-ref").unwrap_err();
    assert!(err.to_string().contains("no-such-ref"), "{err}");
}

/// A project living in `app/` of a larger repository, whose inputs and
/// imports climb out of it: what changed next to it is still its business.
fn nested_repository() -> tempfile::TempDir {
    repository_with(&[
        (
            "app/Beamfile",
            "import \"../shared/Beamfile\" as shared\n\
             beam build { inputs [\"../libs/**\"] run \"build\" }\n",
        ),
        ("shared/Beamfile", "beam lint { run \"lint\" }\n"),
        ("libs/lib.txt", "v1"),
        ("elsewhere/file.txt", "v1"),
    ])
}

fn affected_in_app(dir: &tempfile::TempDir) -> Vec<BeamId> {
    let app = dir.path().join("app");
    let (project, sources) = load_project(&app.join("Beamfile")).unwrap();
    affected(&project, &sources, &app, "HEAD").unwrap()
}

#[test]
fn a_changed_input_outside_the_beamfile_directory_affects_its_beam() {
    let dir = nested_repository();
    write(dir.path().join("libs/lib.txt"), "v2");
    assert_eq!(names(&affected_in_app(&dir)), ["build"]);
}

#[test]
fn an_untracked_input_outside_the_beamfile_directory_affects_its_beam() {
    let dir = nested_repository();
    write(dir.path().join("libs/new.txt"), "v1");
    assert_eq!(names(&affected_in_app(&dir)), ["build"]);
}

#[test]
fn a_changed_import_outside_the_beamfile_directory_affects_its_beams() {
    let dir = nested_repository();
    let shared = dir.path().join("shared/Beamfile");
    let mut content = std::fs::read_to_string(&shared).unwrap();
    content.push_str("\n# v2\n");
    write(shared, &content);
    assert_eq!(names(&affected_in_app(&dir)), ["shared:lint"]);
}

#[test]
fn a_change_no_input_names_affects_nothing_even_outside_the_directory() {
    let dir = nested_repository();
    write(dir.path().join("elsewhere/file.txt"), "v2");
    assert!(affected_in_app(&dir).is_empty());
}

/// A reference git would read as one of its own options (`--output=x`
/// writes a file) is refused before git ever sees it.
#[test]
fn a_reference_shaped_like_an_option_is_refused() {
    let dir = repository();
    let (project, sources) = load_project(&dir.path().join("Beamfile")).unwrap();
    let err = affected(&project, &sources, dir.path(), "--output=leak").unwrap_err();
    assert!(err.to_string().contains("--output=leak"), "{err}");
    assert!(!dir.path().join("leak").exists());
}

/// What landed on the target after the branch was cut is not the branch's
/// change: only the branch's own commits count.
#[test]
fn commits_landing_on_the_target_after_the_fork_do_not_count() {
    let dir = repository();
    git(dir.path(), &["checkout", "-q", "-b", "feature"]);
    git(dir.path(), &["checkout", "-q", "main"]);
    write(dir.path().join("api/src/lib.rs"), "landed on main");
    git(dir.path(), &["commit", "-q", "-am", "api change on main"]);
    git(dir.path(), &["checkout", "-q", "feature"]);
    write(dir.path().join("docs/index.md"), "feature change");
    git(
        dir.path(),
        &["commit", "-q", "-am", "docs change on feature"],
    );

    let (project, sources) = load_project(&dir.path().join("Beamfile")).unwrap();
    let beams = affected(&project, &sources, dir.path(), "main").unwrap();
    let beams = names(&beams);
    assert!(beams.contains(&"docs"), "{beams:?}");
    assert!(!beams.contains(&"api:lint"), "{beams:?}");
}

/// Histories with nothing in common have no merge base: the error names
/// the reference rather than reporting an empty change set.
#[test]
fn no_merge_base_is_an_error_naming_the_reference() {
    let dir = repository();
    git(dir.path(), &["checkout", "-q", "--orphan", "other"]);
    write(dir.path().join("unrelated.txt"), "x");
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "unrelated root"]);

    let (project, sources) = load_project(&dir.path().join("Beamfile")).unwrap();
    let err = affected(&project, &sources, dir.path(), "main").unwrap_err();
    let message = err.to_string();
    assert!(message.contains("main"), "{message}");
    assert!(message.contains("merge base"), "{message}");
    assert!(!message.ends_with(": "), "{message}");
}

/// In a shallow clone the fork point may be out of reach: the error tells
/// how to get it back.
#[test]
fn a_shallow_clone_without_the_fork_point_suggests_fetching_more_history() {
    let source = repository();
    git(source.path(), &["checkout", "-q", "-b", "feature"]);
    write(source.path().join("docs/index.md"), "feature change");
    git(
        source.path(),
        &["commit", "-q", "-am", "docs change on feature"],
    );
    git(source.path(), &["checkout", "-q", "main"]);
    write(source.path().join("api/src/lib.rs"), "landed on main");
    git(
        source.path(),
        &["commit", "-q", "-am", "api change on main"],
    );

    let parent = tempfile::tempdir().unwrap();
    let url = format!("file://{}", source.path().display());
    let clone = parent.path().join("clone");
    git(
        parent.path(),
        &[
            "clone", "-q", "--depth", "1", "--branch", "feature", &url, "clone",
        ],
    );
    git(&clone, &["fetch", "-q", "--depth", "1", "origin", "main"]);

    let (project, sources) = load_project(&clone.join("Beamfile")).unwrap();
    let err = affected(&project, &sources, &clone, "origin/main").unwrap_err();
    let message = err.to_string();
    assert!(message.contains("origin/main"), "{message}");
    assert!(message.contains("git fetch --unshallow"), "{message}");
}
