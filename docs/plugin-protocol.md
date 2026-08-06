# Alba plugin protocol

An executor plugin is a standalone binary that runs a beam's commands on
Alba's behalf, outside the process. Alba never links against it: it spawns
the binary and talks to it over stdin/stdout in a small, line-oriented JSON
protocol. Any language that can read a line, parse JSON, and write a line
back can implement one.

This document is the protocol's specification, written for someone with no
access to Alba's own source. `crates/alba-executor-example` is the reference
implementation: a complete, working plugin in under 300 lines of Rust that
depends on nothing but `serde_json` and the standard library, deliberately
written against this same wire format rather than against any of Alba's own
crates. Reading it end to end alongside this document is the fastest way to
get the shape of a real one.

## Discovery

`executor <name> { ... }` for any `name` that is not a built-in executor
(`shell`, `system_shell`, `docker`) refers to a plugin. Alba resolves it to a
binary called `alba-executor-<name>` on the `PATH`.

Resolution happens once per run, at plan time, before any beam starts: a
beam whose plugin cannot be found fails the whole run immediately, rather
than partway through. If the name is a close match for a built-in
executor's, the error suggests it:

```sh
$ alba run ghost
beam `ghost` uses executor `nosuchthing`: `nosuchthing` is neither a
built-in executor nor `alba-executor-nosuchthing` on the PATH

$ alba run ghost   # with `executor dokcer` instead
beam `ghost` uses executor `dokcer`: `dokcer` is neither a built-in
executor nor `alba-executor-dokcer` on the PATH — did you mean `docker`?
```

Every option written inside the `executor <name> { ... }` block is forwarded
to the plugin verbatim, as the `open` message's `options` field (see
[Messages](#messages) below). Alba does not know or validate the shape of
those options — that is entirely the plugin's job, including reporting a bad
one back through the `error` message at handshake time.

## Lifecycle

Alba spawns one plugin process per beam. The process is started when the
beam's session opens and stays alive for every command that beam's `run`
list declares — state a command leaves behind (files on disk, anything the
plugin itself keeps in memory) is visible to the next command in the same
beam, exactly like a shell session would behave.

stdin and stdout together carry the protocol, in both directions, one JSON
object per line. Nothing else may share those streams: everything the
plugin writes to stdout must be one of the reply messages below, terminated
by a newline and flushed immediately, not buffered until later.

stderr is different: it is free-form. The plugin can write whatever
diagnostics it likes there at any point in its lifetime, and Alba relays
each line into the beam's own output as it arrives, tagged as that beam's
stderr.

The session ends with `close`, sent once, after the last `execute` has been
answered; the plugin is expected to exit its process in response. See
[Timeouts](#timeouts) for what happens if it does not.

If the plugin's process exits at any other point without being asked to —
its stdout pipe closes while Alba is still expecting a reply — that is
treated as an error naming the exit code it left behind (`plugin exited
with code <N>`, with `before answering the handshake` appended if it
happened during `open`), not a silent end of the session.

## Messages

Every message is a JSON object with a `type` field naming it, on its own
line. The examples below are the literal, byte-for-byte wire encoding —
copy them, do not reformat or re-order the fields.

### Host to plugin

Sent on the plugin's stdin.

| Message | When | Wire example |
| --- | --- | --- |
| `open` | Once, first, to start the session. | `{"type":"open","protocol":1,"beam":"deploy","dir":"/abs/path","options":{"image":"x"}}` |
| `execute` | Once per command in the beam's `run` list. Never sent again before the previous `execute` has been answered with `exit` or `error`. | `{"type":"execute","command":"echo hi","env":[["K","V"]],"cwd":"/abs/path"}` |
| `cancel` | While a command is running, to ask the plugin to stop it early. | `{"type":"cancel"}` |
| `close` | Once, after the last `execute` has been answered, to end the session. | `{"type":"close"}` |

`open`'s fields:

- `protocol` — the protocol version Alba speaks. Always `1` today; see
  [Versioning](#versioning).
- `beam` — the beam's name.
- `dir` — the beam's directory, as an absolute path.
- `options` — whatever JSON value the beam's `executor <name> { ... }` block
  rendered to, forwarded verbatim. A beam with no options at all sends
  `options` as JSON `null`, not an absent field.

`execute`'s fields:

- `command` — the exact, already-rendered command text from the beam's
  `run` entry (template interpolation already applied).
- `env` — the environment variables for this command, as an array of
  two-element `[key, value]` arrays — **not** a JSON object. `{"K":"V"}`
  is not valid on this wire; `[["K","V"]]` is.
- `cwd` — the working directory this command should run in, as an absolute
  path.

`cancel` and `close` carry no fields beyond `type`.

### Plugin to host

Sent on the plugin's stdout.

| Message | When | Wire example |
| --- | --- | --- |
| `ready` | Answers `open` when the plugin accepts the session. | `{"type":"ready"}` |
| `output` | Zero or more times per command, as it produces output. | `{"type":"output","stream":"stderr","text":"warm"}` |
| `exit` | Exactly once per command, to end it. | `{"type":"exit","code":3}` |
| `error` | Answers `open` when the plugin refuses the session (see [Versioning](#versioning)), or ends an `execute` the plugin cannot finish normally. | `{"type":"error","message":"protocol 2 not supported"}` |

- `output`'s `stream` is either `"stdout"` or `"stderr"` (lowercase); `text`
  is the line the command produced on that stream.
- `exit`'s `code` is the command's exit code, a signed 32-bit integer (a
  negative value, such as `-1`, is how the reference plugin reports a
  command it stopped early because of a `cancel`; any other convention is
  up to the plugin).
- `error`'s `message` is free-form text shown to whoever is watching the
  beam run. Answering `open` with `error` ends the session immediately —
  the plugin is expected to exit right after sending it, and Alba does not
  send anything further to it.

A line that fails to parse as JSON, or parses but does not match any of
these shapes, is a protocol violation: Alba reports it (quoting the
offending line) and tears the session down. `ready` is only ever valid as
the answer to `open`; sending it at any other point is also a violation.

## Versioning

`open` carries the protocol version Alba speaks as `protocol`, currently
always `1`. A plugin that supports it answers `ready`. A plugin that does
not — an older Alba speaking a version the plugin has since dropped, or a
newer one the plugin does not know yet — answers `error` (a message
mentioning the unsupported version is conventional, though not required)
and is expected to exit; Alba does not send it anything past that.

## Cancellation

While a command is running (after `execute`, before its `exit` or `error`),
Alba can send `cancel` — when the beam's run is interrupted, or another beam
it depends on fails without `allow_failure`. The plugin should stop the
command as soon as it reasonably can and then still answer for it, exactly
as any other command ends: one or more `output` lines if there is anything
left to report, followed by exactly one `exit` (or `error`).

Alba gives the plugin **5 seconds** from sending `cancel` to receive that
final answer. A plugin that has not replied by then is killed outright —
process and, on unix, its whole process group — rather than waited on
further, so answer promptly rather than trying to finish the command's
remaining work first.

## Timeouts

`open` is bounded by a **10-second** handshake timeout: if the plugin has
not answered `ready` or `error` within 10 seconds of receiving `open`, Alba
gives up, kills the process, and reports the beam as failed. A plugin doing
nontrivial setup during `open` (pulling an image, warming a cache) needs to
finish it inside that window.

## Conformance

`alba plugin check <BINARY>` drives a plugin binary through the whole
protocol end to end and reports whether it conforms, without needing a
Beamfile or a beam to run it from:

```sh
$ alba plugin check target/debug/alba-executor-example --command "echo probe" --cancel-command "sleep 30000"
✓ handshake
✓ execute: exit code 0, 1 output line
✓ cancel (answered in 102.537167ms)
✓ close
conformant: protocol v1
```

Four checks run in order:

1. **handshake** — opens a session against the binary (`open`, waiting for
   `ready` or `error`).
2. **execute** — runs `--command` (default `echo alba-plugin-check`) on that
   same session and waits for its `exit`.
3. **cancel** — opens a **fresh** session (a new process, never the one
   `execute` just used) and runs `--cancel-command` (default `sleep 30`) on
   it, sending `cancel` 100ms in. This check does not just check that
   `execute` eventually returns: a plugin that never answers `cancel` at all
   is absorbed by Alba's own 5-second grace and force-killed, which would
   otherwise look identical to a plugin that answered promptly. The check
   times how long the answer actually took and only passes a plugin that
   replied well inside the grace — a plugin that only "answers" because the
   host had to kill it is reported as **not** conformant.
4. **close** — closes that same fresh session.

If the handshake itself fails, the other three checks are skipped rather
than attempted against a binary already known to be unresponsive. A plugin
binary that does not exist at all, or that never answers the handshake, is
reported and the command exits non-zero:

```sh
$ alba plugin check /no/such/binary
cannot check `/no/such/binary`: no such file

$ alba plugin check ./a-binary-that-never-answers
✗ handshake: plugin did not answer the handshake within 10s
not conformant
```

`alba plugin check` exits `0` when every check passes, `1` when the binary
ran but failed one or more checks, and `2` for a problem on Alba's own side
(a missing binary path, a runtime that fails to start) rather than a verdict
on the plugin.
