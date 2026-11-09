//! Shutdown: interrupt the running turn, take the session off the runtime,
//! and save every message the model never received
//! ([`crate::workbench::unsent`]).
//!
//! Steered messages are durable runtime inputs: left alone, a queued one
//! would run the next time the session is resumed. Quitting cancels them
//! with the turn instead, and saves their text.

use std::time::Duration;

use super::Runner;
use crate::workbench::SavedUnsent;

/// How long quitting waits for an interrupted turn to wind down.
pub const QUIT_TURN_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on the whole runner shutdown (turn, teardown, file).
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(20);

impl Runner {
    /// Quit: interrupt a running turn and wait (up to [`QUIT_TURN_TIMEOUT`])
    /// for its inputs to end, abort the runner's remaining tasks, leave the
    /// session (off the runtime, its approval state dropped) and shut the
    /// service down, release the realm lease, then append every message the
    /// model never got to
    /// [`crate::workbench::unsent::UNSENT_FILE`].
    pub(super) async fn shutdown(mut self) -> Option<SavedUnsent> {
        if self.turn.is_active() {
            self.interrupt();
            let deadline = tokio::time::sleep(QUIT_TURN_TIMEOUT);
            tokio::pin!(deadline);
            while self.turn.is_active() && !self.turn.settled() {
                tokio::select! {
                    Some(done) = self.completion_rx.recv() => self.on_completed(done).await,
                    Some(stopped) = self.stopped_rx.recv() => self.on_stopped(stopped).await,
                    () = &mut deadline => {
                        tracing::warn!("the interrupted turn did not end in time on quit");
                        break;
                    }
                }
            }
            if self.turn.is_active() && self.turn.all_done() {
                // The interrupt's report did not come in time: end anyway.
                self.finish_turn().await;
            }
        }
        // Abort the runner's tasks and wait until each has actually stopped:
        // an aborted task is only dropped at its next await point.
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
        self.bench.cancel_shell();
        if let Err(err) = self.session.close().await {
            tracing::warn!(error = %err, "could not take the session off the runtime on quit");
        }
        if let Err(err) = self.runtime.service.try_shutdown().await {
            tracing::warn!(error = %err, "session service shutdown failed");
        }
        self.runtime.release_realm_lease().await;
        self.bench.save_unsent(&self.delivered)
    }
}
