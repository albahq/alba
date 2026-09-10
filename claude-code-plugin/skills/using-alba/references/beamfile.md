# Beamfile DSL

Brace-delimited, whitespace-insensitive. A field may appear at most once
per block; lists accept a trailing comma.

## File-level declarations

```
version "1"                          # optional
import "api/Beamfile" as api         # beams reachable as api:<name>
let profile = env("PROFILE", "debug")
default check                        # the beam a bare `alba` runs

beam <name>[(param, ...)] { ... }
hook <git-hook-name> { beam <name> }
```

Only the root Beamfile's `default` and `hook` declarations count; an
imported file's are ignored. Namespaces chain: a beam imported through two
levels is `api:db:migrate`.

## Beam fields

| Field | Value | Notes |
| --- | --- | --- |
| `description` | string template | Shown in listings. |
| `needs` | `[beam, api:beam, ...]` | Prerequisites; all must succeed first. |
| `inputs` | `["src/**/*.rs", ...]` | Globs, relative to the declaring Beamfile's directory. Turns caching on, and drives `--watch` and `--affected`. |
| `outputs` | `["target/release/alba"]` | Same base directory as `inputs`. A missing output forces a rerun. |
| `run` | `"cmd"` or `["cmd1", "cmd2"]` | String templates. Commands run in order on the same executor. |
| `env` | `{ NAME = "value", OTHER = param }` | Values are string templates or a bare `let` / parameter name. |
| `cwd` | string template | Working directory for the commands. |
| `executor` | `system_shell`, `docker { ... }`, or `<plugin> { ... }` | Omitted means the embedded shell. |
| `allow_failure` | `true` | The beam may fail without failing its dependents. |

Example:

```
beam deploy(target) {
  description "Deploy to {target}"
  needs [build, api:migrate]
  inputs ["dist/**"]
  env { DEPLOY_TARGET = target }
  executor docker {
    image "deployer:latest"
    volumes ["/host/cache:/cache"]
    workdir "/srv"
  }
  run ["./scripts/preflight.sh", "./scripts/deploy.sh {target}"]
}
```

Parameters bind positionally: `alba run deploy prod`. A parameterized beam
is not cached, is skipped by a bare `--affected` run, and is not statically
shell-checked.

## Expressions

Anything inside `{...}` in a string is an expression. Inside the braces,
string literals use **single** quotes (`"{env('VAR')}"`); `{{` and `}}`
give literal braces.

- `let name = <expr>` at file level.
- `env("NAME", "fallback")` reads an environment variable. Without a
  fallback, `env("NAME")` is allowed only in `run` and `env` values, where
  it resolves at run time; anywhere resolved at load time (`let`,
  `description`, `inputs`, `outputs`, `cwd`, executor options) it is a load
  error.
- `git.branch`, `git.sha`, `git.short_sha` (strings), `git.dirty` (bool).
  Usable in every expression position. `git` is a reserved name.
- Operators, low to high precedence: `||`, `&&`, `==` / `!=`, `+`.
- `if cond then a else b`, for example `{if git.dirty then '-dirty' else ''}`.
- `true`, `false`, and bare identifiers referring to `let` bindings or beam
  parameters.

## Embedded shell (the default executor)

Supported: `;`, `&&`, `||`, `!`, pipelines, redirections (`>`, `>>`, `<`,
`2>`, `2>>`, `2>&1`), POSIX quoting, `$VAR`, `${VAR}`, `$?`, `FOO=bar`
assignment, `FOO=bar cmd` prefix, `$(...)`, `~`, globs (`*`, `?`, `[...]`;
never a leading dot). A newline in a `run` string acts like `;`.

Builtins, which always win over a PATH binary of the same name: `cd`,
`pwd`, `exit`, `true`, `false`, `export`, `unset`, `echo` (`-n` only),
`cat`, `cp` (`-r`), `mv`, `rm` (`-r -f`), `mkdir` (`-p`), `touch`, `sleep`,
`test` / `[`. Use an explicit path (`/bin/rm`) to reach the system binary.

Rejected at `alba check` time: `if`, `for`, `while`, `case`, functions,
heredocs, `&`, `wait`, `(...)`, `${VAR:-default}`, `$((...))`, `{a,b}`,
and every special parameter but `$?`. Move the logic into a script, or
declare `executor system_shell` on that beam.

Frozen behaviours: an unset variable is empty (no `set -u`), a pipeline's
exit code is its last command's (no `pipefail`), an unmatched glob stays
literal.

## Executors

- **`system_shell`**: `sh` on Unix, PowerShell on Windows. No static check.
- **`docker { image "..." [volumes [...]] [workdir "..."] }`**: one
  container per beam, kept alive across the beam's commands, removed when
  the beam ends. The project directory is bind-mounted. `image` is
  required; `volumes` entries are `host:container` with no `:ro` suffix.
- **`<name> { option value ... }`**: an external `alba-executor-<name>`
  binary on the `PATH`. Resolution happens at plan time; a missing plugin
  fails the whole run before any beam starts.

The cache fingerprints an executor by its option text, not by what the
text resolves to: retagging a mutable docker tag or updating a plugin
binary does not invalidate a cached beam.

## Git hooks

```
hook pre-commit { beam fmt }
hook commit-msg { beam check_message }   # beam check_message(path) { ... }
```

`<name>` must be a git hook name. The target beam may declare at most as
many parameters as git passes to that hook. `alba hooks install` writes
scripts under `.alba/hooks/` and sets `core.hooksPath`; `alba hooks
uninstall` reverts it. `.alba/` belongs in `.gitignore`.
