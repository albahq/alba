//! The one test that runs the real plumbing: a pseudo-terminal, the real
//! binary, the real alternate screen. Deliberately small — confidence
//! comes from the layers below; this only proves they are actually wired.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};

/// A green, one-beam project: the fixture the original smoke test drives.
const GREEN: &str = "version \"1\"\n\nbeam green {\n  run \"echo hello-from-the-beam\"\n}\n";

/// A beam that fails after printing a coloured line, for the replay's own
/// colour test.
const RED: &str = "version \"1\"\n\nbeam red {\n  run \"sh red.sh\"\n}\n";
const RED_SCRIPT: &str = "printf '\\033[31mred\\033[0m\\n'\nexit 1\n";

/// Entering and leaving the alternate screen: the two escape sequences a
/// real terminal session must produce and, on quit, retract.
const ALTERNATE_SCREEN_ENTER: &str = "\u{1b}[?1049h";
const ALTERNATE_SCREEN_LEAVE: &str = "\u{1b}[?1049l";

/// The outcome word the footer draws once a run is over (see
/// `alba-tui/src/ui/footer.rs::outcome_line`: `ok · {duration}` or
/// `failed · {duration}`, right-aligned) and nothing else in this
/// interface ever renders: the fixtures' beams are `green` and `red`,
/// the bottom bar's default keymap text, the pane titles, and the follow
/// state spell neither word, and the beams' own output does not either
/// (a failed copy takes over the bar with the literal `copy failed`
/// from `copy.rs`, but this test never enters copy mode, so that
/// exception never fires here). Unlike that output,
/// which lands on the pty as soon as `RunEvent::BeamOutput` is applied,
/// the word is only drawn once `Phase::Finished` and `last_summary` are
/// set, in the very same `RunEvent::RunFinished` match arm that sets
/// `AppState::outcome` (`state.rs`, the field `exit_outcome()` reads).
/// So seeing it on the pty is synchronized with the exit code actually
/// being decided.
///
/// It also has to survive ratatui's own diffing, which this test does
/// not get to skip: `Buffer::diff` only forwards a cell whose symbol or
/// style changed from the previously drawn frame, and silently
/// cursor-jumps over one that did not. The frame right before the
/// outcome's first appearance shows, at the footer's right end, either
/// `idle` (drawn once before any event lands) or the in-flight text
/// `{bar} {done}/{total} · {duration}` (drawn on every `RunEvent` batch
/// while the beam runs). Both words are right-aligned, so their letters
/// land on cells that held, in the predecessor, a bar cell, a digit, a
/// `/`, a space, or nothing (`idle` is four cells wide and both words
/// sit further left than that), never the same Latin letter. Every
/// letter therefore differs from its predecessor and is emitted,
/// contiguously, whatever the run's timings.
const RUN_OK: &str = "ok";
const RUN_FAILED: &str = "failed";

/// What the exit replay owes for this fixture's green run: the count
/// `alba`'s `render::print_summary` puts in its summary line
/// (`✓ 1 succeeded · 0.0s`), written to stderr once the alternate screen
/// is already restored. Nothing the interface draws ever spells this —
/// the frame's footer counts with glyphs instead (`✔ 1`) — so seeing it
/// in the bytes that follow `q` is evidence of the replay itself, not of
/// a frame.
const RUN_SUMMARY: &str = "1 succeeded";

/// Kills the child on every exit path — including a failed assertion or a
/// deadline expiry — so a failing smoke test never leaves a stray `alba`
/// holding the pty open behind it.
struct KillOnDrop(Box<dyn Child + Send + Sync>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Runs `alba run <beam>` on a pty against `beamfile`, with `env` added
/// to the child's environment, waits for the run to finish, sends `q`,
/// and returns what the interface drew, what followed the quit, and the
/// exit status.
fn drive(
    beamfile: &str,
    beam: &str,
    env: &[(&str, &str)],
) -> (String, String, portable_pty::ExitStatus) {
    drive_with_script(beamfile, beam, "", env)
}

/// Same as [`drive`], but also writes `script` to `red.sh` next to the
/// Beamfile when it is not empty, for a beam whose `run` shells out to it.
fn drive_with_script(
    beamfile: &str,
    beam: &str,
    script: &str,
    env: &[(&str, &str)],
) -> (String, String, portable_pty::ExitStatus) {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("Beamfile"), beamfile).unwrap();
    if !script.is_empty() {
        std::fs::write(project.path().join("red.sh"), script).unwrap();
    }

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(assert_cmd::cargo::cargo_bin("alba"));
    command.args(["run", beam]);
    command.cwd(project.path());
    for (name, value) in env {
        command.env(name, value);
    }
    let mut child = KillOnDrop(pty.slave.spawn_command(command).unwrap());
    // The slave's own handle is not needed once the child holds it; drop
    // it so the master sees EOF once the child's copy closes too, rather
    // than being kept alive by a slave reference this test never uses.
    drop(pty.slave);

    let mut reader = pty.master.try_clone_reader().unwrap();
    let mut writer = pty.master.take_writer().unwrap();

    // Read on a background thread and forward raw chunks over a channel:
    // a `Read::read` on a pty can itself block indefinitely, so the only
    // way to bound the wait is to never let the main thread call it
    // directly — `recv_timeout` below is what actually enforces the
    // deadline, regardless of what the reader thread is doing.
    let (chunks_tx, chunks_rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if chunks_tx.send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = String::new();

    // Read until the run has actually finished, evidenced by the footer's
    // own outcome word — not merely the alternate screen opening,
    // and not merely the beam's output showing up. Entering the
    // alternate screen happens well before the beam completes, and the
    // beam's output reaches the pty (via `RunEvent::BeamOutput`) before
    // the run is scored (via the later, distinct `RunEvent::RunFinished`
    // that sets `AppState::outcome`) — so neither is proof the run is
    // over. Quitting before it is over now correctly earns exit code 130
    // (a run that never finished does not get to vouch for a code), so
    // `q` must not be sent until `RUN_OK` or `RUN_FAILED` — synchronized with the
    // outcome by construction, see its doc comment — has appeared. Output
    // arrives inside the alternate screen, so it may be split across
    // reads and interleaved with escape sequences — search the
    // accumulated buffer, not a single chunk.
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("the run never finished on the pty; got: {seen:?}");
        }
        match chunks_rx.recv_timeout(remaining) {
            Ok(chunk) => seen.push_str(&String::from_utf8_lossy(&chunk)),
            Err(_) => panic!("the run never finished on the pty; got: {seen:?}"),
        }
        if seen.contains(ALTERNATE_SCREEN_ENTER)
            && (seen.contains(RUN_OK) || seen.contains(RUN_FAILED))
        {
            break;
        }
    }

    writer.write_all(b"q").unwrap();
    writer.flush().unwrap();

    // Poll rather than block on `wait()`: a session that stops honouring
    // `q` must fail this test with a deadline, not hang the suite.
    let status = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("alba never exited after q; got: {seen:?}");
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        std::thread::sleep(Duration::from_millis(20).min(remaining));
    };

    // Drain whatever the quit produced (the restore sequence and the
    // replay) into its *own* buffer: bounded by the same deadline, so a
    // stuck restore fails the test instead of hanging it.
    //
    // Kept apart from `seen` because everything the interface drew is in
    // there already — the beam's own output included, painted into the
    // alternate screen long before `q` was sent (the wait loop above
    // depends on exactly that). An assertion over the whole buffer
    // therefore proves nothing about the replay: it passes just as well
    // with `replay` deleted outright. Only what arrives after the quit
    // can.
    let mut after_quit = String::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match chunks_rx.recv_timeout(remaining) {
            Ok(chunk) => after_quit.push_str(&String::from_utf8_lossy(&chunk)),
            Err(_) => break,
        }
    }

    assert!(
        seen.contains(ALTERNATE_SCREEN_ENTER),
        "the alternate screen was never entered"
    );
    assert!(
        after_quit.contains(ALTERNATE_SCREEN_LEAVE),
        "the alternate screen was never left; got after q: {after_quit:?}"
    );

    (seen, after_quit, status)
}

#[test]
fn the_tui_opens_restores_and_replays_on_q() {
    let (seen, after_quit, status) = drive(GREEN, "green", &[]);

    assert!(
        seen.contains("hello-from-the-beam"),
        "the beam's output never reached the screen; got tail: {:?}",
        &seen[seen.len().saturating_sub(500)..]
    );
    // The exit replay itself: this fixture's run is green, so what it
    // owes is `render::print_summary`'s own summary line — the one thing
    // here written to stderr *after* the screen is restored, and so the
    // one thing nothing the interface drew can stand in for.
    assert!(
        after_quit.contains(RUN_SUMMARY),
        "the exit replay never printed the run's summary; got after q: {after_quit:?}"
    );
    assert_eq!(status.exit_code(), 0, "a green run quit with q exits 0");
}

/// A failing beam that printed colour is replayed with that colour on a
/// terminal, and without it under NO_COLOR. The script prints colour
/// unconditionally, so this pins the replay's choice, not the forcing.
#[cfg(unix)]
#[test]
fn the_replay_keeps_colour_on_a_terminal_and_drops_it_under_no_color() {
    let (_, after_quit, status) = drive_with_script(RED, "red", RED_SCRIPT, &[]);
    assert_eq!(status.exit_code(), 1);
    assert!(
        after_quit.contains("\u{1b}[31mred"),
        "got after q: {after_quit:?}"
    );

    let (_, after_quit, status) = drive_with_script(RED, "red", RED_SCRIPT, &[("NO_COLOR", "1")]);
    assert_eq!(status.exit_code(), 1);
    assert!(
        after_quit.contains("── red ──"),
        "got after q: {after_quit:?}"
    );
    let replay = &after_quit[after_quit.find("── red ──").unwrap()..];
    assert!(
        replay.contains("red\r\n") || replay.contains("red\n"),
        "got: {replay:?}"
    );
    assert!(
        !replay.contains("\u{1b}[31m"),
        "colour leaked under NO_COLOR: {replay:?}"
    );
}
