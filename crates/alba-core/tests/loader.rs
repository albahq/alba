//! Integration tests for [`alba_core::load_project`]: multi-file loading,
//! import namespacing, and the errors this crate's loader owns (a missing
//! import, an import cycle, a duplicate alias). Uses real files under a
//! [`tempfile::tempdir`] rather than [`alba_core::load_str`], since
//! resolving `import` paths relative to the importing file's directory is
//! the behavior under test.

use std::path::{Path, PathBuf};

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
    // A beam reached through two levels of aliasing gets a two-segment id
    // once the root absorbs `api`'s beams under the `api:` prefix, and the
    // root can name it in full.
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path().join("Beamfile"),
        "import \"api/Beamfile\" as api\n\
         beam all { needs [api:build, api:db:migrate] run \"echo ok\" }",
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

    // The root's own reference to that two-segment id resolves: graph
    // validation would have rejected the load otherwise.
    let all = project.beams.iter().find(|b| b.id.0 == "all").unwrap();
    let needs: Vec<&str> = all.needs.iter().map(|n| n.value.0.as_str()).collect();
    assert_eq!(needs, ["api:build", "api:db:migrate"]);
}

/// A file reached from two import sites is read, parsed, and evaluated
/// once, not once per site. Without that, cost is exponential in nesting
/// depth: nineteen three-line files that each import the next twice used
/// to take half a million loads.
///
/// The pin is the source map: every load registers one entry, so a second
/// registration for `shared/Beamfile` (a fourth id here) means the file
/// was loaded twice. The beams themselves must still be namespaced per
/// site — that is a purely textual step, applied to the memoized result.
#[test]
fn a_file_imported_from_two_sites_is_loaded_once() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path().join("Beamfile"),
        "import \"left/Beamfile\" as l\nimport \"right/Beamfile\" as r\n\
         beam all { needs [l:x, r:x] run \"echo ok\" }",
    );
    write(
        dir.path().join("left/Beamfile"),
        "import \"../shared/Beamfile\" as s\nbeam x { needs [s:base] run \"echo l\" }",
    );
    write(
        dir.path().join("right/Beamfile"),
        "import \"../shared/Beamfile\" as s\nbeam x { needs [s:base] run \"echo r\" }",
    );
    write(
        dir.path().join("shared/Beamfile"),
        "beam base { run \"echo s\" }",
    );

    let (project, sources) = load_project(&dir.path().join("Beamfile")).unwrap();

    let mut ids: Vec<&str> = project.beams.iter().map(|b| b.id.0.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["all", "l:s:base", "l:x", "r:s:base", "r:x"]);

    assert!(
        sources.get(SourceId(3)).is_some(),
        "four distinct files were read"
    );
    assert!(
        sources.get(SourceId(4)).is_none(),
        "`shared/Beamfile` must be read once, not once per import site"
    );
}

/// The memoized result must not leak one import site's namespacing into
/// another's: both copies of a shared beam keep their own prefix, and both
/// point back at the single file that declared them.
#[test]
fn both_copies_of_a_shared_import_keep_their_own_namespace() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path().join("Beamfile"),
        "import \"shared/Beamfile\" as a\nimport \"shared/Beamfile\" as b\n\
         beam all { needs [a:base, b:base] run \"echo ok\" }",
    );
    write(
        dir.path().join("shared/Beamfile"),
        "beam base { run \"echo s\" }",
    );

    let (project, _) = load_project(&dir.path().join("Beamfile")).unwrap();

    let shared: Vec<&alba_core::Beam> = project
        .beams
        .iter()
        .filter(|beam| beam.id.0.ends_with(":base"))
        .collect();
    let ids: Vec<&str> = shared.iter().map(|beam| beam.id.0.as_str()).collect();
    assert_eq!(ids, ["a:base", "b:base"]);
    assert_eq!(shared[0].source, shared[1].source);
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

fn git(dir: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A repository with one commit holding `src/lib.rs` and `README.md`.
fn repository() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.email", "alba@example.com"]);
    git(dir.path(), &["config", "user.name", "Alba"]);
    git(dir.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "fn a() {}").unwrap();
    std::fs::write(dir.path().join("README.md"), "# x").unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

#[test]
fn git_fields_evaluate_from_the_repository_holding_the_beamfile() {
    let dir = repository();
    write(
        dir.path().join("Beamfile"),
        "let tag = git.branch + \"-\" + git.short_sha\n\
         beam b { description \"{tag} {if git.dirty then 'dirty' else 'clean'}\" run \"echo {git.sha}\" }",
    );
    // The Beamfile itself is untracked, so the tree is dirty.
    let (project, _) = load_project(&dir.path().join("Beamfile")).unwrap();
    let description = project.beams[0].description.as_deref().unwrap();
    assert!(description.starts_with("main-"), "{description}");
    assert!(description.ends_with(" dirty"), "{description}");
    let sha =
        alba_core::render_template(&project.beams[0].run[0], &project.beams[0].scope).unwrap();
    assert_eq!(sha.len(), "echo ".len() + 40, "{sha}");
}

#[test]
fn git_is_only_spawned_when_an_expression_reads_it() {
    // Not a repository at all: loading must still succeed.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path().join("Beamfile"), "beam b { run \"echo plain\" }");
    load_project(&dir.path().join("Beamfile")).unwrap();
}

#[test]
fn reading_git_outside_a_repository_points_at_the_expression() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path().join("Beamfile"),
        "let b = git.branch\nbeam x { run \"echo\" }",
    );
    let err = load_project(&dir.path().join("Beamfile")).unwrap_err();
    assert!(
        err.error.message.contains("cannot read `git.branch`"),
        "{}",
        err.error.message
    );
    let source = "let b = git.branch";
    let span = err.error.span.unwrap();
    assert_eq!(&source[span.start..span.end], "git.branch");
}

#[test]
fn an_unknown_git_field_is_rejected_without_spawning_git() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path().join("Beamfile"),
        "let b = git.tag\nbeam x { run \"echo\" }",
    );
    let err = load_project(&dir.path().join("Beamfile")).unwrap_err();
    assert!(
        err.error.message.contains("unknown git field `tag`"),
        "{}",
        err.error.message
    );
    assert!(err.error.help.as_deref().unwrap().contains("short_sha"));
}

#[test]
fn only_git_has_fields() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path().join("Beamfile"),
        "let a = \"x\"\nlet b = a.len\nbeam x { run \"echo\" }",
    );
    let err = load_project(&dir.path().join("Beamfile")).unwrap_err();
    assert!(
        err.error.message.contains("`a` has no fields"),
        "{}",
        err.error.message
    );
}

#[test]
fn git_dirty_is_a_boolean_usable_in_a_condition() {
    let dir = repository();
    write(
        dir.path().join("Beamfile"),
        "let flag = git.dirty == true\nbeam x { run \"echo {flag}\" }",
    );
    let (project, _) = load_project(&dir.path().join("Beamfile")).unwrap();
    let rendered =
        alba_core::render_template(&project.beams[0].run[0], &project.beams[0].scope).unwrap();
    assert_eq!(rendered, "echo true");
}
