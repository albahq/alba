# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1](https://github.com/albahq/alba/compare/v0.1.0...v0.2.1) - 2026-09-10

### Added

- *(cli)* add alba update to self-update from the latest release

### Other

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
