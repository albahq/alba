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

    // Read until the run has actually finished, evidenced by the beam's
    // own output landing on the pty — not merely the alternate screen
    // opening. Entering the alternate screen happens well before the
    // beam completes; quitting any earlier now correctly earns exit code
    // 130 (a run that never finished does not get to vouch for a code),
    // so `q` must not be sent until there is a completed run to report
    // on. The output arrives inside the alternate screen, so it may be
    // split across reads and interleaved with escape sequences — search
    // the accumulated buffer, not a single chunk.
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("the run never finished on the pty; got: {seen:?}");
        }
        match chunks_rx.recv_timeout(remaining) {
            Ok(chunk) => seen.push_str(&String::from_utf8_lossy(&chunk)),
            Err(_) => panic!("the run never finished on the pty; got: {seen:?}"),
        }
        if seen.contains(ALTERNATE_SCREEN_ENTER) && seen.contains("hello-from-the-beam") {
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
