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
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.email", "alba@example.com"]);
    git(dir.path(), &["config", "user.name", "Alba"]);
    git(dir.path(), &["config", "commit.gpgsign", "false"]);
    write(dir.path().join("Beamfile"), BEAMFILE);
    write(dir.path().join("api/Beamfile"), API_BEAMFILE);
    for file in [
        "schema/a.json",
        "src/lib.rs",
        "docs/index.md",
        "deploy/x",
        "api/src/lib.rs",
    ] {
        write(dir.path().join(file), "v1");
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

#[test]
fn select_a_plain_beam_is_the_old_behaviour() {
    let dir = repository();
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
