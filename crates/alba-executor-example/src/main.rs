//! `alba-executor-example`: the reference implementation of Alba's plugin
//! wire protocol.
//!
//! This is the file a third-party plugin author should read end to end
//! before writing their own `alba-executor-<name>` binary. It intentionally
//! depends on nothing but `serde_json` and the standard library — no
//! `alba-executors` crate, no async runtime — to prove that the wire, not
//! any Rust type, is the actual contract a plugin author codes against.
//! Any language that can read a line, parse JSON, and write a line back
//! can implement this same protocol.
//!
//! # The protocol, from a plugin's point of view
//!
//! Alba spawns your binary and talks to it over its own stdin and
//! stdout — one JSON object per line, in both directions. Nothing else may
//! share those streams: your stdin only ever carries host messages, and
//! anything you write to stdout must be one of the reply messages below,
//! newline-terminated, and flushed before you go back to waiting for the
//! next line. Your stderr is free: write whatever diagnostics you like
//! there and Alba relays them into the beam's own output as they arrive.
//!
//! The host (Alba) sends, one per line:
//!
//! - `{"type":"open","protocol":1,"beam":"<name>","dir":"<cwd>","options":{..}}`
//!   — sent once, first, to start a session for one beam. Answer with
//!   `{"type":"ready"}` if you support `protocol`, or with
//!   `{"type":"error","message":"..."}` and exit if you do not. Alba waits
//!   up to 10 seconds for this answer before giving up on you.
//! - `{"type":"execute","command":"<text>","env":[["K","V"],..],"cwd":"<dir>"}`
//!   — run one command. Reply with zero or more
//!   `{"type":"output","stream":"stdout"|"stderr","text":"..."}` lines as
//!   the command produces output, then exactly one
//!   `{"type":"exit","code":<i32>}` to end it. Every `execute` in a
//!   session happens one at a time: you will not be asked to start a new
//!   command before you have replied `exit` (or `error`) to the current
//!   one.
//! - `{"type":"cancel"}` — asks you to stop the command currently running
//!   as soon as you reasonably can, and then still reply `exit` (or
//!   `error`) for it, the same as any other command's end. Alba gives you
//!   5 seconds; a plugin that takes longer gets killed outright, so answer
//!   promptly.
//! - `{"type":"close"}` — the session is over; exit your process. Sent at
//!   most once, after the last `execute` has been answered.
//!
//! Your obligations, restated plainly: answer the handshake, one JSON
//! object per line, flush every reply immediately, and answer `cancel`
//! promptly rather than ignoring it. Everything else — how you run
//! commands, what `options` means to you, whether you keep state across
//! `execute` calls — is entirely up to you.
//!
//! # This plugin's command vocabulary
//!
//! `alba-executor-example` understands exactly four kinds of command text,
//! matched on the first word:
//!
//! - `echo <text>` — writes `<text>` to stdout, then exits 0.
//! - `fail <code>` — exits with `<code>` (default 1 if it does not parse)
//!   without any output.
//! - `sleep <ms>` — waits `<ms>` milliseconds, exiting 0 when the time is
//!   up, or exiting -1 immediately if a `cancel` arrives first. This is
//!   the one command here that demonstrates answering `cancel` — see
//!   `run_sleep` below.
//! - anything else — writes `unknown command` to stderr (as a protocol
//!   `output` message, not a raw stderr write) and exits 127.
//!
//! # Why a thread, not async
//!
//! One background thread does nothing but read stdin line by line and
//! forward each parsed line to the main thread over an `mpsc` channel.
//! The main thread — the one actually implementing the protocol above —
//! never touches stdin directly; it only ever reads from that channel,
//! which is what lets `run_sleep` wait for either a timeout or a `cancel`
//! line at the same time using nothing more exotic than
//! `Receiver::recv_timeout`. That is the whole reason this plugin needs no
//! async runtime: a single blocking reader thread plus a channel gives the
//! same "wait for one of several things" ability tokio would, at a
//! fraction of the machinery.

use std::io::{self, BufRead, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

fn main() {
    let (sender, receiver) = mpsc::channel::<serde_json::Value>();

    // The one thread that ever reads stdin. A line that is not valid JSON
    // is skipped rather than treated as fatal — the protocol is
    // line-delimited JSON, and a single stray line must not take the
    // whole plugin down. When stdin reaches EOF (or a read fails) the
    // thread ends and drops `sender`, which is what lets the main loop's
    // `recv`/`recv_timeout` calls unblock with an error instead of
    // hanging forever on a host that went away.
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line)
                && sender.send(value).is_err()
            {
                break;
            }
        }
    });

    run(&receiver);
}

/// The session: the handshake, then commands until `close` (or the host
/// goes away).
fn run(receiver: &Receiver<serde_json::Value>) {
    if !handshake(receiver) {
        return;
    }

    while let Ok(message) = receiver.recv() {
        match message_type(&message) {
            Some("execute") => {
                let command = message
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                execute(command, receiver);
            }
            Some("close") => std::process::exit(0),
            // A stray `cancel` with no command running, or any other
            // message, has nothing to act on here.
            _ => {}
        }
    }
    // Stdin closed without an explicit `close`. A real host always sends
    // one, but exiting cleanly on a dropped connection instead of hanging
    // is the safer default for a plugin to have.
}

/// Waits for the opening `{"type":"open",...}` message and answers it.
/// Returns whether the session may continue (`true` for `ready`, `false`
/// for an unsupported protocol version or a host that vanished before
/// sending `open` at all).
fn handshake(receiver: &Receiver<serde_json::Value>) -> bool {
    let Ok(open) = receiver.recv() else {
        return false;
    };
    match open.get("protocol").and_then(serde_json::Value::as_u64) {
        Some(1) => {
            reply(&serde_json::json!({"type": "ready"}));
            true
        }
        other => {
            let got = other.map_or("missing".to_string(), |p| p.to_string());
            reply(&serde_json::json!({
                "type": "error",
                "message": format!("protocol {got} not supported"),
            }));
            false
        }
    }
}

/// Runs one `execute` command to completion, replying with whatever the
/// matched command kind requires.
fn execute(command: &str, receiver: &Receiver<serde_json::Value>) {
    let mut words = command.splitn(2, ' ');
    let verb = words.next().unwrap_or_default();
    let rest = words.next().unwrap_or_default();

    match verb {
        "echo" => {
            reply(&serde_json::json!({
                "type": "output",
                "stream": "stdout",
                "text": rest,
            }));
            reply(&serde_json::json!({"type": "exit", "code": 0}));
        }
        "fail" => {
            let code: i32 = rest.trim().parse().unwrap_or(1);
            reply(&serde_json::json!({"type": "exit", "code": code}));
        }
        "sleep" => {
            let millis: u64 = rest.trim().parse().unwrap_or(0);
            run_sleep(Duration::from_millis(millis), receiver);
        }
        _ => {
            reply(&serde_json::json!({
                "type": "output",
                "stream": "stderr",
                "text": "unknown command",
            }));
            reply(&serde_json::json!({"type": "exit", "code": 127}));
        }
    }
}

/// Waits out `duration`, answering a `cancel` that arrives within that
/// window immediately instead of waiting out the rest of the sleep.
///
/// This is the one place in the plugin that has to wait for two different
/// things at once — the clock and the host — and it does so with nothing
/// more than `Receiver::recv_timeout` against a shrinking deadline: no
/// select loop, no runtime, just a blocking read with a bound on it.
fn run_sleep(duration: Duration, receiver: &Receiver<serde_json::Value>) {
    let deadline = Instant::now() + duration;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            reply(&serde_json::json!({"type": "exit", "code": 0}));
            return;
        }
        match receiver.recv_timeout(remaining) {
            Ok(message) if message_type(&message) == Some("cancel") => {
                reply(&serde_json::json!({"type": "exit", "code": -1}));
                return;
            }
            // Anything else while sleeping (there is nothing else the
            // host is expected to send mid-command besides `cancel`) is
            // ignored, and the wait continues for whatever time is left.
            Ok(_other) => continue,
            Err(RecvTimeoutError::Timeout) => {
                reply(&serde_json::json!({"type": "exit", "code": 0}));
                return;
            }
            Err(RecvTimeoutError::Disconnected) => {
                // Stdin closed mid-sleep: the host is gone, so there is
                // no `cancel` left to arrive and nobody left to read a
                // reply either.
                return;
            }
        }
    }
}

fn message_type(message: &serde_json::Value) -> Option<&str> {
    message.get("type").and_then(serde_json::Value::as_str)
}

/// Writes one reply as a single JSON line to stdout and flushes it
/// immediately. Every reply in this file goes through here: a plugin that
/// buffers instead of flushing per line leaves the host waiting on a line
/// that is sitting in this process's userspace buffer rather than the
/// pipe it is reading from.
fn reply(message: &serde_json::Value) {
    let mut stdout = io::stdout();
    let _ = writeln!(stdout, "{message}");
    let _ = stdout.flush();
}
