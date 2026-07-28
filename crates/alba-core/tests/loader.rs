//! Integration tests for [`alba_core::load_project`]: multi-file loading,
//! import namespacing, and the errors this crate's loader owns (a missing
//! import, an import cycle, a duplicate alias). Uses real files under a
//! [`tempfile::tempdir`] rather than [`alba_core::load_str`], since
//! resolving `import` paths relative to the importing file's directory is
//! the behavior under test.

use std::path::PathBuf;

use alba_core::{SourceId, load_project};

/// Writes `content` to `path`, creating any missing parent directories
/// first, so a test fixture can lay out a small tree of Beamfiles (e.g.
/// `api/Beamfile`) with a single call per file.
fn write(path: PathBuf, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

#[test]
fn imports_namespace_beams_and_resolve_local_needs() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path().join("Beamfile"),
        "import \"api/Beamfile\" as api\nbeam all { needs [api:test] run \"echo ok\" }",
    );
    write(
        dir.path().join("api/Beamfile"),
        "beam build { run \"echo b\" }\nbeam test { needs [build] run \"echo t\" }",
    );
    let (project, _) = load_project(&dir.path().join("Beamfile")).unwrap();
    let ids: Vec<&str> = project.beams.iter().map(|b| b.id.0.as_str()).collect();
    assert!(ids.contains(&"api:build") && ids.contains(&"api:test") && ids.contains(&"all"));
    let api_test = project.beams.iter().find(|b| b.id.0 == "api:test").unwrap();
    assert_eq!(api_test.needs[0].value.0, "api:build"); // local need namespaced
}

#[test]
fn import_cycle_is_reported_with_chain() {
    // a/Beamfile imports ../b/Beamfile, b imports ../a/Beamfile.
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path().join("a/Beamfile"),
        "import \"../b/Beamfile\" as b\nbeam x { needs [b:y] run \"echo a\" }",
    );
    write(
        dir.path().join("b/Beamfile"),
        "import \"../a/Beamfile\" as a\nbeam y { needs [a:x] run \"echo b\" }",
    );

    let err = load_project(&dir.path().join("a/Beamfile")).unwrap_err();

    assert!(err.error.message.contains("import cycle"));
    // A structural check on the chain, not just a substring match: every
    // message here contains the letters "a" and "b" regardless of whether
    // a real chain was reported (e.g. "Beamfile" alone contains "a"), so
    // assert on the shape instead — three file names joined by two
    // arrows (a -> b -> a, closing the cycle).
    assert_eq!(err.error.message.matches("Beamfile").count(), 3);
    assert_eq!(err.error.message.matches(" -> ").count(), 2);
}

#[test]
fn imported_default_is_ignored() {
    // Root has no `default`; the imported child does. The child's
    // `default` must not leak into the project's default.
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path().join("Beamfile"),
        "import \"api/Beamfile\" as api\nbeam all { run \"echo ok\" }",
    );
    write(
        dir.path().join("api/Beamfile"),
        "default build\nbeam build { run \"echo b\" }",
    );

    let (project, _) = load_project(&dir.path().join("Beamfile")).unwrap();

    assert!(project.default.is_none());
}

#[test]
fn root_default_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path().join("Beamfile"),
        "default all\nbeam all { run \"echo ok\" }",
    );

    let (project, _) = load_project(&dir.path().join("Beamfile")).unwrap();

    assert_eq!(
        project.default.as_ref().map(|d| d.value.0.as_str()),
        Some("all")
    );
}

#[test]
fn nested_imports_join_namespaces_with_colon() {
    // `needs [...]` syntax only ever spells one namespace level
    // (`alba_syntax`'s `BeamRef` grammar), so the root can reference
    // `api:build` but not a two-segment id like `api:db:migrate` directly.
    // `api/Beamfile` referencing its own import's `db:migrate` (one level,
    // relative to itself) is what produces that two-segment id once the
    // root absorbs `api`'s beams under the `api:` prefix.
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path().join("Beamfile"),
        "import \"api/Beamfile\" as api\nbeam all { needs [api:build] run \"echo ok\" }",
    );
    write(
        dir.path().join("api/Beamfile"),
        "import \"db/Beamfile\" as db\nbeam build { needs [db:migrate] run \"echo b\" }",
    );
    write(
        dir.path().join("api/db/Beamfile"),
        "beam migrate { run \"echo m\" }",
    );

    let (project, _) = load_project(&dir.path().join("Beamfile")).unwrap();

    let ids: Vec<&str> = project.beams.iter().map(|b| b.id.0.as_str()).collect();
    assert!(ids.contains(&"api:db:migrate"));
    let build = project
        .beams
        .iter()
        .find(|b| b.id.0 == "api:build")
        .unwrap();
    assert_eq!(build.needs[0].value.0, "api:db:migrate");
}

#[test]
fn duplicate_import_alias_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path().join("Beamfile"),
        "import \"a/Beamfile\" as x\nimport \"b/Beamfile\" as x\nbeam all { run \"echo ok\" }",
    );
    write(dir.path().join("a/Beamfile"), "beam one { run \"echo 1\" }");
    write(dir.path().join("b/Beamfile"), "beam two { run \"echo 2\" }");

    let err = load_project(&dir.path().join("Beamfile")).unwrap_err();

    assert!(err.error.message.contains("duplicate import alias"));
}

#[test]
fn missing_import_reports_span_and_importing_source_id() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path().join("Beamfile"),
        "import \"missing/Beamfile\" as m\nbeam all { run \"echo ok\" }",
    );

    let err = load_project(&dir.path().join("Beamfile")).unwrap_err();

    // The root file is always assigned SourceId 0; the missing import is
    // reported against the *importing* file, i.e. the root.
    assert_eq!(err.error.source_id, SourceId(0));
    // The span points at the import's path text, not the start of the file.
    assert!(err.error.span.expect("the import path has a span").start > 0);
    // The root source is still available for rendering even though
    // loading failed on one of its imports.
    assert!(err.sources.get(err.error.source_id).is_some());
}

#[test]
fn beam_dir_is_its_defining_files_directory() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path().join("Beamfile"),
        "import \"api/Beamfile\" as api\nbeam all { run \"echo ok\" }",
    );
    write(
        dir.path().join("api/Beamfile"),
        "beam build { run \"echo b\" }",
    );

    let (project, _) = load_project(&dir.path().join("Beamfile")).unwrap();

    // `dir` is the *display* path (see `loader.rs`'s module doc comment):
    // exactly what was passed/joined, never run through `canonicalize`.
    // This matters beyond tidiness — on Windows, `canonicalize`'s verbatim
    // (`\\?\`) output can't be joined onto for further relative paths (a
    // beam's `cwd`), so `dir` must never be derived from it.
    let build = project
        .beams
        .iter()
        .find(|b| b.id.0 == "api:build")
        .unwrap();
    assert_eq!(build.dir, dir.path().join("api"));
    let all = project.beams.iter().find(|b| b.id.0 == "all").unwrap();
    assert_eq!(all.dir, dir.path());
}

#[test]
fn nested_relative_imports_avoid_verbatim_windows_paths() {
    // Regression guard for a bug where `dir` (the join base for further
    // imports, and a beam's `cwd` base) was derived from
    // `std::fs::canonicalize`'s output. On Windows that's a `\\?\`
    // verbatim path, and the standard library passes verbatim paths to
    // Win32 completely unnormalized, so joining a further relative import
    // (especially one using `..`) onto it silently fails to resolve. This
    // can't reproduce the Windows failure on this platform, but it does
    // pin down the two things that matter: importing through a
    // multi-segment relative path *and* a `..` still succeeds, and no
    // `Beam::dir` ever contains the verbatim prefix.
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path().join("Beamfile"),
        "import \"a/b/Beamfile\" as ab\nbeam all { run \"echo ok\" }",
    );
    write(
        dir.path().join("a/b/Beamfile"),
        "import \"../../back/Beamfile\" as back\nbeam here { run \"echo h\" }",
    );
    write(
        dir.path().join("back/Beamfile"),
        "beam there { run \"echo t\" }",
    );

    let (project, _) = load_project(&dir.path().join("Beamfile")).unwrap();

    let ids: Vec<&str> = project.beams.iter().map(|b| b.id.0.as_str()).collect();
    assert!(ids.contains(&"ab:here") && ids.contains(&"ab:back:there"));
    for beam in &project.beams {
        let dir_str = beam.dir.to_string_lossy();
        assert!(
            !dir_str.starts_with(r"\\?\"),
            "beam `{}`'s dir must not be a verbatim path, got {dir_str}",
            beam.id.0
        );
    }
}

#[test]
fn source_map_tracks_path_and_text_per_source_id() {
    let dir = tempfile::tempdir().unwrap();
    let root_src = "import \"api/Beamfile\" as api\nbeam all { run \"echo ok\" }";
    let api_src = "beam build { run \"echo b\" }";
    write(dir.path().join("Beamfile"), root_src);
    write(dir.path().join("api/Beamfile"), api_src);

    let (project, sources) = load_project(&dir.path().join("Beamfile")).unwrap();

    let build = project
        .beams
        .iter()
        .find(|b| b.id.0 == "api:build")
        .unwrap();
    let (_, text) = sources.get(build.source).unwrap();
    assert_eq!(text, api_src);

    let all = project.beams.iter().find(|b| b.id.0 == "all").unwrap();
    let (path, text) = sources.get(all.source).unwrap();
    assert_eq!(text, root_src);
    assert!(path.ends_with("Beamfile"));
}
