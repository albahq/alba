# Alba TUI Polish Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the interactive interface read like a finished tool: a status colour theme, coloured tool output rendered from ANSI, wrapped log lines, a selection that jumps to the first failure, dimmed stderr, truncated beam names, and a lighter bottom bar.

**Architecture:** The engine gains one field (`RunOptions.extra_env`) that reaches the executors but never the cache fingerprint. Everything else lives in `alba-tui`: `LogLine` stores each output line three ways (raw bytes, plain text, styled spans), `LogBuffer::view` returns wrapped visual rows that name the buffer line and char range they show, and a small `ui/theme.rs` is the one place a status becomes a colour. The CLI passes a colour flag in, sets the forcing variables on the TUI path only, and picks raw or plain text for the exit replay.

**Tech Stack:** Rust 2024, ratatui 0.29, crossterm 0.28, `ansi-to-tui` 7 (new: SGR parsing into ratatui spans; version 8 targets ratatui 0.30 and does not build here), `unicode-width` 0.2 (new: rendered char widths, already in the dependency tree through ratatui), insta snapshots, `portable-pty` (already a dev-dependency of `alba-cli`).

**Spec:** `.claude/superpowers/specs/2026-08-26-alba-tui-polish-design.md`

## Global Constraints

- Every file, comment, and commit message is English. No em-dashes in anything you write (the existing code uses them; leave those alone, do not add new ones).
- Commit messages: gitmoji + Conventional Commits, `<emoji> <type>(<scope>): <summary>`. No `Co-Authored-By`, no tool attribution.
- The gate before every commit: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace`. A `pre-commit` hook runs `cargo fmt --check` on its own; `cargo fmt` first if it fails.
- Colour gate: `stdout` is a terminal and `NO_COLOR` is unset (`alba_cli::color_enabled`). With colour off the interface must render exactly as it does today.
- Forced colour: `FORCE_COLOR=1` and `CLICOLOR_FORCE=1`, only on the TUI path with colour enabled, applied before the Beamfile's own `env` so a beam's own value wins.
- The cache fingerprint (`CacheableBeam.env` in `crates/alba-engine/src/scheduler.rs`) never sees `extra_env`.
- Wrapping is by character, using rendered width. The scroll offset stays in logical lines. Search and copy stay in `(line, column)` coordinates over the plain `text`.
- The tree stays 30 columns wide (`TREE_WIDTH` in `crates/alba-tui/src/ui/mod.rs`).
- Normal-mode bottom bar text: `q quit · r rerun · c cancel · w watch · / search · ? help`.
- Snapshot tests: after an intended change to rendered text, run `INSTA_UPDATE=always cargo test -p alba-tui --test render`, then read the diff of `crates/alba-tui/tests/snapshots/` and confirm only the intended cells changed before committing.

---

## File map

| File | Responsibility after this plan |
|------|-------------------------------|
| `crates/alba-engine/src/scheduler.rs` | `RunOptions.extra_env`, merged into `CommandSpec.env` in `run_commands`, absent from `CacheableBeam` |
| `crates/alba-engine/tests/scheduler.rs`, `tests/cache.rs` | `extra_env` reaches the executor; the fingerprint ignores it |
| `crates/alba-tui/src/logs.rs` | `LogLine { raw, text, spans, stream, replayed }`, ANSI parsing at `push`, `Row`, wrapped `view(height, width)`, row-based scrolling |
| `crates/alba-tui/src/copy.rs` | `row_at`, `scroll_window`, `top_visible_line` over wrapped rows |
| `crates/alba-tui/src/state.rs` | `colour`, `pane_width`, the failure-jump flags, callers of the new `push`/`view` |
| `crates/alba-tui/src/ui/theme.rs` (new) | `status_style`, `outcome_style`, `key_style`, `bar_line` |
| `crates/alba-tui/src/ui/logpane.rs` | Renders `Row` spans, patches search and copy highlights on top, dims stderr |
| `crates/alba-tui/src/ui/tree.rs` | Coloured glyphs, truncated names, coloured counts |
| `crates/alba-tui/src/ui/header.rs` | Returns a styled `Line` (green bar, outcome colour) |
| `crates/alba-tui/src/ui/graphpane.rs` | Nodes in their status colour |
| `crates/alba-tui/src/ui/mod.rs` | Lighter Normal bar, bold keys, styled header title |
| `crates/alba-tui/src/lib.rs` | `TuiOptions.colour`, `ReplayLine` in `TuiOutcome`, pane width to the state, row-based scroll actions |
| `crates/alba-cli/src/commands/run.rs` | Passes colour, sets `extra_env`, replays raw or plain |
| `crates/alba-cli/tests/tui_pty.rs` | A coloured failing beam replayed raw, and plain under `NO_COLOR` |
| `README.md` | Colour paragraph, layout and keymap updates |

---

### Task 1: `RunOptions.extra_env` reaches the executors, not the cache

**Files:**
- Modify: `crates/alba-engine/src/scheduler.rs` (`RunOptions` at line 69, `BeamTask` at line 531, `run` at line 265, `run_commands` at line 1035)
- Modify: every `RunOptions { ... }` literal: `crates/alba-engine/tests/scheduler.rs:47`, `tests/cache.rs:52`, `tests/perf.rs:28`, `tests/watch.rs:204` and `:1113`, `crates/alba-cli/src/commands/run.rs:169`, `:273`, `:377`
- Test: `crates/alba-engine/tests/scheduler.rs`, `crates/alba-engine/tests/cache.rs`

**Interfaces:**
- Produces: `pub extra_env: Vec<(String, String)>` on `alba_engine::RunOptions`. Task 7 sets it from the CLI.

- [ ] **Step 1: Write the failing tests**

Append to `crates/alba-engine/tests/scheduler.rs`:

```rust
/// `extra_env` is the session's ambient environment (the TUI forcing
/// colour): it reaches every command, and a beam's own `env` wins on a
/// name both set.
#[tokio::test]
async fn extra_env_reaches_the_executor_and_the_beam_env_wins() {
    let source = r#"
beam plain {
  run "echo plain"
}

beam own {
  env { FORCE_COLOR = "0" }
  needs [plain]
  run "echo own"
}
"#;
    let executor = Arc::new(FakeExecutor::new());
    let mut options = options(1, false);
    options.extra_env = vec![
        ("FORCE_COLOR".to_string(), "1".to_string()),
        ("CLICOLOR_FORCE".to_string(), "1".to_string()),
    ];
    let outcome = run_target(source, "own", options, executor.clone()).await;
    outcome.summary();

    let calls = executor.calls();
    let env_of = |command: &str| -> Vec<(String, String)> {
        calls
            .iter()
            .find(|call| call.command == command)
            .unwrap_or_else(|| panic!("no call for {command}"))
            .env
            .clone()
    };
    assert_eq!(
        env_of("echo plain"),
        vec![
            ("FORCE_COLOR".to_string(), "1".to_string()),
            ("CLICOLOR_FORCE".to_string(), "1".to_string()),
        ]
    );
    let own = env_of("echo own");
    assert_eq!(
        own.iter().filter(|(name, _)| name == "FORCE_COLOR").count(),
        1,
        "one entry per name, never a duplicate the executor would have to arbitrate: {own:?}"
    );
    assert!(
        own.contains(&("FORCE_COLOR".to_string(), "0".to_string())),
        "the beam's own value wins: {own:?}"
    );
    assert!(own.contains(&("CLICOLOR_FORCE".to_string(), "1".to_string())));
}
```

Check the `env { ... }` syntax against an existing test in the same file (`grep -n "env {" crates/alba-engine/tests/scheduler.rs`); if the DSL spells it differently, use that spelling.

Append to `crates/alba-engine/tests/cache.rs`:

```rust
/// The TUI and a pipe run the same beam with different `extra_env`; the
/// second must still be a hit, or switching front ends would rebuild the
/// world.
#[tokio::test]
async fn extra_env_does_not_change_the_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");

    let first = run_once(GEN, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;
    assert_eq!(first.executed(), vec!["generate"]);

    let mut forced = options(dir.path(), false);
    forced.extra_env = vec![("FORCE_COLOR".to_string(), "1".to_string())];
    let second = run_once(GEN, "gen", dir.path(), forced, FakeExecutor::new()).await;
    assert_eq!(second.executed(), Vec::<String>::new(), "a hit runs nothing");
    assert_eq!(ids(&second.summary.cached), vec!["gen"]);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alba-engine --test scheduler extra_env --test cache extra_env`
Expected: compile error, `no field extra_env on type RunOptions`.

- [ ] **Step 3: Add the field and thread it to the executor**

In `crates/alba-engine/src/scheduler.rs`, `RunOptions`:

```rust
    /// Ambient variables the caller adds to every command of the run, on
    /// top of the process environment and under the beam's own `env`
    /// (a name both set keeps the beam's value). Reaches the executors
    /// only, never the cache fingerprint: the interactive front end uses
    /// it to force colour, and a beam must hit the same cache entry
    /// whether a terminal or a pipe ran it.
    pub extra_env: Vec<(String, String)>,
```

`BeamTask` gains `extra_env: Vec<(String, String)>`; in `run`, next to `keep_going: options.keep_going,` add `extra_env: options.extra_env.clone(),`.

In `run_commands`, replace `env: plan.env.clone(),` with `env: command_env(&task.extra_env, &plan.env),` and add near `RenderedBeam`:

```rust
/// The environment a command receives: the run's ambient `extra_env`
/// first, then the beam's own `env`, with a name both set kept once
/// with the beam's value. One entry per name, so no executor has to
/// decide which duplicate wins (docker's repeated `-e` would take the
/// last, the embedded shell overwrites in place: same answer, but only
/// because nothing is duplicated here).
fn command_env(extra: &[(String, String)], beam: &[(String, String)]) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = extra
        .iter()
        .filter(|(name, _)| !beam.iter().any(|(own, _)| own == name))
        .cloned()
        .collect();
    env.extend(beam.iter().cloned());
    env
}
```

Add `extra_env: Vec::new(),` to every `RunOptions { ... }` literal listed under **Files** (`grep -rn "RunOptions {" crates | grep -v "pub struct"` finds them all).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p alba-engine --test scheduler extra_env --test cache extra_env`
Expected: 2 passed.

- [ ] **Step 5: Gate and commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace
git add crates/alba-engine crates/alba-cli/src/commands/run.rs
git commit -m "✨ feat(engine): add an ambient extra_env the cache fingerprint ignores"
```

---

### Task 2: `LogLine` keeps raw, plain, and styled forms; ANSI parsed at `push`

**Files:**
- Modify: `Cargo.toml` (workspace dependencies), `crates/alba-tui/Cargo.toml`
- Modify: `crates/alba-tui/src/logs.rs`
- Modify: `crates/alba-tui/src/state.rs` (`BeamOutput` and `ProjectBroken` arms of `apply`, around lines 258 and 331; every `push(` in its tests)
- Modify: `crates/alba-tui/src/copy.rs` (`line_text` and `line_for_pane_row`, unchanged semantics)
- Modify: `crates/alba-tui/src/ui/logpane.rs`
- Modify: `crates/alba-tui/src/lib.rs` (`TuiOutcome.failed_logs`, `buffered_lines`)
- Modify: `crates/alba-cli/src/commands/run.rs` (`replay`, lines 545 to 556; its tests build `failed_logs`)
- Modify: `crates/alba-tui/tests/render.rs` (compiles unchanged unless it calls `push` directly)

**Interfaces:**
- Produces:
  - `pub struct LogLine { pub raw: String, pub text: String, pub spans: Vec<(Style, String)>, pub stream: Stream, pub replayed: bool }`
  - `LogBuffer::push(&mut self, raw: impl Into<String>, stream: Stream, replayed: bool)`
  - `pub struct Row { pub line: Option<usize>, pub chars: Range<usize>, pub spans: Vec<(Style, String)>, pub text: String }` and `LogBuffer::view(&self, height: usize) -> Vec<Row>` (Task 3 adds `width`)
  - `pub struct ReplayLine { pub raw: String, pub text: String }` in `alba_tui`; `TuiOutcome.failed_logs: Vec<(String, Vec<ReplayLine>)>`
  - `logpane::patch(spans: Vec<Span<'static>>, from: usize, to: usize, style: Style) -> Vec<Span<'static>>` (private to `logpane`, used again by Task 4)

- [ ] **Step 1: Add the dependencies**

In `Cargo.toml` under `[workspace.dependencies]`, keep alphabetical order:

```toml
ansi-to-tui = "7"
```

In `crates/alba-tui/Cargo.toml`, `[dependencies]` gains `alba-executors = { path = "../alba-executors" }` and `ansi-to-tui.workspace = true`; remove `alba-executors` from `[dev-dependencies]` (it is now a regular dependency). Run `cargo build -p alba-tui` to fetch it.

- [ ] **Step 2: Write the failing tests in `logs.rs`**

Replace the `filled` helper and add the tests below inside `mod tests` of `crates/alba-tui/src/logs.rs`:

```rust
    use alba_executors::Stream;
    use ratatui::style::{Color, Modifier, Style};

    fn filled(count: usize) -> LogBuffer {
        let mut buffer = LogBuffer::new();
        for index in 0..count {
            buffer.push(format!("line {index}"), Stream::Stdout, false);
        }
        buffer
    }

    fn texts(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|row| row.text.as_str()).collect()
    }

    /// SGR colour becomes spans; the plain text loses every escape.
    #[test]
    fn a_coloured_line_is_split_into_styled_spans_and_plain_text() {
        let mut buffer = LogBuffer::new();
        buffer.push("\u{1b}[1;31merror\u{1b}[0m: it broke", Stream::Stderr, false);
        let line = buffer.lines().next().unwrap();
        assert_eq!(line.raw, "\u{1b}[1;31merror\u{1b}[0m: it broke");
        assert_eq!(line.text, "error: it broke");
        assert_eq!(line.stream, Stream::Stderr);
        assert_eq!(
            line.spans,
            vec![
                (
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    "error".to_string()
                ),
                (Style::default(), ": it broke".to_string()),
            ]
        );
    }

    /// Cursor movement, erase, and OSC sequences carry no text: dropped,
    /// never shown raw.
    #[test]
    fn non_sgr_sequences_are_dropped() {
        let mut buffer = LogBuffer::new();
        buffer.push(
            "\u{1b}[2K\u{1b}]0;title\u{7}hello \u{1b}[1Aworld\u{1b}]8;;http://x\u{1b}\\",
            Stream::Stdout,
            false,
        );
        let line = buffer.lines().next().unwrap();
        assert_eq!(line.text, "hello world");
        assert_eq!(line.spans, vec![(Style::default(), "hello world".to_string())]);
    }

    /// A line with no escapes at all is one default span.
    #[test]
    fn a_plain_line_is_one_default_span() {
        let mut buffer = LogBuffer::new();
        buffer.push("plain", Stream::Stdout, true);
        let line = buffer.lines().next().unwrap();
        assert_eq!(line.spans, vec![(Style::default(), "plain".to_string())]);
        assert!(line.replayed);
    }

    /// The view carries the spans and names the line each row shows.
    #[test]
    fn view_rows_name_their_line_and_carry_spans() {
        let mut buffer = LogBuffer::new();
        buffer.push("a", Stream::Stdout, false);
        buffer.push("\u{1b}[32mb\u{1b}[0m", Stream::Stdout, false);
        let rows = buffer.view(5);
        assert_eq!(rows[0].line, Some(0));
        assert_eq!(rows[0].chars, 0..1);
        assert_eq!(rows[1].line, Some(1));
        assert_eq!(rows[1].text, "b");
        assert_eq!(
            rows[1].spans,
            vec![(Style::default().fg(Color::Green), "b".to_string())]
        );
    }

    /// The marker row names no line.
    #[test]
    fn the_marker_row_names_no_line() {
        let mut buffer = filled(MAX_LINES + 5);
        buffer.scroll_up(MAX_LINES);
        let rows = buffer.view(4);
        assert_eq!(rows[0].line, None);
        assert_eq!(rows[0].text, "… 5 older lines truncated");
        assert_eq!(rows[1].line, Some(0));
        assert_eq!(rows[1].text, "line 5");
    }
```

Update every existing test in that module: `buffer.push(X, false)` becomes `buffer.push(X, Stream::Stdout, false)`, and every `assert_eq!(buffer.view(n), vec![...])` compares `texts(&buffer.view(n))` against `vec![...]` (a `Vec<&str>`); `view[0]` comparisons become `view[0].text`. `a_zero_height_view_shows_nothing` asserts `buffer.view(0).is_empty()`.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p alba-tui logs::`
Expected: compile errors (`push` takes 2 arguments, no `Row`, no `raw` field).

- [ ] **Step 4: Implement `LogLine`, parsing, and `Row`**

Replace the top of `crates/alba-tui/src/logs.rs` (imports, `LogLine`, `push`, `view`) with:

```rust
use std::collections::VecDeque;
use std::ops::Range;

use alba_executors::Stream;
use ansi_to_tui::IntoText;
use ratatui::style::Style;

/// The memory bound a day-long watch session relies on.
pub const MAX_LINES: usize = 10_000;

/// One output line, kept three ways: `raw` as the command wrote it
/// (the exit replay prints it into a terminal), `text` with every
/// escape sequence removed (search, copy, wrapping widths, and the
/// `NO_COLOR` replay read this), and `spans` for the renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub raw: String,
    pub text: String,
    pub spans: Vec<(Style, String)>,
    pub stream: Stream,
    pub replayed: bool,
}

impl LogLine {
    fn new(raw: String, stream: Stream, replayed: bool) -> Self {
        let (text, spans) = parse(&raw);
        Self {
            raw,
            text,
            spans,
            stream,
            replayed,
        }
    }
}

/// One rendered row of the log pane: which buffer line it shows (`None`
/// for the truncation marker), which char range of that line, and that
/// slice's own text and spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub line: Option<usize>,
    pub chars: Range<usize>,
    pub spans: Vec<(Style, String)>,
    pub text: String,
}

impl Row {
    fn marker(truncated: usize) -> Self {
        let text = format!("… {truncated} older lines truncated");
        Self {
            line: None,
            chars: 0..0,
            spans: vec![(Style::default(), text.clone())],
            text,
        }
    }

    fn whole(index: usize, line: &LogLine) -> Self {
        Self {
            line: Some(index),
            chars: 0..line.text.chars().count(),
            spans: line.spans.clone(),
            text: line.text.clone(),
        }
    }
}

/// Splits `raw` into plain text and styled spans. Only SGR sequences
/// reach `ansi-to-tui`; everything else an escape can start (cursor
/// movement, erase, OSC titles and hyperlinks) is removed first, so
/// the parser's answer is deterministic and nothing ever shows raw.
fn parse(raw: &str) -> (String, Vec<(Style, String)>) {
    let sgr_only = keep_only_sgr(raw);
    let spans: Vec<(Style, String)> = match sgr_only.as_bytes().into_text() {
        Ok(text) => text
            .lines
            .into_iter()
            .flat_map(|line| line.spans)
            .filter(|span| !span.content.is_empty())
            .map(|span| (span.style, span.content.into_owned()))
            .collect(),
        Err(_) => vec![(Style::default(), sgr_only.replace('\u{1b}', ""))],
    };
    let spans = if spans.is_empty() {
        vec![(Style::default(), String::new())]
    } else {
        spans
    };
    let text = spans.iter().map(|(_, content)| content.as_str()).collect();
    (text, spans)
}

/// Removes every escape sequence that is not an SGR (`ESC [ ... m`):
/// OSC (`ESC ] ... BEL` or `ESC ] ... ESC \`) and every other CSI
/// sequence (a final byte in `@..=~` other than `m`). A lone `ESC`
/// followed by anything else is dropped together with that character.
fn keep_only_sgr(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                let mut sequence = String::from("\u{1b}[");
                let mut final_byte = None;
                for c in chars.by_ref() {
                    sequence.push(c);
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        final_byte = Some(c);
                        break;
                    }
                }
                if final_byte == Some('m') {
                    out.push_str(&sequence);
                }
            }
            Some(']') => {
                let mut previous = '\0';
                for c in chars.by_ref() {
                    if c == '\u{7}' || (previous == '\u{1b}' && c == '\\') {
                        break;
                    }
                    previous = c;
                }
            }
            _ => {}
        }
    }
    out
}
```

`push` becomes:

```rust
    pub fn push(&mut self, raw: impl Into<String>, stream: Stream, replayed: bool) {
        self.lines.push_back(LogLine::new(raw.into(), stream, replayed));
        // ... the rest unchanged (cap, truncated, paused offset)
```

`view` becomes (same window arithmetic, rows instead of strings):

```rust
    pub fn view(&self, height: usize) -> Vec<Row> {
        let len = self.lines.len();
        let offset = match self.scroll {
            Scroll::Following => 0,
            Scroll::Paused { offset } => offset,
        };
        let end = len.saturating_sub(offset);
        let start = end.saturating_sub(height);

        if start == 0 && self.truncated > 0 {
            let real_count = height.saturating_sub(1).min(end);
            std::iter::once(Row::marker(self.truncated))
                .chain(
                    self.lines
                        .iter()
                        .take(real_count)
                        .enumerate()
                        .map(|(index, line)| Row::whole(index, line)),
                )
                .collect()
        } else {
            self.lines
                .iter()
                .enumerate()
                .skip(start)
                .take(end - start)
                .map(|(index, line)| Row::whole(index, line))
                .collect()
        }
    }
```

Update the module doc comment's first paragraph to mention the three forms.

- [ ] **Step 5: Fix the callers**

`crates/alba-tui/src/state.rs`, `BeamOutput` arm:

```rust
                self.logs
                    .entry(id.0.clone())
                    .or_default()
                    .push(line.text.as_str(), line.stream, *replayed);
```

`ProjectBroken` arm: `buffer.push(line, alba_executors::Stream::Stdout, false);`

In the tests of `state.rs`, `lib.rs`, and `crates/alba-tui/tests/render.rs`, every direct `push(X, bool)` call gains `alba_executors::Stream::Stdout` as the middle argument (`grep -rn "\.push(" crates/alba-tui/src crates/alba-tui/tests` and fix the `LogBuffer` ones).

`crates/alba-tui/src/copy.rs`: `line_text` is unchanged (`line.text`). Nothing else changes in this task.

`crates/alba-tui/src/lib.rs`:

```rust
/// One replayed line, both ways the CLI may print it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayLine {
    pub raw: String,
    pub text: String,
}

#[derive(Debug)]
pub struct TuiOutcome {
    pub last_run_code: Option<i32>,
    pub last_summary: Option<RunSummary>,
    /// `(beam id, its buffered lines)` for the last summary's failed
    /// beams, for the CLI's exit replay.
    pub failed_logs: Vec<(String, Vec<ReplayLine>)>,
}

fn buffered_lines(state: &AppState, beam: &str) -> Vec<ReplayLine> {
    state
        .logs
        .get(beam)
        .map(|buffer| {
            buffer
                .lines()
                .map(|line| ReplayLine {
                    raw: line.raw.clone(),
                    text: line.text.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}
```

`crates/alba-cli/src/commands/run.rs`, `replay`: `err.line(line);` becomes `err.line(&line.text);` (Task 7 chooses raw or text). Its tests that build `failed_logs` with `Vec<String>` wrap each string: `alba_tui::ReplayLine { raw: s.clone(), text: s }`.

`crates/alba-tui/src/ui/logpane.rs`: `draw` maps rows, and `styled_line` takes a `&Row`:

```rust
    let rows_shown = buffer.map(|buffer| buffer.view(body_height)).unwrap_or_default();
    let lines: Vec<Line> = rows_shown
        .iter()
        .map(|row| styled_line(row, buffer, &state.mode, query))
        .collect();
```

```rust
fn styled_line(row: &Row, buffer: Option<&LogBuffer>, mode: &Mode, query: Option<&str>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = row
        .spans
        .iter()
        .map(|(style, content)| Span::styled(content.clone(), *style))
        .collect();
    if let (Mode::Copy(selection), Some(line)) = (mode, row.line) {
        let covered = selection.covers_line(line, row.text.chars().count());
        if let Some((from, to)) = covered {
            return Line::from(patch(spans, from, to, Style::new().reversed()));
        }
    }
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        for (from, to) in match_ranges(&row.text, query) {
            spans = patch(spans, from, to, Style::new().reversed());
        }
    }
    let _ = buffer;
    Line::from(spans)
}

/// Every case-insensitive occurrence of `query` in `text`, as inclusive
/// char ranges. Folds case with `to_ascii_lowercase`, which never
/// changes a string's byte length, so byte positions found in the
/// folded copy convert to char indices of `text` itself.
fn match_ranges(text: &str, query: &str) -> Vec<(usize, usize)> {
    let haystack = text.to_ascii_lowercase();
    let needle = query.to_ascii_lowercase();
    let mut ranges = Vec::new();
    let mut pos = 0;
    while let Some(found) = haystack[pos..].find(&needle) {
        let start = pos + found;
        let end = start + needle.len();
        let from = text[..start].chars().count();
        let to = from + text[start..end].chars().count() - 1;
        ranges.push((from, to));
        pos = end;
    }
    ranges
}

/// `spans` with `style` patched over the inclusive char range
/// `[from, to]`, splitting whichever spans the range crosses. Styles
/// already on the spans stay underneath (`Style::patch`), so a search
/// highlight over a red `error` keeps it red and reversed.
fn patch(spans: Vec<Span<'static>>, from: usize, to: usize, style: Style) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(spans.len() + 2);
    let mut index = 0;
    for span in spans {
        let chars: Vec<char> = span.content.chars().collect();
        let len = chars.len();
        let (start, end) = (index, index + len);
        index = end;
        if len == 0 || to < start || from >= end {
            out.push(span);
            continue;
        }
        let cut_from = from.max(start) - start;
        let cut_to = (to + 1).min(end) - start;
        if cut_from > 0 {
            out.push(Span::styled(chars[..cut_from].iter().collect::<String>(), span.style));
        }
        out.push(Span::styled(
            chars[cut_from..cut_to].iter().collect::<String>(),
            span.style.patch(style),
        ));
        if cut_to < len {
            out.push(Span::styled(chars[cut_to..].iter().collect::<String>(), span.style));
        }
    }
    out
}
```

Remove `highlighted_line` and `copy_selected_line`; their existing unit tests in the same file become tests of `patch` and `match_ranges` with the same inputs (a highlight in the middle of a plain line yields three spans, the middle one reversed; a multi-byte line is never split mid-char). Drop the `buffer` parameter from `styled_line` entirely if nothing needs it after the rewrite (the `let _ = buffer;` line is a reminder to remove it, not to keep).

Add one test in `logpane.rs`:

```rust
    /// A search highlight on a coloured span keeps the colour underneath.
    #[test]
    fn patch_keeps_the_underlying_colour() {
        use ratatui::style::Color;
        let spans = vec![Span::styled("error here", Style::new().fg(Color::Red))];
        let patched = patch(spans, 0, 4, Style::new().reversed());
        assert_eq!(patched.len(), 2);
        assert_eq!(patched[0].content, "error");
        assert_eq!(patched[0].style, Style::new().fg(Color::Red).reversed());
        assert_eq!(patched[1].content, " here");
        assert_eq!(patched[1].style, Style::new().fg(Color::Red));
    }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p alba-tui && cargo test -p alba-cli replay`
Expected: all pass, snapshots unchanged (plain text renders the same).

If `non_sgr_sequences_are_dropped` fails on `\u{1b}[1A` or the OSC, the bug is in `keep_only_sgr`, not in `ansi-to-tui`: fix it there.

- [ ] **Step 7: Gate and commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace
git add Cargo.toml Cargo.lock crates/alba-tui crates/alba-cli/src/commands/run.rs
git commit -m "✨ feat(tui): parse ANSI output into styled spans and keep the raw line"
```

---

### Task 3: Wrapped rows in the log pane

**Files:**
- Modify: `Cargo.toml`, `crates/alba-tui/Cargo.toml` (`unicode-width = "0.2"`)
- Modify: `crates/alba-tui/src/logs.rs` (`view(height, width)`, `scroll_up_rows`, `scroll_down_rows`)
- Modify: `crates/alba-tui/src/copy.rs` (`scroll_window`, `top_visible_line`, `row_at` replacing `line_for_pane_row`)
- Modify: `crates/alba-tui/src/state.rs` (`pane_width`, `set_pane_size`, `enter_copy`, `sync_copy_scroll`, `handle_mouse`)
- Modify: `crates/alba-tui/src/lib.rs` (`dispatch`: pane size, row-based scroll actions)
- Modify: `crates/alba-tui/src/ui/logpane.rs` (pass the width, clip highlight ranges to the row)
- Test: `crates/alba-tui/src/logs.rs`, `crates/alba-tui/src/copy.rs`, `crates/alba-tui/tests/render.rs`

**Interfaces:**
- Consumes: `Row`, `LogBuffer::view` from Task 2.
- Produces:
  - `LogBuffer::view(&self, height: usize, width: usize) -> Vec<Row>`
  - `LogBuffer::scroll_up_rows(&mut self, rows: usize, width: usize)` and `scroll_down_rows`
  - `copy::scroll_window(buffer, height, width) -> (usize, usize)`, `copy::top_visible_line(buffer, height, width) -> usize`, `copy::row_at(buffer, height, width, pane_row) -> Option<(usize, usize)>` (line, first char of the row)
  - `AppState::set_pane_size(&mut self, height: usize, width: usize)` (replaces `set_pane_height`)
  - `AppState::handle_mouse(kind, height, width, pane_row, pane_col)`

- [ ] **Step 1: Write the failing tests**

In `crates/alba-tui/src/logs.rs` tests, change every `view(n)` call to `view(n, 80)`, then add:

```rust
    /// A line wider than the pane wraps by character; the rows name the
    /// same line and consecutive char ranges.
    #[test]
    fn a_long_line_wraps_into_consecutive_rows() {
        let mut buffer = LogBuffer::new();
        buffer.push("abcdefghij", Stream::Stdout, false);
        let rows = buffer.view(5, 4);
        assert_eq!(texts(&rows), vec!["abcd", "efgh", "ij"]);
        assert_eq!(rows[1].line, Some(0));
        assert_eq!(rows[1].chars, 4..8);
        assert_eq!(rows[2].chars, 8..10);
    }

    /// A double-width char counts two cells, so it never straddles a row.
    #[test]
    fn wrapping_counts_rendered_width() {
        let mut buffer = LogBuffer::new();
        buffer.push("ab⚡cd", Stream::Stdout, false);
        assert_eq!(texts(&buffer.view(5, 3)), vec!["ab", "⚡c", "d"]);
    }

    /// Following shows the last `height` rows, cutting the top line if
    /// it does not fit whole.
    #[test]
    fn following_shows_the_last_rows_even_mid_line() {
        let mut buffer = LogBuffer::new();
        buffer.push("aaaaaaaa", Stream::Stdout, false);
        buffer.push("bb", Stream::Stdout, false);
        let rows = buffer.view(2, 4);
        assert_eq!(texts(&rows), vec!["aaaa", "bb"]);
        assert_eq!(rows[0].chars, 4..8);
    }

    /// The marker is never wrapped and still displaces the bottom row.
    #[test]
    fn the_marker_stays_one_row_at_the_top() {
        let mut buffer = filled(MAX_LINES + 5);
        buffer.scroll_up(MAX_LINES);
        let rows = buffer.view(3, 6);
        assert_eq!(rows[0].line, None);
        assert_eq!(texts(&rows)[1..], ["line 5", "line 6"]);
    }

    /// The offset is in lines: the same line stays pinned at two widths.
    #[test]
    fn a_paused_offset_pins_the_same_line_at_any_width() {
        let mut buffer = filled(100);
        buffer.scroll_up(10);
        let narrow = buffer.view(3, 4);
        let wide = buffer.view(3, 80);
        assert_eq!(narrow.last().unwrap().line, wide.last().unwrap().line);
        assert_eq!(wide.last().unwrap().text, "line 89");
    }

    /// An empty line is one empty row; width 0 never wraps.
    #[test]
    fn empty_lines_and_zero_width_are_one_row() {
        let mut buffer = LogBuffer::new();
        buffer.push("", Stream::Stdout, false);
        buffer.push("abc", Stream::Stdout, false);
        assert_eq!(texts(&buffer.view(5, 2)), vec!["", "ab", "c"]);
        assert_eq!(texts(&buffer.view(5, 0)), vec!["", "abc"]);
    }

    /// Row scrolling moves by whole lines, at least as far as asked.
    #[test]
    fn row_scrolling_rounds_toward_the_direction_of_travel() {
        let mut buffer = LogBuffer::new();
        for _ in 0..5 {
            buffer.push("aaaaaaaa", Stream::Stdout, false); // 2 rows each at width 4
        }
        buffer.scroll_up_rows(3, 4); // 3 rows up: covers 2 lines
        assert!(matches!(buffer.scroll(), Scroll::Paused { offset: 2 }));
        buffer.scroll_down_rows(1, 4); // 1 row down: one whole line
        assert!(matches!(buffer.scroll(), Scroll::Paused { offset: 1 }));
        buffer.scroll_down_rows(1, 4);
        assert!(matches!(buffer.scroll(), Scroll::Following));
    }
```

In `crates/alba-tui/src/copy.rs` tests (find the existing tests of `line_for_pane_row` and `top_visible_line`, rewrite them against `row_at` with a width of 80 so they keep their meaning), then add:

```rust
    /// A wrapped line's second row reports the line and the char it
    /// starts at, so a click on it lands on the right column.
    #[test]
    fn row_at_reports_the_line_and_its_first_char() {
        let mut buffer = LogBuffer::new();
        buffer.push("abcdefgh", Stream::Stdout, false);
        buffer.push("x", Stream::Stdout, false);
        assert_eq!(row_at(&buffer, 5, 4, 0), Some((0, 0)));
        assert_eq!(row_at(&buffer, 5, 4, 1), Some((0, 4)));
        assert_eq!(row_at(&buffer, 5, 4, 2), Some((1, 0)));
        assert_eq!(row_at(&buffer, 5, 4, 3), None);
    }

    /// A selection across a wrap boundary is the same text as unwrapped.
    #[test]
    fn a_selection_across_a_wrap_boundary_reads_the_logical_line() {
        let mut buffer = LogBuffer::new();
        buffer.push("abcdefgh", Stream::Stdout, false);
        let selection = CopyState {
            anchor: (0, 2),
            cursor: (0, 5),
        };
        assert_eq!(selection.selected_text(&buffer), "cdef");
    }

    /// The window is in lines, its top the first line with a visible row.
    #[test]
    fn scroll_window_starts_at_the_first_partly_visible_line() {
        let mut buffer = LogBuffer::new();
        buffer.push("aaaaaaaa", Stream::Stdout, false);
        buffer.push("bb", Stream::Stdout, false);
        assert_eq!(scroll_window(&buffer, 2, 4), (0, 2));
        assert_eq!(top_visible_line(&buffer, 2, 4), 0);
        assert_eq!(scroll_window(&buffer, 1, 4), (1, 2));
    }
```

Add to `crates/alba-tui/tests/render.rs`:

```rust
/// A line wider than the log pane wraps rather than being cut.
#[test]
fn a_long_log_line_wraps_in_the_pane() {
    let mut state = AppState::new("build", false);
    let now = Instant::now();
    state.apply(
        &RunEvent::RunStarted {
            targets: vec![id("build")],
            affected_by: None,
            beams: vec![id("build")],
            edges: Vec::new(),
        },
        now,
    );
    state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
    state.apply(&output("build", &"x".repeat(60)), now);
    state.apply(&output("build", "tail"), now);
    insta::assert_snapshot!(drawn(&state, 80, 24));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alba-tui`
Expected: compile errors (`view` takes 1 argument, no `row_at`, no `scroll_up_rows`).

- [ ] **Step 3: Implement wrapping in `logs.rs`**

Add `unicode-width = "0.2"` to `[workspace.dependencies]` and `unicode-width.workspace = true` to `crates/alba-tui/Cargo.toml`.

In `logs.rs`:

```rust
use unicode_width::UnicodeWidthChar;

/// The char ranges of `text` that fit `width` cells each, by rendered
/// width (a `⚡` counts two). Width 0 means "do not wrap". An empty
/// line is one empty row, so it still occupies a row on screen.
fn wrap_ranges(text: &str, width: usize) -> Vec<Range<usize>> {
    let count = text.chars().count();
    if width == 0 || count == 0 {
        return vec![0..count];
    }
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut used = 0;
    for (index, c) in text.chars().enumerate() {
        let cells = c.width().unwrap_or(0).max(if c == '\t' { 1 } else { 0 });
        if used + cells > width && index > start {
            ranges.push(start..index);
            start = index;
            used = 0;
        }
        used += cells;
    }
    ranges.push(start..count);
    ranges
}

impl Row {
    fn slice(index: usize, line: &LogLine, chars: Range<usize>) -> Self {
        let text: String = line.text.chars().skip(chars.start).take(chars.len()).collect();
        let mut spans = Vec::new();
        let mut at = 0;
        for (style, content) in &line.spans {
            let len = content.chars().count();
            let (span_start, span_end) = (at, at + len);
            at = span_end;
            let from = chars.start.max(span_start);
            let to = chars.end.min(span_end);
            if from < to {
                let piece: String = content.chars().skip(from - span_start).take(to - from).collect();
                spans.push((*style, piece));
            }
        }
        if spans.is_empty() {
            spans.push((Style::default(), String::new()));
        }
        Self {
            line: Some(index),
            chars,
            spans,
            text,
        }
    }
}
```

Replace `view` (and delete `Row::whole`):

```rust
    /// The rows a pane of `height` by `width` shows, honoring the scroll
    /// state: the last `height` rows of the lines up to the tail minus
    /// the paused offset, the top line cut to its last rows when it does
    /// not fit whole. When line 0's first row is in view and lines were
    /// dropped past the cap, the marker `"… N older lines truncated"`
    /// is prepended as a row of its own, displacing the bottom row.
    pub fn view(&self, height: usize, width: usize) -> Vec<Row> {
        if height == 0 {
            return Vec::new();
        }
        let offset = match self.scroll {
            Scroll::Following => 0,
            Scroll::Paused { offset } => offset,
        };
        let end = self.lines.len().saturating_sub(offset);
        let mut rows: VecDeque<Row> = VecDeque::with_capacity(height);
        let mut line = end;
        while line > 0 && rows.len() < height {
            line -= 1;
            let text = &self.lines[line];
            for range in wrap_ranges(&text.text, width).into_iter().rev() {
                if rows.len() == height {
                    break;
                }
                rows.push_front(Row::slice(line, text, range));
            }
        }
        let at_top = rows
            .front()
            .is_some_and(|row| row.line == Some(0) && row.chars.start == 0);
        if at_top && self.truncated > 0 {
            if rows.len() == height {
                rows.pop_back();
            }
            rows.push_front(Row::marker(self.truncated));
        }
        rows.into()
    }

    /// `PgUp`, `Ctrl-u`, the wheel: moves the window up by whole lines
    /// covering at least `rows` rendered rows, so a page never lands
    /// mid-line and always travels at least as far as asked.
    pub fn scroll_up_rows(&mut self, rows: usize, width: usize) {
        let offset = match self.scroll {
            Scroll::Following => 0,
            Scroll::Paused { offset } => offset,
        };
        let end = self.lines.len().saturating_sub(offset);
        let mut covered = 0;
        let mut lines = 0;
        while end > lines && covered < rows {
            covered += wrap_ranges(&self.lines[end - lines - 1].text, width).len();
            lines += 1;
        }
        self.scroll_up(lines.max(1));
    }

    /// The other way; reaching the tail resumes following, as `scroll_down` does.
    pub fn scroll_down_rows(&mut self, rows: usize, width: usize) {
        let Scroll::Paused { offset } = self.scroll else {
            return;
        };
        let end = self.lines.len().saturating_sub(offset);
        let mut covered = 0;
        let mut lines = 0;
        while end + lines < self.lines.len() && covered < rows {
            covered += wrap_ranges(&self.lines[end + lines].text, width).len();
            lines += 1;
        }
        self.scroll_down(lines.max(1));
    }
```

`VecDeque` indexing (`self.lines[line]`) is `Index<usize>` on `VecDeque`; it exists.

- [ ] **Step 4: Rewrite the geometry helpers in `copy.rs`**

Replace `top_visible_line`, `line_for_pane_row`, and `scroll_window`:

```rust
/// The buffer-absolute index of the first line with a row on screen
/// (possibly a partial one: the top line cut to its last rows).
/// Clamped to the buffer's own last line for a zero-height pane.
pub fn top_visible_line(buffer: &LogBuffer, height: usize, width: usize) -> usize {
    scroll_window(buffer, height, width)
        .0
        .min(buffer.len().saturating_sub(1))
}

/// Translates a row of the log pane's content area into the buffer
/// line it shows and the char index that row starts at, or `None` on
/// the marker row or below the last rendered row. The mouse hit test
/// and the highlights both go through this, so a click always lands
/// on the line the highlight would mark.
pub fn row_at(buffer: &LogBuffer, height: usize, width: usize, pane_row: usize) -> Option<(usize, usize)> {
    let rows = buffer.view(height, width);
    let row = rows.get(pane_row)?;
    Some((row.line?, row.chars.start))
}

/// The `(start, end)` line window `LogBuffer::view` currently draws
/// from: `start` is the first line with a visible row, `end` one past
/// the last. `(0, 0)` for an empty view.
pub fn scroll_window(buffer: &LogBuffer, height: usize, width: usize) -> (usize, usize) {
    let rows = buffer.view(height, width);
    let mut lines = rows.iter().filter_map(|row| row.line);
    match (lines.next(), lines.last()) {
        (Some(first), Some(last)) => (first, last + 1),
        (Some(only), None) => (only, only + 1),
        (None, _) => (0, 0),
    }
}
```

- [ ] **Step 5: Thread the width through `state.rs` and `lib.rs`**

`state.rs`: rename the field and setter:

```rust
    pane_height: usize,
    pane_width: usize,
```

```rust
    pub fn set_pane_size(&mut self, height: usize, width: usize) {
        self.pane_height = height;
        self.pane_width = width;
    }
```

`enter_copy`: `crate::copy::top_visible_line(buffer, self.pane_height, self.pane_width)`.

`sync_copy_scroll`: read `let width = self.pane_width;` next to `pane_height`, call `scroll_window(buffer, pane_height, width)`, and replace the `cursor_line < start` branch with a walk (one step per key press in practice, bounded by the buffer):

```rust
        if cursor_line < start {
            for _ in 0..len {
                buffer.scroll_up(1);
                if crate::copy::scroll_window(buffer, pane_height, width).0 <= cursor_line {
                    break;
                }
            }
        } else if cursor_line >= end {
            // unchanged
```

`handle_mouse` takes `pane_width: usize` after `pane_height`, calls `crate::copy::row_at(buffer, pane_height, pane_width, pane_row)`, and sets `copy.cursor = (line, column_offset + pane_col)` (same for the anchor on `Down`).

Every `set_pane_height(n)` in tests becomes `set_pane_size(n, 80)`; every `handle_mouse(kind, h, row, col)` in tests becomes `handle_mouse(kind, h, 80, row, col)`.

`lib.rs`, `dispatch`:

```rust
    let pane = ui::log_pane_content_area(terminal_size.width, terminal_size.height);
    let (height, width) = pane
        .map(|area| (area.height as usize, area.width as usize))
        .unwrap_or((0, 0));
    state.set_pane_size(height, width);
```

The four scroll arms become row-based (`ScrollUp(n)` is the wheel's three rows, `ScrollHalfPageUp` is `half_pane`):

```rust
        Action::ScrollUp(rows) => {
            if let Some(buffer) = selected_buffer_mut(state) {
                buffer.scroll_up_rows(rows, width);
            }
        }
        Action::ScrollDown(rows) => {
            if let Some(buffer) = selected_buffer_mut(state) {
                buffer.scroll_down_rows(rows, width);
            }
        }
        Action::ScrollHalfPageUp => {
            let rows = (height / 2).max(1);
            if let Some(buffer) = selected_buffer_mut(state) {
                buffer.scroll_up_rows(rows, width);
            }
        }
        Action::ScrollHalfPageDown => {
            let rows = (height / 2).max(1);
            if let Some(buffer) = selected_buffer_mut(state) {
                buffer.scroll_down_rows(rows, width);
            }
        }
```

Delete `half_pane` and `log_pane_height` if nothing else uses them. `dispatch_mouse` passes `area.width as usize` as the new argument.

`ui/logpane.rs`, `draw`: `buffer.view(body_height, rows[1].width as usize)`. In `styled_line`, clip highlight ranges to the row and shift them:

```rust
/// A `(from, to)` inclusive char range of the whole line, as the same
/// range inside `row`, or `None` when the two do not overlap.
fn within(row: &Row, from: usize, to: usize) -> Option<(usize, usize)> {
    let start = row.chars.start;
    let end = row.chars.end; // exclusive
    if end == 0 || to < start || from >= end {
        return None;
    }
    Some((from.max(start) - start, to.min(end - 1) - start))
}
```

Copy: `selection.covers_line(line, full_len)` where `full_len` is the whole line's char count (`buffer.lines().nth(line).map(|l| l.text.chars().count())`), then `within(row, from, to)`. Search: `match_ranges` over the whole line's `text` (not `row.text`), each range through `within`. The highlight therefore continues across a wrap boundary on both rows.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `INSTA_UPDATE=always cargo test -p alba-tui` then `git diff crates/alba-tui/tests/snapshots/`
Expected: all pass; the only new snapshot is `render__a_long_log_line_wraps_in_the_pane.snap`, showing the 60 `x` on one row (the pane is 47 wide, so 47 then 13) followed by `tail`; every other snapshot is byte-identical.

- [ ] **Step 7: Gate and commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace
git add Cargo.toml Cargo.lock crates/alba-tui
git commit -m "✨ feat(tui): wrap long log lines by rendered width"
```

---

### Task 4: Colour theme, colour gate, and dimmed stderr

**Files:**
- Create: `crates/alba-tui/src/ui/theme.rs`
- Modify: `crates/alba-tui/src/ui/mod.rs`, `tree.rs`, `header.rs`, `graphpane.rs`, `logpane.rs`
- Modify: `crates/alba-tui/src/state.rs` (`pub colour: bool`, `with_colour`)
- Modify: `crates/alba-tui/src/lib.rs` (`TuiOptions.colour`)
- Modify: `crates/alba-cli/src/commands/run.rs` (`colour: false` for now; Task 7 wires the gate)
- Test: `crates/alba-tui/src/ui/theme.rs`, `tree.rs`, `header.rs`, `graphpane.rs`, `logpane.rs`

**Interfaces:**
- Produces:
  - `AppState.colour: bool` (default `false`), `AppState::with_colour(self, bool) -> Self`
  - `TuiOptions.colour: bool`
  - `theme::status_style(&BeamState, colour: bool) -> Style`, `theme::outcome_style(&RunSummary, colour) -> Style`, `theme::parked_style(colour) -> Style`, `theme::bar_style(colour) -> Style`, `theme::key_style(colour) -> Style`, `theme::bar_line(text: &str, colour) -> Line<'static>`
  - `header::line(state, now) -> Line<'static>` (replaces `header::text`)

- [ ] **Step 1: Write the failing tests**

Create `crates/alba-tui/src/ui/theme.rs` with only the tests for now:

```rust
//! The one place a status becomes a colour. Sixteen ANSI colours only,
//! and every function answers `Style::default()` when `colour` is off,
//! so `NO_COLOR` renders exactly the monochrome interface it always did.

use std::time::{Duration, Instant};

use alba_engine::{BeamStatus, RunSummary};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};

use crate::state::BeamState;

#[cfg(test)]
mod tests {
    use super::*;
    use alba_core::BeamId;

    fn done(status: BeamStatus) -> BeamState {
        BeamState::Done {
            status,
            duration: Duration::from_secs(1),
        }
    }

    #[test]
    fn each_status_has_its_colour() {
        assert_eq!(status_style(&done(BeamStatus::Succeeded), true), Style::new().fg(Color::Green));
        assert_eq!(status_style(&done(BeamStatus::Cached), true), Style::new().fg(Color::Cyan));
        assert_eq!(
            status_style(&BeamState::Running { since: Instant::now() }, true),
            Style::new().fg(Color::Yellow)
        );
        assert_eq!(
            status_style(&done(BeamStatus::Failed { exit_code: 1 }), true),
            Style::new().fg(Color::Red)
        );
        assert_eq!(
            status_style(&done(BeamStatus::FailedAllowed { exit_code: 1 }), true),
            Style::new().fg(Color::Red)
        );
        assert_eq!(status_style(&BeamState::Pending, true), Style::new().dim());
        assert_eq!(status_style(&done(BeamStatus::Cancelled), true), Style::new().dim());
    }

    #[test]
    fn colour_off_is_the_default_style_everywhere() {
        assert_eq!(status_style(&done(BeamStatus::Failed { exit_code: 1 }), false), Style::default());
        assert_eq!(status_style(&BeamState::Pending, false), Style::default());
        assert_eq!(outcome_style(&RunSummary::default(), false), Style::default());
        assert_eq!(key_style(false), Style::default());
        assert_eq!(bar_style(false), Style::default());
        assert_eq!(parked_style(false), Style::default());
    }

    #[test]
    fn the_outcome_is_red_on_any_failure_and_green_otherwise() {
        let green = RunSummary {
            succeeded: vec![BeamId("a".into())],
            ..RunSummary::default()
        };
        assert_eq!(outcome_style(&green, true), Style::new().fg(Color::Green));
        let allowed = RunSummary {
            failed_allowed: vec![BeamId("a".into())],
            ..RunSummary::default()
        };
        assert_eq!(outcome_style(&allowed, true), Style::new().fg(Color::Red));
        let failed = RunSummary {
            failed: vec![BeamId("a".into())],
            ..RunSummary::default()
        };
        assert_eq!(outcome_style(&failed, true), Style::new().fg(Color::Red));
    }

    /// `q quit · r rerun` becomes bold keys and plain labels, separated
    /// by the same ` · ` the plain text had.
    #[test]
    fn bar_line_bolds_the_keys() {
        let line = bar_line("q quit · Esc cancel", true);
        let spans: Vec<(String, Style)> = line
            .spans
            .iter()
            .map(|span| (span.content.to_string(), span.style))
            .collect();
        assert_eq!(
            spans,
            vec![
                ("q".to_string(), Style::new().bold()),
                (" quit".to_string(), Style::default()),
                (" · ".to_string(), Style::default()),
                ("Esc".to_string(), Style::new().bold()),
                (" cancel".to_string(), Style::default()),
            ]
        );
        assert_eq!(bar_line("q quit", false).spans.len(), 1);
    }
}
```

In `crates/alba-tui/src/ui/tree.rs` tests (create `mod tests` if there is none), add:

```rust
    #[test]
    fn a_failed_row_carries_the_status_colour_on_its_glyph() {
        use ratatui::style::Color;
        let row = BeamRow {
            id: "build".to_string(),
            state: BeamState::Done {
                status: BeamStatus::Failed { exit_code: 1 },
                duration: Duration::from_secs(1),
            },
        };
        let line = row_line(&row, Instant::now(), 30, false, true);
        assert_eq!(line.spans[0].style, Style::new().fg(Color::Red));
        assert_eq!(line.spans[0].content.trim(), "✖");
        let selected = row_line(&row, Instant::now(), 30, true, true);
        assert!(selected.spans.iter().all(|span| span.style.add_modifier.contains(Modifier::REVERSED)));
    }
```

In `crates/alba-tui/src/ui/logpane.rs` tests:

```rust
    /// stderr without colour of its own is dimmed; stderr that brought
    /// colour keeps it.
    #[test]
    fn plain_stderr_is_dimmed_and_coloured_stderr_is_not() {
        use alba_executors::Stream;
        let mut buffer = LogBuffer::new();
        buffer.push("warning", Stream::Stderr, false);
        buffer.push("\u{1b}[31merror\u{1b}[0m", Stream::Stderr, false);
        buffer.push("out", Stream::Stdout, false);
        let rows = buffer.view(5, 80);
        let plain = styled_line(&rows[0], &buffer, &Mode::Normal, None, true);
        assert_eq!(plain.spans[0].style, Style::new().dim());
        let coloured = styled_line(&rows[1], &buffer, &Mode::Normal, None, true);
        assert_eq!(coloured.spans[0].style, Style::new().fg(ratatui::style::Color::Red));
        let stdout = styled_line(&rows[2], &buffer, &Mode::Normal, None, true);
        assert_eq!(stdout.spans[0].style, Style::default());
        let no_colour = styled_line(&rows[0], &buffer, &Mode::Normal, None, false);
        assert_eq!(no_colour.spans[0].style, Style::default());
    }
```

(`styled_line`'s signature becomes `(row: &Row, buffer: &LogBuffer, mode: &Mode, query: Option<&str>, colour: bool)`; adjust Task 3's version accordingly.)

In `crates/alba-tui/src/ui/graphpane.rs`, rewrite the existing `node_style` test:

```rust
    #[test]
    fn nodes_take_their_status_colour_and_the_focused_one_is_reversed() {
        use ratatui::style::Color;
        let failed = BeamState::Done {
            status: BeamStatus::Failed { exit_code: 1 },
            duration: Duration::from_secs(1),
        };
        assert_eq!(node_style(&failed, 2, 2, true), Style::new().fg(Color::Red).reversed());
        assert_eq!(node_style(&failed, 1, 2, true), Style::new().fg(Color::Red));
        assert_eq!(node_style(&failed, 1, 2, false), Style::default());
        assert_eq!(node_style(&failed, 2, 2, false), Style::default().reversed());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alba-tui ui::`
Expected: compile errors (no `theme` module, no `row_line`, wrong `node_style` arity).

- [ ] **Step 3: Implement the theme**

Complete `theme.rs` (above its tests):

```rust
pub fn status_style(state: &BeamState, colour: bool) -> Style {
    if !colour {
        return Style::default();
    }
    match state {
        BeamState::Pending => Style::new().dim(),
        BeamState::Running { .. } => Style::new().fg(Color::Yellow),
        BeamState::Done { status, .. } => match status {
            BeamStatus::Succeeded => Style::new().fg(Color::Green),
            BeamStatus::Cached => Style::new().fg(Color::Cyan),
            BeamStatus::Failed { .. } | BeamStatus::FailedAllowed { .. } => Style::new().fg(Color::Red),
            BeamStatus::Cancelled => Style::new().dim(),
        },
    }
}

/// Green when no beam failed, red otherwise; an allowed failure is a
/// failure here, as it is in the tree's `✖`.
pub fn outcome_style(summary: &RunSummary, colour: bool) -> Style {
    if !colour {
        return Style::default();
    }
    if summary.failed.is_empty() && summary.failed_allowed.is_empty() {
        Style::new().fg(Color::Green)
    } else {
        Style::new().fg(Color::Red)
    }
}

pub fn parked_style(colour: bool) -> Style {
    if colour { Style::new().fg(Color::Yellow) } else { Style::default() }
}

/// The filled part of the progress bar.
pub fn bar_style(colour: bool) -> Style {
    if colour { Style::new().fg(Color::Green) } else { Style::default() }
}

pub fn key_style(colour: bool) -> Style {
    if colour { Style::new().bold() } else { Style::default() }
}

/// A bottom-bar text (`q quit · r rerun`) as a line whose first word of
/// every ` · `-separated item is a key drawn with `key_style`. Off
/// colour it is one plain span, the text unchanged.
pub fn bar_line(text: &str, colour: bool) -> Line<'static> {
    if !colour {
        return Line::from(text.to_string());
    }
    let mut spans = Vec::new();
    for (index, item) in text.split(" · ").enumerate() {
        if index > 0 {
            spans.push(Span::raw(" · "));
        }
        match item.split_once(' ') {
            Some((key, label)) => {
                spans.push(Span::styled(key.to_string(), key_style(colour)));
                spans.push(Span::raw(format!(" {label}")));
            }
            None => spans.push(Span::styled(item.to_string(), key_style(colour))),
        }
    }
    Line::from(spans)
}
```

Register `mod theme;` in `ui/mod.rs` (`pub(crate)` if `header`/`tree` need it from sibling modules: they are all under `ui`, so `super::theme` works with a private `mod theme;`).

`state.rs`: add `pub colour: bool,` to `AppState` (initialised `false` in `new`), and:

```rust
    /// Whether the interface draws colour at all: the CLI's own gate
    /// (`stdout` is a terminal, `NO_COLOR` unset), decided once.
    pub fn with_colour(mut self, colour: bool) -> Self {
        self.colour = colour;
        self
    }
```

`lib.rs`: `TuiOptions { target, watch, colour: bool }`; `AppState::new(&options.target, options.watch).with_colour(options.colour)`. `crates/alba-cli/src/commands/run.rs` passes `colour: false` for now.

`tree.rs`: replace the `map` in `draw` with `row_line(row, now, area.width as usize, index == state.selected, state.colour)` and add:

```rust
/// One tree row: the glyph in its status colour, the name and the
/// duration plain; the whole row reversed when selected.
fn row_line(row: &BeamRow, now: Instant, width: usize, selected: bool, colour: bool) -> Line<'static> {
    let glyph = glyph_for(&row.state);
    let duration = duration_text(&row.state, now);
    let name_width = width.saturating_sub(2 + glyph_width(glyph) + DURATION_WIDTH);
    let name = fit_name(&row.id, name_width);
    let mut spans = vec![
        Span::styled(format!(" {glyph}"), super::theme::status_style(&row.state, colour)),
        Span::raw(format!(" {name:<name_width$}{duration:>DURATION_WIDTH$}")),
    ];
    if selected {
        for span in &mut spans {
            span.style = span.style.reversed();
        }
    }
    Line::from(spans)
}
```

Define `fit_name(id: &str, width: usize) -> String` for now as `id.to_string()` (Task 6 makes it truncate); remove `row_text`. `counts_line` returns a `Line` with each `glyph count` pair styled by `status_style` of a representative state (`BeamState::Done { status: Succeeded, .. }` for `✔`, and so on; `Pending` for `○`), separated by two spaces.

`header.rs`: rename `text` to `line`, returning `Line<'static>`:

- `Running`: `Span::raw("alba · run {target} ── ")`, `Span::styled("▰".repeat(filled), bar_style(colour))`, `Span::raw("▱".repeat(rest) + " {done}/{total} · {elapsed}")`. Split `bar` into `filled_cells(done, total) -> usize`.
- `Parked`: the whole text styled `parked_style(colour)`.
- `Finished` with a summary: `Span::raw("alba · run {target} finished · ")`, `Span::styled(counts_text, outcome_style(summary, colour))`, `Span::raw(" · {duration}")`.
- `Waiting` and idle: plain.

Existing tests of `header::text` compare strings: keep them by comparing `line(state, now).to_string()`.

`ui/mod.rs`: `title_top` becomes a `Line` built from `Span::raw("─ ")`, the header line's spans, `Span::raw(" ")`; `title_bottom` the same around `theme::bar_line(&bottom_bar(state, now), state.colour)`. `bottom_bar` keeps returning `String` (its tests stand).

`graphpane.rs`: `node_style(state: &BeamState, beam: usize, focused: usize, colour: bool) -> Style` is `status_style(state, colour)`, `.reversed()` when `beam == focused`; the call site passes `&row.state`.

`logpane.rs`: `styled_line` gains `colour`, and before applying highlights:

```rust
    let own_colour = row.spans.iter().any(|(style, _)| *style != Style::default());
    let dim_stderr = colour
        && row.line.and_then(|line| buffer.lines().nth(line)).is_some_and(|line| line.stream == Stream::Stderr)
        && !own_colour;
    if dim_stderr {
        for span in &mut spans {
            span.style = span.style.dim();
        }
    }
```

`draw` passes `state.colour`; the diagnostic pane (no buffer) passes through with no dimming.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p alba-tui`
Expected: all pass, snapshots byte-identical (they drop styles and the text is unchanged).

- [ ] **Step 5: Gate and commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace
git add crates/alba-tui crates/alba-cli/src/commands/run.rs
git commit -m "✨ feat(tui): colour statuses, the header, the graph, and the bar behind a NO_COLOR gate"
```

---

### Task 5: The selection jumps to the first failure

**Files:**
- Modify: `crates/alba-tui/src/state.rs` (`AppState` fields, `new`, `start_run`, `select_next`, `select_previous`, `handle_graph_key`, the `BeamFinished` arm of `apply`)
- Test: `crates/alba-tui/src/state.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: private fields `selection_moved: bool`, `jumped_to_failure: bool`; no public API change.

- [ ] **Step 1: Write the failing tests**

In `state.rs` tests (the helpers `run_started`, `finished`, `id` exist there):

```rust
    fn three_beams() -> AppState {
        let mut state = AppState::new("build", false);
        state.apply(
            &run_started("build", &["a", "b", "c"], &[]),
            Instant::now(),
        );
        state
    }

    /// The first failure of a run pulls the selection onto itself.
    #[test]
    fn the_first_failure_selects_its_beam() {
        let mut state = three_beams();
        state.apply(&finished(BeamStatus::Failed { exit_code: 1 }, "c"), Instant::now());
        assert_eq!(state.selected_beam().unwrap().id, "c");
    }

    /// A second failure leaves the reader on the first one.
    #[test]
    fn a_second_failure_does_not_move_the_selection_again() {
        let mut state = three_beams();
        state.apply(&finished(BeamStatus::Failed { exit_code: 1 }, "b"), Instant::now());
        state.apply(&finished(BeamStatus::Failed { exit_code: 1 }, "c"), Instant::now());
        assert_eq!(state.selected_beam().unwrap().id, "b");
    }

    /// A reader who moved since the run started is not interrupted.
    #[test]
    fn a_moved_selection_is_not_hijacked_by_a_failure() {
        let mut state = three_beams();
        state.select_next(); // b
        state.apply(&finished(BeamStatus::Failed { exit_code: 1 }, "c"), Instant::now());
        assert_eq!(state.selected_beam().unwrap().id, "b");
    }

    /// An allowed failure is not the failure the reader needs to see.
    #[test]
    fn an_allowed_failure_does_not_jump() {
        let mut state = three_beams();
        state.apply(&finished(BeamStatus::FailedAllowed { exit_code: 1 }, "c"), Instant::now());
        assert_eq!(state.selected_beam().unwrap().id, "a");
    }

    /// A new run starts the rule afresh: the earlier jump and the
    /// earlier movement are both forgotten.
    #[test]
    fn a_new_run_resets_the_jump_rule() {
        let mut state = three_beams();
        state.select_next();
        state.apply(&run_started("build", &["a", "b", "c"], &[]), Instant::now());
        state.apply(&finished(BeamStatus::Failed { exit_code: 1 }, "c"), Instant::now());
        assert_eq!(state.selected_beam().unwrap().id, "c");
    }

    /// Choosing a beam in the graph counts as moving.
    #[test]
    fn selecting_in_the_graph_counts_as_moving() {
        let mut state = three_beams();
        state.enter_graph();
        state.handle_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        state.apply(&finished(BeamStatus::Failed { exit_code: 1 }, "c"), Instant::now());
        assert_eq!(state.selected_beam().unwrap().id, "a");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alba-tui state::tests::the_first_failure state::tests::a_second_failure state::tests::a_moved state::tests::an_allowed state::tests::a_new_run_resets state::tests::selecting_in_the_graph`
Expected: `the_first_failure_selects_its_beam`, `a_second_failure_does_not_move_the_selection_again`, and `a_new_run_resets_the_jump_rule` fail (selection stays on `a`); the rest pass by accident and stay as guards.

- [ ] **Step 3: Implement the two flags**

`AppState` fields (private, next to `waiting_seen`):

```rust
    /// `j`/`k`/the graph's `Enter` moved the selection since the run
    /// started: the reader chose a place, so a failure must not pull
    /// them off it.
    selection_moved: bool,
    /// A failure already pulled the selection once this run; the second
    /// one leaves the reader on the first.
    jumped_to_failure: bool,
```

Both `false` in `new`; both reset to `false` at the end of `start_run`. `select_next` and `select_previous` set `self.selection_moved = true` before calling `select`; `handle_graph_key`'s `Enter` arm sets it too. In the `BeamFinished` arm, after `row.state = BeamState::Done { .. }`:

```rust
                let real_failure = matches!(status, BeamStatus::Failed { .. });
                if real_failure && !self.selection_moved && !self.jumped_to_failure {
                    if let Some(index) = self.index_of(&id.0) {
                        self.select(index);
                        self.jumped_to_failure = true;
                    }
                }
```

(`select` itself must not set `selection_moved`; only the user-driven entry points do.)

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p alba-tui state::`
Expected: all pass.

- [ ] **Step 5: Gate and commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace
git add crates/alba-tui/src/state.rs
git commit -m "✨ feat(tui): select the first failed beam unless the reader moved"
```

---

### Task 6: Truncated beam names and the lighter bottom bar

**Files:**
- Modify: `crates/alba-tui/src/ui/tree.rs` (`fit_name`)
- Modify: `crates/alba-tui/src/ui/mod.rs` (`bottom_bar`'s Normal arm and its doc comment, the test at line 217)
- Modify: `crates/alba-tui/src/ui/help.rs` (doc comment only if it describes the bar)
- Modify: `crates/alba-tui/tests/snapshots/*.snap` (the bottom bar)
- Test: `crates/alba-tui/src/ui/tree.rs`, `crates/alba-tui/tests/render.rs`

- [ ] **Step 1: Write the failing tests**

`tree.rs` tests:

```rust
    #[test]
    fn a_name_wider_than_its_column_is_truncated_with_an_ellipsis() {
        assert_eq!(fit_name("services:payment:integration", 19), "services:payment:i…");
        assert_eq!(fit_name("build", 19), "build");
        assert_eq!(fit_name("abc", 0), "");
        assert_eq!(fit_name("abc", 1), "…");
    }
```

`render.rs`:

```rust
/// A long beam id never pushes the duration out of its column.
#[test]
fn a_long_beam_name_is_truncated_in_the_tree() {
    let mut state = AppState::new("build", false);
    let now = Instant::now();
    state.apply(
        &RunEvent::RunStarted {
            targets: vec![id("build")],
            affected_by: None,
            beams: vec![id("services:payment:integration"), id("build")],
            edges: Vec::new(),
        },
        now,
    );
    state.apply(
        &RunEvent::BeamStarted {
            id: id("services:payment:integration"),
        },
        now,
    );
    state.apply(
        &RunEvent::BeamFinished {
            id: id("services:payment:integration"),
            status: BeamStatus::Succeeded,
            duration: Duration::from_millis(1200),
        },
        now,
    );
    insta::assert_snapshot!(drawn(&state, 80, 24));
}
```

Update the `bottom_bar` test in `ui/mod.rs` (line 217) to expect `"q quit · r rerun · c cancel · w watch · / search · ? help"`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alba-tui`
Expected: `fit_name` test fails (no truncation), the `bottom_bar` test fails on the old text, the new snapshot is missing.

- [ ] **Step 3: Implement**

`tree.rs`:

```rust
/// `id` cut to `width` cells with a trailing `…` when it does not fit,
/// so the duration column stays where it is. Beam ids are ASCII
/// identifiers joined by `:`, so chars are cells here.
fn fit_name(id: &str, width: usize) -> String {
    if id.chars().count() <= width {
        return id.to_string();
    }
    let kept: String = id.chars().take(width.saturating_sub(1)).collect();
    if width == 0 { String::new() } else { format!("{kept}…") }
}
```

`ui/mod.rs`, `bottom_bar`'s Normal arm:

```rust
        Mode::Normal => "q quit · r rerun · c cancel · w watch · / search · ? help".to_string(),
```

Rewrite the paragraph of `bottom_bar`'s doc comment that justifies `t` and `n`/`N` in the bar: the Normal bar now names the six actions a first-time reader needs; `f`, `t`, `n`/`N`, `g`, and `v` live in the help overlay (`?`), which the bar itself points at. `help.rs`'s `keymap_lines` already lists all of them; leave it.

- [ ] **Step 4: Run the tests and accept the snapshots**

Run: `INSTA_UPDATE=always cargo test -p alba-tui` then `git diff crates/alba-tui/tests/snapshots/`
Expected: every snapshot's last line changes from `└─ q quit · r rerun · f force · t target · c cancel · w watch · n/N step ──────┘` to `└─ q quit · r rerun · c cancel · w watch · / search · ? help ─────────────────┘` and nothing else changes in them; the new `render__a_long_beam_name_is_truncated_in_the_tree.snap` shows `✔ services:payment:i…    1.2s`.

- [ ] **Step 5: Gate and commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace
git add crates/alba-tui
git commit -m "💄 style(tui): truncate long beam names and lighten the bottom bar"
```

---

### Task 7: The CLI wires colour, forces it in commands, and replays raw or plain

**Files:**
- Modify: `crates/alba-cli/src/commands/run.rs` (`tui_execute`: `RunOptions.extra_env`, `TuiOptions.colour`; `replay`)
- Modify: `crates/alba-cli/tests/tui_pty.rs`
- Test: `crates/alba-cli/src/commands/run.rs` (replay tests), `crates/alba-cli/tests/tui_pty.rs`

**Interfaces:**
- Consumes: `RunOptions.extra_env` (Task 1), `TuiOptions.colour` (Task 4), `ReplayLine` (Task 2), `crate::color_enabled()` (`crates/alba-cli/src/main.rs:226`).
- Produces: `replay<W>(err, outcome, affected_by, colour: bool)`.

- [ ] **Step 1: Write the failing tests**

In the `replay` tests of `run.rs` (the `outcome` helper is there; it now builds `ReplayLine`s):

```rust
    /// The replay keeps the compiler's colours on a terminal and drops
    /// them under NO_COLOR.
    #[test]
    fn the_replay_prints_raw_lines_with_colour_and_plain_lines_without() {
        let summary = RunSummary {
            failed: vec![BeamId("red".to_string())],
            ..RunSummary::default()
        };
        let line = alba_tui::ReplayLine {
            raw: "\u{1b}[31mred\u{1b}[0m".to_string(),
            text: "red".to_string(),
        };
        let outcome = outcome(Some(summary), vec![("red".to_string(), vec![line])]);

        let coloured = replayed_with(&outcome, None, true);
        assert!(coloured.iter().any(|l| l == "\u{1b}[31mred\u{1b}[0m"), "{coloured:?}");
        let plain = replayed_with(&outcome, None, false);
        assert!(plain.iter().any(|l| l == "red"), "{plain:?}");
        assert!(plain.iter().all(|l| !l.contains('\u{1b}')), "{plain:?}");
    }
```

Find the existing `replayed(...)` helper in those tests and add `replayed_with(outcome, affected_by, colour)` next to it, with `replayed` calling `replayed_with(.., false)`.

In `crates/alba-cli/tests/tui_pty.rs`, extract the body of the existing test into a helper and add a second test. The helper:

```rust
/// Runs `alba run <beam>` on a pty against `beamfile`, with `env` added
/// to the child's environment, waits for the run to finish, sends `q`,
/// and returns what the interface drew, what followed the quit, and the
/// exit status.
fn drive(beamfile: &str, beam: &str, env: &[(&str, &str)]) -> (String, String, portable_pty::ExitStatus) {
    // ... the existing body from `let project = tempfile::tempdir()` to the
    // drain of `after_quit`, with `command.args(["run", beam])`, the Beamfile
    // written from `beamfile`, and `for (name, value) in env { command.env(name, value); }`
    // right after `command.cwd(..)`; returns `(seen, after_quit, status)`.
}
```

The existing test becomes `drive(GREEN, "ok", &[])` plus its current assertions. The new test (unix only: it needs `sh`):

```rust
const RED: &str = "version \"1\"\n\nbeam red {\n  run \"sh red.sh\"\n}\n";
const RED_SCRIPT: &str = "printf '\\033[31mred\\033[0m\\n'\nexit 1\n";

/// A failing beam that printed colour is replayed with that colour on a
/// terminal, and without it under NO_COLOR. The script prints colour
/// unconditionally, so this pins the replay's choice, not the forcing.
#[cfg(unix)]
#[test]
fn the_replay_keeps_colour_on_a_terminal_and_drops_it_under_no_color() {
    let (_, after_quit, status) = drive_with_script(RED, "red", RED_SCRIPT, &[]);
    assert_eq!(status.exit_code(), 1);
    assert!(after_quit.contains("\u{1b}[31mred"), "got after q: {after_quit:?}");

    let (_, after_quit, status) = drive_with_script(RED, "red", RED_SCRIPT, &[("NO_COLOR", "1")]);
    assert_eq!(status.exit_code(), 1);
    assert!(after_quit.contains("── red ──"), "got after q: {after_quit:?}");
    let replay = &after_quit[after_quit.find("── red ──").unwrap()..];
    assert!(replay.contains("red\r\n") || replay.contains("red\n"), "got: {replay:?}");
    assert!(!replay.contains("\u{1b}[31m"), "colour leaked under NO_COLOR: {replay:?}");
}
```

Make `drive` take an optional script: `drive_with_script(beamfile, beam, script: &str, env)` writes `red.sh` next to the Beamfile when `script` is not empty; `drive` calls it with `""`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alba-cli replay && cargo test -p alba-cli --test tui_pty`
Expected: `replay` has no `colour` parameter (compile error); the pty test fails on `\u{1b}[31mred` absent from the replay.

- [ ] **Step 3: Wire the CLI**

In `tui_execute` (around line 377):

```rust
    let colour = crate::color_enabled();
    let options = RunOptions {
        jobs: jobs(flags.jobs),
        keep_going: flags.keep_going,
        params,
        cache: Some(CacheOptions {
            dir: cache_dir(beamfile),
            force: flags.force,
        }),
        // The interface renders ANSI, and commands on a pipe emit none
        // unless told to; a monochrome interface asks for none.
        extra_env: if colour {
            vec![
                ("FORCE_COLOR".to_string(), "1".to_string()),
                ("CLICOLOR_FORCE".to_string(), "1".to_string()),
            ]
        } else {
            Vec::new()
        },
    };
```

`alba_tui::TuiOptions { target: .., watch: .., colour }`, and `replay(&mut err, &outcome, affected_by, colour)`:

```rust
fn replay<W: std::io::Write>(
    err: &mut LineSink<W>,
    outcome: &alba_tui::TuiOutcome,
    affected_by: Option<&str>,
    colour: bool,
) {
    // ...
        for line in lines {
            err.line(if colour { &line.raw } else { &line.text });
        }
```

`color_enabled` is `pub(crate)` in `main.rs`; it is reachable as `crate::color_enabled()` from `commands/run.rs` (it already is at line 627).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p alba-cli`
Expected: all pass, including both pty tests.

- [ ] **Step 5: Gate and commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace
git add crates/alba-cli
git commit -m "✨ feat(cli): force colour under the interface and replay failures in colour"
```

---

### Task 8: Documentation, dogfooding, and the spec's deviations

**Files:**
- Modify: `README.md` (Interactive interface, Layout, Keymap)
- Modify: `.claude/superpowers/specs/2026-08-26-alba-tui-polish-design.md`

- [ ] **Step 1: README**

In **Interactive interface**, after the paragraph on watch mode, add a **Colour** subsection (`### Colour`, before `### Layout`):

- The theme: `✔` green, `⚡` cyan, `▶` yellow, `✖` red, `○` dimmed; the progress bar green while running; the outcome green or red; the header yellow while parked; keys in the bottom bar bold.
- `NO_COLOR` (or stdout not being a terminal) turns all of it off and the interface renders monochrome.
- Commands run under the interface receive `FORCE_COLOR=1` and `CLICOLOR_FORCE=1` (a beam's own `env` wins on those names), so `cargo`, `eslint`, and friends print the colours they would in a terminal, which the log pane renders; the cache does not see those two variables, so a beam hits the same entry whether the interface or a pipe ran it. The headless renderers set nothing and print the command's bytes as they are.
- stderr lines that carry no colour of their own are dimmed; ones that do keep it.
- The exit replay of failed beams keeps their colours when the interface had colour, and prints plain text otherwise.

In **Layout**:

- Mockup bottom line: `└─ q quit · r rerun · c cancel · w watch · / search · ? help ─────────────────┘` (keep the frame 80 wide; recount the dashes).
- **Tree** bullet: add "a name longer than its column is cut with a trailing `…`".
- **Logs** bullet: add "long lines wrap by character; scrolling moves by whole lines" and "when a beam fails and you have not moved the selection since the run started, the selection jumps to it (the first failure only)".
- Remove the `[/] search   [g] graph   [↑↓] scroll` footer from the mockup only if the current code does not draw it (it draws `● following` / `↑ paused`): replace that mockup cell with `● following`.

In **Keymap**:

- Under the Normal table, add a sentence: the bottom bar shows `q`, `r`, `c`, `w`, `/`, and `?`; the rest of this table lives in the help overlay.
- `j`/`k` row: append "moving the selection by hand also stops a failure from moving it for the rest of the run".
- Graph paragraph: "colored by status" is now true; leave it.

Follow `markdown-conventions`: wrap at the file's existing width, no em-dashes.

- [ ] **Step 2: Dogfooding**

```bash
cargo install --path crates/alba-cli
alba run check
```

Confirm by eye: green `✔` glyphs, a yellow `▶` while `cargo test` runs, `cargo`'s own colours in the log pane (`Compiling` in green, warnings in yellow). Then break a test deliberately in a scratch change (do not commit it), run `alba run test`, and confirm the selection lands on `test` and the red `error` line shows without a keystroke; revert the scratch change. Run `NO_COLOR=1 alba run fmt` and confirm the monochrome interface. Run `alba run fmt` twice and confirm the second is `⚡` cached.

- [ ] **Step 3: The spec**

In `.claude/superpowers/specs/2026-08-26-alba-tui-polish-design.md`, under **Colour theme**, replace "The gate moves out of `alba-cli/src/main.rs` into a place both crates reach:" with "The gate stays `alba_cli::color_enabled`:" (the TUI receives a `bool`; nothing needed to move). Under **Wrapping**, replace "`copy::line_for_pane_row` becomes `row_at(buffer, height, width, pane_row) -> Option<(line, column_offset)>`" wording if it differs from the implemented signature. Under **Where it lives**, add `ui/theme.rs` holds `bar_line` as well.

- [ ] **Step 4: Gate and commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace
git add README.md .claude/superpowers/specs/2026-08-26-alba-tui-polish-design.md
git commit -m "📝 docs(tui): document colour, wrapping, and the failure jump"
```

---

## Self-review

**Spec coverage.** Colour theme: Task 4. Forced colour and the fingerprint: Tasks 1 and 7. ANSI storage and rendering: Task 2. Exit replay both ways: Tasks 2 and 7. Wrapping, row scrolling, copy and search over wrapped rows: Task 3. Failure jump: Task 5. stderr dim: Task 4. Tree names: Task 6. Bottom bar: Task 6. README and spec deviations: Task 8. Tests named in the spec's testing strategy: `logs.rs` (Tasks 2 and 3), `copy.rs` (Task 3), `state.rs` (Task 5), `tree.rs` (Tasks 4 and 6), `theme.rs` (Task 4), snapshots (Tasks 3 and 6), engine (Task 1), pty (Task 7).

**Type consistency.** `LogBuffer::push(raw: impl Into<String>, stream: Stream, replayed: bool)` throughout. `view(height)` in Task 2 becomes `view(height, width)` in Task 3 and stays so afterwards (Task 4's `logpane` test calls `view(5, 80)`). `styled_line(row, buffer, mode, query, colour)` is the final signature (Task 3 introduces `row`/`buffer`, Task 4 adds `colour`). `row_at` returns `(line, first char)`. `set_pane_size(height, width)` replaces `set_pane_height`. `handle_mouse(kind, height, width, pane_row, pane_col)`. `TuiOptions { target, watch, colour }`. `ReplayLine { raw, text }`. `replay(err, outcome, affected_by, colour)`.

**Placeholders.** None: every step carries its code or the exact edit.
