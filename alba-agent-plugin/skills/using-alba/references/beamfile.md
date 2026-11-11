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

In `needs` only (alba 0.3.0 and later), `*` stands for any one import
alias: `needs [*:test]` depends on the `test` beam of every direct import
that declares one, in import order. Each `*` matches exactly one level
(`*:*:test` reaches `api:db:test`), the beam name is always spelled out
(`api:*` is rejected), and a pattern matching no beam is a load error.

## Beam fields

| Field | Value | Notes |
| --- | --- | --- |
| `description` | string template | Shown in listings. |
| `needs` | `[beam, api:beam, *:beam, ...]` | Prerequisites; all must succeed first. `*` stands for any one import alias. |
| `inputs` | `["src/**/*.rs", ...]` | Globs, relative to the declaring Beamfile's directory; leading `..` components climb out of it (`"../../Dockerfile"`), while `.` or `..` past the start is a load error. Turns caching on, and drives `--watch` and `--affected`. |
| `outputs` | `["target/release/alba"]` | Same base directory as `inputs`. A missing output forces a rerun. |
| `run` | `"cmd"` or `["cmd1", "cmd2"]` | String templates. Commands run in order on the same executor. |
| `env` | `{ NAME = "value", OTHER = param }` | Values are string templates or a bare `let` / parameter name. |
| `cwd` | string template | Working directory for the commands. |
| `executor` | `system_shell`, `docker { ... }`, `compose { ... }`, or `<plugin> { ... }` | Omitted means the embedded shell. |
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
    user "host"
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
- **`docker { image "..." [build "..."] [dockerfile "..."] [volumes [...]]
  [workdir "..."] [user "..."] }`**: one container per beam, kept alive
  across the beam's commands, removed when the beam ends. The project
  directory is bind-mounted. `build`, `dockerfile`, `user`, and a
  relative volume path need alba 0.4.0 or later.
  - `volumes` entries are `host:container` with no `:ro` suffix; a
    container part starting with `./` is relative to the declaring
    Beamfile's directory (`"deps:./node_modules"`).
  - `build "docker/php"` builds the image from that context (relative to
    the declaring Beamfile's directory) before the beam runs, instead of
    naming one; `image` is then optional and tags the result. List the
    `Dockerfile` in `inputs`: the cache does not look inside the build
    context.
  - `dockerfile "Dockerfile.ci"` picks a file other than `Dockerfile`,
    relative to the build context; it requires `build`.
  - `user "host"` runs the commands as whoever runs Alba, so files
    written into the project are not owned by root; any other value goes
    to `docker run --user` as declared.
- **`compose { service "..." [file "..."] }`**: runs the beam in a one-off
  container of a service of the project's Compose file; everything else
  (image, build, mounts, user, environment) stays in the Compose file.
  `service` is required; `file` is the Compose file's path relative to
  the declaring Beamfile's directory, and without it Compose's own lookup
  applies (the beam's directory, then each parent).
  - A command's host working directory maps to the container through
    the service's bind mounts (longest matching host path); if none
    covers it the command fails naming the `cwd` and the mounts.
  - The service's command is replaced, its ports are not published, no
    user is passed. The image needs `sh`.
  - Dependencies (`depends_on`) are started when absent, honouring their
    conditions, left alone when running, and put back in their previous
    state when the command ends. List `compose.yaml` and the Dockerfiles
    in `inputs`: the cache only sees the block's text.
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
