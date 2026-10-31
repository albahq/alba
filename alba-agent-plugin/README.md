# Alba agent plugin

Teaches AI coding agents how Alba works so they can read and write
`Beamfile`s and run the `alba` CLI.

## What it provides

- **Skill `using-alba`**: Alba's execution model, the Beamfile DSL, and the
  CLI, with reference files loaded on demand.
- **Claude Code hooks**: validate a `Beamfile` after it is edited (`alba
  check`) and announce the declared beams at session start. Codex uses the
  shared skill instructions to run `alba check` after editing instead.

## Install

```text
/plugin marketplace add albahq/alba
/plugin install alba
```

For Codex, add the Alba repository as a plugin marketplace with
`codex plugin marketplace add albahq/alba`, then install **Alba Agent** from
the Plugins directory in the Codex desktop app.

The plugin assumes the `alba` binary is installed separately (see the
repository README).

## License

MIT.
