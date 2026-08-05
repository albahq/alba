//! [`FakeExecutor`]: a scriptable, in-memory [`Executor`] used to test the
//! scheduler without spawning real processes.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{
    BeamContext, CommandSpec, ExecContext, ExecError, ExecResult, ExecSession, Executor, OutputLine,
};

/// What a scripted command should do when [`FakeExecutor`] runs it: the
/// exit code to report, how long to (asynchronously) delay before
/// reporting it, and what output lines to emit first.
#[derive(Debug, Clone, Default)]
pub struct FakeBehavior {
    pub exit_code: i32,
    pub delay: Duration,
    pub output_lines: Vec<OutputLine>,
}

/// One thing a [`FakeExecutor`] (or a session it opened) was asked to do,
/// in the order it happened — what the scheduler tests assert against to
/// pin the exact shape of the `open`/`execute`/`close` bracket around a
/// beam's commands.
#[derive(Debug, Clone, PartialEq)]
pub enum FakeEvent {
    Opened {
        beam: String,
        options: serde_json::Value,
    },
    Executed {
        command: String,
    },
    Closed {
        beam: String,
    },
}

/// The state shared between [`FakeExecutor`] and every [`FakeSession`] it
/// opens, behind an `Arc` so a session outlives the borrow of the `&self`
/// call that created it.
///
/// All state is interior-mutable (`Mutex`/`AtomicUsize`) so a single
/// instance can be shared behind `Arc<dyn Executor>` and driven
/// concurrently by a scheduler running several beams in parallel — exactly
/// the scenario [`FakeExecutor::running_peak`] exists to assert against.
struct FakeState {
    behaviors: Mutex<Vec<(String, FakeBehavior)>>,
    calls: Mutex<Vec<CommandSpec>>,
    events: Mutex<Vec<FakeEvent>>,
    /// `(beam_substring, message)`: `open` fails with `message` for any
    /// beam whose label contains `beam_substring`.
    open_failures: Mutex<Vec<(String, String)>>,
    running: AtomicUsize,
    peak: AtomicUsize,
}

impl FakeState {
    fn behavior_for(&self, command: &str) -> FakeBehavior {
        self.behaviors
            .lock()
            .unwrap()
            .iter()
            .find(|(substring, _)| command.contains(substring.as_str()))
            .map(|(_, behavior)| behavior.clone())
            .unwrap_or_default()
    }
}

/// A test double for [`Executor`] that records every session it opened and
/// every command it was asked to run, and plays back a scripted
/// [`FakeBehavior`] instead of spawning anything.
///
/// Matching is by substring against [`CommandSpec::command`]: the first
/// registered `(substring, behavior)` pair whose substring appears in the
/// command wins. A command matching nothing gets the default behavior
/// (exit code 0, no delay, no output).
pub struct FakeExecutor {
    state: Arc<FakeState>,
}

impl FakeExecutor {
    pub fn new() -> Self {
        Self {
            state: Arc::new(FakeState {
                behaviors: Mutex::new(Vec::new()),
                calls: Mutex::new(Vec::new()),
                events: Mutex::new(Vec::new()),
                open_failures: Mutex::new(Vec::new()),
                running: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
            }),
        }
    }

    /// Registers a behavior for any command whose text contains
    /// `command_substring`. Consumes and returns `self` so registrations
    /// can be chained: `FakeExecutor::new().on("build", ..).on("test", ..)`.
    pub fn on(self, command_substring: impl Into<String>, behavior: FakeBehavior) -> Self {
        self.state
            .behaviors
            .lock()
            .unwrap()
            .push((command_substring.into(), behavior));
        self
    }

    /// Makes `open` fail for any beam whose label contains
    /// `beam_substring`, with `message` as the resulting [`ExecError`].
    /// Consumes and returns `self`, chainable like [`FakeExecutor::on`].
    pub fn fail_open(self, beam_substring: impl Into<String>, message: impl Into<String>) -> Self {
        self.state
            .open_failures
            .lock()
            .unwrap()
            .push((beam_substring.into(), message.into()));
        self
    }

    /// Every command handed to a session's [`ExecSession::execute`], in
    /// invocation order.
    pub fn calls(&self) -> Vec<CommandSpec> {
        self.state.calls.lock().unwrap().clone()
    }

    /// Every [`FakeEvent`] this executor and the sessions it opened
    /// recorded, in the order they happened.
    pub fn events(&self) -> Vec<FakeEvent> {
        self.state.events.lock().unwrap().clone()
    }

    /// The maximum number of executions that were running concurrently at
    /// any point in this executor's lifetime.
    pub fn running_peak(&self) -> usize {
        self.state.peak.load(Ordering::SeqCst)
    }
}

impl Default for FakeExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Executor for FakeExecutor {
    async fn open(&self, beam: BeamContext) -> Result<Box<dyn ExecSession>, ExecError> {
        self.state.events.lock().unwrap().push(FakeEvent::Opened {
            beam: beam.beam.clone(),
            options: beam.options.clone(),
        });

        let failure = self
            .state
            .open_failures
            .lock()
            .unwrap()
            .iter()
            .find(|(substring, _)| beam.beam.contains(substring.as_str()))
            .map(|(_, message)| message.clone());
        if let Some(message) = failure {
            return Err(ExecError { message });
        }

        Ok(Box::new(FakeSession {
            state: Arc::clone(&self.state),
            beam: beam.beam,
        }))
    }
}

/// The session a [`FakeExecutor`] opens for one beam: shares the
/// executor's interior state, so calls made through this session still show
/// up in [`FakeExecutor::calls`], [`FakeExecutor::events`], and
/// [`FakeExecutor::running_peak`].
struct FakeSession {
    state: Arc<FakeState>,
    beam: String,
}

/// Decrements the running counter on drop, so the count is correct even if
/// `execute` returns early. Holding only this guard (never a lock) across
/// the `.await` below is what keeps concurrent invocations from serializing
/// on each other.
struct RunningGuard<'a>(&'a AtomicUsize);

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ExecSession for FakeSession {
    async fn execute(
        &mut self,
        cmd: CommandSpec,
        ctx: ExecContext,
    ) -> Result<ExecResult, ExecError> {
        // Lock, record, and drop the guards before ever awaiting: no lock
        // is held across an await point, so concurrent calls do not
        // serialize on `behaviors`/`calls` and `running_peak` observes true
        // concurrency rather than an artifact of lock contention.
        self.state.calls.lock().unwrap().push(cmd.clone());
        self.state.events.lock().unwrap().push(FakeEvent::Executed {
            command: cmd.command.clone(),
        });
        let behavior = self.state.behavior_for(&cmd.command);

        let now_running = self.state.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.state.peak.fetch_max(now_running, Ordering::SeqCst);
        let _guard = RunningGuard(&self.state.running);

        if !behavior.delay.is_zero() {
            // Race the scripted delay against cancellation, mirroring
            // SystemShellExecutor's contract: a cancelled run returns
            // promptly rather than waiting out its full scripted delay.
            // -1 (no discrete exit code) matches the fallback
            // SystemShellExecutor reports for a killed child.
            tokio::select! {
                _ = tokio::time::sleep(behavior.delay) => {}
                _ = ctx.cancel.cancelled() => {
                    return Ok(ExecResult { exit_code: -1 });
                }
            }
        }

        for line in &behavior.output_lines {
            let _ = ctx.output.send(line.clone());
        }

        Ok(ExecResult {
            exit_code: behavior.exit_code,
        })
    }

    async fn close(self: Box<Self>) -> Result<(), ExecError> {
        self.state
            .events
            .lock()
            .unwrap()
            .push(FakeEvent::Closed { beam: self.beam });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(command: &str) -> CommandSpec {
        CommandSpec {
            command: command.into(),
            env: vec![],
            cwd: std::env::current_dir().unwrap(),
        }
    }

    fn ctx() -> (
        ExecContext,
        tokio::sync::mpsc::UnboundedReceiver<OutputLine>,
    ) {
        let (output, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            ExecContext {
                output,
                cancel: Default::default(),
            },
            rx,
        )
    }

    fn beam_context(
        beam: &str,
    ) -> (
        BeamContext,
        tokio::sync::mpsc::UnboundedReceiver<OutputLine>,
    ) {
        let (output, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            BeamContext {
                beam: beam.to_string(),
                dir: std::env::current_dir().unwrap(),
                options: serde_json::Value::Null,
                output,
                cancel: Default::default(),
            },
            rx,
        )
    }

    #[tokio::test]
    async fn records_calls_in_invocation_order() {
        let executor = FakeExecutor::new();
        let (beam, _rx) = beam_context("beam");
        let mut session = executor.open(beam).await.unwrap();
        for command in ["first", "second", "third"] {
            let (context, _rx) = ctx();
            session.execute(spec(command), context).await.unwrap();
        }
        let calls: Vec<_> = executor.calls().into_iter().map(|c| c.command).collect();
        assert_eq!(calls, vec!["first", "second", "third"]);
    }

    #[tokio::test]
    async fn matches_behavior_by_substring_and_emits_its_output() {
        let executor = FakeExecutor::new().on(
            "build",
            FakeBehavior {
                exit_code: 7,
                delay: Duration::default(),
                output_lines: vec![OutputLine {
                    stream: crate::Stream::Stdout,
                    text: "building".into(),
                }],
            },
        );
        let (beam, _rx) = beam_context("beam");
        let mut session = executor.open(beam).await.unwrap();
        let (context, mut rx) = ctx();
        let result = session
            .execute(spec("run build now"), context)
            .await
            .unwrap();
        assert_eq!(result.exit_code, 7);
        assert_eq!(rx.recv().await.unwrap().text, "building");
    }

    #[tokio::test]
    async fn unmatched_command_gets_the_default_behavior() {
        let executor = FakeExecutor::new().on(
            "build",
            FakeBehavior {
                exit_code: 7,
                ..Default::default()
            },
        );
        let (beam, _rx) = beam_context("beam");
        let mut session = executor.open(beam).await.unwrap();
        let (context, _rx) = ctx();
        let result = session.execute(spec("test"), context).await.unwrap();
        assert_eq!(result.exit_code, 0);
    }

    /// The scenario the counter exists for: two beams that overlap
    /// under `--jobs 2` must show `running_peak() == 2`, not 1. Uses a
    /// real multi-threaded runtime so this exercises actual cross-thread
    /// atomic accounting, not just single-threaded cooperative
    /// interleaving.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn running_peak_reflects_true_concurrency_not_a_running_total() {
        let executor = Arc::new(FakeExecutor::new().on(
            "slow",
            FakeBehavior {
                exit_code: 0,
                delay: Duration::from_millis(100),
                output_lines: vec![],
            },
        ));

        let mut handles = Vec::new();
        for i in 0..3 {
            let executor = Arc::clone(&executor);
            handles.push(tokio::spawn(async move {
                let (beam, _rx) = beam_context(&format!("beam-{i}"));
                let mut session = executor.open(beam).await.unwrap();
                let (context, _rx) = ctx();
                session
                    .execute(spec(&format!("slow {i}")), context)
                    .await
                    .unwrap();
                session.close().await.unwrap();
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }

        assert_eq!(executor.running_peak(), 3);
        assert_eq!(executor.calls().len(), 3);
    }

    #[tokio::test]
    async fn cancellation_during_delay_returns_before_the_full_delay_elapses() {
        let executor = FakeExecutor::new().on(
            "slow",
            FakeBehavior {
                exit_code: 0,
                delay: Duration::from_secs(30),
                output_lines: vec![],
            },
        );
        let (beam, _rx) = beam_context("beam");
        let mut session = executor.open(beam).await.unwrap();

        let cancel = tokio_util::sync::CancellationToken::new();
        let (output, _rx) = tokio::sync::mpsc::unbounded_channel();
        let context = ExecContext {
            output,
            cancel: cancel.clone(),
        };

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel.cancel();
        });

        let start = tokio::time::Instant::now();
        let result = session.execute(spec("slow"), context).await.unwrap();

        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(result.exit_code, -1);
    }

    #[tokio::test]
    async fn open_records_the_opened_event_with_the_beam_and_options() {
        let executor = FakeExecutor::new();
        let (beam, _rx) = beam_context("build");
        let _session = executor.open(beam).await.unwrap();

        assert_eq!(
            executor.events(),
            vec![FakeEvent::Opened {
                beam: "build".to_string(),
                options: serde_json::Value::Null,
            }]
        );
    }

    #[tokio::test]
    async fn fail_open_matches_by_substring_and_carries_the_message() {
        let executor = FakeExecutor::new().fail_open("bui", "no runtime here");
        let (beam, _rx) = beam_context("build");
        let error = match executor.open(beam).await {
            Err(error) => error,
            Ok(_) => panic!("expected `open` to fail"),
        };
        assert_eq!(error.to_string(), "no runtime here");

        let (other, _rx) = beam_context("test");
        assert!(executor.open(other).await.is_ok());
    }

    #[tokio::test]
    async fn close_records_the_closed_event_with_the_beam() {
        let executor = FakeExecutor::new();
        let (beam, _rx) = beam_context("build");
        let session = executor.open(beam).await.unwrap();
        session.close().await.unwrap();

        assert_eq!(
            executor.events(),
            vec![
                FakeEvent::Opened {
                    beam: "build".to_string(),
                    options: serde_json::Value::Null,
                },
                FakeEvent::Closed {
                    beam: "build".to_string(),
                },
            ]
        );
    }
}
