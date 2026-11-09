//! Context upkeep: the compaction trigger and `/context` (`/remember` and
//! `/memory` are the workbench's, [`crate::workbench::memory`]).
//!
//! Compaction is native (see [`crate::ui::compaction`]): every session build
//! carries the policy's trigger for the model's context window
//! ([`Runner::session_trigger`]). `/compact now` asks the engine to compact
//! at the next compaction check (`MeerkatMachine::request_compaction`), with
//! no rebuild. Upstream reads the trigger when the agent is built and
//! exposes no call to change it on a live session, so a trigger change goes
//! through one workaround, [`Runner::rebuild_session_workaround`]: retire the
//! idle session and resume it with the new trigger. It runs only on what the
//! user does: a model switch or a resume that moves the trigger
//! ([`Runner::ensure_trigger`]) and a model switch that needed a new
//! profile. A plain model switch does not rebuild: the engine's `delegate`
//! reads the session's current model on every call (#2102). A background
//! list fetch never rebuilds. The upstream ask (a live trigger
//! update) is recorded in `docs/evidence.md`.

use super::Runner;

use crate::error::TuiError;
use crate::session::SessionHandle;
use crate::ui::{CompactFailure, CompactionTrigger, Msg, NoticeLevel};
use crate::workbench::context::{context_breakdown, instruction_files};

/// Why the session is rebuilt (see [`Runner::rebuild_session_workaround`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RebuildReason {
    /// The compaction trigger moved (model switch, resume onto another
    /// model). Upstream ask: a live trigger update.
    CompactionTrigger,
    /// The agent's model registry predates a profile the switch needs.
    /// Upstream ask: profiles read at reconfigure time.
    ModelProfile,
}

/// How [`Runner::rebuild_session_workaround`] failed.
#[derive(Debug, thiserror::Error)]
pub(super) enum RebuildError {
    /// The session could not be retired; it runs on as it was.
    #[error("{0}")]
    Retire(#[source] TuiError),
    /// The rebuild failed and the session was resumed as it was before.
    #[error("{0} (the session was resumed as it was)")]
    KeptPrevious(#[source] TuiError),
    /// The session was retired and could not be resumed: it is detached
    /// until the user resumes it.
    #[error(
        "the session could not be resumed after a rebuild ({first}; retry: {retry}); \
         it is detached: run /resume to reattach it"
    )]
    Detached {
        /// The failed resume with the new settings.
        first: TuiError,
        /// The failed retry with the previous ones.
        #[source]
        retry: TuiError,
    },
}

impl RebuildError {
    /// Whether the runner holds no live session any more.
    pub(super) fn detached(&self) -> bool {
        matches!(self, Self::Detached { .. })
    }
}

impl Runner {
    /// The native auto-compaction trigger the policy gives the current
    /// model (its profile's window; an estimated window counts as unknown).
    pub(super) fn session_trigger(&self) -> CompactionTrigger {
        let (window, estimated) = self.context_window();
        self.template.policy.trigger((!estimated).then_some(window))
    }

    /// The one place the TUI retires and resumes its own idle session (see
    /// the module docs): the agent is rebuilt from the current config with
    /// `trigger`, the event stream followed again, and the identity re-read.
    ///
    /// The runner never keeps a retired session behind a resume that
    /// failed: the resume is retried once with the session's previous
    /// trigger ([`RebuildError::KeptPrevious`]); only when that fails too is
    /// the session left detached ([`RebuildError::Detached`]).
    pub(super) async fn rebuild_session_workaround(
        &mut self,
        reason: RebuildReason,
        trigger: CompactionTrigger,
    ) -> Result<(), RebuildError> {
        tracing::debug!(
            ?reason,
            ?trigger,
            "rebuilding the session (upstream workaround)"
        );
        let id = self.session.id().clone();
        let previous = self.session.compaction();
        self.session.retire().await.map_err(RebuildError::Retire)?;
        let result =
            match SessionHandle::resume(&self.runtime, &id, self.template.options(None, trigger))
                .await
            {
                Ok(session) => {
                    self.session = session;
                    Ok(())
                }
                Err(first) => {
                    tracing::warn!(error = %first, "session rebuild failed; resuming as it was");
                    match SessionHandle::resume(
                        &self.runtime,
                        &id,
                        self.template.options(None, previous),
                    )
                    .await
                    {
                        Ok(session) => {
                            self.session = session;
                            Err(RebuildError::KeptPrevious(first))
                        }
                        Err(retry) => return Err(RebuildError::Detached { first, retry }),
                    }
                }
            };
        self.follow_events().await;
        self.refresh_identity().await;
        result
    }

    /// Rebuild the idle session when its agent runs on another trigger than
    /// [`Runner::session_trigger`] (after a model switch or a resume onto
    /// another model). A running turn keeps the session as it is.
    pub(super) async fn ensure_trigger(&mut self) {
        let wanted = self.session_trigger();
        if self.turn.is_active() || self.session.compaction() == wanted {
            return;
        }
        if let Err(err) = self
            .rebuild_session_workaround(RebuildReason::CompactionTrigger, wanted)
            .await
        {
            let level = if err.detached() {
                NoticeLevel::Error
            } else {
                NoticeLevel::Warn
            };
            self.notice(
                level,
                format!("could not apply the compaction trigger to this session: {err}"),
            );
        }
    }

    /// [`crate::ui::Effect::CompactNow`]: ask the engine to compact at the
    /// session's next compaction check (`MeerkatMachine::request_compaction`;
    /// no rebuild); answered with [`Msg::CompactRequested`].
    pub(super) async fn compact_now(&mut self) {
        let result = match self.session.request_compaction().await {
            Ok(receipt) => {
                tracing::debug!(?receipt, "compaction requested");
                Ok(())
            }
            Err(err) => Err(CompactFailure::Request(err.to_string())),
        };
        self.send(Msg::CompactRequested(result));
    }

    /// [`crate::ui::Effect::ContextReport`]: the UI's `header` plus what the
    /// context holds, as one notice ([`crate::workbench::context`]).
    pub(super) async fn context_report(&mut self, header: &str) {
        let session = self.session.read().await.map_err(|err| err.to_string());
        let files = instruction_files(&self.runtime.scope, &self.template.instructions);
        let tools = format!(
            "{} registered by rkat-tui (file, notes, fork)",
            self.template.external_tools.tools().len()
        );
        let text = context_breakdown(
            header,
            session.as_ref().map(|s| s.messages()).map_err(Clone::clone),
            &files,
            &tools,
        );
        self.notice(NoticeLevel::Info, text);
    }
}
