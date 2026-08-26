# Alba - TUI Polish

Date: 2026-08-26 Status: approved

## Overview

A follow-up to the [TUI design](2026-07-30-alba-tui-design.md). The interface works (modes, search, copy,
watch, graph, help, render snapshots) but reads worse than the tools it competes with: no colour anywhere,
tool output arrives monochrome and would show raw escape codes if it were not, long lines are cut, a failure
in flight does not draw the eye, and stderr is indistinguishable from stdout. This sub-project fixes the
presentation without touching the session contract or the keymap's semantics.

Five blocking items and two small ones:

1. A colour theme for the tree, header, counts, graph, and bottom bar.
2. Coloured tool output: force colour in the commands the TUI runs, and render the ANSI it produces.
3. Line wrapping in the log pane.
4. The selection jumps to the first failure of a run, unless the reader has moved it.
5. stderr lines drawn dimmed.
6. Long beam names truncated with an ellipsis instead of overflowing the tree column.
7. A lighter Normal-mode bottom bar.

## Goals

- Colour that carries meaning (status, outcome, focus) and respects `NO_COLOR`.
- `cargo`, `eslint`, `pytest`, and friends look in the log pane the way they look in a terminal.
- Nothing a command prints is hidden by the pane's width.
- A failed `alba run` shows the failure without navigation.
- Search, copy, scroll, and the exit replay keep working exactly as documented, over the new rendering.

## Non-goals

- A configurable theme or keymap (still on demand).
- Word wrapping; wrapping is by character.
- 256-colour or truecolour in Alba's own theme (tool output that uses them is rendered as is).
- Tree indentation by dependency, or mouse clicks on the tree (the graph view and `j`/`k` cover both).
- Changing what the headless renderers print: they stay monochrome pipes of the command's own bytes.

## Key decisions

| Topic               | Decision                                                                                          |
|---------------------|---------------------------------------------------------------------------------------------------|
| Colour gate         | One `bool` decided in `alba-cli` (`stdout` is a terminal and `NO_COLOR` unset), passed to the TUI |
| Theme               | Fixed, the 16 ANSI colours only; status colours shared by tree, counts, header, and graph         |
| Forcing colour      | `RunOptions.extra_env`, set by the CLI to `FORCE_COLOR=1` and `CLICOLOR_FORCE=1` for a coloured TUI |
| Cache fingerprint   | `extra_env` reaches the executors, never `CacheableBeam.env`: TUI and headless runs share entries |
| ANSI rendering      | Parsed once at `push` with `ansi-to-tui` into styled spans plus a plain `text`                    |
| Search and copy     | Operate on the plain `text`; only the renderer sees the spans                                    |
| Exit replay         | Raw bytes when colour is enabled, plain `text` otherwise                                          |
| Wrapping            | Visual rows computed at render time from the pane width; scroll stays in logical lines            |
| Failure jump        | First failing beam of a run, only if the selection has not moved since `RunStarted`               |
| stderr              | Dimmed when the line carries no colour of its own; a coloured stderr line keeps its colours        |
| Bottom bar (Normal) | `q quit · r rerun · c cancel · w watch · / search · ? help`; the rest lives in `?`                |

## Colour theme

The gate stays `alba_cli::color_enabled`: `alba_tui::run` takes a
`colour: bool` alongside its existing inputs, and the CLI computes it with the same rule `color_enabled`
applies today. With colour off, every style below collapses to the current monochrome rendering, so the
existing snapshot tests keep describing the `NO_COLOR` output.

| Element                | Style                                                                              |
|------------------------|------------------------------------------------------------------------------------|
| `✔` succeeded          | green                                                                              |
| `⚡` cached             | cyan                                                                               |
| `▶` running            | yellow                                                                             |
| `✖` failed             | red (an allowed failure included)                                                  |
| `○` pending, cancelled | dim                                                                                |
| Selected tree row      | reversed, unchanged                                                                |
| Progress bar           | filled part green while running                                                    |
| Header outcome         | green when no beam failed, red otherwise; yellow while parked; default when idle   |
| Counts footer          | each glyph and its count in the glyph's colour                                     |
| Graph nodes            | the status colour of the beam; the focused node stays reversed                     |
| Bottom bar             | key names bold, labels default                                                     |
| Search matches         | reversed, unchanged                                                                |
| Copy selection         | reversed, unchanged                                                                |

The status colour lives in one function (`status_style(&BeamState, colour) -> Style`) that the tree, the
counts line, the header, and the graph all call; the README's "colored by status" becomes true.

## Coloured tool output

### Forcing colour in the commands

Commands run on pipes, so on their own they never emit colour. `RunOptions` gains
`extra_env: Vec<(String, String)>`, empty by default. When the CLI opens the TUI with colour enabled it sets
`FORCE_COLOR=1` and `CLICOLOR_FORCE=1`; headless runs, `alba check`, and hooks leave it empty. `extra_env` is
applied first, so anything the Beamfile's own `env` sets on the same name wins.

The scheduler appends `extra_env` to the environment handed to the executor (`CommandSpec.env`), which reaches
the system shell, the embedded shell, docker (through the `-e` flags `exec_args` already builds), and plugins
(through the environment the protocol already carries). It is not part of `CacheableBeam.env`, so the
fingerprint of a beam is the same whether the TUI or a pipe ran it, and switching front ends never invalidates
the cache.

### Storing what arrives

`LogLine` becomes:

```rust
pub struct LogLine {
    /// The line with every escape sequence removed. Search, copy, wrapping widths,
    /// and the NO_COLOR replay read this.
    pub text: String,
    /// The same content split into styled runs. Only the renderer reads this.
    pub spans: Vec<(Style, String)>,
    pub stream: Stream,
    pub replayed: bool,
}
```

`LogBuffer::push` takes the raw `OutputLine` and parses it once with `ansi-to-tui` (new dependency; SGR
parsing has enough corner cases, 256-colour and truecolour included, that a hand-written parser is the wrong
economy). Anything the parser does not understand (cursor movement, OSC) is dropped from both `text` and
`spans`. The parked diagnostic goes through the same path, so `miette`'s coloured output renders too.

### Replaying on exit

`TuiOutcome.failed_logs` carries both forms. `alba-cli` prints the raw bytes when its own colour gate is on
and `text` otherwise, so a failure replayed into a terminal keeps the compiler's colours, and a captured one
stays clean.

## Wrapping

`LogBuffer::view(height)` becomes `view(height, width) -> Vec<Row>` where a `Row` names the logical line it
comes from, its char range within that line, and whether it is the first row of that line. Wrapping is by
character, using the rendered width (so a `⚡` in tool output counts two cells). The follow state and the
scroll offset keep their meaning in logical lines: following shows the last `height` rows of the buffer;
paused at `offset` shows the rows ending at the tail minus `offset` lines. `PgUp`/`PgDn`, `Ctrl-u`/`Ctrl-d`,
and the wheel still move by rows and translate to whole lines, rounding toward the direction of travel, so a
page never lands mid-line.

The truncation marker `… N older lines truncated` is a row of its own, never wrapped.

Copy mode and search keep `(line, column)` coordinates over `text`. `copy::line_for_pane_row` becomes
`row_at(buffer, height, width, pane_row) -> Option<(line, column_offset)>` and is the single translation the
mouse hit test, the selection highlight, and the search highlight all use; a highlight that spans a wrap
boundary is drawn on both rows.

## Failure jump

`AppState` gains `selection_moved: bool` and `jumped_to_failure: bool`, both reset to `false` on
`RunStarted`. `select_next`, `select_previous`, and the graph's `Enter` set `selection_moved`. On
`BeamFinished` with a failing status that is not an allowed failure: if neither flag is set, the beam is
selected and `jumped_to_failure` is set. A second failure in the same run does not move the selection, and a
rerun (`r`, `f`, `t`) starts the rule afresh.

`--keep-going` changes nothing: the first failure still jumps while the rest of the run continues.

## stderr

A line whose `stream` is `Stderr` and whose spans all carry the default style is drawn dimmed. A stderr line
that brought its own colours (a compiler's red `error:`) is drawn as it came. No prefix, so column alignment
of tool output is preserved, and the copy result stays free of decoration.

## Tree names

`row_text` truncates a name longer than the name column to `width - 1` characters followed by `…`, keeping
the duration column in place. The tree stays 30 columns wide.

## Bottom bar

Normal mode shows `q quit · r rerun · c cancel · w watch · / search · ? help`. `f`, `t`, `n`/`N`, `g`, and
`v` move to the help overlay only. Search, Copy, Graph, and Help bars are unchanged, as is the shared-constant
rule between `bottom_bar` and `help::keymap_lines`.

## Where it lives

- `alba-engine`: `RunOptions.extra_env` and its two uses in `scheduler.rs` (executor environment, not the
  cache facts).
- `alba-tui`: `logs.rs` (parsing, `Row`, `view`), `copy.rs` (`row_at`), `state.rs` (the two flags and the
  jump), `ui/theme.rs` (new: `status_style` and `bar_line`, the bold key style), `ui/tree.rs`,
  `ui/header.rs`, `ui/logpane.rs`, `ui/graphpane.rs`, `ui/mod.rs`.
- `alba-cli`: the colour gate passed to the TUI, `extra_env` set on the TUI path, the replay choosing raw or
  plain.
- `README.md`: the Layout and Keymap sections, the new "Colour" paragraph (theme, `NO_COLOR`, forced colour
  in commands and why the cache does not notice), and the corrected graph description.

## Error handling

- An ANSI sequence the parser rejects is dropped, never shown raw; the line is still stored.
- A terminal that reports no colour support is not detected: `NO_COLOR` is the reader's lever, as it is for
  the headless renderers.
- `FORCE_COLOR` reaching a plugin executor that ignores it is harmless: the output stays monochrome and
  renders as before.

## Testing strategy

Render snapshots (`TestBackend::to_string()`) drop styles, so every styling rule gets a unit test over the
`Style` values it produces:

- `logs.rs`: a line with SGR colours yields the expected spans and a clean `text`; an unknown sequence is
  dropped; `view` wraps a long line into the expected rows, keeps the marker unwrapped, and paused offsets
  pin the same logical line at two widths.
- `copy.rs`: `row_at` across a wrapped line; a selection spanning a wrap boundary yields the same
  `selected_text` as before wrapping.
- `state.rs`: the jump fires on the first failure, not on the second, not after `j`, not on an allowed
  failure, and resets on `RunStarted`.
- `ui/tree.rs`: truncation at the column width, duration intact.
- `ui/theme.rs`: `status_style` per state with colour on and off; `NO_COLOR` yields `Style::default()`.
- Existing render snapshots are re-accepted where the bottom bar changed; the monochrome output of every
  other snapshot is unchanged.
- `alba-engine`: `extra_env` reaches `FakeExecutor` and leaves the fingerprint of a beam unchanged.
- `alba-cli`: under `portable-pty`, a beam that prints `\x1b[31mred\x1b[0m` and exits `1`; quitting replays
  the raw bytes; the same run with `NO_COLOR=1` replays `red`.

## Success criteria

1. `alba run` on this repository shows green ticks, a yellow running beam, and `cargo`'s own colours in the
   log pane; a failing `cargo test` selects the failed beam and shows the red `error:` without a keystroke.
2. `NO_COLOR=1 alba run` renders exactly today's monochrome interface, and `alba run | cat` is unchanged.
3. Running a beam under the TUI then headless (or the reverse) hits the cache both times.
4. CI green on macOS, Linux, and Windows.
