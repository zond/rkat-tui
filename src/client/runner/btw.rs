//! `/btw`: the side question goes to the daemon (`rkat/btw`), beside the
//! agent's run and never to it; Esc cancels it (`rkat/btw_cancel`).

use super::*;

/// The longest the client waits for a side question's answer (the
/// daemon's own bound, [`crate::daemon::btw::TIMEOUT`], plus the way).
const BTW_WAIT: Duration = Duration::from_secs(75);

/// The runner's `/btw` questions: how many it asked (each is numbered),
/// and the pending one's number and call (one at a time).
#[derive(Debug, Default)]
pub(super) struct Btws {
    asked: u64,
    pending: Option<(u64, tokio::task::AbortHandle)>,
}

impl Btws {
    /// The next question's number.
    fn next(&mut self) -> u64 {
        self.asked += 1;
        self.asked
    }
}

impl Runner {
    /// Ask the daemon `question` about the agent's session (with the tool
    /// it runs now); the answer comes as [`Msg::BtwAnswered`]. One at a
    /// time.
    pub(super) fn ask_btw(
        &mut self,
        question: String,
        running_tool: Option<(String, u64)>,
        current_turn: Option<String>,
    ) {
        if self.btw.pending.is_some() {
            // The UI asks one at a time; this guards another sender.
            self.notice(
                NoticeLevel::Warn,
                "another /btw question is still waiting for its answer; it was not asked",
            );
            return;
        }
        let Some(connection) = self.connection() else {
            self.send(Msg::BtwAnswered(Err(format!(
                "{}; ask again once it is",
                self.unattached()
            ))));
            return;
        };
        let id = self.btw.next();
        let reports = self.reporter();
        let task = tokio::spawn(async move {
            let params = protocol::BtwParams {
                id,
                question,
                running_tool,
                current_turn,
            };
            let result = bounded(
                BTW_WAIT,
                remote::call::<protocol::method::Btw>(&connection, &params),
            )
            .await
            .map(|answer| answer.answer)
            .map_err(|err| match err {
                CallFailure::Failed(ClientError::Rpc { message, .. }) => message,
                other => other.to_string(),
            });
            reports.send(Report::Btw { id, result });
        });
        self.btw.pending = Some((id, task.abort_handle()));
    }

    /// Esc on the pending `/btw`: the call is dropped here and cancelled in
    /// the daemon; the UI already shows it cancelled.
    pub(super) fn cancel_btw(&mut self) {
        let Some((id, task)) = self.btw.pending.take() else {
            return;
        };
        task.abort();
        let Some(connection) = self.connection() else {
            return;
        };
        tokio::spawn(async move {
            let params = protocol::BtwCancelParams { id };
            if let Err(err) = bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::BtwCancel>(&connection, &params),
            )
            .await
            {
                tracing::info!(%err, "rkat/btw_cancel failed");
            }
        });
    }

    /// The daemon's answer to the `/btw` numbered `id`, if it is still the
    /// pending one.
    pub(super) fn btw_answered(&mut self, id: u64, result: Result<String, String>) {
        if self
            .btw
            .pending
            .as_ref()
            .is_some_and(|(pending, _)| *pending == id)
        {
            self.btw.pending = None;
            self.send(Msg::BtwAnswered(result));
        }
    }
}
