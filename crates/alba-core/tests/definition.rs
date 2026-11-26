//! [`alba_core::definitions`]: where each beam is written and the commands
//! it runs, computed from the sources rather than from the evaluated model.

use std::path::{Path, PathBuf};

use alba_core::{BeamId, definitions, load_project};

fn write(path: PathBuf, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

fn id(name: &str) -> BeamId {
    BeamId(name.into())
}

#[test]
fn a_root_beam_is_located_by_file_and_line() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path().join("Beamfile"),
        "beam build {\n  run \"cargo build\"\n}\n\nbeam test {\n  run [\"cargo build\", \"cargo test --workspace\"]\n}\n",
    );
    let (project, sources) = load_project(&dir.path().join("Beamfile")).unwrap();

    let defs = definitions(&project, &sources, dir.path());

    assert_eq!(defs[&id("build")].location, "Beamfile:1");
    assert_eq!(defs[&id("build")].commands, vec!["cargo build"]);
    assert_eq!(defs[&id("test")].location, "Beamfile:5");
    assert_eq!(
        defs[&id("test")].commands,
        vec!["cargo build", "cargo test --workspace"]
    );
}

#[test]
fn an_imported_beam_is_located_relative_to_the_root() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path().join("Beamfile"),
        "import \"services/payment/Beamfile\" as payment\nbeam all { needs [payment:integration] run \"echo ok\" }\n",
    );
    let mut payment = String::from("# payment beams\n");
    for _ in 0..10 {
        payment.push('\n');
    }
    payment.push_str("beam integration {\n  run \"cargo test --test integration\"\n}\n");
    write(dir.path().join("services/payment/Beamfile"), &payment);
    let (project, sources) = load_project(&dir.path().join("Beamfile")).unwrap();

    let defs = definitions(&project, &sources, dir.path());

    assert_eq!(
        defs[&id("payment:integration")].location,
        "services/payment/Beamfile:12"
    );
    assert_eq!(defs[&id("all")].location, "Beamfile:2");
}

#[test]
fn commands_are_kept_as_written_without_rendering_or_unescaping() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path().join("Beamfile"),
        "let profile = \"release\"\nbeam build {\n  run [\"cargo build --profile {profile}\", \"echo \\\"hi\\\"\"]\n}\n",
    );
    let (project, sources) = load_project(&dir.path().join("Beamfile")).unwrap();

    let defs = definitions(&project, &sources, dir.path());

    assert_eq!(
        defs[&id("build")].commands,
        vec!["cargo build --profile {profile}", "echo \\\"hi\\\""]
    );
}

#[test]
fn a_beam_without_run_has_no_commands() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path().join("Beamfile"),
        "beam a { run \"echo a\" }\nbeam all { needs [a] }\n",
    );
    let (project, sources) = load_project(&dir.path().join("Beamfile")).unwrap();

    let defs = definitions(&project, &sources, Path::new(dir.path()));

    assert!(defs[&id("all")].commands.is_empty());
    assert_eq!(defs[&id("all")].location, "Beamfile:2");
}

#[test]
fn a_beam_whose_source_is_not_in_the_map_has_no_definition() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path().join("Beamfile"), "beam a { run \"echo a\" }\n");
    let (project, _) = load_project(&dir.path().join("Beamfile")).unwrap();

    let defs = definitions(&project, &alba_core::SourceMap::default(), dir.path());

    assert!(defs.is_empty());
}

#[test]
fn a_source_outside_the_root_keeps_its_full_path() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    write(dir.path().join("Beamfile"), "beam a { run \"echo a\" }\n");
    let (project, sources) = load_project(&dir.path().join("Beamfile")).unwrap();

    let defs = definitions(&project, &sources, elsewhere.path());

    // Locations always use `/`, whatever the platform's separator.
    let expected = format!("{}:1", dir.path().join("Beamfile").display()).replace('\\', "/");
    assert_eq!(defs[&id("a")].location, expected);
}

#[test]
fn an_absolute_path_outside_the_root_has_a_single_leading_slash() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    write(dir.path().join("Beamfile"), "beam a { run \"echo a\" }\n");
    let (project, sources) = load_project(&dir.path().join("Beamfile")).unwrap();

    let defs = definitions(&project, &sources, elsewhere.path());

    let location = &defs[&id("a")].location;
    let path = location.strip_suffix(":1").unwrap();
    assert!(std::path::Path::new(path).is_absolute(), "{location}");
    assert!(!location.contains("//"), "{location}");
    assert!(location.ends_with("/Beamfile:1"), "{location}");
}
