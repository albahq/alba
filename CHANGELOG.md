# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.9.0](https://github.com/albahq/alba/compare/v0.8.0...v0.9.0) - 2026-11-26

### Added

- *(cli)* say what forced a beam into full mode
- *(cli)* diff from the declared base on every run
- *(engine)* run beams on their changed files only
- *(git)* list a beam's changed files with git.changes
- *(git)* declare the reference to diff from in a git block
- *(syntax)* add a match expression over glob patterns

### Fixed

- *(git)* leave Alba's own state out of changes
- *(affected)* diff from the merge base with the reference
- *(engine)* judge unchanged on the calls rendered
- *(git)* keep non-UTF-8 changed names byte for byte
- *(tui)* show the base unless the label names it

### Other

- *(render)* restore the colour test's rationale
- *(cli)* pin the interleaved line of an unchanged beam
- *(cli)* cover unchanged and full mode end to end only
- *(git)* clarify flag order, ignored inputs and quoting
- *(cli)* quote changed names through the system shell
- *(core)* share the run directory and segment globs

## [0.8.0](https://github.com/albahq/alba/compare/v0.7.0...v0.8.0) - 2026-11-26

### Added

- *(cli)* rename --force to --no-cache
- *(tui)* wrap the beam selection at both ends

## [0.7.0](https://github.com/albahq/alba/compare/v0.6.0...v0.7.0) - 2026-11-26

### Added

- *(tui)* add the Alba palette and colour modes
- *(engine)* attach a report to broken projects
- *(engine)* announce each command before it runs
- *(engine)* announce beam details in RunStarted
- *(core)* compute beam definitions from sources
- *(diagnostics)* add structured diagnostic report
- *(tui)* turn help into a page, restyle search and copy
- *(tui)* join graph layers through three-row bands
- *(tui)* restyle the graph beside the beams pane
- *(tui)* draw a structured diagnostic pane
- *(tui)* draw the output pane with its title and lines
- *(tui)* size the beams pane to its names and scroll it
- *(tui)* model line kinds and the stream filter

### Fixed

- *(tui)* replay the error that failed a beam on exit
- *(tui)* say when the exit replay lost lines to the cap
- *(core)* keep one slash after the path root
- *(core)* use slash separators in beam locations
- *(diagnostics)* floor report spans to char boundaries
- *(tui)* keep the graph focus on its beam across a reload
- *(tui)* scroll a search only as far as its match needs
- *(tui)* leave skipped beams out of the completed count
- *(tui)* show no duration for a skipped beam
- *(tui)* keep selection and matches visible on styled lines
- *(tui)* highlight only what copy mode copies
- *(tui)* replace the plain diagnostic on each break
- *(tui)* follow the displayed log on a parked diagnostic
- *(tui)* keep search and copy on a parked diagnostic
- *(tui)* count the output window from the rows drawn
- *(tui)* keep the search query whole in the key hints
- *(tui)* keep filtered views consistent
- *(tui)* keep accent distinct from error in 256 colours

### Other

- *(cli)* ignore the Windows line erase in the replay check
- *(tui)* document the redesigned interface
- *(tui)* lay the frame out in bands on a painted surface
- *(cli)* cover the report of a run failure
- *(engine)* cover notices in headless output
- *(engine)* emit beam notices as events
- *(render)* test the JSON renderer through handle
- *(cli)* reuse the project root for definitions
- *(core)* check definition paths the same way on Windows
- *(tui)* bring stale and dashed doc comments up to date
- *(tui)* drop a useless format! in a title test
- *(tui)* sweep the diagnostic, copy mode and short heights
- *(tui)* drop the graph keymap's unreachable Enter arm
- *(tui)* read the shown search in one place
- *(tui)* cut a long diagnostic path, not its position
- *(tui)* share dispatch's session-free steps with the tests
- *(tui)* reach the scrolled and copy screens through keys
- *(tui)* say where the beams list starts beside the output
- *(tui)* tell a skipped beam apart in one place
- *(tui)* give back the graph's alignment row before COMMANDS
- *(tui)* leave a blank row under the diagnostic title
- *(tui)* size the pane before the committed search snapshot
- *(tui)* say which beams leave the count and when search pins
- *(tui)* decide a skipped row's duration where it is built
- *(tui)* snapshot every mockup screen
- *(tui)* indent the help page's right column like the left
- *(tui)* set the current size apart on the small screen
- *(tui)* align the graph with the output's first line
- *(tui)* leave a blank row under the beams title
- *(tui)* share section titles, follow graph focus
- *(tui)* correct the beams pane comments
- *(tui)* explain the copy scroll bound
- *(tui)* cover every ANSI table row and fix remap

## [0.6.0](https://github.com/albahq/alba/compare/v0.5.1...v0.6.0) - 2026-11-25

### Added

- *(engine)* mask declared secrets in beam output
- *(cache)* fingerprint declared ambient variables

### Fixed

- *(cache)* fingerprint what an executor runs on
- *(engine)* distrust cache entries from another Alba
- *(engine)* rerun a beam whose outputs changed
- *(engine)* keep the cache readable by its owner only
- *(executors)* keep env values off the docker argv
- *(cli)* stop the run on SIGTERM and SIGHUP
- *(shell)* refuse to remove the filesystem root
- *(engine)* see changes outside the Beamfile directory
- *(core)* hash a literal input even when ignored
- *(shell)* retry a script the kernel reports busy

### Other

- Merge remote-tracking branch 'origin/main' into feat/mask-secrets
- Merge remote-tracking branch 'origin/main' into feat/cache-env-inputs
- *(cli)* keep the pty open for the whole session
- Merge branch 'feat/cache-env-inputs' into feat/mask-secrets
- Merge branch 'fix/cache-trust' into feat/cache-env-inputs
- Merge remote-tracking branch 'origin/main' into fix/cache-trust
- Merge pull request #35 from albahq/fix/cli-terminate-on-sigterm
- Merge pull request #34 from albahq/fix/shell-rm-refuses-root
- Merge pull request #32 from albahq/fix/cache-ignored-literal-input
- shield test repositories from global excludes
- Merge pull request #36 from albahq/test/reliable-timing-guards
- run wall-clock budgets alone on each platform
- widen hang guards past machine load

## [0.5.1](https://github.com/albahq/alba/compare/v0.5.0...v0.5.1) - 2026-11-20

### Fixed

- *(executors)* resolve volume hosts from the beam

### Other

- *(cli)* find the example plugin beside alba

## [0.5.0](https://github.com/albahq/alba/compare/v0.4.0...v0.5.0) - 2026-11-11

### Added

- *(executors)* add the compose executor
- *(engine)* shut executors down when a session ends
- *(executors)* let an executor release state after a run
- *(executors)* honour optional compose dependencies
- *(executors)* manage a compose service's dependencies
- *(executors)* explain a compose container slow to start
- *(executors)* run a beam in a compose service

### Fixed

- *(cli)* warn that a second Ctrl-C skips cleanup
- *(cli)* print the session report after the replay
- *(cli)* exit 1 when a session cannot clean up
- *(executors)* read an exited compose one-shot's code
- *(executors)* name the dependencies in the compose state read
- *(executors)* ignore one-off containers in compose state
- *(executors)* mark a compose up ended by Ctrl-C as interrupted
- *(executors)* refuse a compose service read as a flag
- *(executors)* word the compose spawn error without a dash
- *(executors)* mark a dropped compose start as interrupted
- *(executors)* wait only for healthy compose dependencies
- *(executors)* report a container removal that does not end
- *(executors)* harden the start of a compose session
- *(executors)* remove a cancelled container within the grace
- *(executors)* accept a container removal already in progress

### Other

- *(cli)* prove compose beams overlap without a clock
- *(executors)* clarify the compose crash cleanup wording
- *(executors)* advise removing a compose dependency after a crash
- *(cli)* say where a watch session reports cleanup errors
- *(executors)* reflow the compose notes and scope the cleanup advice
- *(executors)* document the compose executor
- *(cli)* share the session report between watch front ends
- *(engine)* tidy comments and an expectation message
- *(core)* judge the project load by its fastest sample
- *(engine)* cover the compose did-you-mean suggestion
- *(engine)* judge the cached run by its fastest sample
- *(engine)* end shutdown-command tests through the command alone
- *(executors)* free a compose test network before down
- *(executors)* wrap compose dependency docs to the file width
- *(executors)* cover a compose one-shot that already ran
- *(executors)* move compose dependencies to a module
- *(executors)* cut the dead wait of a cancelled container stop
- *(executors)* drop caller vocabulary from a container comment
- *(executors)* word the container comments for every caller
- *(executors)* share the container plumbing

## [0.4.0](https://github.com/albahq/alba/compare/v0.3.0...v0.4.0) - 2026-11-10

### Added

- *(executors)* name a docker beam's Dockerfile
- *(executors)* mount a volume relative to the beam
- *(executors)* build a docker beam's image
- *(executors)* let a docker beam choose its user

## [0.3.0](https://github.com/albahq/alba/compare/v0.2.1...v0.3.0) - 2026-11-06

### Added

- *(core)* accept a wildcard alias in needs

### Other

- Make PTY color test independent of NO_COLOR
- Make Alba agent plugin Codex compatible

## [0.2.1](https://github.com/albahq/alba/compare/v0.2.0...v0.2.1) - 2026-10-21

### Fixed

- *(cli)* watch inputs above the project root
- *(core)* reject `.` and `..` inside an inputs pattern
- *(core)* match inputs outside the Beamfile directory
- *(engine)* watch inputs above their Beamfile
- *(engine)* affect beams through inputs above their Beamfile

### Other

- *(cli)* correct how plugin tests find the example binary
- *(readme)* document inputs above the Beamfile directory
- update Cargo.lock dependencies

## [0.2.0](https://github.com/albahq/alba/compare/v0.1.0...v0.2.0) - 2026-09-10

### Added

- *(cli)* add alba update to self-update from the latest release

### Added

- `alba update`: fetch the latest release and replace the binary in place.
  Only for an alba the release installers put in place; one built with
  `cargo install` is told to update the same way it was installed.

## [0.1.0](https://github.com/albahq/alba/releases/tag/v0.1.0) - 2026-09-10

The first release of Alba, a task runner driven by a Beamfile.

### Added

- The Beamfile DSL: beams with a description, a command, dependencies, inputs
  and outputs, immutable `let` variables with expressions and `{}`
  interpolation, `env()` and `glob()`, namespaced imports for monorepos, and
  a `default` beam.
- `alba run`: parallel execution of the beam graph, a headless renderer, a
  newline-delimited JSON event stream, and exit codes that distinguish a
  failed beam from a broken Beamfile.
- `alba check`: validate a Beamfile; a bare `alba` without a `default` beam
  lists the beams with their descriptions.
- Content-addressed caching: a beam whose inputs, command, and executor are
  unchanged is skipped and its outputs restored; `--force` reruns it.
- Affected runs: `alba run --affected <ref>` runs only the beams a git diff
  touches and their dependents; `alba affected <ref>` lists them for CI.
- Watch mode (`alba run --watch`): rerun on every change to a beam's inputs,
  cancel a run when the Beamfile itself changes, and park the session until
  a broken Beamfile is fixed.
- The interactive interface, on by default on a terminal: a beam tree, a log
  pane with ANSI colour, wrapping, search, selection and copy through OSC 52,
  a DAG view, a help overlay, and a jump to the first failure.
- The embedded shell: a deterministic, cross-platform, POSIX-like interpreter
  that is the default executor on Linux, macOS, and Windows, with a per-beam
  opt-out to the system shell.
- The docker executor: run a beam in a container with volumes, environment,
  and a working directory declared in the Beamfile.
- Executor plugins: external binaries on the `PATH` that speak a JSON wire
  protocol, `alba plugin check` to verify one, and a reference plugin.
- Git integration: `git.branch`, `git.sha`, `git.short_sha`, and `git.dirty`
  in expressions, `.gitignore`-aware globs, and `alba hooks install` to run
  beams as git hooks.
- A Claude Code plugin and its marketplace, teaching Claude Code how to read,
  write, and validate Beamfiles.
