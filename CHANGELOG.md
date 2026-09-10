# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/albahq/alba/releases/tag/v0.1.0) - 2026-09-10

### Added

- add a Claude Code plugin and its marketplace
- *(tui)* keep the header to the target and the session
- *(tui)* scroll the log pane from the keyboard
- *(tui)* return to the session's own target with t
- *(shell)* expand `$?` and reject the other special parameters
- *(syntax)* allow nested namespaces in needs
- add a performance guard and README

### Fixed

- *(run)* announce a cancellation the run ends with
- *(tests)* keep the windows lint free of unix-only leftovers
- *(tui)* make Esc cancel a search instead of committing it
- *(shell)* keep wildcards away from hidden entries
- *(core)* validate every load-time branch

### Other

- *(tui)* answer the pty's cursor queries
- document the installers and how a release is cut
- publish the workspace crates as alba on crates.io
- add community health files
- *(readme)* document forced colour's blast radius and escape hatch
- *(tui)* document colour, wrapping, and the failure jump
- *(readme)* drop em-dashes and align the affected usage comment
- document affected runs, git variables, and git hooks
- document the executor's place in the cache key and other gaps
- *(plugin-protocol)* correct empty options, close's timeout, and volume syntax
- document the docker executor and the plugin protocol
- *(readme)* document the JSON stream's event kinds
- document the interactive interface
- *(readme)* correct what watch mode promises
- document watch mode
- tell readers what changes when the embedded shell takes over
- document the embedded shell
- state when inputs and outputs resolve to nothing
- clarify that --force only rewrites the cache on success
- document caching and dogfood it in the Beamfile
- *(perf)* exercise the needs lookup
- scaffold cargo workspace with core crates
