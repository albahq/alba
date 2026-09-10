# Contributing to Alba

Thanks for taking the time to contribute. Bug reports, feature requests, and
pull requests are all welcome.

## Setup

Alba is a Cargo workspace on stable Rust. `rust-toolchain.toml` pins the
channel and pulls in `rustfmt` and `clippy`, so a plain `rustup` install is
enough. Build the binary once, then let Alba drive its own checks:

```sh
cargo install --path crates/alba
alba hooks install
```

`alba hooks install` wires the repository's git hooks: `fmt` runs before every
commit and the full `check` beam before every push. It only needs to run once
per clone.

## Checks

The `Beamfile` at the root is the source of truth. `alba run check` (or a
bare `alba`, since `check` is the default beam) is the gate every change
must pass; it runs the three beams below, and CI runs the same commands on
Linux, macOS, and Windows.

```sh
alba run check      # fmt, lint, and test
alba run fmt        # cargo fmt --check
alba run lint       # cargo clippy --all-targets -- -D warnings
alba run test       # cargo test --workspace
```

Any change to behaviour comes with a test. Any change to user-facing
behaviour comes with the matching README update.

## Commits

Commit messages follow [gitmoji](https://gitmoji.dev/) plus
[Conventional Commits](https://www.conventionalcommits.org/):

```text
<emoji> <type>(<scope>): <summary>
```

For example: `✨ feat(engine): skip beams whose inputs are unchanged`. The
scope is usually the crate name without its `alba-` prefix (`syntax`, `core`,
`engine`, `executors`, `shell`, `git`, `tui`, `cli`). Keep the summary under
50 characters and write the body, when there is one, in plain English.

## Pull requests

1. Branch from `main`.
2. Make the change, keeping the pull request to one concern.
3. Run `alba run check` and confirm it passes.
4. Open the pull request against `main` and fill in the template.

A pull request is merged once CI is green on all three platforms and a
maintainer has reviewed it.

## Releases

Releases are cut from `main` by [release-plz](https://release-plz.ieni.dev/):
every push opens or refreshes a pull request that bumps the workspace
version and writes `CHANGELOG.md` from the commits since the last release.
Nothing else is needed from a contributor; the commit convention above is
what decides the next version (`✨ feat` is a minor bump, `💥` a major one,
anything else a patch) and where a change lands in the changelog.

Merging that pull request publishes the crates to crates.io and pushes the
`v<version>` tag, which builds the binaries and the GitHub release.
