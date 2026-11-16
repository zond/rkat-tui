//! The approval gate as Meerkat's `ToolDispatchAdmission`.
//!
//! The factory wraps every agent's tool dispatcher in an
//! `ExecutionPolicyGatedDispatcher` when a build carries
//! `SessionBuildOptions::tool_dispatch_admission`; that dispatcher awaits
//! [`ApprovalGate::await_dispatch_admission`] immediately before each call
//! reaches a tool. The gate asks the [`ApprovalEngine`]:
//!
//! - allow ⇒ `Ok(())`: the call runs (the ordinary access policy still
//!   applies after it);
//! - deny ⇒ `ToolError::PolicyDenied` with code [`DENIAL_CODE`] and
//!   [`deny_message`] as its model-facing text. It is an ordinary tool
//!   result (`policy_denied`), so the model reads the reason and the run
//!   continues;
//! - escalate ⇒ the engine waits for the human (or the escalation timeout,
//!   which denies) inside this call, while the UI shows the dialog.
//!
//! Before deciding, the gate asks the engine where the session's tool of
//! the call's name comes from (its live tool definitions, see
//! [`super::catalog`]): a name is a builtin, file or note tool only when
//! its provenance says so.
//!
//! The daemon gives its agent a gate as the root member's admission
//! (`crate::daemon::approvals::RootGate`), composed after any admission
//! the build already carries ([`ApprovalGate::after`]), so every build of
//! it (fresh or restored) runs behind one. A helper it delegates to (a mob
//! member) gets a gate
//! from [`ApprovalGate::for_helper`], installed on its spawn spec by the
//! rkat-tui's spawn customizer ([`crate::mobs`]): its calls are decided as the
//! delegating session's, naming the helper.

use std::sync::Arc;

use async_trait::async_trait;
use meerkat_core::types::ToolCallView;
use meerkat_core::{
    LiveBridgeEffectKind, LiveBridgeEffectOutcome, SessionId, ToolConsequenceDenial,
    ToolDispatchAdmission, ToolDispatchContext, ToolError,
};

use super::engine::{ApprovalEngine, Caller, ToolCall};
use super::{DENIAL_CODE, Verdict, deny_message};

/// One agent's view of the approval engine.
pub struct ApprovalGate {
    engine: Arc<ApprovalEngine>,
    /// The session this gate was built for. For the session's own agent a
    /// dispatch context that names its origin session wins; a helper's calls
    /// always count for it.
    session_id: SessionId,
    /// The helper this gate was built for (`None`: the session's own agent).
    helper: Option<String>,
    /// An admission the build already carried; it decides first.
    preceding: Option<Arc<dyn ToolDispatchAdmission>>,
    /// What stops the helper this gate was built for, when the user
    /// answers one of its dialogs with Stop.
    on_stop: Option<OnStop>,
    /// A latch every call waits for first ([`Self::waiting_for`]).
    wired: Option<tokio::sync::watch::Receiver<bool>>,
}

/// The longest a fork child's call waits for its edge to its parent
/// ([`ApprovalGate::waiting_for`]): the follower's own bounds (the mob's
/// reach, the member's join) and the wire itself.
pub const WIRE_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// Stops one helper ([`ApprovalGate::stopping_with`]).
pub type OnStop = Arc<dyn Fn() + Send + Sync>;

impl std::fmt::Debug for ApprovalGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalGate")
            .field("session_id", &self.session_id)
            .field("helper", &self.helper)
            .field("preceding", &self.preceding.is_some())
            .field("on_stop", &self.on_stop.is_some())
            .finish_non_exhaustive()
    }
}

impl ApprovalGate {
    /// A gate for `session_id`'s calls.
    #[must_use]
    pub fn new(engine: Arc<ApprovalEngine>, session_id: SessionId) -> Self {
        Self {
            engine,
            session_id,
            helper: None,
            preceding: None,
            on_stop: None,
            wired: None,
        }
    }

    /// A gate for the calls of `helper`, which works for `session_id`: they
    /// are judged with that session's transcript and allow cache, and
    /// count against the shared denial counters (see
    /// [`ApprovalEngine::decide_as`]). The session's Stop does not deny
    /// them: an interrupt stops its own agent only.
    #[must_use]
    pub fn for_helper(
        engine: Arc<ApprovalEngine>,
        session_id: SessionId,
        helper: impl Into<String>,
    ) -> Self {
        Self {
            engine,
            session_id,
            helper: Some(helper.into()),
            preceding: None,
            on_stop: None,
            wired: None,
        }
    }

    /// Hold every call until `wired` reads true or its sender is gone, at
    /// most [`WIRE_WAIT`] (`None`: no wait). A fork child's: the engine
    /// seats it with no edge to its parent, and the follower wires it a
    /// moment after; its first call (often `peers` or `send_message`) then
    /// finds the parent.
    #[must_use]
    pub fn waiting_for(mut self, wired: Option<tokio::sync::watch::Receiver<bool>>) -> Self {
        self.wired = wired;
        self
    }

    /// Call `on_stop` when the user answers a dialog of this helper's calls
    /// with Stop: it stops the helper (the call itself is denied as
    /// stopped).
    #[must_use]
    pub fn stopping_with(mut self, on_stop: OnStop) -> Self {
        self.on_stop = Some(on_stop);
        self
    }

    /// The session this gate decides for.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The helper this gate decides for, if any.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn helper(&self) -> Option<&str> {
        self.helper.as_deref()
    }

    /// Run `preceding` before this gate.
    #[must_use]
    pub fn after(mut self, preceding: Option<Arc<dyn ToolDispatchAdmission>>) -> Self {
        self.preceding = preceding;
        self
    }
}

/// Wait until `wired` reads true or its sender is gone (the follower gave
/// up), at most [`WIRE_WAIT`]; `None` returns at once.
async fn wait_wired(wired: Option<&tokio::sync::watch::Receiver<bool>>) {
    if let Some(wired) = wired {
        let mut wired = wired.clone();
        let _ = tokio::time::timeout(WIRE_WAIT, wired.wait_for(|wired| *wired)).await;
    }
}

/// The model-facing error for a denial.
#[must_use]
pub fn denial_error(reason: &super::DenyReason) -> ToolError {
    ToolError::PolicyDenied {
        denial: ToolConsequenceDenial::new(DENIAL_CODE, deny_message(reason)),
    }
}

#[async_trait]
impl ToolDispatchAdmission for ApprovalGate {
    async fn await_dispatch_admission(
        &self,
        call: ToolCallView<'_>,
        context: Option<&ToolDispatchContext>,
        effect_kind: LiveBridgeEffectKind,
    ) -> Result<(), ToolError> {
        wait_wired(self.wired.as_ref()).await;
        if let Some(preceding) = &self.preceding {
            preceding
                .await_dispatch_admission(call, context, effect_kind)
                .await?;
        }
        let origin_session = context.and_then(ToolDispatchContext::origin_session_id);
        // A helper's run is its own member session's; its calls are judged
        // by the run of the session it works for.
        let caller = match &self.helper {
            Some(helper) => Caller::helper(&self.session_id, helper),
            None => Caller::session(origin_session.unwrap_or(&self.session_id))
                .in_run(context.and_then(ToolDispatchContext::run_id)),
        };
        // Where the calling agent's tool of this name comes from: the
        // session that is dispatching (a helper's own member session; for
        // a helper without a dispatch context nothing is known, so its
        // calls are judged as foreign tools).
        let tools_of = match &self.helper {
            Some(_) => origin_session,
            None => Some(origin_session.unwrap_or(&self.session_id)),
        };
        let origin = match tools_of {
            Some(session) => self.engine.tool_origin(session, call.name).await,
            None => None,
        };
        let tool_call = ToolCall {
            id: call.id,
            name: call.name,
            args: call.args,
            origin: origin.as_ref(),
        };
        match self.engine.decide_as(caller, tool_call).await {
            Verdict::Allow { .. } => Ok(()),
            Verdict::Deny { reason, .. } => {
                // A helper's call is denied as stopped only by a Stop
                // answer to its own dialog: that stops the helper.
                if reason == super::DenyReason::Stopped
                    && self.helper.is_some()
                    && let Some(on_stop) = &self.on_stop
                {
                    on_stop();
                }
                Err(denial_error(&reason))
            }
        }
    }

    async fn record_dispatch_outcome(
        &self,
        call: ToolCallView<'_>,
        context: Option<&ToolDispatchContext>,
        effect_kind: LiveBridgeEffectKind,
        outcome: LiveBridgeEffectOutcome,
    ) -> Result<(), ToolError> {
        match &self.preceding {
            Some(preceding) => {
                preceding
                    .record_dispatch_outcome(call, context, effect_kind, outcome)
                    .await
            }
            None => Ok(()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::DenyReason;
    use super::*;

    /// A fork's calls wait for its edge: held until the latch opens, let go
    /// when the follower drops it or the bound passes; no latch, no wait.
    #[tokio::test(start_paused = true)]
    async fn a_call_waits_for_the_fork_s_edge() {
        let (open, latch) = tokio::sync::watch::channel(false);
        let mut waiting = Box::pin(wait_wired(Some(&latch)));
        assert!(futures::poll!(&mut waiting).is_pending());
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        assert!(futures::poll!(&mut waiting).is_pending());
        open.send_replace(true);
        assert!(futures::poll!(&mut waiting).is_ready());
        let (dropped, latch) = tokio::sync::watch::channel(false);
        let mut waiting = Box::pin(wait_wired(Some(&latch)));
        assert!(futures::poll!(&mut waiting).is_pending());
        drop(dropped);
        assert!(futures::poll!(&mut waiting).is_ready());
        let (_kept, latch) = tokio::sync::watch::channel(false);
        let started = tokio::time::Instant::now();
        wait_wired(Some(&latch)).await;
        assert_eq!(started.elapsed(), WIRE_WAIT);
        assert!(futures::poll!(Box::pin(wait_wired(None))).is_ready());
    }

    #[test]
    fn denials_are_policy_denied_with_the_reason() {
        let error = denial_error(&DenyReason::Human { comment: None });
        assert_eq!(error.error_code(), "policy_denied");
        let text = error.to_string();
        assert!(
            text.starts_with("Tool call denied (auto_mode): Blocked by auto-mode safety policy"),
            "{text}"
        );
        assert!(text.contains("the user denied this action"));
        let text = denial_error(&DenyReason::Human {
            comment: Some("use git mv".into()),
        })
        .to_string();
        assert!(text.contains("denied by the user: use git mv."), "{text}");
        assert!(text.contains("Do what the user says instead"), "{text}");
    }
}
