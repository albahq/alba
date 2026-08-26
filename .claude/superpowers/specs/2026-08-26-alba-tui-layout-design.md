# Alba - TUI Layout

Date: 2026-08-26 Status: approved

## Overview

A follow-up to the [TUI design](2026-07-30-alba-tui-design.md) and the [TUI polish](2026-08-26-alba-tui-polish-design.md).
The interface works, but its top edge carries too much: the run's identity, its progress bar, its counts,
its duration, and the session state all share one left-aligned title, while the rest of the frame is
mostly empty. Two symptoms follow from that:

- Once a run ends, the header's counts (`✔ 1  ✖ 1`) repeat the tree's own footer (`✔ 0  ⚡ 0  ✖ 1  ○ 0`).
- In a watch session, the waiting state (`waiting · 3 files watched`) evicts the last run's outcome from
  the header entirely; only the tree's footer still says the run failed.

This sub-project gives every band of the frame one job. It changes nothing in the session contract, the
keymap, the log buffers, search, copy, or the graph's own drawing.

## Goals

- Each band of the frame carries one kind of information: identity and session state on the top edge, the
  run's progress on a footer of its own, the keymap on the bottom edge, and each pane's own state on its
  title row.
- No information is drawn twice.
- The two panes read as two columns closed by a proper junction, not as columns a stray line cuts short.
- A watch session shows the last run's outcome and the watch state at the same time.
- The progress bar sizes itself to the terminal.

## Non-goals

- A configurable layout or a resizable tree column.
- Changing what the headless renderers print.
- New keys, modes, or colours: the theme of the polish sub-project is reused as is.
- Word-level changes to the help overlay.

## Layout

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

From top to bottom:

- **Top edge**: `alba · {target}` on the left. On the right, the session state when there is one:
  `watching {n} file(s)` between the runs of a watch session, `parked · waiting for a valid Beamfile` (in
  the parked colour) when the project cannot load, nothing otherwise. Both sit in the border as titles, as
  the header does today.
- **Pane title rows**: `BEAMS` on the left. `LOGS` on the right, with the follow state (`● following` or
  `↑ paused`) right-aligned on the same row. While parked, the right title reads `DIAGNOSTIC`, since the
  pane shows the diagnostic that parked the project rather than a beam's output. The beam's name is no
  longer printed: the selected row of the tree already names it.
- **Panes**: unchanged, except that the tree loses its counts row and the log pane loses its follow-state
  row. The divider between them runs the full height of the body and ends in a `┴`.
- **Junction line**: `├───┴───┤`, part of the frame, closing the two columns.
- **Footer**: the run. Left, the counts by status in the tree's own colours (`✔ ⚡ ✖ ○`, the same four
  buckets as today, `○` counting pending and cancelled beams alike). Right, aligned to the edge:
  - while a run is going: the progress bar, `{done}/{total}`, and the elapsed time ticking live;
  - once it ends: `ok · {duration}` or `failed · {duration}`, in the outcome colour (`outcome_style`, which
    already treats an allowed failure as a failure); `nothing ran` when the summary is empty;
  - before any run: `idle`.
- **Bottom edge**: the keymap, unchanged.

Graph mode replaces the body (both panes and the junction line) and keeps the footer: the run is still the
run while the reader looks at its graph. The help overlay dims everything inside the frame, footer
included, as it dims the body today.

## Progress bar

The bar fills the footer between the counts and the right-hand text, capped at 30 cells. At 80 columns
that is 30 cells; narrower terminals shrink it down to the mockup's original 14, and below that (fewer than
14 cells left once the counts, the gaps, and `{done}/{total} · {elapsed}` are placed) the bar is omitted and
only the count and the time are drawn. The bar has only `total + 1` distinct states, so cells past 30 would
make each step jump further without saying anything new. `filled_cells` keeps its rounding rule
(`round(done / total * width)`), with the width as a parameter instead of a constant.

## Terminal floor

The frame now spends five rows on chrome (top edge, title row, junction, footer, bottom edge). The
too-small floor moves from `40x10` to `40x12`, so a terminal that clears it always shows at least a few
beams and a few log lines.

## Key decisions

| Topic               | Decision                                                                                       |
|---------------------|------------------------------------------------------------------------------------------------|
| Header              | Identity left, session state right, both as titles in the top border                           |
| Outcome placement   | Footer, right-aligned; the header never shows a run's outcome or progress                      |
| Counts              | Footer, left; removed from the tree                                                            |
| Follow state        | Right-aligned on the `LOGS` title row; removed from the log pane's last row                    |
| Junction            | A real `├ ┴ ┤` line, one row, rather than a footer that cuts the divider short                 |
| Bar width           | Footer width minus counts and right text, capped at 30, omitted under 14; `ui/mod.rs` decides  |
| Outcome word        | `ok` / `failed`; the pty smoke test synchronizes on it (see Testing)                           |
| Floor               | `MIN_HEIGHT` 10 to 12                                                                          |

## Where it lives

- `alba-tui/src/ui/mod.rs`: the layout (body, junction, footer), the junction glyphs (a custom border set
  for the body block's bottom corners plus the `┴` cell at the divider's foot), the bar width, and
  `log_pane_content_area`, which now excludes the junction and the footer rows and no longer reserves a
  follow-state row. The copy mode's mouse handling reads that function, so its hit-testing follows.
- `alba-tui/src/ui/header.rs`: `line` becomes the identity title; a new `session` returns the optional
  right-hand title. The bar, the outcome, and `finished_line` leave this module.
- `alba-tui/src/ui/footer.rs` (new): the counts line (moved from `tree.rs`), the run text, and the bar.
- `alba-tui/src/ui/tree.rs`: loses its counts row.
- `alba-tui/src/ui/logpane.rs`: the title row carries `LOGS`/`DIAGNOSTIC` and the follow state; the last
  row is gone.
- `alba-tui/src/ui/graphpane.rs`, `help.rs`: unchanged; they receive the areas `mod.rs` hands them.
- `alba-tui/tests/render.rs` and its snapshots: re-accepted against the new layout.
- `alba-cli/tests/tui_pty.rs`: the finished-run sentinel.
- `README.md`: the Layout section and its mockup.

The two earlier specs stay as they are; this one supersedes their Layout sections.

## Error handling

Nothing new fails here. A terminal too narrow for the bar drops it (see Progress bar); below the `40x12`
floor the too-small screen takes over. At 40 columns the frame leaves 38 cells, the counts take at most 21,
and the longest bar-less right text (`failed · 12.3s`, 14 cells) still fits beside them.

## Testing strategy

- `ui/footer.rs`: the running line at a given width draws the expected bar, count, and time; the finished
  line reads `ok`/`failed` with the outcome colour; an empty summary reads `nothing ran`; no summary reads
  `idle`; the counts line matches what `tree.rs` produced.
- `ui/header.rs`: identity only on the left; `session` is `None` when idle or finished, names the watched
  files while waiting, and carries the parked colour while parked.
- `ui/mod.rs`: the bar width at 40, 60, 80 columns is omitted, 14 or more, and the 30-cell cap;
  `log_pane_content_area` at `80x24` returns the content rectangle one row shorter than today and starting
  on the same row; `None` below the new floor.
- `ui/logpane.rs`: the title row reads `LOGS` with `● following` at its right edge, `↑ paused` once
  scrolled, `DIAGNOSTIC` while parked.
- Render snapshots re-accepted; each is read against this spec's mockup before acceptance.
- `alba-cli/tests/tui_pty.rs`: `RUN_FINISHED` becomes the outcome word the fixture produces (`ok`). The
  argument that kept `finished` safe under ratatui's cell diffing still holds: the footer's right-hand text
  while running is made of bar cells, digits, `/`, `·`, spaces, and the trailing `s` of the duration, never
  a Latin letter other than that `s`; the idle text (`idle`) occupies the last four cells only. Both
  letters of `ok` therefore land on cells no predecessor frame drew a letter on, so the diff always emits
  them.

## Success criteria

1. `alba run` on this repository shows `alba · build` top left, `BEAMS` and `LOGS ... ● following` as pane
   titles, a full-height divider closed by `┴`, and the counts plus the bar on one footer row above the keys.
2. `alba watch`, after a failing run, shows `failed · {duration}` in the footer and `watching {n} files`
   top right at the same time.
3. At 80 columns the bar is 30 cells wide; at 40 columns, the footer shows the count and the time alone.
4. Copy mode's mouse selection still lands on the clicked line after the layout change.
5. CI green on macOS, Linux, and Windows.
