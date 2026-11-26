# Alba - TUI Redesign

Date: 2026-11-25 Status: draft

## Overview

A redesign of the interactive interface in `crates/alba-tui`, driven by two goals stated together:

- **A visual identity for the open source release.** The interface is what people see first in the
  README, in screenshots and in demos. It has to be recognisable as Alba, not as a generic ratatui
  layout in the terminal's default palette.
- **Better readability and user experience during a run.** The reader should know at a glance what
  is running, what failed, where the error is, and where they are in a long list or a long output.

The reference is the Paper file **Alba**, page **TUI v2**. It holds twelve screens drawn on a strict
terminal grid (one 11x23 px cell per character, 104 columns by 23 rows), so every column in them is a
column ratatui will draw. When this document and a mockup disagree, this document wins and the mockup
is fixed.

| Artboard                       | What it shows                                              |
|--------------------------------|------------------------------------------------------------|
| Run · Live output              | A run in progress, the selected beam's output following    |
| Graph · Dependencies           | The dependency view with the focused beam's path           |
| Watch · Parked on diagnostic   | A watch session parked on an invalid Beamfile              |
| Run · Passed                   | A finished run, every beam passed or cached                |
| Run · Failed                   | A finished run with a failed beam and a skipped dependent  |
| Watch · Waiting for changes    | A watch session idle between two runs                      |
| Run · Scrolling 48 beams       | A list and an output both longer than the screen           |
| Search · Matches in output     | A search being typed, with its matches                     |
| Copy · Line selection          | Copy mode with a five-line selection                       |
| Help · Keyboard page           | The keyboard reference page                                |
| Terminal too small · 36×10     | The screen below the size floor                            |
| Output · stderr only           | The output filtered to stderr                              |

## Goals

- One palette, painted in truecolor, identical in every terminal that supports it.
- A frame with one job per band: identity and state, progress, the two panes, the keys.
- Every piece of information the mockups show is real data from the run, not decoration.
- Long beam lists and long outputs stay navigable: the reader always sees where they are.
- The output tells apart what the process wrote and what Alba says about the process.

## Non-goals

- No change to the keymap beyond the additions listed in [Keys](#keys).
- No change to the headless renderers in `crates/alba/src/render/` (the non-interactive output).
- No light theme. The interface paints its own dark background everywhere.
- No mouse features beyond what copy mode already supports.

## Anti-goals

- A screen that looks right in the mockups but drifts by a column in a real terminal. Every glyph
  the interface draws must occupy exactly one cell.
- Hiding or dimming error output to make the screen calmer. Readability never comes from muting the
  lines a reader is looking for.
- Information that is drawn but not true (a placeholder executor, a guessed line number).

## Approaches

Three ways to deliver this were considered.

1. **Restyle the current layout only.** Swap the sixteen ANSI colours for the palette and keep the
   outer frame, the border titles and the footer. Cheap, but it keeps every readability problem the
   mockups solve (no definition, no command echo, no scrolling list, no stream marker), so it serves
   the identity goal and fails the user experience goal.
2. **Implement the mockups in the existing crate, with the data plumbing they need.** The layout,
   the drawing modules and the state change; the engine and the CLI pass the TUI what it needs to
   draw (beam definitions, executed commands, a structured diagnostic). **Chosen.** It serves both
   goals and keeps the crate boundaries the project already has.
3. **A new TUI crate beside the old one.** Lets both coexist behind a flag. Rejected: two interfaces
   to maintain for one product, and nothing in the old one is worth keeping as a fallback.

For colour, three strategies were considered: truecolor palette with Alba's own background
(**chosen by the owner**), truecolor palette on the terminal's background, and the sixteen ANSI
colours. The second fails on light terminals (amber and pale green become unreadable); the third
loses the identity.

## Visual system

### Palette

Eleven tokens, defined once in `ui/theme.rs` and mirrored as Paper design tokens with the same names.

| Token               | Value     | Role                                                                    |
|---------------------|-----------|-------------------------------------------------------------------------|
| `surface`           | `#151719` | Background of every cell the interface draws                            |
| `text`              | `#E3E4DC` | Primary text: names, values, output                                     |
| `text-muted`        | `#A1A7A4` | Secondary text: durations, labels, descriptions, pending beams          |
| `track`             | `#79807C` | Unfilled part of the progress bar                                       |
| `line`              | `#626A67` | Separators, divider, graph edges, stderr marker, scrollbar rail         |
| `accent`            | `#F3C280` | Selection bar, running state, keys, focused beam, paused state          |
| `on-accent`         | `#24221E` | Text on an accent background                                            |
| `success`           | `#A5CF9A` | Passed beams, `ok`, cargo verbs, progress fill, `help:`                 |
| `cached`            | `#9CCBC9` | Beams restored from cache                                               |
| `error`             | `#F0A29A` | Failed beams, `error:`, carets, failed progress fill, exit lines        |
| `selection`         | `#353C40` | Copy-mode selection and non-current search matches                      |

### Colour modes

The interface picks one of three modes at start and keeps it for the session:

1. **Truecolor**, when `COLORTERM` is `truecolor` or `24bit`: the palette exactly, background
   included.
2. **256 colours**, otherwise when colour is on: each token mapped to its nearest xterm-256 index,
   background included. The mapping is a constant table, computed once and pinned by a test.
3. **Monochrome**, when `NO_COLOR` is set or colour is off: no colour and no background at all.
   Hierarchy falls back to bold, reversed (selection bar, badge, current match) and dim (muted text),
   exactly the attributes the monochrome interface already uses.

The background is painted on every cell of the frame, so the palette reads the same on a light
terminal. On exit the terminal is restored as today.

### Output colours

Colours a command writes itself (ANSI SGR) are kept, with the sixteen base colours remapped onto the
palette so tool output does not clash with the frame:

| ANSI colour            | Drawn as      |
|------------------------|---------------|
| red, bright red        | `error`       |
| green, bright green    | `success`     |
| yellow, bright yellow  | `accent`      |
| cyan, bright cyan      | `cached`      |
| blue, magenta (both)   | unchanged     |
| white, bright white    | `text`        |
| black, bright black    | `text-muted`  |
| default foreground     | `text`        |

256-colour and truecolor sequences from the command are drawn as written. In monochrome mode every
command colour is dropped, as today.

### Glyphs

Every status glyph is a single-cell character. `⚡` (double width) is retired.

| State                  | Glyph | Colour        |
|------------------------|-------|---------------|
| pending                | `○`   | `text-muted`  |
| running                | `▶`   | `accent`      |
| succeeded              | `✓`   | `success`     |
| cached                 | `↯`   | `cached`      |
| failed                 | `✗`   | `error`       |
| failed, allowed        | `✗`   | `error`       |
| cancelled or skipped   | `○`   | `text-muted`  |
| parked (header only)   | `Ⅱ`   | `accent`      |
| paused output          | `Ⅱ`   | `accent`      |
| following output       | `●`   | `success`     |
| stderr line            | `▎`   | `line`        |

A beam with no duration yet shows an em dash glyph (U+2014) in `text-muted` in its duration column.

### Type

The terminal's font does the typography; the interface only uses bold and colour. Bold is used for:
section titles (`BEAMS`, `OUTPUT`, `GRAPH`, `DIAGNOSTIC`, `COMMANDS`, `HELP`), the target in the
breadcrumb, the header state, keys in the key hints, the selected row, the command echo line, and
the labels `error:` and `help:`.

## Frame

Top to bottom, for a terminal of `W` columns and `H` rows:

| Row          | Content                                                                    |
|--------------|----------------------------------------------------------------------------|
| 0            | empty                                                                      |
| 1            | header                                                                     |
| 2            | empty                                                                      |
| 3            | progress row, or banner row in a watch session between runs                |
| 4            | top separator, `┬` where the divider meets it                              |
| 5 to H-4     | body: beams pane, divider, right pane                                      |
| H-3          | bottom separator, `┴` where the divider meets it                           |
| H-2          | key hints                                                                  |
| H-1          | empty                                                                      |

There is no outer border. Content starts at column 2 and ends at column `W-3`; separators run from
column 1 to `W-2`. The help page replaces the body and drops the divider and its `┬`/`┴`.

The floor stays 40x12. Below it the whole screen is the too-small screen: the `ALBA` badge, then
`Terminal too small` (bold), `Resize to at least 40×12` and `Now WxH` (muted), centred.

### Header

- Left: the badge ` ALBA ` (`on-accent` bold on `accent`), two spaces, then the breadcrumb
  `mode / target`: the mode (`run` or `watch`) in `text-muted`, the slash in `line`, the target in
  `text` bold.
- Right, ending at column `W-3`: the session state, then three spaces, then a detail.

| Session state                     | Right side                       |
|-----------------------------------|----------------------------------|
| before the first run              | `○ IDLE` (muted)                 |
| run in progress                   | `▶ RUNNING` + elapsed            |
| run finished, no failure          | `✓ PASSED` + total duration      |
| run finished with a failure       | `✗ FAILED` + total duration      |
| run cancelled                     | `○ CANCELLED` + elapsed          |
| watch, waiting between runs       | `● WATCHING` + `N files`         |
| watch, parked on a diagnostic     | `Ⅱ PARKED`                       |

An allowed failure counts as a failure here, as in the current outcome colour. Durations use the
existing `format_duration` shape; above one minute they read `2m 14s`. Several targets are joined
with `, `.

When the header is too narrow, the detail goes first, then the target is cut with `…`. The badge and
the state are never cut.

### Progress row

- Left: a bar of `━` in `success` for the done part and `─` in `track` for the rest. Once any beam
  has failed, the filled part turns `error`.
- Right, ending at column `W-3`: `done/total complete`, then each non-zero count with its glyph:
  `✓ N passed`, `↯ N cached`, `✗ N failed`, `○ N pending` while running or `○ N skipped` once the
  run is over. Groups are separated by three spaces.
- The bar takes the room left, two spaces from the counts, and is dropped below 14 cells as today.
- When the counts alone do not fit, groups drop in this order: pending or skipped, cached, passed.
  `done/total complete` and `✗ N failed` are never dropped; below that, `complete` is dropped.

### Banner row

Replaces the progress row while a watch session is waiting or parked. Left in `text` bold, right in
`text-muted`:

- waiting: `Last run passed in 9.4s.` / `Save a watched file to run again.` (or `failed`)
- parked: `Invalid Beamfile.` / `Fix the error and save to resume.`

### Key hints

One row, `key label` pairs separated by three spaces, keys in `accent` bold, labels in `text`. The
content depends on the mode; see [Keys](#keys). When the row is too narrow, pairs are dropped from
the end, except `? help` and `q quit`, which are always kept.

## Beams pane

- **Width.** The pane's content width is the longest beam name plus 12 cells (glyph, gaps, duration),
  capped at 40 percent of `W`, and never under 20 cells; the right pane takes the rest. At the 40x12
  floor that gives 20 cells to the beams and 15 to the output. A name that still does not fit is cut
  with `…`, budgeted by display width as today.
- **Title row.** `BEAMS` bold, or `BEAMS / last run` in a watch session between runs; the counter
  `selected/total` (for example `3/4`, `22/48`) right-aligned in `text-muted`, without zero padding.
- **Rows.** Glyph at column 4, name at column 6, duration right-aligned against the divider. Done
  beams keep their name in `text`; pending, cancelled and skipped beams show it in `text-muted`.
  The running duration ticks live with no trailing ellipsis.
- **Selection.** The whole row in `on-accent` bold on `accent`.
- **Scrolling.** When the list is taller than the pane, it scrolls so the selection stays visible
  with one row of context above and below when there is one. The divider becomes the scrollbar: rail
  `│` in `line`, thumb `┃` in `text-muted`, thumb size and position proportional to the window. No
  scrollbar is drawn when the list fits.

## Output pane

### Title and definition rows

- **Title row.** Left: `OUTPUT / beam` (`OUTPUT` bold, the beam name in `accent` bold), then four
  spaces and `EXECUTOR kind` (`EXECUTOR` muted bold, then the executor label). Right: the pane's
  state, one of:

  | State                                  | Text                       |
  |----------------------------------------|----------------------------|
  | following a beam still producing       | `● following`              |
  | scrolled away from the end             | `Ⅱ paused`                 |
  | beam finished, viewing its output      | `end of output`            |
  | search active or committed             | `match i/n`                |
  | copy mode                              | `COPY · n lines`           |
  | output filtered to one stream          | `▎ stdout only`, `▎ stderr only` |

  When the row is too narrow, the executor goes first, then the beam name is cut with `…`. The state
  is never cut.

- **Executor labels.** `shell (embedded)`, `system shell`, `docker <image>`, `compose <service>`,
  `plugin <name>`; the part after the kind is muted. They come from the beam's `ExecutorKind`.
- **Definition row.** The beam's description in `text-muted` on the left, its location on the right
  (`Beamfile:23`, or `services/payment/Beamfile:12` for an imported file, path relative to the
  project root). A beam without description keeps the row with the location only, so the output does
  not jump between beams. The row is dropped when `H` is under 16.
- One empty row separates the definition row from the output.

### Output lines

- **Command echo.** Each command the executor runs is shown before its output as `$ command`, `$`
  in `accent` bold and the command in `text` bold. It is the rendered command, with secrets masked
  as everywhere else. Echo lines belong to Alba, not to a stream: the stream filter never hides
  them. A beam restored from the cache has no echo lines (the cache format does not change); its
  Alba line says it was restored.
- **Streams.** stdout and stderr stay interleaved in arrival order. A stderr line gets `▎` in `line`
  in the cell right after the divider; its text keeps its colours. The current dimming of plain
  stderr is removed.
- **Alba lines.** When a beam ends, Alba writes one line of its own across the pane:
  `── <glyph> <text> ───…` with the dashes in `line` and the text bold in the state colour:

  | Status            | Line                                     |
  |-------------------|------------------------------------------|
  | succeeded         | `✓ exit 0 · 0.1s` in `success`           |
  | failed            | `✗ exit 101 · 4.2s` in `error`           |
  | failed, allowed   | `✗ exit 1 · allowed · 0.3s` in `error`   |
  | cached            | `↯ restored from cache · 0.8s` in `cached` |
  | cancelled         | `○ cancelled · 2.0s` in `text-muted`     |
  | skipped           | `○ skipped` in `text-muted`              |

  The engine reports both a cancelled and a skipped beam as `BeamStatus::Cancelled`. The TUI tells
  them apart itself: a beam that ends `Cancelled` without a `BeamStarted` or `BeamCached` before it
  was skipped, and its line has no duration.

  Alba's own notices during a beam are Alba lines too, drawn `── <text> ───…` with no glyph: the
  text in `text-muted` for an information notice (why the beam runs without the cache) and bold in
  `error` for an error notice (a template that failed to render, an executor that failed to open).
  A notice longer than the pane wraps; its continuation rows carry no dashes.

  Alba lines are part of the beam's log buffer, so they scroll, search and copy like output, and the
  stream filter never hides them. They are not counted as stdout or stderr.
- **Wrapping** and the line cap stay as today.
- **Scrollbar.** When the output is taller than the pane, a scrollbar is drawn in the pane's last
  column, same style as the beams pane.
- **Position badge.** While paused, ` 1 240/2 400 ` (current line / total, thousands separated by a
  space) floats at the bottom right of the output area, left of the scrollbar, `text-muted` on
  `selection`, covering the end of the last visible line. Hidden while following and when the pane
  is under 20 cells wide.

### Stream filter

`s` cycles all streams, stdout only, stderr only. The filter applies to the selected beam's view;
it persists when the selection changes. Scrolling, the position badge, search and copy all work on
the filtered view. The key hints in a filtered view start with `s all streams`.

## Diagnostic pane

Replaces the output pane while a watch session is parked.

- Title row: `DIAGNOSTIC / Beamfile:5:21` (location in `text-muted`).
- Then: `error:` bold in `error` with the message bold in `text`; an empty row; the source snippet;
  an empty row; `help:` bold in `success` with the help text in `text`.
- Snippet: line numbers right-aligned in a gutter, then ` │ ` in `line`, then the source. Context
  lines are `text-muted`, the faulty line is `text` with its number bold. The faulty span is bold in
  `error`, and a caret line `^^^^` in `error` sits under it, with the gutter bar continued.
- When no structured diagnostic is available (an error without a span), the rendered string is shown
  as today.

## Graph view

- Edge routing and node placement stay as `graphpane.rs` does them today (including edges that
  skip layers and layers wider than the pane); this redesign changes the styling, the focused path
  and the section below, not the layout algorithm.
- When the body is too short, the `COMMANDS` section is dropped first (it needs at least five rows
  below the DAG), then the DAG scrolls vertically to keep the focused node visible.
- The beams pane stays on the left. The right pane shows:
  - title row: `GRAPH / target`, with `N beams · M layers` muted on the right;
  - the DAG from `graph::layers`, one row of nodes per layer, three connector rows between layers
    (`│` under each node, a `└─┼─┘` row, `│` into the next node). Nodes read `[ glyph name ]` in the
    beam's status colour, bold; the focused node is `on-accent` on `accent`; pending nodes are muted.
  - The path from the focused beam down to the target is drawn in `accent`; other edges in `line`.
  - Below the DAG: `COMMANDS / beam` with `NEEDED BY a, b` and the location on the right, the
    description muted, then the beam's commands as `$ command` lines, unrendered templates.
- `Enter` opens the focused beam's output and leaves the graph.
- The key hints read `↑↓←→ move   Enter open output   Esc back`.

## Help page

`?` replaces the body with a keyboard page instead of an overlay: no divider, title `HELP / keyboard`,
two columns of groups. Each binding is one line, key in `accent` bold in a 12-cell column, action in
`text`. Group titles are muted bold.

- RUN: `r` rerun, `f` rerun without the cache, `c` cancel the run, `t` back to the session target,
  `w` start or stop watching.
- MOVE: `↑↓  j k` select a beam, `PgUp PgDn` scroll the output, `G` follow the output again.
- FIND AND COPY: `/` search the output, `n  N` next or previous match, `v` select lines to copy,
  `y` copy the selection, `s` show stdout, stderr or both.
- VIEWS: `g` dependency graph, `?` this help, `q` quit, `Ctrl-C` cancel, or quit when idle.

A muted last line reads `Copy and graph modes list their own keys in the footer.` The key hints read
`Esc close help`; `?` also closes it.

## Search and copy

- **Search.** The key hints row becomes the prompt: `/` in `accent` bold, the query bold, a block
  cursor, then `i/n` muted, `Enter commit`, `Esc cancel`. Matches are drawn on `selection`; the
  current match is `on-accent` bold on `accent`. The output title shows `match i/n`.
- **Copy.** Selected lines are drawn on `selection` from the text column to the pane's edge, text in
  `text`; the cursor cell is `accent`. Stream markers stay visible. The output title shows
  `COPY · n lines`. Key hints: `hjkl/arrows move   v anchor   y copy   Esc cancel`. The copy result
  message takes over the key hints row for its existing window.

## Keys

The normal-mode row is built from a base chosen by the session state, then two modifiers:

| Session state      | Base row                                                                      |
|--------------------|-------------------------------------------------------------------------------|
| run in progress    | `↑↓ beam  / search  g graph  v copy  r rerun  c cancel  w watch  ? help  q quit` |
| run finished       | `↑↓ beam  / search  g graph  v copy  r rerun  f force  w watch  ? help  q quit`  |
| watch, waiting     | `↑↓ beam  / search  g graph  v copy  r rerun  w stop watching  ? help  q quit`   |
| watch, parked      | `↑↓ beam  / search  g graph  v copy  w stop watching  ? help  q quit`            |

- **Output scrolled away from the end:** `G follow` is inserted after `↑↓ beam`.
- **Stream filtered:** `s all streams` is prepended.
- Then pairs drop from the end to fit, `? help` and `q quit` always kept.

Other modes replace the row entirely: graph `↑↓←→ move  Enter open output  Esc back`; search, copy
and help as described in their sections. (Pairs are separated by three spaces on screen.)

`w` keeps its current behaviour; in a watch session it reads `stop watching` because that is what it
does there. `r` keeps its current behaviour (rerun the selected beam) in every state.

New bindings: `s` (stream filter) and `Enter` in the graph (open the focused beam's output).
Everything else is unchanged.

## Data the TUI needs

Today the TUI receives beam names and edges only. The mockups need four more facts.

1. **Beam definitions** (description, location, executor label, unrendered commands). `RunStarted`
   gains a `details: Vec<BeamDetails>` field, one entry per beam in `beams`. The scheduler holds the
   `Project` but not the sources, so the location and the commands as written are computed by the
   callers that hold the `SourceMap` and the root (the CLI and the watch session) and passed in
   `RunOptions`; the description and executor come from the `Beam` itself. Fields: `description: Option<String>`, `location: String` (file path
   relative to the project root, colon, 1-based line of the beam's span), `executor: ExecutorLabel`
   (kind plus detail), `commands: Vec<String>` (the `run` templates as written). A snapshot per run,
   for the same reason the graph is one: a watch session reloads the Beamfile.
2. **Executed commands.** A new event `CommandStarted { id: BeamId, command: String }`, emitted by
   the engine before each command of a beam, carrying the rendered, secret-masked command. The TUI
   appends it to the beam's buffer as an echo line. The headless renderers ignore it.
3. **Exit details.** Already present in `BeamFinished` (`status`, `duration`); the TUI turns them
   into the Alba line.
4. **Structured diagnostic.** `ProjectBroken` gains `report: Option<DiagnosticReport>` beside the
   existing rendered `diagnostic` string, which stays for every other consumer. `DiagnosticReport`
   holds the message, the location (path, line, column), the snippet lines with their numbers, the
   faulty line's span as a column range, and the help text. It is built where the string is built
   today (the CLI's `render_error`, which has the sources), so `alba-syntax` keeps `Diagnostic`'s
   fields private and exposes one constructor for the report.

5. **Alba notices.** The scheduler stops disguising its own messages as stderr `OutputLine`s (today:
   the notice explaining why a beam runs without the cache, a template rendering error, an executor
   opening error, a command spawn error, a session close error). Messages executors write themselves
   (a Docker image build notice, a Compose wait notice) stay stderr output: they come from the
   executor process boundary, not from the scheduler. It emits a new event `BeamNotice { id: BeamId, level: NoticeLevel, text: String }`
   instead, with `NoticeLevel` being `Info` or `Error`. Consequences:
   - The TUI draws them as Alba lines.
   - The headless renderers print them exactly where and how they print those lines today, so their
     output does not change; their snapshot tests prove it.
   - Notices describe one run and are never written to the cache. If the cache stores them today
     (they travel through the same forwarder as output), the builder removes them from what is
     stored, and entries already on disk that contain them replay them as stderr, as before.

Event additions are additive; every existing consumer keeps compiling by ignoring the new field or
event, which the builder verifies across `crates/alba`.

## Behaviour changes from today

- No outer border; titles leave the border for their own rows.
- Status glyphs `✔ ⚡ ✖` become `✓ ↯ ✗`.
- The help overlay becomes a page.
- Plain stderr is no longer dimmed.
- The footer counts move to the progress row; the footer becomes the key hints row.
- The beams pane scrolls and its width adapts.
- Alba's own notices leave stderr and become `BeamNotice` events, drawn as Alba lines.
- New: definition row, command echo, Alba end lines, stream filter, position badge, scrollbars,
  structured diagnostic, graph commands section.

## Testing

Tests go through the crate's public entry points, as the current suite does: `AppState::apply` with
`RunEvent`s and `ui::draw` into a `TestBackend`.

- **Snapshots.** Every existing `insta` snapshot in `tests/render.rs` is regenerated and reviewed
  against the matching artboard. New snapshots: scrolling list, scrolled output with badge, stderr
  filter, help page, structured diagnostic, graph with commands, watch waiting, cancelled run.
- **Styles.** Snapshots drop styles, so style assertions cover: the palette in each colour mode, the
  ANSI remap table, the selection bar, the stream marker, the Alba line colours, the current versus
  other search matches.
- **Geometry.** Beams pane width clamping, selection kept in view while scrolling, scrollbar thumb
  size and position, title-row priority when narrow, badge hiding under 20 cells, definition row
  hiding under 16 rows, key hints dropping from the end.
- **Engine and CLI.** `RunStarted.details` content (location of an imported beam, executor labels),
  `CommandStarted` order and masking, `DiagnosticReport` built for a span and absent without one,
  each former stderr notice now arriving as a `BeamNotice` with the right level, headless output
  unchanged, notices absent from newly written cache entries.

## Decisions made without the owner (to check)

- The running duration loses its trailing ellipsis; `▶` already says the beam is running.
- In a watch session, `w` reads `stop watching`; `r` keeps reading `rerun` and doing what it does.
- Narrow terminals: beams pane at least 20 cells, header and progress drop order as specified.
- A cached beam shows no command echo; the cache format is unchanged.
- A skipped beam is told apart from a cancelled one in the TUI, without an engine change.
- The graph keeps today's layout algorithm; `COMMANDS` drops before the DAG scrolls.
- The help page replaces the body rather than overlaying it.
- An allowed failure shows `✗` like a failure, with `allowed` in its Alba line.
- The stream filter persists across beam selection.
- `○ IDLE` before the first run, `○ CANCELLED` after a cancelled run.

## Left to the builder

- The exact xterm-256 index for each token.
- How `ui/` is split into modules for the new bands.
- Whether `CommandStarted` is emitted by the engine's command loop or by each executor, as long as
  every executor produces it in order.
