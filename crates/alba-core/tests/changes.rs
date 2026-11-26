//! `git.changes(glob)` and `.dirs(glob)`: validated through
//! `load_project`, rendered through `render_template` against a scope
//! carrying a [`ChangeSet`], as the engine renders a beam's `run`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use alba_core::{
    Beam, ChangeMode, ChangeSet, CoreError, FullReason, Project, Quoting, load_project,
    render_template,
};
use alba_syntax::Span;

/// A project directory holding `files` (empty content) and `beamfile` at
/// its root, kept alive for as long as the test needs the files.
struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new(beamfile: &str, files: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        for file in files {
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        std::fs::write(dir.path().join("Beamfile"), beamfile).unwrap();
        Self { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn load(&self) -> Result<Project, CoreError> {
        load_project(&self.root().join("Beamfile"))
            .map(|(project, _)| project)
            .map_err(|err| err.error)
    }

    fn project(&self) -> Project {
        self.load().unwrap_or_else(|err| panic!("{}", err.message))
    }

    /// The absolute paths of `names`, relative to the project root.
    fn paths(&self, names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(|name| self.root().join(name)).collect()
    }
}

fn beam<'a>(project: &'a Project, id: &str) -> &'a Beam {
    project
        .beams
        .iter()
        .find(|beam| beam.id.0 == id)
        .expect("the beam exists")
}

/// The directory a beam's commands run in, as the engine computes it.
fn cwd_of(beam: &Beam) -> PathBuf {
    beam.run_dir()
}

fn incremental(changed: Vec<PathBuf>) -> ChangeSet {
    ChangeSet {
        mode: ChangeMode::Incremental,
        changed: Arc::new(changed),
    }
}

fn full() -> ChangeSet {
    ChangeSet {
        mode: ChangeMode::Full(FullReason::OnTargetBranch),
        changed: Arc::new(Vec::new()),
    }
}

/// Beam `b`'s first command, rendered against `changes` with `quoting`.
fn render_with(project: &Project, changes: ChangeSet, quoting: Quoting) -> String {
    let beam = beam(project, "b");
    let scope = beam
        .scope
        .with_changes(changes, &beam.dir, &cwd_of(beam))
        .with_quoting(quoting);
    render_template(&beam.run[0], &scope).unwrap_or_else(|err| panic!("{}", err.message))
}

fn render(project: &Project, changes: ChangeSet) -> String {
    render_with(project, changes, Quoting::Posix)
}

fn load_error(beamfile: &str) -> CoreError {
    match Fixture::new(beamfile, &[]).load() {
        Ok(_) => panic!("expected a load error"),
        Err(err) => err,
    }
}

fn span_of(source: &str, needle: &str) -> Option<Span> {
    let start = source.find(needle).expect("needle is in the source");
    Some(Span::new(start, start + needle.len()))
}

// Load time.

#[test]
fn git_changes_outside_run_and_env_is_rejected() {
    for beamfile in [
        "beam b { inputs ['src/**'] description \"{git.changes('src/**')}\" run 'x' }",
        "beam b { inputs ['src/**', \"{git.changes('src/**')}\"] run 'x' }",
        "let files = git.changes('src/**')\nbeam b { inputs ['src/**'] run 'x' }",
        "beam b { inputs ['src/**'] cwd \"{git.changes('src/**')}\" run 'x' }",
        "beam b { inputs ['src/**'] executor docker { image \"{git.changes('src/**')}\" } \
         run 'x' }",
    ] {
        let err = load_error(beamfile);
        assert!(
            err.message.contains("belongs to the moment the beam runs"),
            "{beamfile}: {}",
            err.message
        );
        assert_eq!(err.span, span_of(beamfile, "git.changes('src/**')"));
        assert!(err.help.unwrap().contains("`run`"));
    }
}

#[test]
fn a_glob_absent_from_inputs_is_rejected_listing_the_inputs() {
    let beamfile =
        "beam b { inputs ['src/**/*.php', 'phpcs.xml'] run \"phpcs {git.changes('lib/**')}\" }";
    let err = load_error(beamfile);
    assert_eq!(
        err.message,
        "`git.changes` glob `lib/**` is not one of beam `b`'s inputs"
    );
    assert_eq!(err.span, span_of(beamfile, "'lib/**'"));
    let help = err.help.unwrap();
    assert!(help.contains("`src/**/*.php`, `phpcs.xml`"), "{help}");

    let err = load_error("beam b { run \"phpcs {git.changes('lib/**')}\" }");
    assert!(
        err.help.unwrap().contains("declares no `inputs`"),
        "a beam without inputs says so"
    );
}

#[test]
fn a_glob_rendered_from_a_let_is_compared_to_the_inputs() {
    let fixture = Fixture::new(
        "let php = 'src/**/*.php'\n\
         beam b { inputs [\"{php}\"] run \"phpcs {git.changes(php)}\" }",
        &["src/A.php"],
    );
    let project = fixture.project();
    assert_eq!(
        render(&project, incremental(fixture.paths(&["src/A.php"]))),
        "phpcs src/A.php"
    );
}

#[test]
fn git_changes_takes_exactly_one_string() {
    let err = load_error("beam b { inputs ['a'] run \"x {git.changes()}\" }");
    assert_eq!(err.message, "`git.changes` expects 1 argument(s), found 0");

    let err = load_error("beam b { inputs ['a'] run \"x {git.changes('a', 'a')}\" }");
    assert_eq!(err.message, "`git.changes` expects 1 argument(s), found 2");

    let beamfile = "beam b { inputs ['a'] run \"x {git.changes(true)}\" }";
    let err = load_error(beamfile);
    assert_eq!(
        err.message,
        "argument to `git.changes` must be a string, found a boolean"
    );

    let beamfile = "beam b(p) { inputs ['a'] run \"x {git.changes(p)}\" }";
    let err = load_error(beamfile);
    assert_eq!(
        err.message,
        "the glob of `git.changes` must be known when the Beamfile loads"
    );
    let start = beamfile.find("changes(p").unwrap() + "changes(".len();
    assert_eq!(err.span, Some(Span::new(start, start + 1)));
    assert!(err.help.unwrap().contains("a literal or a `let`"));
}

#[test]
fn an_unknown_method_names_the_value_type_and_its_methods() {
    let beamfile = "beam b { inputs ['a'] run \"x {git.changes('a').sort('a')}\" }";
    let err = load_error(beamfile);
    assert_eq!(err.message, "a list has no method `sort`");
    assert_eq!(err.span, span_of(beamfile, "sort"));
    assert_eq!(err.help.as_deref(), Some("a list has one method: `dirs`"));

    let beamfile = "beam b { inputs ['a'] run \"x {git.change('a')}\" }";
    let err = load_error(beamfile);
    assert_eq!(err.message, "`git` has no method `change`");
    assert_eq!(err.span, span_of(beamfile, "change"));
    assert_eq!(err.help.as_deref(), Some("did you mean `changes`?"));

    let err = load_error("beam b { run \"x {'a'.dirs('a')}\" }");
    assert_eq!(err.message, "a string has no method `dirs`");
    assert!(err.help.unwrap().contains("only a list has methods"));

    let err = load_error("beam b { run \"x {git.branch.dirs('a')}\" }");
    assert_eq!(err.message, "a string or a boolean has no method `dirs`");
}

#[test]
fn git_changes_without_parentheses_is_pointed_at_the_call() {
    let err = load_error("beam b { inputs ['a'] run \"x {git.changes}\" }");
    assert_eq!(err.message, "unknown git field `changes`");
    assert!(err.help.unwrap().contains("`git.changes(glob)`"));
}

#[test]
fn dirs_takes_exactly_one_known_glob() {
    let err = load_error("beam b { inputs ['a'] run \"x {git.changes('a').dirs()}\" }");
    assert_eq!(err.message, "`dirs` expects 1 argument(s), found 0");

    let beamfile = "beam b { inputs ['a'] run \"x {git.changes('a').dirs('[')}\" }";
    let err = load_error(beamfile);
    assert!(
        err.message.starts_with("invalid `dirs` glob `[`"),
        "{}",
        err.message
    );
    assert_eq!(err.span, span_of(beamfile, "'['"));

    let err = load_error("beam b(p) { inputs ['a'] run \"x {git.changes('a').dirs(p)}\" }");
    assert_eq!(
        err.message,
        "the glob of `dirs` must be known when the Beamfile loads"
    );
}

#[test]
fn a_list_where_a_string_is_required_is_a_type_error() {
    for (beamfile, call, message) in [
        (
            "beam b { inputs ['x'] run \"{git.changes('x') + 'y'}\" }",
            "git.changes('x')",
            "expected a string, found a list",
        ),
        (
            "beam b { inputs ['x'] run \"{'y' == git.changes('x')}\" }",
            "git.changes('x')",
            "expected a string, found a list",
        ),
        (
            "beam b { inputs ['x'] run \"{if git.changes('x') then 'a' else 'b'}\" }",
            "git.changes('x')",
            "expected a boolean condition for `if`, found a list",
        ),
        (
            "beam b { inputs ['x'] run \"{match git.changes('x') { 'a' => 'a' _ => 'b' }}\" }",
            "git.changes('x')",
            "expected a string, found a list",
        ),
        (
            "let g = 'x'\nbeam b { inputs ['x'] run \"{'files: {git.changes(g)}'}\" }",
            "git.changes(g)",
            "expected a string, found a list",
        ),
        (
            "beam b { inputs ['x'] run \"{env('FILES', git.changes('x'))}\" }",
            "git.changes('x')",
            "expected a string, found a list",
        ),
        (
            "beam b { inputs ['x'] run \"{git.changes('x') && true}\" }",
            "git.changes('x')",
            "expected a boolean, found a list",
        ),
    ] {
        let err = load_error(beamfile);
        assert_eq!(err.message, message, "{beamfile}");
        assert_eq!(err.span, span_of(beamfile, call), "{beamfile}");
    }

    let err = load_error("beam b { inputs ['x'] run \"{git.changes('x') + 'y'}\" }");
    assert!(err.help.unwrap().contains("`run` or `env`"));
}

#[test]
fn method_calls_are_validated_in_branches_not_taken() {
    let beamfile = "beam b { inputs ['src/**'] \
         run \"x {if 'a' == 'a' then 'y' else git.changes('lib/**')}\" }";
    let err = load_error(beamfile);
    assert_eq!(
        err.message,
        "`git.changes` glob `lib/**` is not one of beam `b`'s inputs"
    );

    let beamfile = "let k = 'main'\nbeam b { inputs ['src/**'] \
         run \"x {match k { 'main' => 'y' _ => git.changes('src/**').sort('z') }}\" }";
    let err = load_error(beamfile);
    assert_eq!(err.message, "a list has no method `sort`");
}

#[test]
fn change_globs_lists_every_glob_once() {
    let fixture = Fixture::new(
        "beam b(p) {\n\
           inputs ['src/**/*.php', 'tests/**/*.php', 'phpcs.xml']\n\
           run [\"a {git.changes('tests/**/*.php')}\", \
                \"b {if p == 'x' then git.changes('src/**/*.php') else ''}\"]\n\
           env { FILES = \"{git.changes('tests/**/*.php')}\" }\n\
         }\n\
         beam plain { inputs ['src/**'] run 'x' }",
        &[],
    );
    let project = fixture.project();
    assert_eq!(
        beam(&project, "b").change_globs,
        ["tests/**/*.php", "src/**/*.php"]
    );
    assert!(beam(&project, "plain").change_globs.is_empty());
}

// Incremental mode.

const PHPCS: &str =
    "beam b { inputs ['src/**/*.php', 'phpcs.xml'] run \"phpcs {git.changes('src/**/*.php')}\" }";

#[test]
fn only_existing_matching_files_are_listed() {
    let fixture = Fixture::new(PHPCS, &["src/B.php", "src/A.php", "src/readme.md"]);
    let project = fixture.project();
    let changed = fixture.paths(&[
        "src/B.php",
        "src/Deleted.php",
        "src/readme.md",
        "phpcs.xml",
        "src/A.php",
        "src/B.php",
    ]);
    assert_eq!(
        render(&project, incremental(changed)),
        "phpcs src/A.php src/B.php"
    );
}

#[test]
fn a_rename_keeps_the_new_path() {
    let fixture = Fixture::new(PHPCS, &["src/New.php"]);
    let project = fixture.project();
    let changed = fixture.paths(&["src/Old.php", "src/New.php"]);
    assert_eq!(render(&project, incremental(changed)), "phpcs src/New.php");
}

#[test]
fn no_changed_file_renders_as_nothing() {
    let fixture = Fixture::new(PHPCS, &[]);
    let project = fixture.project();
    assert_eq!(render(&project, incremental(Vec::new())), "phpcs ");
}

#[test]
fn a_changed_path_outside_the_project_is_ignored() {
    let fixture = Fixture::new(PHPCS, &[]);
    let project = fixture.project();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::create_dir(elsewhere.path().join("src")).unwrap();
    std::fs::write(elsewhere.path().join("src/A.php"), "").unwrap();
    let changed = vec![elsewhere.path().join("src/A.php")];
    assert_eq!(render(&project, incremental(changed)), "phpcs ");
}

// Full mode.

#[test]
fn full_mode_lists_the_glob_base_directory() {
    let fixture = Fixture::new(PHPCS, &["src/A.php"]);
    let project = fixture.project();
    assert_eq!(render(&project, full()), "phpcs src");
}

#[test]
fn a_glob_without_literal_prefix_gives_the_current_directory() {
    let fixture = Fixture::new(
        "beam b { inputs ['**/*.php'] run \"phpcs {git.changes('**/*.php')}\" }",
        &[],
    );
    let project = fixture.project();
    assert_eq!(render(&project, full()), "phpcs .");
}

#[test]
fn no_change_set_means_full_mode() {
    let fixture = Fixture::new(
        "beam b { inputs ['src/**/*.php'] cwd 'src' run \"phpcs {git.changes('src/**/*.php')}\" }",
        &["src/A.php"],
    );
    let project = fixture.project();
    let beam = beam(&project, "b");
    assert_eq!(
        render_template(&beam.run[0], &beam.scope).unwrap(),
        "phpcs ."
    );
    assert_eq!(
        render_template(&beam.run[0], &beam.scope.with_params(&[], &[])).unwrap(),
        "phpcs ."
    );
}

// `.dirs()`.

const PHPSTAN: &str = "beam b { inputs ['src/**/*.php'] \
     run \"phpstan analyse {git.changes('src/**/*.php').dirs('src/*')}\" }";

#[test]
fn dirs_reduces_to_the_first_matching_prefix() {
    let fixture = Fixture::new(PHPSTAN, &["src/Billing/Invoice.php"]);
    let project = fixture.project();
    let changed = fixture.paths(&["src/Billing/Invoice.php"]);
    assert_eq!(
        render(&project, incremental(changed)),
        "phpstan analyse src/Billing"
    );
}

#[test]
fn dirs_at_two_levels() {
    let fixture = Fixture::new(
        "beam b { inputs ['src/**/*.php'] \
         run \"phpstan analyse {git.changes('src/**/*.php').dirs('src/*/*')}\" }",
        &[
            "src/Billing/Invoice/Line.php",
            "src/Billing/Invoice/Total.php",
            "src/Billing/Tax/Rate.php",
            "src/Kernel.php",
        ],
    );
    let project = fixture.project();
    let changed = fixture.paths(&[
        "src/Billing/Invoice/Line.php",
        "src/Billing/Invoice/Total.php",
        "src/Billing/Tax/Rate.php",
        "src/Kernel.php",
    ]);
    assert_eq!(
        render(&project, incremental(changed)),
        "phpstan analyse src/Billing/Invoice src/Billing/Tax src/Kernel.php"
    );
}

#[test]
fn dirs_keeps_shorter_paths_and_collapses_nested_ones() {
    // Full mode: `src` is shorter than `src/*` and stays `src`.
    let fixture = Fixture::new(PHPSTAN, &[]);
    assert_eq!(render(&fixture.project(), full()), "phpstan analyse src");

    // A changed directory (a submodule, or a file replaced by a directory)
    // lists `src` beside `src/Billing/Invoice.php`, which `src/*` reduces
    // to `src/Billing`: nested under the kept `src`, it is dropped.
    let fixture = Fixture::new(
        "beam b { inputs ['**'] run \"x {git.changes('**').dirs('src/*')}\" }",
        &["src/Billing/Invoice.php"],
    );
    let project = fixture.project();
    let changed = fixture.paths(&["src", "src/Billing/Invoice.php"]);
    assert_eq!(render(&project, incremental(changed)), "x src");

    // Chained: `src/Billing` (from the first call) reduces to `src`.
    let fixture = Fixture::new(
        "beam b { inputs ['src/**/*.php'] \
         run \"x {git.changes('src/**/*.php').dirs('src/*').dirs('src')}\" }",
        &["src/Billing/Invoice.php", "src/Kernel.php"],
    );
    let project = fixture.project();
    let changed = fixture.paths(&["src/Billing/Invoice.php", "src/Kernel.php"]);
    assert_eq!(render(&project, incremental(changed)), "x src");
}

#[test]
fn dirs_removes_duplicates() {
    let fixture = Fixture::new(
        "beam b { inputs ['**/*.php'] run \"x {git.changes('**/*.php').dirs('lib')}\" }",
        &["lib/A.php", "lib/B.php", "src/Billing/C.php"],
    );
    let project = fixture.project();
    let changed = fixture.paths(&["src/Billing/C.php", "lib/B.php", "lib/A.php"]);
    assert_eq!(
        render(&project, incremental(changed)),
        "x lib src/Billing/C.php"
    );
}

#[test]
fn dirs_without_a_matching_prefix_keeps_the_path() {
    let fixture = Fixture::new(
        "beam b { inputs ['**/*.php'] run \"x {git.changes('**/*.php').dirs('src/*')}\" }",
        &["bin/console.php", "src/A/B.php"],
    );
    let project = fixture.project();
    let changed = fixture.paths(&["bin/console.php", "src/A/B.php"]);
    assert_eq!(
        render(&project, incremental(changed)),
        "x bin/console.php src/A"
    );
}

// Quoting and paths.

#[test]
fn posix_quoting_keeps_awkward_names_whole() {
    let fixture = Fixture::new(
        "beam b { inputs ['src/*.php'] run \"phpcs {git.changes('src/*.php')}\" }",
        &[
            "src/a b.php",
            "src/it's.php",
            "src/café.php",
            "src/Plain_1-x.php",
        ],
    );
    let project = fixture.project();
    let changed = fixture.paths(&[
        "src/a b.php",
        "src/it's.php",
        "src/café.php",
        "src/Plain_1-x.php",
    ]);
    assert_eq!(
        render(&project, incremental(changed)),
        "phpcs src/Plain_1-x.php 'src/a b.php' src/café.php 'src/it'\\''s.php'"
    );
}

#[test]
fn posix_quoting_quotes_shell_syntax() {
    let fixture = Fixture::new(
        "beam b { inputs ['src/*', '*.php'] \
         run \"x {git.changes('src/*')} {git.changes('*.php')}\" }",
        &[
            "src/$HOME",
            "src/a;b",
            "src/~x",
            "src/a\u{a0}b",
            "-x.php",
            "--autoload-file=evil.php",
        ],
    );
    let project = fixture.project();
    let changed = fixture.paths(&[
        "src/$HOME",
        "src/a;b",
        "src/~x",
        "src/a\u{a0}b",
        "-x.php",
        "--autoload-file=evil.php",
    ]);
    // A name starting with `-` would be read as an option: `./` keeps it
    // a path.
    assert_eq!(
        render(&project, incremental(changed)),
        "x 'src/$HOME' 'src/a;b' 'src/a\u{a0}b' 'src/~x' \
         ./--autoload-file=evil.php ./-x.php"
    );
}

#[test]
fn powershell_quoting_doubles_single_quotes() {
    let fixture = Fixture::new(
        "beam b { inputs ['src/**/*.php', '-*.php'] \
         run \"phpcs {git.changes('src/**/*.php')} {git.changes('-*.php')}\" }",
        &[
            "src/it's.php",
            "src/Billing/Invoice.php",
            "src/a,b.php",
            "src/it\u{2019}s.php",
            "-x.php",
            "--%.php",
        ],
    );
    let project = fixture.project();
    let changed = fixture.paths(&[
        "src/it's.php",
        "src/Billing/Invoice.php",
        "src/a,b.php",
        "src/it\u{2019}s.php",
        "-x.php",
        "--%.php",
    ]);
    assert_eq!(
        render_with(&project, incremental(changed), Quoting::PowerShell),
        "phpcs src\\Billing\\Invoice.php 'src\\a,b.php' 'src\\it''s.php' \
         'src\\it\u{2019}\u{2019}s.php' .\\--%.php .\\-x.php"
    );
}

#[test]
fn paths_are_relative_to_the_beam_cwd() {
    let fixture = Fixture::new(
        "beam b { inputs ['src/**/*.php'] cwd 'src' run \"phpcs {git.changes('src/**/*.php')}\" }",
        &["src/A.php", "src/Billing/B.php"],
    );
    let project = fixture.project();
    let changed = fixture.paths(&["src/A.php", "src/Billing/B.php"]);
    assert_eq!(
        render(&project, incremental(changed)),
        "phpcs A.php Billing/B.php"
    );

    let fixture = Fixture::new(
        "beam b { inputs ['src/**/*.php'] cwd 'tools' run \"phpcs {git.changes('src/**/*.php')}\" }",
        &["src/A.php", "tools/.keep"],
    );
    let project = fixture.project();
    let changed = fixture.paths(&["src/A.php"]);
    assert_eq!(render(&project, incremental(changed)), "phpcs ../src/A.php");
    assert_eq!(render(&project, full()), "phpcs ../src");
}

#[test]
fn a_glob_climbing_out_of_the_beamfile_directory_matches() {
    let root = tempfile::tempdir().unwrap();
    for file in ["libs/Money/Amount.php", "libs/readme.md", "app/src/A.php"] {
        let path = root.path().join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }
    std::fs::write(
        root.path().join("app/Beamfile"),
        "beam b { inputs ['../libs/**/*.php', 'src/**/*.php'] \
         run \"phpcs {git.changes('../libs/**/*.php')}\" }",
    )
    .unwrap();
    let (project, _) = load_project(&root.path().join("app/Beamfile")).unwrap();
    let changed = ["libs/Money/Amount.php", "libs/readme.md", "app/src/A.php"]
        .iter()
        .map(|name| root.path().join(name))
        .collect();
    assert_eq!(
        render(&project, incremental(changed)),
        "phpcs ../libs/Money/Amount.php"
    );
    assert_eq!(render(&project, full()), "phpcs ../libs");
}

#[test]
fn git_changes_renders_in_env_values() {
    let fixture = Fixture::new(
        "beam b { inputs ['src/*.php'] env { FILES = \"{git.changes('src/*.php')}\" } run 'x' }",
        &["src/A.php"],
    );
    let project = fixture.project();
    let beam = beam(&project, "b");
    let scope = beam.scope.with_changes(
        incremental(fixture.paths(&["src/A.php"])),
        &beam.dir,
        &cwd_of(beam),
    );
    assert_eq!(
        render_template(&beam.env[0].1, &scope).unwrap(),
        "src/A.php"
    );
}

/// A command is text: a changed path whose name is not valid UTF-8 fails
/// the render, naming it, rather than reaching the tool mangled.
#[cfg(unix)]
#[test]
fn a_name_that_is_not_utf8_fails_the_render() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let fixture = Fixture::new(PHPCS, &["src/A.php"]);
    let name = OsStr::from_bytes(b"caf\xe9.php");
    let path = fixture.root().join("src").join(name);
    if std::fs::write(&path, "").is_err() {
        // A filesystem that only stores UTF-8 names (APFS refuses this
        // one) cannot hold such a file; nothing to check there.
        return;
    }
    let project = fixture.project();
    let beam = beam(&project, "b");
    let mut changed = fixture.paths(&["src/A.php"]);
    changed.push(path);
    let scope = beam
        .scope
        .with_changes(incremental(changed), &beam.dir, &cwd_of(beam));

    let err = render_template(&beam.run[0], &scope).unwrap_err();

    assert!(
        err.message.contains("is not valid UTF-8"),
        "{}",
        err.message
    );
    assert!(err.message.contains("caf"), "{}", err.message);
    assert_eq!(err.span, span_of(PHPCS, "git.changes('src/**/*.php')"));
}

/// The same refusal through a real repository: git lists the file by its
/// raw bytes, and the list keeps them, so the render names the file
/// instead of silently dropping a name decoded into one that does not
/// exist. Linux only: its filesystems store any bytes as a name, where
/// APFS and NTFS refuse this one.
#[cfg(target_os = "linux")]
#[test]
fn a_name_git_lists_that_is_not_utf8_fails_the_render() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let fixture = Fixture::new(PHPCS, &["src/A.php"]);
    let root = fixture.root();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "alba@example.com"]);
    git(&["config", "user.name", "Alba"]);
    git(&["config", "commit.gpgsign", "false"]);
    git(&["config", "core.excludesFile", ".git/no-global-excludes"]);
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "init"]);
    // Untracked: `ls-files` lists it, byte for byte under `-z`.
    let name = OsStr::from_bytes(b"caf\xe9.php");
    std::fs::write(root.join("src").join(name), "").unwrap();

    let changed = alba_core::git::changed_files(root, "HEAD").unwrap();
    let project = fixture.project();
    let beam = beam(&project, "b");
    let scope = beam
        .scope
        .with_changes(incremental(changed), &beam.dir, &cwd_of(beam));

    let err = render_template(&beam.run[0], &scope).unwrap_err();

    assert!(
        err.message.contains("is not valid UTF-8"),
        "{}",
        err.message
    );
    assert!(err.message.contains("caf"), "{}", err.message);
    assert_eq!(err.span, span_of(PHPCS, "git.changes('src/**/*.php')"));
}
