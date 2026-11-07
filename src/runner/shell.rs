//! `!` shell mode, the runner's side: run the command
//! ([`crate::user_shell::UserShell`]) off the loop, add its output to the
//! session as durable system context (the call `/remember` uses, under a key
//! unique to the run, so the model reads it on its next call without a turn
//! being started), and answer with [`Msg::ShellFinished`]. The context is
//! appended before the answer is sent, so a message the UI sends after the
//! command always finds it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::Runner;
use crate::ui::{Msg, ShellOutcome, ShellRunId};
use crate::user_shell::{UserShell, context_text};

/// A per-process counter for the idempotency keys of runs.
static RUNS: AtomicU64 = AtomicU64::new(0);

/// The `!` command running now.
pub(super) struct ShellTask {
    id: ShellRunId,
    task: tokio::task::JoinHandle<()>,
}

impl Runner {
    /// [`crate::ui::Effect::RunShell`].
    pub(super) fn run_shell(&mut self, id: ShellRunId, command: String) {
        self.cancel_shell();
        let shell: std::sync::Arc<UserShell> = std::sync::Arc::clone(&self.user_shell);
        let session = self.session.clone();
        let tx = self.ui_tx.clone();
        let task = tokio::spawn(async move {
            let outcome = match shell.run(&command).await {
                Ok(run) => {
                    let context = session
                        .append_system_context(context_text(&command, &run), run_key())
                        .await
                        .map(|_| ())
                        .map_err(|err| err.to_string());
                    ShellOutcome::Ran { run, context }
                }
                Err(reason) => ShellOutcome::Failed(reason),
            };
            let _ = tx.send(Msg::ShellFinished { id, outcome });
        });
        self.shell_task = Some(ShellTask { id, task });
    }

    /// [`crate::ui::Effect::CancelShell`] (and shutdown): drop the running
    /// command's task, which kills its processes; the UI hears
    /// [`ShellOutcome::Cancelled`] unless it already finished.
    pub(super) fn cancel_shell(&mut self) {
        let Some(ShellTask { id, task }) = self.shell_task.take() else {
            return;
        };
        if task.is_finished() {
            return;
        }
        task.abort();
        self.send(Msg::ShellFinished {
            id,
            outcome: ShellOutcome::Cancelled,
        });
    }
}

/// An idempotency key no other run shares, in this process or another
/// (a resumed session keeps the keys of earlier processes' runs).
fn run_key() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "rkat-tui:shell:{}-{nanos}-{}",
        std::process::id(),
        RUNS.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::run_key;

    #[test]
    fn run_keys_are_unique() {
        let (a, b) = (run_key(), run_key());
        assert_ne!(a, b);
        assert!(a.starts_with("rkat-tui:shell:"), "{a}");
    }
}
