# Incremental Beams on Git Changes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a beam run only on the files its branch changed since the merge base with a reference declared in
the Beamfile, through `git.changes('glob')`, `.dirs('glob')`, a `match` expression and a `git { base }` block.

**Architecture:** Syntax first (`alba-syntax`: `match`, method calls, the `git` block, parser-driven
interpolation), then evaluation (`alba-core`: `Value::List`, `match`, `git.changes()` and `.dirs()` rendered against
a per-beam `ChangeSet` carried by `Scope`), then the engine (`alba-engine`: one change set per run, one
`ChangeMode` per beam, the `unchanged` status), then the CLI (`--affected` with an optional reference,
`--no-affected`). Selection is unchanged; only the value of `git.changes()` becomes incremental.

**Tech Stack:** Rust workspace, `globset` (already a dependency of `alba-core` and `alba-engine`), `clap`, `serde`,
the `git` CLI, `assert_cmd` and `tempfile` in tests, `insta` snapshots in `alba-syntax` and `alba-tui`.

**Spec:** [`.claude/superpowers/specs/2026-11-26-git-changes-design.md`](../specs/2026-11-26-git-changes-design.md)

## Global Constraints

- No new crate dependency.
- Git 2.30 is not required: the merge base is computed with `git merge-base`, never `git diff --merge-base`.
- A reference starting with `-` is refused before any git command runs, for `merge-base` as for `diff`.
- Every load error carries a span and, where one exists, a `help:` suggestion, in the existing style.
- No string valid today changes meaning; Beamfiles without a `git` block behave exactly as before, except that
  `--affected` diffs from the merge base.
- `match`, method calls, `git.changes()` and `.dirs()` are validated on every branch at load time, like `if`.
- `git.changes()` is allowed only in `run` and `env`; its glob must equal one of the beam's rendered `inputs`.
- Full mode never silently degrades to incremental: any doubt (no reference, unresolved reference, target branch on
  a clean tree, Beamfile changed, a dependency affected, an input outside the `git.changes()` globs) is full mode.
- The cache fingerprint recipe is not changed: rendered commands already carry the file list. If a task does
  change it, it bumps `FORMAT_VERSION` in `crates/alba-engine/src/cache/store.rs`.
- Files, code, comments and commits in English; commits in gitmoji + Conventional Commits, no plan or task number.
- Feedback loop: while iterating run only the touched test files plus `cargo clippy -p <crate> --all-targets -- -D
  warnings`; run the full gate `alba run check` (format, clippy, build, test) once at the end of each task, in the
  background or with a timeout covering it, never polled with `sleep` loops.
- Tests go through public entry points only: `alba_syntax::parse`, `alba_core::load_str` / `load_project` /
  `render_template`, `alba_engine::run` / `affected` / `select`, and the `alba` binary. Git is real (temporary
  repositories), never doubled.

## Review Focus

1. A changed file whose name holds a space, a quote or a non-ASCII character must reach the tool as one argument.
   Test owned by Task 4 (`posix_quoting_keeps_awkward_names_whole`) and Task 6 (`posix_quoting_end_to_end`).
2. A beam with a `cwd` different from its Beamfile directory must receive paths relative to that `cwd`. Test owned
   by Task 4 (`paths_are_relative_to_the_beam_cwd`).
3. A shallow clone in CI (no merge base reachable) with a declared `base` must warn and run full, not fail. Test
   owned by Task 5 (`unresolved_base_runs_full_with_a_warning`) and Task 6 (`an_unresolved_base_warns_and_runs_full`).
4. An `inputs` glob that climbs out of the Beamfile directory (`../libs/**/*.php`) must still match changed files
   in `git.changes()`. Test owned by Task 4 (`a_glob_climbing_out_of_the_beamfile_directory_matches`).
5. A detached `HEAD` (CI checkouts) makes `git.branch` equal `HEAD`, which must fall through to `_` in a `match`.
   Test owned by Task 3 (`a_detached_head_falls_through_to_the_default_arm`).

---

### Task 1: Diff from the merge base

**Files:**
- Modify: `crates/alba-core/src/git.rs` (`changed_files`)
- Modify: `README.md` (section "Affected runs")
- Test: `crates/alba-engine/tests/affected.rs`, `crates/alba/tests/cli_affected.rs`

**Interfaces:**
- Produces: `pub fn merge_base(cwd: &Path, reference: &str) -> Result<String, GitError>` in
  `alba_core::git`, returning the full SHA; `changed_files(cwd, reference)` keeps its signature and diffs from
  `merge_base(cwd, reference)`.

- [ ] **Step 1: Write the failing tests** in `crates/alba-engine/tests/affected.rs`, reusing `repository_with`
  and `git`:
  - `commits_landing_on_the_target_after_the_fork_do_not_count`: on `main`, create branch `feature`, commit a
    change to `api/src/lib.rs` on `main`, switch to `feature`, commit a change to `web/src/app.ts`;
    `affected(&project, &sources, dir, "main")` contains the web beam and not the api beam.
  - `no_merge_base_is_an_error_naming_the_reference`: an orphan branch (`git checkout --orphan other`, commit)
    and `affected(..., "main")` from it returns an error whose message contains `main` and `merge base`.
- [ ] **Step 2: Run** `cargo test -p alba-engine --test affected`. Expected: both FAIL (the api beam is
  reported; the orphan case reports nothing).
- [ ] **Step 3: Implement `merge_base`** with `git merge-base <reference> HEAD`, refusing a leading `-` first.
  On failure, the `GitError` message reads ``no merge base between `<reference>` and HEAD: <git stderr>`` and, when
  `git rev-parse --is-shallow-repository` prints `true`, appends `; the clone is shallow, fetch more history
  (git fetch --unshallow)`. `changed_files` passes the SHA to `git diff --name-only -z <sha> --`.
- [ ] **Step 4: Run** the affected tests of both crates (`cargo test -p alba-engine --test affected`, `cargo test
  -p alba --test cli_affected`). Expected: PASS, older tests included.
- [ ] **Step 5: Update the README** "Affected runs" section: the paths are `git diff --name-only` from the merge
  base of `<ref>` and `HEAD`, and say why (commits that landed on the target after the branch was cut no longer
  count).
- [ ] **Step 6: Gate and commit**

```bash
git add crates/alba-core/src/git.rs crates/alba-engine/tests/affected.rs crates/alba/tests/cli_affected.rs README.md
git commit -m "🐛 fix(affected): diff from the merge base with the reference" -m "Commits that landed on the reference after the branch was cut were counted as changes of the branch. This changes which beams --affected selects." -m "Refs: #49"
```

---

### Task 2: `match` expression and parser-driven interpolation

**Files:**
- Modify: `crates/alba-syntax/src/token.rs`, `lexer.rs` (tokens), `expr.rs` (AST and parser), `template.rs`
  (`scan_interpolation`)
- Modify: `crates/alba-core/src/eval.rs` (every `match` on `Expr`: `expr_span`, `eval_with_fallback`,
  `check_expr`, `reject_param_references`, and any walker the compiler flags)
- Modify: `README.md` (new "Choosing a value" subsection under "A Beamfile", after "Interpolation")
- Test: `crates/alba-syntax/tests/parse.rs`, new `crates/alba-core/tests/match_expression.rs`

**Interfaces:**
- Produces in `alba_syntax`:
  - `TokenKind::KwMatch`, `TokenKind::FatArrow` (`=>`), `TokenKind::Pipe` (a lone `|`, today a lex error).
  - `Expr::Match { subject: Box<Expr>, arms: Vec<MatchArm>, default: Box<Expr>, span: Span }`.
  - `pub struct MatchArm { pub patterns: Vec<Spanned<String>>, pub value: Expr }`, exported from the crate root.
- Produces in `alba_core`: evaluation of `Expr::Match` (first arm whose glob matches the subject, else `default`).

- [ ] **Step 1: Write the failing syntax tests** in `crates/alba-syntax/tests/parse.rs`:
  - `a_match_parses_arms_alternatives_and_default`: `let t = match b { 'hotfix/*' | 'master' => 'm' \n
    'release/*' => 'r' \n _ => 'd' }` parses into two arms (first with patterns `hotfix/*`, `master`) and a
    default.
  - `a_match_without_default_is_rejected`, `an_arm_after_the_default_is_rejected`,
    `a_duplicate_pattern_is_rejected`, `an_interpolated_pattern_is_rejected`: each asserts the error message and
    that the span covers the offending token (`_` expected / the arm / the duplicate / the `{`).
  - `a_match_inside_an_interpolation_closes_on_its_own_brace`: `run "echo {match x { 'a' => '1' _ => '2' }} done"`
    parses into literal `echo `, one `Expr::Match`, literal ` done`.
  - `extra_tokens_in_an_interpolation_are_rejected`: `"{a b}"` fails with ``expected `}` to close the
    interpolation`` spanning `b`.
  - The existing "unclosed interpolation" and "empty interpolation" tests stay green unchanged.
- [ ] **Step 2: Run** `cargo test -p alba-syntax`. Expected: the new tests FAIL.
- [ ] **Step 3: Implement the tokens and `parse_match`** in `expr.rs`, dispatched from `parse_atom` on
  `KwMatch`. Arms are separated by newlines or commas. A pattern is a `Str` token parsed with
  `template::parse_template_at`; any `TemplatePart::Expr` in it is the "patterns must be known at load time" error.
  The pattern value is the template's decoded literal text. `_` is `Ident("_")`.
- [ ] **Step 4: Rework `scan_interpolation`**: lex the rest of the string content into tokens (newlines
  dropped, as today), hand them to a `Parser` with a synthetic `Eof`, call `parse_expr`, then require the next
  token to be `RBrace`; the returned end offset is that `RBrace`'s end. Keep "empty interpolation" (the first token
  is `RBrace`) and "unclosed interpolation" (no `RBrace` follows) with their current spans.
- [ ] **Step 5: Run** `cargo test -p alba-syntax`. Expected: PASS. Accept new snapshots only if they are the new
  tests'.
- [ ] **Step 6: Write the failing evaluation tests** in `crates/alba-core/tests/match_expression.rs`, through
  `load_str`, reading the rendered `description` of a beam:
  - `the_first_matching_arm_wins`: `let branch = "hotfix/x"` and `let t = match branch { 'hotfix/*' => 'm' \n
    'hotfix/**' => 'never' \n _ => 'd' }` gives `m`.
  - `a_star_does_not_cross_a_slash`: subject `hotfix/x/y` against `'hotfix/*'` gives the default.
  - `an_alternative_pattern_matches`: subject `master` with `'hotfix/*' | 'master'` gives the arm value.
  - `an_invalid_glob_in_an_arm_not_taken_fails_the_load`: `'[' => 'x'` in an arm that does not match is a load
    error spanning `'['`.
  - `a_boolean_subject_is_a_type_error`: `match true { _ => 'x' }` is a load error ("expected a string, found a
    boolean").
  - `match_works_inside_a_run_template`: `run "echo {match branch { _ => 'ok' }}"` loads and
    `render_template(&beam.run[0], &beam.scope)` gives `echo ok`.
- [ ] **Step 7: Run** `cargo test -p alba-core --test match_expression`. Expected: FAIL to compile or FAIL.
- [ ] **Step 8: Implement evaluation and validation** in `eval.rs`: compile each pattern with
  `globset::Glob::new(p)?.compile_matcher()` (`literal_separator(true)` so `*` does not cross `/`), match against
  the subject string. `check_expr` validates the subject, every arm value, every pattern and the default; arm
  values follow the `if` typing rule. `expr_span` returns the `Match` span.
- [ ] **Step 9: Run** `cargo test -p alba-core --test match_expression` then `cargo test -p alba-core`.
  Expected: PASS.
- [ ] **Step 10: Document** `match` in the README, with the branch-to-target example and the `|` alternatives,
  and state that `{` inside an interpolation now closes where the expression ends.
- [ ] **Step 11: Gate and commit**

```bash
git add crates/alba-syntax crates/alba-core README.md
git commit -m "✨ feat(syntax): add a match expression over glob patterns" -m "Interpolations now close where the parsed expression ends rather than at the first closing brace, so a match can be used inside a string." -m "Refs: #49"
```

---

### Task 3: The `git { base }` block

**Files:**
- Modify: `crates/alba-syntax/src/ast.rs` (`File`), `parser.rs` (`parse_file`)
- Modify: `crates/alba-core/src/model.rs` (`Project`), `eval.rs` (`build_project`), `loader.rs` (root-only check,
  if imports are walked there)
- Modify: `README.md` ("Git variables" gains "Declaring the reference")
- Test: `crates/alba-syntax/tests/parse.rs`, `crates/alba-core/tests/git.rs`, `crates/alba-core/tests/loader.rs`

**Interfaces:**
- Produces in `alba_syntax`: `File.git: Option<GitDecl>`, `pub struct GitDecl { pub base: Option<Expr>, pub span:
  Span }`. Inside the block, `base <expr>` is the only key; an unknown key is a parse error with a "did you mean
  `base`?" help when close.
- Produces in `alba_core`: `Project.git: GitSettings`, `#[derive(Debug, Clone, Default, PartialEq)] pub struct
  GitSettings { pub base: Option<String>, pub warning: Option<String> }`, exported from the crate root. `base` is
  `None` without a block, for an empty string, or when git could not answer; `warning` holds git's message in that
  last case.

- [ ] **Step 1: Write the failing tests**:
  - Syntax: `a_git_block_holds_a_base_expression`, `a_second_git_block_is_rejected` (span on the second `git`),
    `an_unknown_git_key_is_rejected_with_a_suggestion` (`bse` suggests `base`).
  - `crates/alba-core/tests/git.rs`, in a temporary repository on branch `hotfix/x`:
    `base_is_evaluated_from_the_branch` (a `match git.branch` gives `origin/master`),
    `an_empty_base_means_no_reference`, `base_may_read_lets` (any `let` in the file, since lets are evaluated
    before the block), `calling_git_changes_in_base_is_rejected` is written in Task 4, which adds the method.
  - `a_detached_head_falls_through_to_the_default_arm` (Review Focus 5): `git checkout --detach`, the `_` arm is
    taken.
  - `outside_a_repository_the_project_loads_with_a_warning`: a temporary directory without `.git`; `load_project`
    succeeds, `git.base` is `None`, `git.warning` is `Some` and names the git failure.
  - `crates/alba-core/tests/loader.rs`: `a_git_block_in_an_imported_beamfile_is_rejected`, span in the imported
    file.
- [ ] **Step 2: Run** `cargo test -p alba-syntax --test parse`, `cargo test -p alba-core --test git --test
  loader`. Expected: FAIL.
- [ ] **Step 3: Implement**: `parse_file` accepts `Ident("git")` followed by `LBrace` (`git` stays a reserved
  name, not a keyword); `build_project` evaluates `base` after all `let`s with `Timing::Load`, catching a
  `git.*` failure inside this expression only and turning it into `GitSettings { base: None, warning }`; an empty
  result is `None`; `is_root == false` with a block is a load error ("a `git` block belongs in the root
  Beamfile").
- [ ] **Step 4: Run** the same tests. Expected: PASS.
- [ ] **Step 5: Document** the block in the README ("Declaring the reference"), with the
  `env('GITHUB_BASE_REF', match git.branch { ... })` example.
- [ ] **Step 6: Gate and commit**

```bash
git add crates/alba-syntax crates/alba-core README.md
git commit -m "✨ feat(git): declare the reference to diff from in a git block" -m "Refs: #49"
```

---

### Task 4: Lists, method calls, `git.changes()` and `.dirs()`

**Files:**
- Modify: `crates/alba-syntax/src/expr.rs` (postfix method calls)
- Modify: `crates/alba-core/src/model.rs` (`Value::List`, `Beam.change_globs`, `ChangeMode`, `FullReason`),
  `eval.rs` (method evaluation and validation, list rendering, `Scope` change set and quoting), `lib.rs` (exports)
- Modify: `README.md` (new section "Incremental beams", before "Watch mode")
- Test: `crates/alba-syntax/tests/parse.rs`, new `crates/alba-core/tests/changes.rs`

**Interfaces:**
- Consumes: `GitSettings` (Task 3) only for the `calling_git_changes_in_base_is_rejected` test.
- Produces in `alba_syntax`: `Expr::Method { receiver: Box<Expr>, method: Spanned<String>, args: Vec<Expr>,
  span: Span }`. `git.changes(...)` parses as `Method { receiver: Var("git"), method: "changes", .. }`; `git.branch`
  stays `Field`. Method calls chain left to right (`a.b(x).c(y)`).
- Produces in `alba_core` (all exported from the crate root):
  - `Value::List(Vec<String>)`.
  - `Beam.change_globs: Vec<String>`: every rendered glob the beam passes to `git.changes()` in `run` or `env`,
    deduplicated, in order of appearance.
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub enum ChangeMode { Incremental, Full(FullReason) }` and
    `pub enum FullReason { NoReference, ReferenceUnresolved, OnTargetBranch, BeamfileChanged,
    DependencyAffected(BeamId), InputOutsideChanges(String) }`, the `String` being the changed path relative to
    the project root.
  - `#[derive(Debug, Clone)] pub struct ChangeSet { pub mode: ChangeMode, pub changed: Arc<Vec<PathBuf>> }`,
    `changed` holding absolute, lexically normal paths, deleted ones included.
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)] pub enum Quoting { #[default] Posix, PowerShell }`.
  - `Scope::with_changes(&self, changes: ChangeSet, beam_dir: &Path, cwd: &Path) -> Self` and
    `Scope::with_quoting(&self, quoting: Quoting) -> Self`. A scope without a change set evaluates `git.changes()`
    in full mode with `FullReason::NoReference`.

- [ ] **Step 1: Write the failing syntax tests**: `a_method_call_on_git_parses`, `method_calls_chain`
  (`git.changes('a').dirs('b')`), `a_field_is_still_a_field` (`git.branch`).
- [ ] **Step 2: Run** `cargo test -p alba-syntax --test parse`. Expected: FAIL. **Step 3: Implement** the postfix
  loop in `parse_atom` (after an atom, while `Dot Ident LParen` follows). **Step 4: Run** again. Expected: PASS.
- [ ] **Step 5: Write the failing core tests** in `crates/alba-core/tests/changes.rs`, through `load_project` on a
  temporary directory and `render_template(&beam.run[0], &beam.scope.with_changes(set, &beam.dir, &cwd))`:
  - Load-time: `git_changes_outside_run_and_env_is_rejected` (in `description` and `inputs`),
    `a_glob_absent_from_inputs_is_rejected_listing_the_inputs`, `git_changes_takes_exactly_one_string`,
    `an_unknown_method_names_the_value_type_and_its_methods`, `a_list_where_a_string_is_required_is_a_type_error`
    (`{git.changes('x') + 'y'}`), `change_globs_lists_every_glob_once`, and in `tests/git.rs`
    `calling_git_changes_in_base_is_rejected`.
  - Incremental: `only_existing_matching_files_are_listed` (a deleted path and a non-matching path in `changed`
    are dropped, output sorted), `a_rename_keeps_the_new_path` (only the new path exists).
  - Full: `full_mode_lists_the_glob_base_directory` (`src/**/*.php` gives `src`), `a_glob_without_literal_prefix_
    gives_the_current_directory` (gives `.`), `no_change_set_means_full_mode`.
  - `.dirs()`: `dirs_reduces_to_the_first_matching_prefix` (`src/Billing/Invoice.php` with `src/*` gives
    `src/Billing`), `dirs_at_two_levels`, `dirs_keeps_shorter_paths_and_collapses_nested_ones` (`src`,
    `src/Billing` gives `src`), `dirs_without_a_matching_prefix_keeps_the_path`.
  - Review Focus: `posix_quoting_keeps_awkward_names_whole` (`a b.php`, `it's.php`, `café.php` render as
    `'a b.php' 'it'\''s.php' café.php`; plain names stay bare), `powershell_quoting_doubles_single_quotes`
    (`'it''s.php'`, and `\` separators), `paths_are_relative_to_the_beam_cwd` (`cwd "src"` makes `src/A.php`
    render as `A.php`), `a_glob_climbing_out_of_the_beamfile_directory_matches` (`../libs/**/*.php` in `app/`).
- [ ] **Step 6: Run** `cargo test -p alba-core --test changes`. Expected: FAIL.
- [ ] **Step 7: Implement** in `eval.rs`:
  - `check_expr` gains `Expr::Method`: receiver `git` with method `changes` only under `Timing::Schedule`, one
    argument statically known as a string, compiled like `glob()`, present in the beam's rendered `inputs` (pass
    them in as a new parameter of the beam-field validation path); method `dirs` only on a list receiver, one
    known string argument, compiled. A list in `Concat`, `Eq`, `NotEq`, an `if` condition, a `match` subject, or a
    load-time field is a type error ("expected a string, found a list").
  - `git.changes(glob)` evaluation: incremental filters `changed` through `anchor_pattern(beam_dir, glob)` the way
    `affected_beams` does, keeps paths that exist on disk, makes them relative to `cwd` (climbing with `..` when
    needed), `/`-separated, sorted, deduplicated; full gives `pattern_dir(beam_dir, glob)` relative to `cwd`.
  - `.dirs(glob)` follows the spec's section "`.dirs(glob)`", with a matcher built with `literal_separator(true)`.
  - Rendering a list in `render_template`: each element quoted per the scope's `Quoting` (POSIX: bare when made of
    `[A-Za-z0-9_./:@%+=,-]` and non-ASCII letters, else `'…'` with `'` written `'\''`; PowerShell: bare under the
    same rule, else `'…'` with `'` doubled and `/` turned into `\`), joined by single spaces, empty list renders as
    nothing.
  - `build_beam` fills `change_globs`.
- [ ] **Step 8: Run** `cargo test -p alba-core`. Expected: PASS.
- [ ] **Step 9: Document** `git.changes()`, `.dirs()`, full versus incremental mode and quoting in the README
  section "Incremental beams", using the spec's success-criteria Beamfile.
- [ ] **Step 10: Gate and commit**

```bash
git add crates/alba-syntax crates/alba-core README.md
git commit -m "✨ feat(git): list a beam's changed files with git.changes" -m "Adds method calls, a list value rendered shell-quoted in commands, and a dirs method reducing paths to the directories matching a glob." -m "Refs: #49"
```

---

### Task 5: Per-beam mode and the `unchanged` status in the engine

**Files:**
- Modify: `crates/alba-engine/src/affected.rs` (mode computation), `scheduler.rs` (`RunOptions`, `render`,
  `process`, `wait_for_dependencies`, cache write gate), `event.rs` (`BeamStatus`, `RunSummary`, `BeamStarted`),
  `lib.rs` (exports)
- Modify: `crates/alba/src/render/mod.rs`, `json.rs`, `interleaved.rs`, `grouped.rs` (status label, JSON fields)
- Modify: `crates/alba-tui/src/state.rs`, `ui/theme.rs`, `ui/beams.rs`, `ui/progress.rs` (the new status)
- Test: new `crates/alba-engine/tests/changes.rs`,
  `crates/alba-engine/tests/cache.rs`, `crates/alba-tui/tests/render.rs`

**Interfaces:**
- Consumes: `ChangeSet`, `ChangeMode`, `FullReason`, `Quoting`, `Scope::with_changes`, `Scope::with_quoting`,
  `Beam.change_globs` (Task 4); `alba_core::git::merge_base` (Task 1); `GitSettings` (Task 3).
- Produces in `alba_engine`:
  - `#[derive(Debug, Clone)] pub struct RunChanges { pub reference: String, pub merge_base: String, pub
    on_target_branch: bool, pub changed: Arc<Vec<PathBuf>> }`; `on_target_branch` is merge base equal to `HEAD`
    with a clean tree.
  - `pub fn resolve_changes(root: &Path, reference: &str) -> Result<RunChanges, EngineError>`.
  - `RunOptions.changes: Option<RunChanges>` (`None`: full mode, `FullReason::NoReference`) and
    `RunOptions.unresolved_base: bool` (full mode with `FullReason::ReferenceUnresolved`).
  - `pub(crate) fn beam_modes(project: &Project, sources: &SourceMap, changes: &RunChanges) ->
    HashMap<BeamId, ChangeMode>` in `affected.rs`, sharing the anchor-grouped matching of `affected_beams`.
  - `BeamStatus::Unchanged`; `RunSummary.unchanged: Vec<BeamId>`; `RunEvent::BeamStarted` gains `mode:
    Option<ChangeMode>` (`Some` only for beams with `change_globs`); `RunEvent::RunStarted` gains `base:
    Option<(String, String)>` (reference, merge-base SHA).
  - JSON: `beam_finished.status` `"unchanged"`, `run_finished.unchanged`, `beam_started.mode` (`"incremental"` /
    `"full"`) with `full_reason` (`"no_reference"`, `"reference_unresolved"`, `"on_target_branch"`,
    `"beamfile_changed"`, `"dependency_affected"`, `"input_outside_changes"`), `run_started.base` (`{"reference",
    "merge_base"}`), all omitted when absent.

- [ ] **Step 1: Write the failing engine tests**, in temporary repositories with a `git` block, through
  `alba_engine::run` with `RunOptions.changes` from `resolve_changes`, asserting on the collected `RunEvent`s and
  on the commands' output (an `echo {git.changes('src/**/*.php')}` beam):
  - `only_the_changed_files_reach_the_command`.
  - `a_config_input_change_runs_full` (`phpcs.xml` changed: output `src`, `mode` full with
    `InputOutsideChanges("phpcs.xml")`).
  - `a_beamfile_change_runs_full`, `an_affected_dependency_runs_full`.
  - `the_target_branch_on_a_clean_tree_runs_full`, `the_target_branch_with_uncommitted_work_stays_incremental`,
    `a_fresh_branch_with_uncommitted_work_stays_incremental`.
  - `a_beam_with_no_changed_file_ends_unchanged`: status `Unchanged`, its dependent runs, `RunSummary.unchanged`
    holds it, the exit summary has no failure.
  - `a_beam_with_only_deleted_files_ends_unchanged`.
  - `a_beam_without_git_changes_runs_as_before` (no `mode` on its `BeamStarted`).
  - `unresolved_base_runs_full_with_a_warning` (Review Focus 3; the warning itself is the CLI's, Task 6; here
    `unresolved_base: true` gives `ReferenceUnresolved`).
  - In `tests/cache.rs`: `an_incremental_pass_does_not_answer_a_full_run` (incremental run succeeds, then a run
    with `changes: None` on the same tree executes the beam instead of replaying it), `an_unchanged_beam_writes_no_
    cache_entry`.
- [ ] **Step 2: Run** `cargo test -p alba-engine --test changes --test cache`. Expected: FAIL.
- [ ] **Step 3: Implement** `resolve_changes` (merge base, `changed_files`, `head` for the clean check),
  `beam_modes` (rules 3 to 5 of the spec's "`git.changes(glob)`" section, checked in that order), mode lookup in
  `run_once`, `render` building `beam.scope.with_params(..).with_changes(..).with_quoting(..)` (PowerShell only for
  `ExecutorKind::SystemShell` under `cfg!(windows)`), and the `Unchanged` short-circuit in `process` before
  executing: when the mode is `Incremental` and every `git.changes()` glob of the beam yields no existing file,
  emit `BeamFinished { status: Unchanged }`, publish the outcome with the `static_contribution` of the rendered
  commands, skip both cache read and write. `wait_for_dependencies` treats `Unchanged` like `Succeeded`.
- [ ] **Step 4: Run** the same tests. Expected: PASS.
- [ ] **Step 5: Write the failing renderer tests**: in `crates/alba/tests/cli_run.rs`
  `json_reports_unchanged_and_mode` (asserting the JSON fields above), and a `crates/alba-tui/tests/render.rs`
  snapshot `run_with_an_unchanged_beam` showing `○ unchanged`. **Step 6: Implement** every `BeamStatus` match site
  (`render/mod.rs` `status_label`, `json.rs` `status_name` and `exit_code` (`None`), `alba-tui` `end_line`,
  `status_token`, `status_glyph`, progress counts as passed). **Step 7: Run** `cargo test -p alba --test cli_run`
  and `cargo test -p alba-tui`; review and accept only the new snapshot.
- [ ] **Step 8: Gate and commit**

```bash
git add crates/alba-engine crates/alba crates/alba-tui
git commit -m "✨ feat(engine): run beams on their changed files only" -m "Each beam gets an incremental or full mode from the run's change set, and a beam whose changed files are all gone ends unchanged without running." -m "Refs: #49"
```

---

### Task 6: CLI: the reference in effect, `--affected` without a value, `--no-affected`

**Files:**
- Modify: `crates/alba/src/args.rs` (`RunFlags`, `Command::Affected`), `main.rs` (selection building),
  `commands/run.rs`, `commands/affected.rs`, `commands/hook.rs` (if it bypasses `run`)
- Modify: `crates/alba-engine/src/session.rs` (recompute `RunChanges` on every run of a session)
- Modify: `crates/alba-tui/src/state.rs` (`run_label` shows the reference in effect)
- Modify: `README.md` (flags table, "Affected runs", "Incremental beams"), `alba-agent-plugin/skills/using-alba/`
  (Beamfile DSL and CLI references)
- Test: `crates/alba/tests/cli_affected.rs`, `crates/alba/tests/cli_run.rs`, `crates/alba/tests/cli_hooks.rs`,
  `crates/alba/tests/cli_watch.rs`

**Interfaces:**
- Consumes: `RunChanges`, `resolve_changes`, `RunOptions.changes`, `RunOptions.unresolved_base` (Task 5);
  `Project.git` (Task 3).
- Produces: `RunFlags.affected: Option<Option<String>>` (`#[arg(long, value_name = "REF", num_args = 0..=1)]`,
  `Some(None)` meaning "the declared base"), `RunFlags.no_affected: bool` (`conflicts_with = "affected"`),
  `Command::Affected.reference: Option<String>`.

- [ ] **Step 1: Write the failing CLI tests**, with the `repository()` helper of `cli_affected.rs` extended with a
  `git { base 'main' }` block and a feature branch:
  - `alba_run_without_flags_lists_only_changed_files` and `posix_quoting_end_to_end` (a changed `a b.php` reaches
    `echo` as one argument; Review Focus 1).
  - `affected_without_a_value_uses_the_declared_base` (`alba run --affected`, and `alba run phpcs --affected`
    with the beam before the flag), `affected_with_a_value_still_works` (`alba run --affected main phpcs`).
  - `affected_without_a_value_and_no_base_exits_2`.
  - `no_affected_runs_full` and `affected_and_no_affected_conflict` (exit `2`, clap's message).
  - `an_unresolved_base_warns_and_runs_full` (base `origin/nope`: stderr carries one warning naming it, exit `0`,
    output `src`); `an_unresolved_explicit_reference_exits_2`.
  - `a_git_block_outside_a_repository_warns_once` (stderr carries `GitSettings.warning`).
  - `beams_without_inputs_still_run_with_a_base` (`alba run deploy`).
  - `alba_affected_uses_the_declared_base`, `alba_affected_without_reference_or_base_exits_2`.
  - `cli_hooks.rs`: `a_hook_beam_runs_incrementally` (pre-commit beam echoes `git.changes()`).
  - `cli_watch.rs`: `each_watch_run_recomputes_the_changes` (a second save adds a file to the list).
- [ ] **Step 2: Run** `cargo test -p alba --test cli_affected --test cli_run --test cli_hooks --test cli_watch`.
  Expected: FAIL.
- [ ] **Step 3: Implement**: one function in `commands/run.rs`, `fn reference_in_effect(project: &Project, flags:
  &RunFlags) -> Result<Option<(String, bool)>, ...>` returning the reference and whether it is explicit, feeding
  both the `Selection` (`--affected` only) and `RunOptions.changes`; an explicit reference that fails
  `resolve_changes` is exit `2`, a declared one sets `unresolved_base` and prints `warning: cannot diff from
  `<ref>`: <git message>; running in full mode` once on stderr; `GitSettings.warning` printed once as `warning:
  <message>; no reference declared for this run`. The watch session calls `resolve_changes` before every run. The
  TUI header shows `base <ref>` when a reference is in effect.
- [ ] **Step 4: Run** the same tests. Expected: PASS.
- [ ] **Step 5: Document** the flags in the README table (`--affected [<REF>]`, `--no-affected`, with the "beam
  before the flag" note), and update the agent plugin's `using-alba` references (`git` block, `match`,
  `git.changes()`, `.dirs()`, flags).
- [ ] **Step 6: Gate** with `alba run check` on the final state, **and commit**

```bash
git add crates README.md alba-agent-plugin
git commit -m "✨ feat(cli): diff from the declared base on every run" -m "--affected accepts no value to use the declared base, and --no-affected forces full mode." -m "Closes #49"
```
