//! The one test that runs the real plumbing: a pseudo-terminal, the real
//! binary, the real alternate screen. Deliberately small — confidence
//! comes from the layers below; this only proves they are actually wired.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};

/// Entering and leaving the alternate screen: the two escape sequences a
/// real terminal session must produce and, on quit, retract.
const ALTERNATE_SCREEN_ENTER: &str = "\u{1b}[?1049h";
const ALTERNATE_SCREEN_LEAVE: &str = "\u{1b}[?1049l";

/// The word the header's own account of a finished run carries (see
/// `alba-tui/src/ui/header.rs::finished_line`, format `"alba · run
/// {target} finished · {counts} · {duration}"`) and nothing else in this
/// interface ever renders. Unlike the beam's own output — which lands on
/// the pty as soon as `RunEvent::BeamOutput` is applied, well before the
/// run is over — this word is only drawn once `Phase::Finished` and
/// `last_summary` are set, in the very same `RunEvent::RunFinished` match
/// arm that sets `AppState::outcome` (`state.rs`, the field
/// `exit_outcome()` reads). So seeing it on the pty is synchronized with
/// the exit code actually being decided.
///
/// It also has to survive ratatui's own diffing, which this test does not
/// get to skip: the header is one plain, unstyled `Line`
/// (`ui/mod.rs::draw`), and `Buffer::diff` only ever forwards a cell whose
/// symbol or style changed from the previously drawn frame
/// (`ratatui::buffer::Buffer::diff`) — a cell that happens to match its
/// predecessor is silently dropped from the byte stream, cursor-jumped
/// over instead of printed. The frame right before this one's first
/// appearance is either the idle header (`"alba · {target} · idle"`,
/// drawn once before any event lands) or a `Running` header (`"alba · run
/// {target} ── {bar} {done}/{total} · {duration}"`, drawn on every
/// `RunEvent` batch while the beam is in flight) — nothing else is
/// reachable for a one-beam, non-watch run. At the column where
/// `"finished"` starts (right after `"{target} "`), the idle header has
/// either run out of characters (its own text is shorter and ends inside
/// the word "idle") or is drawing "─"/a bar cell/a digit there — never a
/// Latin letter — so every one of the word's 8 cells differs from either
/// possible predecessor at that same screen position, letter by letter,
/// independently of the run's specific timings, digits or bar fill. That
/// is what the original `"run ok finished"` token got wrong: its `"run
/// ok"` prefix is byte-identical to the `Running` header's own `"run
/// ok"`, so it could be skipped by the diff entirely, leaving this loop
/// spinning to the deadline. `"finished"` alone starts past that shared
/// prefix, in the region the two headers always disagree on.
const RUN_FINISHED: &str = "finished";

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

#[test]
fn the_tui_opens_restores_and_replays_on_q() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("Beamfile"),
        "version \"1\"\n\nbeam ok {\n  run \"echo hello-from-the-beam\"\n}\n",
    )
    .unwrap();

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(assert_cmd::cargo::cargo_bin("alba"));
    command.args(["run", "ok"]);
    command.cwd(project.path());
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

    // Read until the run has actually finished, evidenced by the header's
    // own finished-run line — not merely the alternate screen opening,
    // and not merely the beam's output showing up. Entering the
    // alternate screen happens well before the beam completes, and the
    // beam's output reaches the pty (via `RunEvent::BeamOutput`) before
    // the run is scored (via the later, distinct `RunEvent::RunFinished`
    // that sets `AppState::outcome`) — so neither is proof the run is
    // over. Quitting before it is over now correctly earns exit code 130
    // (a run that never finished does not get to vouch for a code), so
    // `q` must not be sent until `RUN_FINISHED` — synchronized with the
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
        if seen.contains(ALTERNATE_SCREEN_ENTER) && seen.contains(RUN_FINISHED) {
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
    // replay): bounded by the same deadline, so a stuck restore fails
    // the test instead of hanging it.
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match chunks_rx.recv_timeout(remaining) {
            Ok(chunk) => seen.push_str(&String::from_utf8_lossy(&chunk)),
            Err(_) => break,
        }
    }

    assert!(
        seen.contains(ALTERNATE_SCREEN_ENTER),
        "the alternate screen was never entered"
    );
    assert!(
        seen.contains(ALTERNATE_SCREEN_LEAVE),
        "the alternate screen was never left"
    );
    assert!(
        seen.contains("hello-from-the-beam"),
        "no trace of the run in the replay; got tail: {:?}",
        &seen[seen.len().saturating_sub(500)..]
    );
    assert_eq!(status.exit_code(), 0, "a green run quit with q exits 0");
}
