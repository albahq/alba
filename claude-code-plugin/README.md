# Alba Claude Code plugin

Teaches [Claude Code](https://claude.ai/code) how Alba works so it can read
and write `Beamfile`s and run the `alba` CLI.

## What it provides

- **Skill `using-alba`**: Alba's execution model, the Beamfile DSL, and the
  CLI, with reference files loaded on demand.
- **Hooks**: validates a `Beamfile` after Claude edits it (`alba check`)
  and announces the declared beams at session start. Without the `alba`
  binary, validation is skipped and the session note asks to install it.

## Install

```text
/plugin marketplace add albahq/alba
/plugin install alba
```

The plugin assumes the `alba` binary is installed separately (see the
repository README).

## License

MIT.
