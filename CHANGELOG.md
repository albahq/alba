# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
