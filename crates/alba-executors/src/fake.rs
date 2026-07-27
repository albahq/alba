//! [`FakeExecutor`]: a scriptable, in-memory [`Executor`] used to test the
//! scheduler (Task 10's `alba-engine`) without spawning real processes.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::{CommandSpec, ExecContext, ExecError, ExecResult, Executor, OutputLine};

/// What a scripted command should do when [`FakeExecutor`] runs it: the
/// exit code to report, how long to (asynchronously) delay before
/// reporting it, and what output lines to emit first.
#[derive(Debug, Clone, Default)]
pub struct FakeBehavior {
    pub exit_code: i32,
    pub delay: Duration,
    pub output_lines: Vec<OutputLine>,
}

/// A test double for [`Executor`] that records every command it was asked
/// to run and plays back a scripted [`FakeBehavior`] instead of spawning
/// anything.
///
/// Matching is by substring against [`CommandSpec::command`]: the first
/// registered `(substring, behavior)` pair whose substring appears in the
/// command wins. A command matching nothing gets the default behavior
/// (exit code 0, no delay, no output).
///
/// All state is interior-mutable (`Mutex`/`AtomicUsize`) so a single
/// instance can be shared behind `Arc<dyn Executor>` and driven
/// concurrently by a scheduler running several beams in parallel — exactly
/// the scenario [`FakeExecutor::running_peak`] exists to assert against.
pub struct FakeExecutor {
    behaviors: Mutex<Vec<(String, FakeBehavior)>>,
    calls: Mutex<Vec<CommandSpec>>,
    running: AtomicUsize,
    peak: AtomicUsize,
}

impl FakeExecutor {
    pub fn new() -> Self {
        Self {
            behaviors: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            running: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    /// Registers a behavior for any command whose text contains
    /// `command_substring`. Consumes and returns `self` so registrations
    /// can be chained: `FakeExecutor::new().on("build", ..).on("test", ..)`.
    pub fn on(self, command_substring: impl Into<String>, behavior: FakeBehavior) -> Self {
        self.behaviors
            .lock()
            .unwrap()
            .push((command_substring.into(), behavior));
        self
    }

    /// Every command handed to [`Executor::execute`], in invocation order.
    pub fn calls(&self) -> Vec<CommandSpec> {
        self.calls.lock().unwrap().clone()
    }

    /// The maximum number of executions that were running concurrently at
    /// any point in this executor's lifetime.
    pub fn running_peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

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

impl Default for FakeExecutor {
    fn default() -> Self {
        Self::new()
    }
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
impl Executor for FakeExecutor {
    async fn execute(&self, cmd: CommandSpec, ctx: ExecContext) -> Result<ExecResult, ExecError> {
        // Lock, record, and drop the guards before ever awaiting: no lock
        // is held across an await point, so concurrent calls do not
        // serialize on `behaviors`/`calls` and `running_peak` observes true
        // concurrency rather than an artifact of lock contention.
        self.calls.lock().unwrap().push(cmd.clone());
        let behavior = self.behavior_for(&cmd.command);

        let now_running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now_running, Ordering::SeqCst);
        let _guard = RunningGuard(&self.running);

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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

    #[tokio::test]
    async fn records_calls_in_invocation_order() {
        let executor = FakeExecutor::new();
        for command in ["first", "second", "third"] {
            let (context, _rx) = ctx();
            executor.execute(spec(command), context).await.unwrap();
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
        let (context, mut rx) = ctx();
        let result = executor
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
        let (context, _rx) = ctx();
        let result = executor.execute(spec("test"), context).await.unwrap();
        assert_eq!(result.exit_code, 0);
    }

    /// The scenario the brief calls out by name: two beams that overlap
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
            let (context, _rx) = ctx();
            handles.push(tokio::spawn(async move {
                executor
                    .execute(spec(&format!("slow {i}")), context)
                    .await
                    .unwrap();
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
        let result = executor.execute(spec("slow"), context).await.unwrap();

        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(result.exit_code, -1);
    }
}
