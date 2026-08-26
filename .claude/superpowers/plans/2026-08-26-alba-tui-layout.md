# Alba TUI Layout Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every band of the interactive interface's frame one job: identity and session state on the top edge, the run's counts, bar, and outcome on a full-width footer under a real junction, the follow state on the `LOGS` title row, and the keymap unchanged on the bottom edge.

**Architecture:** Everything lives in `alba-tui`'s `ui` module. A new `ui/footer.rs` owns the counts line (moved out of `ui/tree.rs`), the run's progress or outcome text, and the bar's sizing; `ui/header.rs` shrinks to the identity title and an optional right-hand session title; `ui/logpane.rs` folds the follow state into its title row; `ui/mod.rs` lays the body, the junction line, and the footer out and keeps `log_pane_content_area`, the one geometry function copy mode's mouse handling reads, in step with what `draw` paints. The pty smoke test's finished-run sentinel moves from the header's `finished` to the footer's outcome word.

**Tech Stack:** Rust 2024, ratatui 0.29 (`Block::title_top` with `Line::right_aligned`, `Paragraph::alignment`, `Buffer` cell indexing), `unicode-width` 0.2, insta snapshots, `portable-pty` (already a dev-dependency of `alba-cli`).

**Spec:** `.claude/superpowers/specs/2026-08-26-alba-tui-layout-design.md`

## Global Constraints

- Every file, comment, and commit message is English. No em-dashes in anything you write (the existing code uses them; leave those alone, do not add new ones).
- Commit messages: gitmoji + Conventional Commits, `<emoji> <type>(<scope>): <summary>`. No `Co-Authored-By`, no tool attribution.
- The gate before every commit: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace`. A `pre-commit` hook runs `cargo fmt --check` on its own; `cargo fmt` first if it fails.
- The tree stays 30 columns wide (`TREE_WIDTH` in `crates/alba-tui/src/ui/mod.rs`).
- The too-small floor becomes `40x12` (`MIN_WIDTH` stays 40, `MIN_HEIGHT` becomes 12).
- The progress bar is at most 30 cells wide and is omitted when fewer than 14 cells are left for it.
- The outcome words are `ok` and `failed`; the empty summary reads `nothing ran`; no summary reads `idle`.
- Normal-mode bottom bar text stays `q quit · r rerun · c cancel · w watch · / search · ? help`.
- With colour off the interface renders the same text as with colour on; only styles differ.
- Snapshot tests: after an intended change to rendered text, run `INSTA_UPDATE=always cargo test -p alba-tui --test render`, then read the diff of `crates/alba-tui/tests/snapshots/` against the spec's mockup and confirm only the intended cells changed before committing.

---

## File map

| File | Responsibility after this plan |
|------|-------------------------------|
| `crates/alba-tui/src/ui/footer.rs` (new) | `draw`, `counts_line`, `run_line`, `bar_width`, `filled_cells` |
| `crates/alba-tui/src/ui/header.rs` | `line` (identity), `session` (optional right-hand title) |
| `crates/alba-tui/src/ui/logpane.rs` | Title row `LOGS`/`DIAGNOSTIC` with the follow state right-aligned; no last row |
| `crates/alba-tui/src/ui/tree.rs` | Title row and beam rows only; no counts row |
| `crates/alba-tui/src/ui/mod.rs` | Body, junction, footer layout; `├ ┴ ┤` cells; both header titles; `MIN_HEIGHT` 12; `log_pane_content_area` |
| `crates/alba-tui/src/lib.rs` | Geometry numbers in its tests follow the new content height |
| `crates/alba-tui/tests/render.rs` and `tests/snapshots/` | Re-accepted against the spec's mockup |
| `crates/alba-cli/tests/tui_pty.rs` | Green fixture beam renamed `green`; waits on `ok` or `failed` |
| `README.md` | Layout section: mockup, bullets, floor |

## Geometry at 80x24, before and after

| Row (0-based) | Today | After Task 1 | After Task 2 |
|---------------|-------|--------------|--------------|
| 0 | top edge (header) | same | same |
| 1 | `BEAMS` / `logs · beam` | `BEAMS` / `LOGS ... ● following` | same |
| 2 to 21 | beams / log content (20 rows) | beams / log content (21 rows, to row 22) | beams / log content (19 rows, to row 20) |
| 22 | counts / `● following` | log content | junction `├───┴───┤` |
| 23 | bottom edge (keys) | same | footer (counts left, run right) at row 22, keys at row 23 |

Log pane content rectangle: today `Rect::new(32, 2, 47, 20)`; after Task 1 `Rect::new(32, 2, 47, 21)`; after Task 2 `Rect::new(32, 2, 47, 19)`.

---

### Task 1: The follow state moves onto the `LOGS` title row

**Files:**
- Modify: `crates/alba-tui/src/ui/logpane.rs:15-78` (the `draw` function)
- Modify: `crates/alba-tui/src/ui/mod.rs:180-200` (`log_pane_content_area` and its doc comment), `:207-212` (its test)
- Modify: `crates/alba-tui/src/lib.rs:805-811`, `:832-836`, `:1083-1101`, `:1130-1131` (geometry numbers in tests)
- Test: `crates/alba-tui/src/ui/logpane.rs` (unit), `crates/alba-tui/tests/render.rs` (snapshots)

**Interfaces:**
- Consumes: `AppState::displayed_log_key`, `AppState::logs`, `LogBuffer::scroll`, `Scroll::Following` (all existing).
- Produces: `logpane::title(state: &AppState) -> &'static str` and `logpane::follow_text(buffer: Option<&LogBuffer>) -> &'static str`, both `pub(crate)`; a log pane whose rows are `[title, content]` only.

- [ ] **Step 1: Write the failing unit tests**

Append to `crates/alba-tui/src/ui/logpane.rs` (create the `tests` module if the file has none; if it already has one, add these two tests inside it):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alba_engine::RunEvent;

    #[test]
    fn the_title_names_the_pane_not_the_beam() {
        let mut state = AppState::new("build", false);
        assert_eq!(title(&state), "LOGS");
        state.apply(
            &RunEvent::ProjectBroken {
                diagnostic: "error: unknown target `nope`\n".to_string(),
            },
            std::time::Instant::now(),
        );
        assert_eq!(title(&state), "DIAGNOSTIC");
    }

    #[test]
    fn the_follow_text_reads_following_without_a_buffer() {
        assert_eq!(follow_text(None), "● following");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alba-tui logpane::tests`
Expected: compile error, `title` and `follow_text` not found.

- [ ] **Step 3: Rewrite `draw` around the two new functions**

Replace the whole `draw` function in `crates/alba-tui/src/ui/logpane.rs` (from `pub fn draw` down to the closing brace after `frame.render_widget(Paragraph::new(footer), rows[2]);`) with:

```rust
pub fn draw(frame: &mut Frame, area: Rect, state: &AppState) {
    let rows = Layout::vertical([
        Constraint::Length(1), // "LOGS" / "DIAGNOSTIC", follow state at the right
        Constraint::Min(0),    // output
    ])
    .split(area);

    // A parked session has no beam worth showing: the diagnostic that
    // parked it, filed under a pseudo-beam key, is the only thing there
    // is to read (see `state::DIAGNOSTIC_LOG`). `displayed_log_key` is
    // the single source of truth for which key that is — copy mode's
    // `v`/`y` resolve their own buffer through the very same method, so
    // the selection highlight below and what `y` actually copies can
    // never disagree about what is on screen.
    let key = state.displayed_log_key();
    let buffer = state.logs.get(&key);

    // Two paragraphs on the one title row: the pane's name at the left
    // edge, the follow state at the right. Neither clears the row, so
    // the second does not paint over the first.
    frame.render_widget(Paragraph::new(title(state)), rows[0]);
    frame.render_widget(
        Paragraph::new(follow_text(buffer)).alignment(Alignment::Right),
        rows[0],
    );

    let body_height = rows[1].height as usize;
    // The query that should still be marked in the pane: the one being
    // typed while search is active, or the last completed one — kept
    // highlighted until a new search session replaces it (see
    // `AppState::last_search`).
    let query = match &state.mode {
        Mode::Search(search) => Some(search.query.as_str()),
        _ => state
            .last_search
            .as_ref()
            .map(|committed| committed.search.query.as_str()),
    };
    // `styled_line` needs a real buffer to look up a row's own line (for
    // the dimmed-stderr check and the full-line text search/copy need),
    // harmless to require, since a `None` buffer never produces a row to
    // begin with (`rows_shown` is empty).
    let lines: Vec<Line> = match buffer {
        Some(buffer) => buffer
            .view(body_height, rows[1].width as usize)
            .iter()
            .map(|row| styled_line(row, buffer, &state.mode, query, state.colour))
            .collect(),
        None => Vec::new(),
    };
    frame.render_widget(Paragraph::new(lines), rows[1]);
}

/// The pane's name. The selected beam is not repeated here: the tree's
/// reversed row already names it. Parked, the pane shows the diagnostic
/// that parked the project instead of any beam's output.
pub(crate) fn title(state: &AppState) -> &'static str {
    if matches!(state.phase, Phase::Parked) {
        "DIAGNOSTIC"
    } else {
        "LOGS"
    }
}

/// The follow state, right-aligned on the title row. No buffer yet
/// (nothing has run) reads the same as following: there is nothing to
/// be paused partway through.
pub(crate) fn follow_text(buffer: Option<&LogBuffer>) -> &'static str {
    let following = buffer.is_none_or(|buffer| matches!(buffer.scroll(), Scroll::Following));
    if following {
        "● following"
    } else {
        "↑ paused"
    }
}
```

Add `Alignment` to the layout import at the top of the file: `use ratatui::layout::{Alignment, Constraint, Layout, Rect};`.

- [ ] **Step 4: Update `log_pane_content_area` and its test in `ui/mod.rs`**

In `crates/alba-tui/src/ui/mod.rs`, replace the body of `log_pane_content_area` after the floor check with:

```rust
    let inner = Block::bordered().inner(Rect::new(0, 0, width, height));
    let panes =
        Layout::horizontal([Constraint::Length(TREE_WIDTH + 1), Constraint::Min(1)]).split(inner);
    let rows = Layout::vertical([
        Constraint::Length(1), // "LOGS" title row
        Constraint::Min(0),    // output
    ])
    .split(panes[1]);
    Some(rows[1])
```

In its doc comment, replace `inside the title/footer rows` with `inside the title row`. In the test `log_pane_content_area_sits_past_the_tree_and_its_borders`, replace the comment and the assertion with:

```rust
        // Outer border: 1 cell each side. Tree pane + divider: 31
        // columns. Title row: 1 line.
        let area = log_pane_content_area(80, 24).expect("80x24 clears the floor");
        assert_eq!(area, Rect::new(32, 2, 47, 21));
```

- [ ] **Step 5: Update the geometry numbers in `lib.rs`'s tests**

In `crates/alba-tui/src/lib.rs`:

- Around line 805 to 811 (the `EnterCopy` anchor test): the comment becomes `// 80x24 gives the log pane a content height of 21 rows` and `// buffer, the top visible line is 30 - 21 = 9.`; both assertions become `assert_eq!(copy.anchor, (9, 0));` and `assert_eq!(copy.cursor, (9, 0));`.
- Around line 832 to 836 (the mouse drag test): `fit inside the 20-row content height` becomes `fit inside the 21-row content height`. The coordinates `(32, 2)` and the assertions are unchanged.
- Around line 1083 to 1101 (`the_page_keys_scroll_the_selected_beams_buffer_by_half_a_pane`): the doc comment `At 80x24 the pane shows 20 rows, so half is 10.` becomes `At 80x24 the pane shows 21 rows, so half is 10.`; the message `"half of the pane's 20 content rows"` becomes `"half of the pane's 21 content rows"`. The offsets `10`, `20`, `10` and the final `Following` are unchanged.
- Around line 1130 to 1131 (`the_page_distance_is_half_the_panes_own_height`): the two assertions become `assert_eq!(half_pane(21), 10, "21 content rows at 80x24");` and `assert_eq!(half_pane(7), 3, "7 content rows at the floor");`, and the doc comment above the test changes `whose 6 content rows halve to 3` to `whose 7 content rows halve to 3`.

- [ ] **Step 6: Run the unit tests**

Run: `cargo test -p alba-tui --lib`
Expected: PASS, including `logpane::tests`, `ui::tests::log_pane_content_area_sits_past_the_tree_and_its_borders`, and the four `lib.rs` tests touched above.

- [ ] **Step 7: Re-accept the render snapshots**

Run: `INSTA_UPDATE=always cargo test -p alba-tui --test render`, then `git diff crates/alba-tui/tests/snapshots/`.
Expected in every 80x24 snapshot: row 1 reads `│BEAMS                         │LOGS                               ● following │` (or `↑ paused` where the pane is scrolled, `DIAGNOSTIC` in the parked one); the old `● following` at row 22 of the log pane is gone and that row now shows log content or blanks; the tree's counts at row 22 are unchanged. Nothing else moves. If anything else changed, fix the code, not the snapshot.

- [ ] **Step 8: Run the gate and commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace
git add crates/alba-tui/src/ui/logpane.rs crates/alba-tui/src/ui/mod.rs crates/alba-tui/src/lib.rs crates/alba-tui/tests/snapshots/
git commit -m "💄 feat(tui): fold the follow state into the LOGS title row" -m "The log pane's title now reads LOGS (or DIAGNOSTIC while parked)
with the follow state right-aligned on the same row, and its last row
goes back to the output: the selected beam is already named by the
tree's reversed row, so repeating it in the title said nothing new."
```

---

### Task 2: The footer band: counts, bar, and outcome under a junction

**Files:**
- Create: `crates/alba-tui/src/ui/footer.rs`
- Modify: `crates/alba-tui/src/ui/tree.rs:1-9` (module doc), `:31-58` (`draw`), `:170-234` (`counts_line`, moved out)
- Modify: `crates/alba-tui/src/ui/header.rs:19-23` (`BAR_WIDTH` stays), `:29-31` (calls the footer's `filled_cells`), `:56-68` (its own `filled_cells`, deleted)
- Modify: `crates/alba-tui/src/ui/mod.rs:22-28` (module list), `:29-33` (`MIN_HEIGHT`), `:44-100` (`draw`), `:180-200` (`log_pane_content_area`), tests
- Modify: `crates/alba-tui/src/lib.rs` (the same four test sites as Task 1)
- Test: `crates/alba-tui/src/ui/footer.rs` (unit), `crates/alba-tui/src/ui/mod.rs` (unit), `crates/alba-tui/tests/render.rs` (snapshots)

**Interfaces:**
- Consumes: `theme::status_style`, `theme::bar_style`, `theme::outcome_style`, `super::format_duration`, `AppState::{phase, beams, last_summary, colour}`, `RunSummary` (all existing); `TREE_WIDTH` in `ui/mod.rs`.
- Produces, all in `ui/footer.rs`:
  - `pub fn draw(frame: &mut Frame, area: Rect, state: &AppState, now: Instant)`
  - `pub(crate) fn counts_line(state: &AppState) -> Line<'static>` (the function `tree.rs` had, unchanged)
  - `pub(crate) fn run_line(state: &AppState, now: Instant, room: usize) -> Line<'static>`
  - `pub(crate) fn bar_width(room: usize) -> Option<usize>`
  - `pub(crate) fn filled_cells(done: usize, total: usize, width: usize) -> usize`

- [ ] **Step 1: Create `ui/footer.rs` with its tests, the counts moved in, and stubs**

Create `crates/alba-tui/src/ui/footer.rs`:

```rust
//! The footer row: the run, and nothing else. Left, the counts by
//! status in the tree's own colours; right, the progress bar with
//! `done/total` and the elapsed time while a run is going, the outcome
//! (`ok`/`failed` and the duration) once it is over, `idle` before any
//! run. It sits under the junction line that closes the two panes and
//! above the bottom edge's keymap (see `ui/mod.rs`).
//!
//! The counts tally only the four statuses a beam can settle into
//! (`✔ ⚡ ✖ ○`) and leave a beam still `▶` running out: it has not
//! settled into an outcome yet, so counting it under any of the four
//! would misreport which bucket it will land in.

use std::time::{Duration, Instant};

use alba_engine::{BeamStatus, RunSummary};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::state::{AppState, BeamState, Phase};

use super::format_duration;
use super::theme::{bar_style, outcome_style, status_style};

/// The bar's widest. It has only `total + 1` distinct states, so more
/// cells would make each step jump further without saying anything new.
const BAR_MAX: usize = 30;
/// Below this, the bar is dropped rather than squeezed: the spec's
/// original mockup drew 14 cells, and fewer stop reading as a bar.
const BAR_MIN: usize = 14;

pub fn draw(frame: &mut Frame, area: Rect, state: &AppState, now: Instant) {
    // One cell of margin at each edge, the same as the tree's rows.
    let row = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(2),
        ..area
    };
    let counts = counts_line(state);
    // What is left for the run's text once the counts and a two-cell
    // gap are placed.
    let room = (row.width as usize).saturating_sub(counts.width() + 2);
    let run = run_line(state, now, room);
    // Two paragraphs on the one row: neither clears it, so the second
    // does not paint over the first.
    frame.render_widget(Paragraph::new(counts), row);
    frame.render_widget(Paragraph::new(run).alignment(Alignment::Right), row);
}

/// The run's own text, fitted into `room` cells: the bar only when it
/// has at least `BAR_MIN` cells to itself after the count and the time.
pub(crate) fn run_line(state: &AppState, now: Instant, room: usize) -> Line<'static> {
    match &state.phase {
        Phase::Running { done, total, since } => {
            let elapsed = now.saturating_duration_since(*since);
            let text = format!("{done}/{total} · {}", format_duration(elapsed));
            match bar_width(room.saturating_sub(text.width() + 1)) {
                Some(width) => {
                    let filled = filled_cells(*done, *total, width);
                    Line::from(vec![
                        Span::styled("▰".repeat(filled), bar_style(state.colour)),
                        Span::raw(format!("{} {text}", "▱".repeat(width - filled))),
                    ])
                }
                None => Line::from(text),
            }
        }
        // A watch waiting between runs, and a parked project, still
        // show the last run's outcome: the session state has its own
        // slot on the top edge, so nothing here has to give way to it.
        Phase::Finished | Phase::Waiting { .. } | Phase::Parked => match &state.last_summary {
            Some(summary) => outcome_line(summary, state.colour),
            None => Line::from("idle"),
        },
    }
}

/// The bar's width for `room` free cells: `None` under `BAR_MIN`,
/// capped at `BAR_MAX`.
pub(crate) fn bar_width(room: usize) -> Option<usize> {
    (room >= BAR_MIN).then(|| room.min(BAR_MAX))
}

/// Deterministic on purpose: `done`/`total` are the whole story, so the
/// same state always draws the same bar and no clock is involved in
/// deciding how full it looks — only in how long the run has taken,
/// which `run_line` prints separately.
pub(crate) fn filled_cells(done: usize, total: usize, width: usize) -> usize {
    if total == 0 {
        0
    } else {
        ((done as f64 / total as f64) * width as f64).round() as usize
    }
    .min(width)
}

/// The last run's outcome, one word in the outcome colour and the
/// duration. An allowed failure reads `failed`, as `outcome_style` and
/// the tree's `✖` both already treat it.
fn outcome_line(summary: &RunSummary, colour: bool) -> Line<'static> {
    let ran = summary.succeeded.len()
        + summary.cached.len()
        + summary.failed.len()
        + summary.failed_allowed.len()
        + summary.cancelled.len();
    if ran == 0 {
        return Line::from("nothing ran");
    }
    let word = if summary.failed.is_empty() && summary.failed_allowed.is_empty() {
        "ok"
    } else {
        "failed"
    };
    Line::from(vec![
        Span::styled(word.to_string(), outcome_style(summary, colour)),
        Span::raw(format!(" · {}", format_duration(summary.duration))),
    ])
}

/// The four buckets a beam can settle into, each `glyph count` pair
/// styled by `status_style` of a representative state for that bucket:
/// the same colour the tree's own rows would show that status in.
pub(crate) fn counts_line(state: &AppState) -> Line<'static> {
    let mut succeeded = 0;
    let mut cached = 0;
    let mut failed = 0;
    let mut pending_or_cancelled = 0;
    for row in &state.beams {
        match &row.state {
            BeamState::Pending => pending_or_cancelled += 1,
            BeamState::Running { .. } => {}
            BeamState::Done { status, .. } => match status {
                BeamStatus::Succeeded => succeeded += 1,
                BeamStatus::Cached => cached += 1,
                BeamStatus::Failed { .. } | BeamStatus::FailedAllowed { .. } => failed += 1,
                BeamStatus::Cancelled => pending_or_cancelled += 1,
            },
        }
    }
    let duration = Duration::from_secs(0);
    let buckets: [(&str, usize, BeamState); 4] = [
        (
            "✔",
            succeeded,
            BeamState::Done {
                status: BeamStatus::Succeeded,
                duration,
            },
        ),
        (
            "⚡",
            cached,
            BeamState::Done {
                status: BeamStatus::Cached,
                duration,
            },
        ),
        (
            "✖",
            failed,
            BeamState::Done {
                status: BeamStatus::Failed { exit_code: 1 },
                duration,
            },
        ),
        ("○", pending_or_cancelled, BeamState::Pending),
    ];
    let mut spans = Vec::with_capacity(buckets.len() * 2 - 1);
    for (index, (glyph, count, representative)) in buckets.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!("{glyph} {count}"),
            status_style(representative, state.colour),
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alba_core::BeamId;
    use ratatui::style::{Color, Style};

    fn state_with_phase(phase: Phase) -> AppState {
        let mut state = AppState::new("build", false);
        state.phase = phase;
        state
    }

    fn running(done: usize, total: usize, now: Instant) -> AppState {
        state_with_phase(Phase::Running {
            done,
            total,
            since: now - Duration::from_secs(3),
        })
    }

    #[test]
    fn the_bar_needs_fourteen_cells_and_stops_at_thirty() {
        assert_eq!(bar_width(13), None);
        assert_eq!(bar_width(14), Some(14));
        assert_eq!(bar_width(20), Some(20));
        assert_eq!(bar_width(55), Some(30));
    }

    /// The spec's own worked example: 2/5 done draws 6 of 14 cells filled.
    #[test]
    fn filled_cells_rounds_to_the_nearest_cell() {
        assert_eq!(filled_cells(2, 5, 14), 6);
        assert_eq!(filled_cells(0, 5, 14), 0);
        assert_eq!(filled_cells(5, 5, 14), 14);
        assert_eq!(filled_cells(0, 0, 14), 0);
    }

    #[test]
    fn a_run_in_flight_draws_the_bar_the_count_and_the_time() {
        let now = Instant::now();
        // 55 cells of room: "2/5 · 3.0s" (10) plus a space leaves 44,
        // capped at 30.
        assert_eq!(
            run_line(&running(2, 5, now), now, 55).to_string(),
            format!("{}{} 2/5 · 3.0s", "▰".repeat(12), "▱".repeat(18))
        );
    }

    #[test]
    fn a_narrow_footer_drops_the_bar_and_keeps_the_count() {
        let now = Instant::now();
        // 20 cells of room: 10 for the text, a space, 9 left: under 14.
        assert_eq!(run_line(&running(2, 5, now), now, 20).to_string(), "2/5 · 3.0s");
    }

    #[test]
    fn a_finished_run_reads_ok_or_failed_with_its_duration() {
        let mut state = state_with_phase(Phase::Finished);
        state.last_summary = Some(RunSummary {
            succeeded: vec![BeamId("a".into())],
            duration: Duration::from_millis(500),
            ..RunSummary::default()
        });
        assert_eq!(run_line(&state, Instant::now(), 55).to_string(), "ok · 0.5s");

        state.last_summary = Some(RunSummary {
            failed_allowed: vec![BeamId("a".into())],
            duration: Duration::from_secs(2),
            ..RunSummary::default()
        });
        assert_eq!(run_line(&state, Instant::now(), 55).to_string(), "failed · 2.0s");
    }

    #[test]
    fn an_empty_summary_reads_nothing_ran_and_no_summary_reads_idle() {
        let mut state = state_with_phase(Phase::Finished);
        assert_eq!(run_line(&state, Instant::now(), 55).to_string(), "idle");
        state.last_summary = Some(RunSummary::default());
        assert_eq!(run_line(&state, Instant::now(), 55).to_string(), "nothing ran");
    }

    /// A watch waiting between runs keeps the outcome on the footer.
    #[test]
    fn a_waiting_watch_still_shows_the_last_outcome() {
        let mut state = state_with_phase(Phase::Waiting { files: 3 });
        state.last_summary = Some(RunSummary {
            failed: vec![BeamId("a".into())],
            duration: Duration::from_secs(1),
            ..RunSummary::default()
        });
        assert_eq!(run_line(&state, Instant::now(), 55).to_string(), "failed · 1.0s");
    }

    /// `TestBackend::to_string()` (the render snapshots) drops styles, so
    /// this is what proves the bar and the outcome reach their colour
    /// functions with `state.colour`.
    #[test]
    fn colour_reaches_the_bar_and_the_outcome() {
        let now = Instant::now();
        let mut state = running(1, 2, now);
        state.colour = true;
        assert_eq!(
            run_line(&state, now, 55).spans[0].style,
            Style::new().fg(Color::Green)
        );

        let mut finished = state_with_phase(Phase::Finished);
        finished.colour = true;
        finished.last_summary = Some(RunSummary {
            failed: vec![BeamId("a".into())],
            ..RunSummary::default()
        });
        assert_eq!(
            run_line(&finished, now, 55).spans[0].style,
            Style::new().fg(Color::Red)
        );
    }
}
```

Register the module in `crates/alba-tui/src/ui/mod.rs`, first in the alphabetical list: `mod footer;` above `mod graphpane;`.

- [ ] **Step 2: Run the footer tests to verify they pass, and see what now fails to compile**

Run: `cargo test -p alba-tui footer::tests`
Expected: the eight tests PASS. (The functions are written alongside their tests here because `counts_line` is moved code and the rest is small; the render snapshots in Step 6 are the failing check for the layout itself.) `cargo clippy` will flag `counts_line` as unused until Step 4 wires it; that is expected for now.

- [ ] **Step 3: Take the counts row out of `ui/tree.rs` and point the header's bar at the footer's `filled_cells`**

In `crates/alba-tui/src/ui/tree.rs`:

- Replace the module doc's second paragraph (`The footer counts only ...` through `... would misreport which bucket it will land in.`) with nothing: the paragraph now lives in `footer.rs`. The first paragraph becomes:

```rust
//! The left pane: one row per beam with a status glyph and a
//! right-aligned duration, the selected row reversed. The counts by
//! status that used to close this pane sit on the frame's footer now
//! (see `ui/footer.rs`).
```

- In `draw`, the rows become two and the last `render_widget` goes away:

```rust
    let rows = Layout::vertical([
        Constraint::Length(1), // "BEAMS" title
        Constraint::Min(0),    // one row per beam
    ])
    .split(area);
```

  and delete the line `frame.render_widget(Paragraph::new(counts_line(state)), rows[2]);`.

- Delete the whole `counts_line` function (its doc comment included) from `tree.rs`. Remove `Duration` from the `std::time` import if nothing else in the file uses it (`duration_text` does not; check with `cargo build`), and remove `BeamStatus` from the `alba_engine` import only if `status_glyph` no longer needs it (it does; keep it).

In `crates/alba-tui/src/ui/header.rs`:

- Replace `let filled = filled_cells(*done, *total);` with `let filled = super::footer::filled_cells(*done, *total, BAR_WIDTH);`.
- Delete the header's own `filled_cells` function and its doc comment (`/// Deterministic on purpose ...` through its closing brace). `BAR_WIDTH` and everything else in the header stay until Task 3.

- [ ] **Step 4: Lay out the body, the junction, and the footer in `ui/mod.rs`**

In `crates/alba-tui/src/ui/mod.rs`:

- `MIN_HEIGHT` becomes 12 and its doc comment's parenthetical becomes `(spec: "terminal too small", roughly 40x12)`. The too-small message becomes `"terminal too small (need at least 40x12)"`.

- Replace everything in `draw` from `let inner = outer.inner(area);` to the end of the function with:

```rust
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    // Inside the frame, top to bottom: the body (the two panes, or the
    // graph), the junction line that closes the panes' columns, and the
    // footer carrying the run. The bottom edge below them is the outer
    // block's own.
    let rows = Layout::vertical([
        Constraint::Min(0),    // body
        Constraint::Length(1), // junction: ├───┴───┤
        Constraint::Length(1), // footer: counts, bar or outcome
    ])
    .split(inner);
    let (body, junction, footer) = (rows[0], rows[1], rows[2]);

    footer::draw(frame, footer, state, now);

    // Graph mode replaces the body entirely — no tree, no log pane, no
    // divider between them, and no junction closing columns that are
    // not there: the graph gets the junction's row too. The footer stays:
    // the run is still the run while the reader looks at its graph.
    if let Mode::Graph(graph) = &state.mode {
        let body = Rect {
            height: body.height + junction.height,
            ..body
        };
        graphpane::draw(frame, body, state, graph);
        return;
    }

    // The tree pane's region includes its own right border, which is
    // the divider between it and the log pane — one column wider than
    // the tree's actual content width.
    let panes =
        Layout::horizontal([Constraint::Length(TREE_WIDTH + 1), Constraint::Min(1)]).split(body);
    let divider = Block::new().borders(Borders::RIGHT);
    let tree_area = divider.inner(panes[0]);
    frame.render_widget(divider, panes[0]);

    tree::draw(frame, tree_area, state, now);
    logpane::draw(frame, panes[1], state);
    draw_junction(frame, area, junction);

    // Help is an overlay, not a replacement: the tree and log panes stay
    // drawn underneath it (dimmed), unlike graph mode's own early return
    // above, which takes over the body entirely instead.
    if matches!(state.mode, Mode::Help) {
        help::draw(frame, inner);
    }
}

/// The line closing the two panes' columns: `─` across the frame's
/// inside, `┴` where the divider lands on it, and the outer border's
/// own `│` on either side turned into `├`/`┤`. Those three cells are
/// written by hand: `Block` draws one rectangle's edges, and this row
/// is where three of them meet.
fn draw_junction(frame: &mut Frame, frame_area: Rect, junction: Rect) {
    frame.render_widget(
        Paragraph::new("─".repeat(junction.width as usize)),
        junction,
    );
    let buffer = frame.buffer_mut();
    buffer[(frame_area.x, junction.y)].set_symbol("├");
    buffer[(junction.x + TREE_WIDTH, junction.y)].set_symbol("┴");
    buffer[(frame_area.right() - 1, junction.y)].set_symbol("┤");
}
```

- Replace the body of `log_pane_content_area` after the floor check with:

```rust
    let inner = Block::bordered().inner(Rect::new(0, 0, width, height));
    let rows = Layout::vertical([
        Constraint::Min(0),    // body
        Constraint::Length(1), // junction
        Constraint::Length(1), // footer
    ])
    .split(inner);
    let panes =
        Layout::horizontal([Constraint::Length(TREE_WIDTH + 1), Constraint::Min(1)]).split(rows[0]);
    let pane = Layout::vertical([
        Constraint::Length(1), // "LOGS" title row
        Constraint::Min(0),    // output
    ])
    .split(panes[1]);
    Some(pane[1])
```

  and update its doc comment: `inside the title row` becomes `inside the title row, and above the junction and footer rows`.

- The test `log_pane_content_area_sits_past_the_tree_and_its_borders` becomes:

```rust
        // Outer border: 1 cell each side. Tree pane + divider: 31
        // columns. Title row: 1 line. Junction and footer: 2 lines.
        let area = log_pane_content_area(80, 24).expect("80x24 clears the floor");
        assert_eq!(area, Rect::new(32, 2, 47, 19));
```

- Add a test next to it pinning the junction cells:

```rust
    /// The junction row is where three block edges meet, and its three
    /// special cells are written by hand, so this pins them: `├` on the
    /// left edge, `┴` at the divider's foot, `┤` on the right edge.
    #[test]
    fn the_junction_closes_both_columns() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let state = AppState::new("build", false);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| draw(frame, &state, Instant::now()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        // Rows at 80x24: 23 is the bottom edge, 22 the footer, 21 the
        // junction, 20 the last body row.
        assert_eq!(buffer[(0, 21)].symbol(), "├");
        assert_eq!(buffer[(31, 21)].symbol(), "┴");
        assert_eq!(buffer[(79, 21)].symbol(), "┤");
        assert_eq!(buffer[(31, 20)].symbol(), "│", "the divider reaches the junction");
    }
```

- [ ] **Step 5: Update the geometry numbers in `lib.rs`'s tests**

In `crates/alba-tui/src/lib.rs`, the same four sites as Task 1, now with the final numbers:

- `EnterCopy` anchor test: `// 80x24 gives the log pane a content height of 19 rows`, `// buffer, the top visible line is 30 - 19 = 11.`, `assert_eq!(copy.anchor, (11, 0));`, `assert_eq!(copy.cursor, (11, 0));`.
- Mouse drag test: `fit inside the 19-row content height`.
- Half-page test: doc comment `At 80x24 the pane shows 19 rows, so half is 9.`; message `"half of the pane's 19 content rows"`; the offsets become `9`, `18`, `9`, then `Following`.
- `the_page_distance_is_half_the_panes_own_height`: `assert_eq!(half_pane(19), 9, "19 content rows at 80x24");` and `assert_eq!(half_pane(7), 3, "7 content rows at the floor");` (at `40x12`: 10 rows inside the frame, 8 in the body, 7 under the title). The doc comment keeps `whose 7 content rows halve to 3`.

- [ ] **Step 6: Run the unit tests**

Run: `cargo test -p alba-tui --lib`
Expected: PASS, including `footer::tests`, `ui::tests::the_junction_closes_both_columns`, and the geometry tests.

- [ ] **Step 7: Re-accept the render snapshots**

Run: `INSTA_UPDATE=always cargo test -p alba-tui --test render`, then `git diff crates/alba-tui/tests/snapshots/`.

Expected in `a_run_in_flight_renders_tree_bar_and_logs`:

```text
"├──────────────────────────────┴───────────────────────────────────────────────┤"
"│ ✔ 1  ⚡ 1  ✖ 0  ○ 2                ▰▰▰▰▰▰▰▰▰▰▰▰▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱ 2/5 · 0.0s │"
"└─ q quit · r rerun · c cancel · w watch · / search · ? help ──────────────────┘"
```

as the last three rows (insta may add a `Hidden by multi-width symbols` note on the footer row for `⚡`, as it does today for the tree). The header still shows its own bar for this one commit; Task 3 removes it. Expected in `a_failed_run_renders_the_cross_and_summary_counts`: the footer reads `✔ 0  ⚡ 0  ✖ 1  ○ 0` left and `failed · 0.5s` right. In `a_waiting_session_renders_the_watch_state` and `a_parked_session_renders_the_diagnostic`: `idle` right. In the two graph snapshots: no junction row, the graph has one more row, the footer is present. In `the_help_overlay_lists_the_full_keymap`: the box is centred in a body one row shorter; the footer row is present. The tree's old counts row is gone everywhere. `a_tiny_terminal_gets_the_too_small_screen` (30x8) changes only its message to `40x12`.

- [ ] **Step 8: Run the gate and commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace
git add crates/alba-tui/src/ui/footer.rs crates/alba-tui/src/ui/tree.rs crates/alba-tui/src/ui/header.rs crates/alba-tui/src/ui/mod.rs crates/alba-tui/src/lib.rs crates/alba-tui/tests/snapshots/
git commit -m "💄 feat(tui): draw the run on a footer under a real junction" -m "The counts leave the tree's last row for a full-width footer above the
bottom edge, joined by the run's progress bar, count, and elapsed time
while it is going and by its outcome (ok or failed, with the duration)
once it is over. A junction line closes the two panes' columns above
it, so the divider runs the full height of the body and ends in a ┴.

The bar fits the room left beside the counts, capped at 30 cells and
dropped under 14. The floor moves to 40x12 for the two extra rows."
```

---

### Task 3: The header keeps to the target and the session state

**Files:**
- Modify: `crates/alba-tui/src/ui/header.rs` (whole file)
- Modify: `crates/alba-tui/src/ui/mod.rs:53-72` (the outer block's titles), `:100-110` (`framed_title`)
- Modify: `crates/alba-cli/tests/tui_pty.rs:11-12` (the green fixture), `:24-58` (the sentinel and its doc comment), `:160-178` (the wait loop), `:234` (the `drive` call)
- Modify: `README.md:458-500` (the Layout section)
- Test: `crates/alba-tui/src/ui/header.rs` (unit), `crates/alba-tui/tests/render.rs` (snapshots), `crates/alba-cli/tests/tui_pty.rs`

**Interfaces:**
- Consumes: `theme::parked_style`, `AppState::{target, phase, colour}`.
- Produces: `header::line(state: &AppState) -> Line<'static>` (no `now`, no bar width) and `header::session(state: &AppState) -> Option<Line<'static>>`.

- [ ] **Step 1: Rewrite `ui/header.rs` with its tests**

Replace the whole of `crates/alba-tui/src/ui/header.rs` with:

```rust
//! The top edge's two titles: the run's identity on the left (`alba ·
//! {target}`), and, on the right, what the session is doing besides the
//! run, when there is anything to say: a watch waiting between runs,
//! or a project parked on a broken Beamfile. The run's own progress and
//! outcome are the footer's (see `ui/footer.rs`), so a watch never has
//! to evict them to say it is watching.
//!
//! Neither text is rendered into its own pane: both sit inside the
//! outer frame's top border (see `ui/mod.rs`), the way the spec's
//! mockup draws them (`┌─ alba · build ──── ... ── watching 3 files ─┐`).

use ratatui::text::{Line, Span};

use crate::state::{AppState, Phase};

use super::theme::parked_style;

/// The left-hand title.
pub fn line(state: &AppState) -> Line<'static> {
    Line::from(format!("alba · {}", state.target))
}

/// The right-hand title, when the session has a state of its own to
/// report. A single styled span, not `Line::styled` (which sets the
/// line's own style rather than a span's): `framed_title` (`ui/mod.rs`)
/// only carries a line's spans into the border's title, so the colour
/// has to live on the span to survive there.
pub fn session(state: &AppState) -> Option<Line<'static>> {
    match &state.phase {
        Phase::Waiting { files } => {
            let plural = if *files == 1 { "" } else { "s" };
            Some(Line::from(format!("watching {files} file{plural}")))
        }
        Phase::Parked => Some(Line::from(vec![Span::styled(
            "parked · waiting for a valid Beamfile".to_string(),
            parked_style(state.colour),
        )])),
        Phase::Running { .. } | Phase::Finished => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};
    use std::time::Instant;

    fn state_with_phase(phase: Phase) -> AppState {
        let mut state = AppState::new("build", false);
        state.phase = phase;
        state
    }

    #[test]
    fn the_left_title_is_the_identity_whatever_the_phase() {
        let running = state_with_phase(Phase::Running {
            done: 2,
            total: 5,
            since: Instant::now(),
        });
        assert_eq!(line(&running).to_string(), "alba · build");
        assert_eq!(
            line(&state_with_phase(Phase::Finished)).to_string(),
            "alba · build"
        );
    }

    #[test]
    fn the_session_title_is_absent_while_running_or_finished() {
        assert!(session(&state_with_phase(Phase::Finished)).is_none());
        assert!(
            session(&state_with_phase(Phase::Running {
                done: 0,
                total: 1,
                since: Instant::now(),
            }))
            .is_none()
        );
    }

    #[test]
    fn a_waiting_watch_names_its_file_count() {
        assert_eq!(
            session(&state_with_phase(Phase::Waiting { files: 3 }))
                .unwrap()
                .to_string(),
            "watching 3 files"
        );
        assert_eq!(
            session(&state_with_phase(Phase::Waiting { files: 1 }))
                .unwrap()
                .to_string(),
            "watching 1 file"
        );
    }

    /// `TestBackend::to_string()` (the render snapshots) drops styles, so
    /// this is what proves the parked text reaches its colour function
    /// with `state.colour`.
    #[test]
    fn a_parked_project_is_named_in_the_parked_colour() {
        let mut parked = state_with_phase(Phase::Parked);
        parked.colour = true;
        let title = session(&parked).unwrap();
        assert_eq!(title.to_string(), "parked · waiting for a valid Beamfile");
        assert_eq!(title.spans[0].style, Style::new().fg(Color::Yellow));
    }
}
```

- [ ] **Step 2: Run the header tests to see the callers fail**

Run: `cargo test -p alba-tui header::tests`
Expected: compile error in `ui/mod.rs`: `header::line` takes one argument, not two.

- [ ] **Step 3: Give the outer block both titles in `ui/mod.rs`**

In `crates/alba-tui/src/ui/mod.rs`'s `draw`, replace the block from the comment `// The outer frame carries the header and the bottom bar ...` down to `.title_bottom(...)` with:

```rust
    // The outer frame carries the header and the bottom bar inside its
    // own border, exactly as the spec's mockup draws them
    // (`┌─ alba · build ──── ... ── watching 3 files ─┐` / `└─ q quit ·
    // ... ─┘`): a leading `─ ` and trailing ` ` (mirrored for the
    // right-hand title) are baked into the title itself so the block's
    // own border fill supplies the rest of the dashes, wrapped around
    // the header's and the bar's own spans rather than their plain text,
    // so the colour underneath survives into the border's title. The
    // session state is a second, right-aligned title on the same edge,
    // so it never displaces the identity.
    let mut outer = Block::bordered()
        .title_top(framed_title(header::line(state)))
        .title_bottom(framed_title(theme::bar_line(
            &bottom_bar(state, now),
            state.colour,
        )));
    if let Some(session) = header::session(state) {
        outer = outer.title_top(framed_title_right(session).right_aligned());
    }
```

Below `framed_title`, add:

```rust
/// `framed_title`'s mirror image for a title on the right end of an
/// edge: ` ... ─`, so the border's fill meets it from the left.
fn framed_title_right(content: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    spans.extend(content.spans);
    spans.push(Span::raw(" ─"));
    Line::from(spans)
}
```

`Line::right_aligned` is in `ratatui::text::Line` already imported; no new import needed.

- [ ] **Step 4: Run the unit tests**

Run: `cargo test -p alba-tui --lib`
Expected: PASS, including the four `header::tests`.

- [ ] **Step 5: Re-accept the render snapshots**

Run: `INSTA_UPDATE=always cargo test -p alba-tui --test render`, then `git diff crates/alba-tui/tests/snapshots/`.

Expected: every 80x24 snapshot's first row is now `┌─ alba · build ───...───┐` with no bar, no counts, no `finished`; `a_waiting_session_renders_the_watch_state` ends its first row with `── watching 42 files ─┐`; `a_parked_session_renders_the_diagnostic` ends it with `── parked · waiting for a valid Beamfile ─┐`. Rows 1 to 23 are unchanged from Task 2. Compare `a_run_in_flight_renders_tree_bar_and_logs` line by line with the spec's mockup: the only differences allowed are the log lines' content (the test feeds one line) and the timings.

- [ ] **Step 6: Move the pty smoke test's sentinel to the footer's outcome word**

In `crates/alba-cli/tests/tui_pty.rs`:

- Line 12: the green fixture's beam is renamed so that its name does not spell the sentinel: `const GREEN: &str = "version \"1\"\n\nbeam green {\n  run \"echo hello-from-the-beam\"\n}\n";`. Line 234: `drive(GREEN, "green", &[])`.
- Replace the `RUN_FINISHED` constant and its whole doc comment (from `/// The word the header's own account ...` to `const RUN_FINISHED: &str = "finished";`) with:

```rust
/// The outcome word the footer draws once a run is over (see
/// `alba-tui/src/ui/footer.rs::outcome_line`: `ok · {duration}` or
/// `failed · {duration}`, right-aligned) and nothing else in this
/// interface ever renders: the fixtures' beams are `green` and `red`,
/// the bottom bar, the pane titles, and the follow state spell neither
/// word, and the beams' own output does not either. Unlike that output,
/// which lands on the pty as soon as `RunEvent::BeamOutput` is applied,
/// the word is only drawn once `Phase::Finished` and `last_summary` are
/// set, in the very same `RunEvent::RunFinished` match arm that sets
/// `AppState::outcome` (`state.rs`, the field `exit_outcome()` reads).
/// So seeing it on the pty is synchronized with the exit code actually
/// being decided.
///
/// It also has to survive ratatui's own diffing, which this test does
/// not get to skip: `Buffer::diff` only forwards a cell whose symbol or
/// style changed from the previously drawn frame, and silently
/// cursor-jumps over one that did not. The frame right before the
/// outcome's first appearance shows, at the footer's right end, either
/// `idle` (drawn once before any event lands) or the in-flight text
/// `{bar} {done}/{total} · {duration}` (drawn on every `RunEvent` batch
/// while the beam runs). Both words are right-aligned, so their letters
/// land on cells that held, in the predecessor, a bar cell, a digit, a
/// `/`, a space, or nothing (`idle` is four cells wide and both words
/// sit further left than that), never the same Latin letter. Every
/// letter therefore differs from its predecessor and is emitted,
/// contiguously, whatever the run's timings.
const RUN_OK: &str = "ok";
const RUN_FAILED: &str = "failed";
```

- In the wait loop's leading comment (around line 162), replace `` `q` must not be sent until `RUN_FINISHED` `` with `` `q` must not be sent until `RUN_OK` or `RUN_FAILED` ``. Replace the loop's break condition with:

```rust
        if seen.contains(ALTERNATE_SCREEN_ENTER)
            && (seen.contains(RUN_OK) || seen.contains(RUN_FAILED))
        {
            break;
        }
```

- Search the file for `RUN_FINISHED`: no mention may remain.

- [ ] **Step 7: Run the pty smoke tests**

Run: `cargo test -p alba-cli --test tui_pty`
Expected: PASS, both tests (`the_tui_opens_restores_and_replays_on_q` and, on unix, `the_replay_keeps_colour_on_a_terminal_and_drops_it_under_no_color`), each well under its own deadline. A timeout with `the run never finished on the pty` means the sentinel never reached the stream: check `git diff crates/alba-tui/tests/snapshots/` for the outcome word's exact spelling before touching the test.

- [ ] **Step 8: Update the README's Layout section**

In `README.md`, replace the mockup (the fenced `text` block after `### Layout`) with:

```text
┌─ alba · build ──────────────────────────────────────────── watching 3 files ─┐
│BEAMS                         │LOGS                               ● following │
│ ✔ codegen                1.2s│Compiling proc-macro2 v1.0.86                  │
│ ⚡ api:codegen           0.8s│Compiling serde v1.0.210                       │
│ ▶ api:build             3.4s…│Compiling api v0.1.0 (/repo/api)               │
│ ○ build                      │warning: unused import: `std::fmt`             │
│ ○ test                       │  --> src/lib.rs:4:5                           │
│                              │                                               │
├──────────────────────────────┴───────────────────────────────────────────────┤
│ ✔ 1  ⚡ 1  ✖ 0  ○ 2                ▰▰▰▰▰▰▰▰▰▰▰▰▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱▱ 2/5 · 4.2s │
└─ q quit · r rerun · c cancel · w watch · / search · ? help ──────────────────┘
```

Replace the paragraph `The header and the bottom bar are not panes of their own ...` and the bullet list that follows it (`- **Header**` through `- **Bottom bar**`) with:

```markdown
The header and the bottom bar are not panes of their own: they sit inside
the top and bottom edge of a single outer frame. Inside it, one vertical
divider separates the tree from the log pane, and a junction line closes
both columns above the footer.

- **Header**: `alba · {target}` on the left; on the right, between runs in
  a watch session, how many files it is watching, or `parked` when the
  project cannot load.
- **Tree** (left, 30 columns): one row per beam, `✔` succeeded, `⚡`
  cached, `▶` running (its own duration ticking), `✖` failed (an allowed
  failure included), `○` pending or cancelled. A name longer than its
  column is cut with a trailing `…`.
- **Logs** (right): the selected beam's output, following the tail by
  default; the title row shows `● following` or `↑ paused`. Scrolling up
  (the wheel, or the keys below) pauses following, so you can read in
  peace while the run continues; `G`, or scrolling back down to the
  bottom, resumes it. Long lines wrap by character; scrolling moves by
  whole lines. When a beam fails and you have not moved the selection
  since the run started, the selection jumps to it (the first failure
  only).
- **Footer**: the run. Left, the beams counted by status (`▶` left out of
  the tally, since it has not settled yet). Right, while a run is going,
  its progress bar, done/total, and elapsed time, ticking live; once it
  ends, `ok` or `failed` and the duration. The bar grows with the
  terminal, up to 30 cells, and gives way to the count alone when fewer
  than 14 cells are left for it.
- **Bottom bar**: the actions available in whatever mode is active, the
  keymap below condensed to what fits.
```

Replace `Below roughly 40 columns by 10 rows` with `Below roughly 40 columns by 12 rows`.

- [ ] **Step 9: Run the gate and commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace
git add crates/alba-tui/src/ui/header.rs crates/alba-tui/src/ui/mod.rs crates/alba-tui/tests/snapshots/ crates/alba-cli/tests/tui_pty.rs README.md
git commit -m "💄 feat(tui): keep the header to the target and the session" -m "The top edge now carries the run's identity on the left and, on the
right, only what the session is doing besides the run: a watch waiting
between runs, or a project parked on a broken Beamfile. The bar, the
counts, and the outcome have moved to the footer, so a watch no longer
evicts the last run's outcome to say it is watching.

The pty smoke test waits on the footer's outcome word (ok or failed)
instead of the header's finished; the green fixture's beam is renamed
so its own name does not spell the sentinel."
```

---

## Self-review against the spec

- Top edge, both titles: Task 3. Pane title rows with the follow state and `DIAGNOSTIC`: Task 1. Tree without counts, full-height divider ending in `┴`, junction line: Task 2. Footer contents (counts left; bar, count, time; `ok`/`failed`; `nothing ran`; `idle`): Task 2. Graph mode keeping the footer and taking the junction's row: Task 2. Help overlay dimming the whole inside of the frame: unchanged (`help::draw(frame, inner)`), Task 2 keeps the call. Bar cap 30, floor 14, omission: Task 2 (`bar_width`). `MIN_HEIGHT` 12: Task 2. `log_pane_content_area` and copy mode's mouse hit-testing: Tasks 1 and 2. Pty sentinel: Task 3. README: Task 3.
- The spec's Key decisions table says the bar width is computed in `ui/mod.rs`; this plan computes it in `ui/footer.rs`, where the room is known. The spec's table cell is updated alongside this plan.
- Names used across tasks: `footer::filled_cells` (Task 2, called by `header.rs` until Task 3 deletes the call), `header::line(state)` and `header::session(state)` (Task 3, both consumed by `ui/mod.rs`'s `draw` in the same task), `logpane::title` and `logpane::follow_text` (Task 1, consumed only inside `logpane.rs`).
