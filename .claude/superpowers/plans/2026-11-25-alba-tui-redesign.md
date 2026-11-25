# Alba TUI Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rebuild the interactive interface of `crates/alba-tui` to the Alba visual identity and the
readability features of the spec, with the engine and CLI passing it the data it needs.

**Architecture:** Part A extends the event contract (`RunStarted.details`, `CommandStarted`,
`BeamNotice`, `ProjectBroken.report`) and the code that produces it, keeping every headless output
unchanged. Part B rebuilds the TUI's theme, log model and drawing on top of that contract, band by
band, ending with the snapshot suite regenerated against the Paper mockups.

**Tech Stack:** Rust workspace, ratatui, crossterm, tokio, insta snapshots, `TestBackend`.

**Spec:** `.claude/superpowers/specs/2026-11-25-alba-tui-redesign-design.md`. Mockups: Paper file
**Alba**, page **TUI v2** (twelve artboards, one 11x23 px cell per character).

## Global Constraints

- Work on a branch `feat/tui-redesign`, never on `main`.
- Code, comments, commit messages in English. No em dash in any written text.
- Commit messages: gitmoji plus Conventional Commits, `<emoji> <type>(<scope>): <summary>`. Load
  `jh3ady-commit-conventions` before writing each one. Never cite this plan, a task or a step.
- TDD, classical school: a failing test first, through public entry points (`AppState::apply`,
  `ui::draw`, `scheduler::run`, the renderers' `handle`), state assertions, no mocks of owned code.
- Feedback loop: while iterating run only the tests of the touched crate and file, plus
  `cargo clippy -p <crate> --all-targets -- -D warnings`. The full gate (`alba check`, which runs
  `cargo fmt --check`, workspace clippy, `cargo build --workspace`, `cargo test --workspace`) runs
  once per task on its final state. Run long commands in the background and wait for completion;
  never poll with sleep loops.
- Palette values, exactly: `surface #151719`, `text #E3E4DC`, `text-muted #A1A7A4`,
  `track #79807C`, `line #626A67`, `accent #F3C280`, `on-accent #24221E`, `success #A5CF9A`,
  `cached #9CCBC9`, `error #F0A29A`, `selection #353C40`.
- Every glyph the interface draws occupies one terminal cell: `○ ▶ ✓ ↯ ✗ Ⅱ ● ▎ ┃ │ ─ ━ ┬ ┴ └ ┼ ┘`.
- Size floor stays 40x12.
- Every commit leaves the workspace green. A Part B task that changes drawing runs
  `cargo insta test -p alba-tui`, reviews each changed snapshot, accepts only the intended changes,
  and commits them with the task. Task 15 adds the per-artboard snapshots on top.
- Headless renderers (`crates/alba/src/render/`) produce byte-identical output for every existing
  test; JSON keeps its current event kinds.

## Review Focus

1. **Wide characters in beam names and output.** A CJK beam name or output line must keep every
   column aligned (duration column, divider, scrollbar, badge). Test owned by Task 10 and Task 11.
2. **Resizing between 40x12 and large sizes mid-run.** No panic, no drawing outside the buffer, at
   every width from 40 to 200. Test owned by Task 9.
3. **A watch session that reloads with a different beam set** (beams added or removed) keeps the
   selection valid and the counter correct. Test owned by Task 8.
4. **Output lines with ANSI sequences split across wraps** keep their remapped colour on every row.
   Test owned by Task 11.
5. **The CLI replay after quitting a failed run** prints only process output, never echo or Alba
   lines. Test owned by Task 8.

---

## Part A: engine, core and CLI

### Task 1: Line and column positions and the diagnostic report

**Files:**
- Modify: `crates/alba-syntax/src/diagnostics.rs`, `crates/alba-syntax/src/lib.rs`
- Modify: `crates/alba-core/src/lib.rs` (re-export)
- Test: `crates/alba-syntax/src/diagnostics.rs` (unit tests module)

**Interfaces:**
- Produces, in `alba_syntax`, re-exported from `alba_core`:
  - `pub fn line_col(source: &str, offset: usize) -> (usize, usize)`: 1-based line, 1-based column
    counted in chars; an offset past the end maps to the last position.
  - `pub struct DiagnosticReport { pub message: String, pub path: String, pub line: usize,
    pub column: usize, pub snippet: Vec<SnippetLine>, pub caret: Range<usize>,
    pub help: Option<String> }` (`Debug, Clone, PartialEq, Eq`).
  - `pub struct SnippetLine { pub number: usize, pub text: String, pub faulty: bool }`.
  - `impl Diagnostic { pub fn report(&self, source: &str, path: &str) -> Option<DiagnosticReport> }`:
    `None` without a span. Snippet is two lines before and two after the faulty line, clipped to the
    file. `caret` is the char range of the span on the faulty line (clipped to that line when the
    span runs past it).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn line_col_counts_from_one() {
    let source = "beam a {\n  needs [b]\n}\n";
    assert_eq!(line_col(source, 0), (1, 1));
    assert_eq!(line_col(source, 11), (2, 3));
}

#[test]
fn a_report_points_at_the_faulty_span_with_context() {
    let source = "beam check {\n  description \"x\"\n  needs [fmt, lint, tset]\n  run \"echo\"\n}\n";
    let start = source.find("tset").unwrap();
    let d = Diagnostic::new("unknown beam `tset`", Some(Span { start, end: start + 4 }),
        Some("did you mean `test`?".into()));
    let r = d.report(source, "Beamfile").unwrap();
    assert_eq!((r.line, r.column), (3, 21));
    assert_eq!(r.snippet.iter().map(|l| l.number).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5]);
    assert!(r.snippet[2].faulty);
    assert_eq!(r.caret, 20..24);
    assert_eq!(r.help.as_deref(), Some("did you mean `test`?"));
}

#[test]
fn a_diagnostic_without_a_span_has_no_report() {
    assert!(Diagnostic::new("boom", None, None).report("", "Beamfile").is_none());
}
```

- [ ] **Step 2: Run** `cargo test -p alba-syntax diagnostics`. Expected: FAIL, unresolved names.
- [ ] **Step 3: Implement** `line_col`, the two structs and `Diagnostic::report` in
  `diagnostics.rs`; export them from `alba_syntax` and re-export from `alba_core`.
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit** (`✨ feat(diagnostics): ...`).

### Task 2: Beam definitions computed from the sources

**Files:**
- Create: `crates/alba-core/src/definition.rs`
- Modify: `crates/alba-core/src/lib.rs`
- Test: `crates/alba-core/tests/definition.rs`

**Interfaces:**
- Consumes: `line_col` (Task 1), `SourceMap::get` (`loader.rs:135`), `Beam.span`, `Beam.source`,
  `Beam.run: Vec<StringTemplate>` (each with a `span`).
- Produces:
  - `pub struct BeamDefinition { pub location: String, pub commands: Vec<String> }`
    (`Debug, Clone, PartialEq, Eq`).
  - `pub fn definitions(project: &Project, sources: &SourceMap, root: &Path) -> HashMap<BeamId, BeamDefinition>`.
  - `location` is `<path relative to root>:<1-based line of the beam span>`; `commands` are the
    source slices of each `run` template span with the surrounding quotes removed.

- [ ] **Step 1: Write the failing tests** (load real fixtures with `load_project` in a temp dir)

```rust
#[test]
fn a_root_beam_is_located_by_file_and_line() {
    // Beamfile: "beam build {\n  run \"cargo build\"\n}\n\nbeam test {\n  run [\"cargo build\", \"cargo test --workspace\"]\n}\n"
    let defs = definitions(&project, &sources, root);
    assert_eq!(defs[&BeamId("test".into())].location, "Beamfile:5");
    assert_eq!(defs[&BeamId("test".into())].commands, vec!["cargo build", "cargo test --workspace"]);
}

#[test]
fn an_imported_beam_is_located_relative_to_the_root() {
    // root Beamfile imports services/payment/Beamfile as `payment`, which declares `integration` on line 12
    assert_eq!(defs[&BeamId("payment:integration".into())].location, "services/payment/Beamfile:12");
}
```

- [ ] **Step 2: Run** `cargo test -p alba-core --test definition`. Expected: FAIL.
- [ ] **Step 3: Implement** `definitions` in `definition.rs`. Path relative to root uses the same
  lexical rule as `watch/mod.rs:187 relative_to`; move that helper into `alba-core` if needed and
  call it from both places.
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit** (`✨ feat(core): ...`).

### Task 3: `RunStarted.details`

**Files:**
- Modify: `crates/alba-engine/src/event.rs`, `crates/alba-engine/src/scheduler.rs` (`RunOptions`
  at :71, construction at :376), `crates/alba-engine/src/session.rs` (`drive` and
  `run_in_session` call, :287), `crates/alba/src/commands/run.rs` (`execute` :198, `tui_execute`)
- Modify (literals): `crates/alba-tui/src/state.rs:257,972`, `crates/alba-tui/src/lib.rs:448,1153`,
  `crates/alba-tui/src/ui/graphpane.rs:218`, `crates/alba-tui/tests/render.rs`,
  `crates/alba/src/render/json.rs:137,364`, `crates/alba-engine/tests/scheduler.rs:977`
- Test: `crates/alba-engine/tests/scheduler.rs`

**Interfaces:**
- Consumes: `BeamDefinition`, `definitions` (Task 2), `ExecutorKind` (`model.rs:34`).
- Produces, in `alba_engine`:
  - `pub struct BeamDetails { pub id: BeamId, pub description: Option<String>,
    pub location: Option<String>, pub executor: ExecutorLabel, pub commands: Vec<String> }`.
  - `pub struct ExecutorLabel { pub kind: String, pub detail: Option<String> }`, with
    `From<&ExecutorKind>`: `Shell` gives `shell` + `embedded`; `SystemShell` gives `system shell`;
    `Docker` gives `docker` + image, else build context; `Compose` gives `compose` + service;
    `Plugin` gives `plugin` + name.
  - `RunEvent::RunStarted` gains `details: Vec<BeamDetails>`, same order as `beams`.
  - `RunOptions` gains `pub definitions: HashMap<BeamId, BeamDefinition>` (empty by default;
    `location` is then `None` and `commands` empty).
- The JSON renderer ignores `details` (wire output unchanged).

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn run_started_carries_each_beam_definition() {
    // project with `build` (docker image "rust:1") and `test` (description "Run the tests")
    let started = first_run_started(&events);
    let test = started.details.iter().find(|d| d.id.0 == "test").unwrap();
    assert_eq!(test.description.as_deref(), Some("Run the tests"));
    assert_eq!(test.location.as_deref(), Some("Beamfile:5"));
    let build = started.details.iter().find(|d| d.id.0 == "build").unwrap();
    assert_eq!((build.executor.kind.as_str(), build.executor.detail.as_deref()), ("docker", Some("rust:1")));
}
```

- [ ] **Step 2: Run** `cargo test -p alba-engine --test scheduler run_started`. Expected: FAIL.
- [ ] **Step 3: Implement** the types and the field; fill `RunOptions.definitions` in `run.rs`
  (`execute`, `tui_execute`) and in `session.rs::drive` from `alba_core::definitions`; fix every
  literal listed above with `details: vec![]` where the test does not care.
- [ ] **Step 4: Run** the crate tests of `alba-engine`, `alba-tui`, `alba`. Expected: PASS.
- [ ] **Step 5: Commit** (`✨ feat(engine): ...`).

### Task 4: `CommandStarted`

**Files:**
- Modify: `crates/alba-engine/src/event.rs`, `crates/alba-engine/src/scheduler.rs`
  (`run_commands` :1226, before `CommandSpec` at :1263)
- Modify: `crates/alba/src/render/{interleaved,grouped,json}.rs`, `crates/alba-tui/src/state.rs:256`
  (exhaustive matches: ignore the event for now)
- Test: `crates/alba-engine/tests/scheduler.rs`, `crates/alba/src/render/json.rs` tests

**Interfaces:**
- Produces: `RunEvent::CommandStarted { id: BeamId, command: String }`, the rendered command passed
  through the beam's `Mask` (clone the mask before it moves into `forward_output`), sent on the same
  `events` sender, before any output of that command.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn each_command_is_announced_before_its_output_and_masked() {
    // beam: secrets ["TOKEN"], env TOKEN="s3cr3t", run ["echo one", "echo $TOKEN"]
    let seq = events_for(&events, "b"); // CommandStarted and BeamOutput texts, in order
    assert_eq!(seq, vec!["$ echo one", "one", "$ echo ***", "***"]);
}
```

and in `json.rs`: `command_started_writes_nothing` (the renderer emits no line for it).
- [ ] **Step 2: Run** `cargo test -p alba-engine --test scheduler each_command`. Expected: FAIL.
- [ ] **Step 3: Implement** the variant and its emission; the three renderers and the TUI ignore it.
- [ ] **Step 4: Run** the crate tests of `alba-engine` and `alba`. Expected: PASS.
- [ ] **Step 5: Commit** (`✨ feat(engine): ...`).

### Task 5: `BeamNotice`

**Files:**
- Modify: `crates/alba-engine/src/event.rs`, `crates/alba-engine/src/scheduler.rs` (:1108 cache
  notice, :1128 render error, :1241 open error, :1282 spawn error, :1308 session close)
- Modify: `crates/alba/src/render/{interleaved,grouped,json}.rs`, `crates/alba-tui/src/state.rs`
- Modify (tests asserting stderr notices): `crates/alba-engine/tests/scheduler.rs:415,545,1331`,
  `crates/alba-engine/tests/cache.rs` (helper `stderr()` :37 and its uses)
- Test: `crates/alba-engine/tests/scheduler.rs`, `crates/alba-engine/tests/cache.rs`,
  `crates/alba/tests/cli_run.rs`

**Interfaces:**
- Produces: `RunEvent::BeamNotice { id: BeamId, level: NoticeLevel, text: String }` and
  `pub enum NoticeLevel { Info, Error }` (`Debug, Clone, Copy, PartialEq, Eq`). Cache notice is
  `Info`; the four others are `Error`. Texts unchanged.
- Notices are no longer sent as `OutputLine`s, so they are never in the lines `forward_output`
  returns, so never stored by `store.store`.
- Renderers: interleaved prints `"{id} │ {text}"` on stdout exactly as a stderr line today; grouped
  buffers it with the beam's lines in arrival order; JSON writes
  `{"event":"beam_output","beam":..,"stream":"stderr","text":..,"replayed":false}`.

- [ ] **Step 1: Write the failing tests**
  - `a_template_error_is_a_notice_not_output`: render failure gives one `BeamNotice` with
    `NoticeLevel::Error` and text containing `ALBA_MISSING_VAR_XYZ`, and no stderr `BeamOutput`.
  - `the_cache_bypass_notice_is_an_info_notice`: `inputs` matching nothing gives
    `BeamNotice { level: Info, text: "cache: `inputs` matched no file; running without cache" }`.
  - `a_session_close_error_is_not_stored_in_the_cache` (cache.rs): a succeeding beam whose session
    close fails; the stored log for it does not contain `session close:`.
  - Existing `cli_run.rs:238` and `:271` stay green unchanged.
- [ ] **Step 2: Run** `cargo test -p alba-engine`. Expected: the new tests FAIL.
- [ ] **Step 3: Implement** the variant; replace the five `lines.send(OutputLine{Stderr})` sites
  with `events.send(BeamNotice)`; update the three renderers; update the listed old assertions to
  expect notices.
- [ ] **Step 4: Run** `cargo test -p alba-engine -p alba`. Expected: PASS, including every
  `cli_run.rs`, `cli_watch.rs`, `cli_ui.rs` test.
- [ ] **Step 5: Commit** (`♻️ refactor(engine): ...`).

### Task 6: `ProjectBroken.report`

**Files:**
- Modify: `crates/alba-engine/src/event.rs`, `crates/alba-engine/src/session.rs` (callback type at
  :128, :167, :570, :615; constructions :191, :237, :340, :421, :578),
  `crates/alba-engine/src/watch/mod.rs:117`, `crates/alba-engine/tests/watch.rs:168,201,725,1308`
- Modify: `crates/alba/src/main.rs:198,210`, `crates/alba/src/commands/run.rs:316,445,1055`
- Modify (patterns/literals): `crates/alba-tui/src/state.rs:363` and tests,
  `crates/alba-tui/src/ui/{logpane.rs:274,mod.rs:309}`, `crates/alba-tui/tests/render.rs:143,195`,
  `crates/alba/src/render/{json.rs:53,175,405,grouped.rs:120,interleaved.rs:103}`
- Test: `crates/alba/tests/cli_watch.rs` or the CLI unit tests near `render_core_error`

**Interfaces:**
- Consumes: `Diagnostic::report`, `DiagnosticReport` (Task 1).
- Produces:
  - `RunEvent::ProjectBroken { diagnostic: String, report: Option<DiagnosticReport> }`.
  - `pub struct RenderedError { pub text: String, pub report: Option<DiagnosticReport> }` in
    `alba_engine`; the render callback type becomes `FnMut(&SessionError) -> RenderedError`.
  - `crate::render_core_report(error: &CoreError, sources: &SourceMap) -> Option<DiagnosticReport>`
    in `main.rs`; only `EngineError::Core` and `LoadError` produce a report.

- [ ] **Step 1: Write the failing test** `a_broken_beamfile_reports_a_structured_diagnostic`: the
  CLI callback for an unknown `needs` entry returns a report with `line == 5`, `column == 21`,
  `help == Some("did you mean `test`?")`, and `text` equal to today's rendered string.
- [ ] **Step 2: Run** `cargo test -p alba`. Expected: FAIL.
- [ ] **Step 3: Implement**; fix every pattern with `..` or `report: None`.
- [ ] **Step 4: Run** `cargo test -p alba-engine -p alba -p alba-tui`. Expected: PASS.
- [ ] **Step 5: Commit** (`✨ feat(engine): ...`).

## Part B: the TUI

### Task 7: Theme and colour modes

**Files:**
- Modify: `crates/alba-tui/src/ui/theme.rs`, `crates/alba-tui/src/lib.rs` (`TuiOptions`),
  `crates/alba-tui/src/state.rs` (`colour` field), `crates/alba/src/commands/run.rs:388`,
  `crates/alba/src/main.rs:232`
- Test: `crates/alba-tui/src/ui/theme.rs` tests

**Interfaces:**
- Produces:
  - `pub enum ColourMode { TrueColour, Indexed, Monochrome }` in `alba_tui`; `TuiOptions.colour`
    becomes `ColourMode`; `AppState.colour: ColourMode`.
  - `pub fn colour_mode(stdout_is_terminal: bool, no_color: bool, colorterm: Option<&str>) -> ColourMode`
    in `alba` (`main.rs`), used by `run.rs`.
  - `pub enum Token { Surface, Text, TextMuted, Track, Line, Accent, OnAccent, Success, Cached, Error, Selection }`
    and `pub fn style(token: Token, mode: ColourMode) -> Style` (foreground) plus
    `pub fn fill(token: Token, mode: ColourMode) -> Style` (background); monochrome returns
    `Style::default()` except `TextMuted` which is `dim`.
  - `pub fn remap_ansi(style: Style, mode: ColourMode) -> Style` applying the spec's table to
    `Color::{Red, LightRed, Green, LightGreen, Yellow, LightYellow, Cyan, LightCyan, White, Gray,
    Black, DarkGray, Reset}`; indexed and RGB colours unchanged; monochrome strips colours.
  - The xterm-256 index for each token is a constant table chosen by nearest colour.
- Every existing theme function is replaced by `style`/`fill` with a token; delete the old ones.

- [ ] **Step 1: Write the failing tests**
  - `truecolour_tokens_are_the_spec_values`: `style(Token::Accent, TrueColour).fg == Some(Color::Rgb(0xF3,0xC2,0x80))`,
    and the same for all eleven tokens.
  - `indexed_tokens_use_the_pinned_table`: every token gives `Color::Indexed(_)` matching the table.
  - `monochrome_has_no_colour`: `style(Token::Error, Monochrome) == Style::default()`.
  - `ansi_red_becomes_the_error_token` and `ansi_blue_is_left_alone`.
  - `colour_mode_reads_colorterm_and_no_color` (in `alba`): `(true, false, Some("truecolor"))` gives
    `TrueColour`, `(true, false, None)` gives `Indexed`, `(true, true, _)` and `(false, _, _)` give
    `Monochrome`.
- [ ] **Step 2: Run** `cargo test -p alba-tui theme` and `cargo test -p alba colour_mode`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same. Expected: PASS.
- [ ] **Step 5: Commit** (`✨ feat(tui): ...`).

### Task 8: Log model and state for the new data

**Files:**
- Modify: `crates/alba-tui/src/logs.rs`, `crates/alba-tui/src/state.rs`,
  `crates/alba-tui/src/input.rs`, `crates/alba-tui/src/lib.rs` (`dispatch`, `buffered_lines` :421)
- Test: `crates/alba-tui/src/state.rs` tests, `crates/alba-tui/src/lib.rs` tests

**Interfaces:**
- Consumes: Tasks 3 to 6 events.
- Produces:
  - `pub enum LineKind { Output(Stream), Command, Notice(NoticeLevel), End(EndTone) }` and
    `pub enum EndTone { Success, Error, Cached, Muted }`; `LogLine` gains `kind: LineKind` (the
    `stream` field is removed; `LineKind::Output` carries it).
  - `LogBuffer::push_output(raw, stream, replayed)`, `push_command(command)`,
    `push_notice(level, text)`, `push_end(tone, text)`.
  - `pub enum StreamFilter { All, Stdout, Stderr }` on `AppState` (`stream_filter`), cycled by
    `Action::CycleStreamFilter` bound to `s` in normal mode; `LogBuffer::view` takes the filter
    (only `Output` lines are filtered).
  - `AppState.details: HashMap<String, BeamDetails>` refreshed on every `RunStarted`.
  - `AppState::apply` on `BeamFinished` pushes the end line with the spec's text:
    `✓ exit 0 · 0.1s`, `✗ exit 101 · 4.2s`, `✗ exit 1 · allowed · 0.3s`,
    `↯ restored from cache · 0.8s`, `○ cancelled · 2.0s`, `○ skipped` (Cancelled with no prior
    `BeamStarted`/`BeamCached` for that id in the run).
  - `buffered_lines` returns only `LineKind::Output` lines.
  - `Action::OpenFocusedOutput` for `Enter` in graph mode (selects and leaves graph, as today).

- [ ] **Step 1: Write the failing tests**
  - `a_finished_beam_ends_with_an_alba_line` (each status above, exact text and tone).
  - `a_beam_that_never_started_is_skipped`.
  - `commands_and_notices_land_in_the_beam_buffer_in_order`.
  - `the_stream_filter_hides_only_process_output`: filter `Stderr` keeps stderr, command, notice
    and end lines, drops stdout.
  - `s_cycles_all_stdout_stderr`.
  - `the_replay_after_quitting_holds_only_process_output` (Review Focus 5).
  - `a_reload_with_fewer_beams_keeps_the_selection_in_range` (Review Focus 3): select the last of
    five beams, apply a `RunStarted` with three, `selected == 2`.
- [ ] **Step 2: Run** `cargo test -p alba-tui state logs`. Expected: FAIL.
- [ ] **Step 3: Implement**; remove the plain-stderr dimming in `ui/logpane.rs:177-184` and its
  two tests, since `LineKind` now drives styling.
- [ ] **Step 4: Run** `cargo test -p alba-tui`. Expected: PASS, after reviewing and accepting the
  snapshots this task intentionally changes (see Global Constraints).
- [ ] **Step 5: Commit** (`✨ feat(tui): ...`).

### Task 9: Frame, header, progress, banner and key hints

**Files:**
- Modify: `crates/alba-tui/src/ui/mod.rs` (drop the outer border and junction; new row layout;
  `log_pane_content_area`), `crates/alba-tui/src/ui/header.rs`
- Create: `crates/alba-tui/src/ui/progress.rs`, `crates/alba-tui/src/ui/keys.rs`
- Delete: `crates/alba-tui/src/ui/footer.rs` (its counts move to `progress.rs`, its tests are
  rewritten there)
- Test: unit tests in the files above

**Interfaces:**
- Produces:
  - `pub fn body_area(width: u16, height: u16) -> Option<Rect>` replacing the old layout math;
    `log_pane_content_area` keeps its signature and calls it.
  - `header::line(state, now, width) -> Line<'static>` with the badge, breadcrumb and right state
    (`○ IDLE`, `▶ RUNNING`, `✓ PASSED`, `✗ FAILED`, `○ CANCELLED`, `● WATCHING`, `Ⅱ PARKED`),
    applying the spec's truncation order.
  - `progress::line(state, now, width) -> Line<'static>` (bar plus counts, drop order: pending or
    skipped, cached, passed, then `complete`) and `progress::banner(state, width)`.
  - `keys::line(state, width) -> Line<'static>`: base row by session state, `G follow` inserted
    when paused, `s all streams` prepended when filtered, pairs dropped from the end keeping
    `? help` and `q quit`.
- Background: `ui::draw` fills the whole frame with `fill(Token::Surface, mode)` first.
- Too-small screen (under 40x12): the ` ALBA ` badge, `Terminal too small` bold,
  `Resize to at least 40×12` and `Now WxH` muted, centred.
- Durations over one minute read `2m 14s`.

- [ ] **Step 1: Write the failing tests**
  - `the_header_names_the_mode_target_and_state` for each state.
  - `a_narrow_header_drops_the_detail_then_cuts_the_target`.
  - `progress_counts_drop_in_order_when_narrow`.
  - `the_progress_fill_turns_red_after_a_failure`.
  - `key_hints_follow_the_session_state` (four base rows verbatim from the spec).
  - `key_hints_keep_help_and_quit_when_narrow`.
  - `drawing_never_panics_from_40_to_200_columns` (Review Focus 2): draw a running state at every
    width 40..=200 and heights 12, 24, 50.
  - `the_frame_paints_the_surface_everywhere`: every cell of a 104x23 `TestBackend` has
    `bg == Rgb(0x15,0x17,0x19)` in `TrueColour`.
  - `the_too_small_screen_names_both_sizes`: at 36x10 the buffer contains
    `Resize to at least 40×12` and `Now 36×10`.
  - `long_durations_read_in_minutes`: 134 seconds formats as `2m 14s`.
- [ ] **Step 2: Run** `cargo test -p alba-tui ui::`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same. Expected: PASS.
- [ ] **Step 5: Commit** (`💄 style(tui): ...`).

### Task 10: Beams pane

**Files:**
- Modify: `crates/alba-tui/src/ui/tree.rs` (rename to `beams.rs`), `crates/alba-tui/src/ui/mod.rs`
- Test: unit tests in `beams.rs`

**Interfaces:**
- Produces:
  - `pub fn beams_width(names: &[&str], frame_width: u16) -> u16`: longest display width plus 12,
    capped at 40 percent of `frame_width`, floor 20.
  - `pub fn window(selected: usize, len: usize, height: usize) -> Range<usize>`: keeps the selection
    visible with one row of context.
  - `pub fn scrollbar(window: Range<usize>, len: usize, height: usize) -> Option<Range<usize>>`
    (thumb rows; `None` when everything fits). Shared with Task 11.
  - Glyphs from the spec table; the counter `selected+1/total`; selection row in
    `on-accent` bold on `accent`; scrollbar drawn on the divider column.

- [ ] **Step 1: Write the failing tests**
  - `the_width_follows_the_longest_name_within_bounds`: `beams_width(&["fmt"], 104) == 20`,
    `beams_width(&["services:payment:integration"], 104) == 40`, `beams_width(&[long], 60) == 24`.
  - `the_window_keeps_the_selection_with_context` and `window_at_the_ends`.
  - `the_thumb_is_proportional`: thumb length `max(1, round(height * height / len))`, start
    `round(window.start * height / len)`, clamped inside the rail;
    `scrollbar(7..20, 48, 13) == Some(2..6)`, and `None` when `len <= height`.
  - `the_counter_reads_selection_over_total`: `3/4`, `22/48`.
  - `a_wide_name_keeps_the_duration_column` (Review Focus 1): ten `国` chars at width 30.
- [ ] **Step 2: Run** `cargo test -p alba-tui beams`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same. Expected: PASS.
- [ ] **Step 5: Commit** (`✨ feat(tui): ...`).

### Task 11: Output pane

**Files:**
- Modify: `crates/alba-tui/src/ui/logpane.rs` (rename to `output.rs`)
- Test: unit tests in `output.rs`

**Interfaces:**
- Consumes: `LineKind`, `StreamFilter`, `details` (Task 8), `scrollbar` (Task 10), `remap_ansi`
  (Task 7).
- Produces:
  - Title row: `OUTPUT / beam`, `EXECUTOR <kind> <detail>`, right state chosen by
    `fn pane_state(state) -> PaneState` with `PaneState::{Following, Paused, EndOfOutput,
    Match{current, total}, Copy{lines}, Filtered(StreamFilter)}`; narrow priority: executor dropped,
    then name cut with `…`, state never cut.
  - Definition row (description muted left, location muted right), dropped under 16 rows.
  - Line drawing per `LineKind`: `▎` in `line` before stderr output; `$ command` echo; notice line
    `── text ───` (`text-muted` for `Info`, bold `error` for `Error`); end line
    `── glyph text ───` in its tone; continuation rows of a wrapped Alba line carry no dashes.
  - Scrollbar in the last column when the output overflows; position badge
    ` <current>/<total> ` (thousands separated by a space) at the bottom right while paused, hidden
    under 20 columns.

- [ ] **Step 1: Write the failing tests**
  - `stderr_lines_carry_the_gutter_marker_and_full_colour`.
  - `alba_lines_span_the_pane_in_their_tone`.
  - `the_title_state_follows_the_pane` (each `PaneState` text from the spec).
  - `a_narrow_title_drops_the_executor_first`.
  - `the_badge_shows_the_position_only_when_paused`: `" 1 240/2 400 "` for line 1240 of 2400.
  - `the_definition_row_hides_under_sixteen_rows`.
  - `a_wrapped_ansi_line_keeps_its_remapped_colour_on_every_row` (Review Focus 4).
  - `a_wide_output_line_does_not_push_the_scrollbar` (Review Focus 1).
- [ ] **Step 2: Run** `cargo test -p alba-tui output`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same. Expected: PASS.
- [ ] **Step 5: Commit** (`✨ feat(tui): ...`).

### Task 12: Diagnostic pane

**Files:**
- Create: `crates/alba-tui/src/ui/diagnostic.rs`; modify `state.rs` (keep the report from
  `ProjectBroken`), `ui/mod.rs`
- Test: unit tests in `diagnostic.rs`

**Interfaces:**
- Consumes: `DiagnosticReport` (Task 1, via `alba_core`), `ProjectBroken.report` (Task 6).
- Produces: `AppState.diagnostic: Option<DiagnosticReport>`; the pane draws the title
  `DIAGNOSTIC / path:line:column`, `error:` line, snippet with gutter and `│`, faulty span bold in
  `error`, caret row `^` under it with the gutter bar continued, `help:` line. Without a report it
  draws the plain string as today.

- [ ] **Step 1: Write the failing tests** `the_caret_sits_under_the_faulty_span` (caret starts at
  the column of `tset` in the drawn snippet) and `without_a_report_the_text_is_shown`.
- [ ] **Step 2: Run** `cargo test -p alba-tui diagnostic`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same. Expected: PASS.
- [ ] **Step 5: Commit** (`✨ feat(tui): ...`).

### Task 13: Graph view

**Files:**
- Modify: `crates/alba-tui/src/ui/graphpane.rs`, `crates/alba-tui/src/ui/mod.rs` (the beams pane
  stays visible in graph mode)
- Test: unit tests in `graphpane.rs`

**Interfaces:**
- Consumes: `details` (Task 8), `graph::layers`, `graph::slot_center_fraction` (layout unchanged).
- Produces: title `GRAPH / target` with `N beams · M layers`; nodes `[ glyph name ]` bold in status
  colours, focused node on `accent`; edges on the path from the focused beam to the target in
  `accent`, others in `line`; `COMMANDS / beam` section with `NEEDED BY`, location, description and
  `$ command` lines, dropped when fewer than five rows remain under the DAG; vertical scroll keeps
  the focused node visible.

- [ ] **Step 1: Write the failing tests** `the_focused_path_is_accent`,
  `the_commands_section_lists_the_focused_beam`, `commands_drop_before_the_graph_scrolls`.
- [ ] **Step 2: Run** `cargo test -p alba-tui graphpane`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same. Expected: PASS.
- [ ] **Step 5: Commit** (`✨ feat(tui): ...`).

### Task 14: Help page, search and copy

**Files:**
- Modify: `crates/alba-tui/src/ui/help.rs` (page instead of overlay), `crates/alba-tui/src/ui/output.rs`
  (match and selection styling), `crates/alba-tui/src/ui/keys.rs` (search prompt, copy and help rows)
- Test: unit tests in those files

**Interfaces:**
- Produces: the help page content verbatim from the spec (four groups, key column 12 cells, last
  muted line); `HELP_KEYS` becomes `Esc close help`; search prompt row; current match
  `on-accent` bold on `accent`, other matches on `selection`; copy selection on `selection` with the
  cursor cell on `accent`; title `COPY · n lines`.
- Keep `COPY_KEYS`/`GRAPH_KEYS` as the shared constants for the footer and the help page.

- [ ] **Step 1: Write the failing tests** `the_help_page_lists_every_binding_once`,
  `the_current_match_stands_out`, `the_copy_title_counts_selected_lines`.
- [ ] **Step 2: Run** `cargo test -p alba-tui help output keys`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same. Expected: PASS.
- [ ] **Step 5: Commit** (`✨ feat(tui): ...`).

### Task 15: Snapshot suite against the mockups

**Files:**
- Modify: `crates/alba-tui/tests/render.rs`, `crates/alba-tui/tests/snapshots/`
- Modify (if they assert old text): `crates/alba/tests/tui_pty.rs`, `crates/alba/tests/cli_ui.rs`

**Interfaces:**
- Consumes: everything above.
- New snapshot tests, one per artboard, at 104x23 unless stated: `run_live_output`,
  `graph_dependencies`, `watch_parked_on_diagnostic`, `run_passed`, `run_failed`,
  `watch_waiting_for_changes`, `run_scrolling_48_beams`, `search_matches_in_output`,
  `copy_line_selection`, `help_keyboard_page`, `terminal_too_small` (36x10),
  `output_stderr_only`, plus the existing floor tests at 40x12 and a cancelled run.

- [ ] **Step 1: Write the new tests** building each state through `AppState::apply` with the same
  beams, outputs and durations as the artboard.
- [ ] **Step 2: Run** `cargo insta test -p alba-tui` and review every snapshot against its
  artboard column by column. Reject any mismatch and fix the drawing, not the snapshot.
- [ ] **Step 3: Accept** with `cargo insta accept` once each one matches; delete obsolete snapshots.
- [ ] **Step 4: Run the full gate** `alba check` in the background. Expected: PASS.
- [ ] **Step 5: Commit** (`✅ test(tui): ...`).
