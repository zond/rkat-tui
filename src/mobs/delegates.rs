//! `delegate` calls that end without their helper's result.
//!
//! The engine retires a `delegate` helper once the helper's turn ends,
//! done or failed (meerkat-mob `DelegationExecutionService::execute`
//! awaits the turn, then retires). A call whose future is dropped before
//! that, by the tool timeout (`[tools.tool_timeouts] delegate`, else
//! `[tools] default_timeout`) or by a run that ends without the call's
//! result, never gets there: the helper stays seated in the caller's
//! implicit mob, running or idle, until something retires it
//! (`docs/evidence.md`, "Orphaned delegate helpers").
//!
//! [`DelegateCalls`] follows one session's tool events and names the
//! helper of every such call: a `delegate` call carries its helper's
//! member id (`member_id`, required by the engine) and spawns it into the
//! mob `implicit-<calling session>`, so the call and the helper match
//! exactly. [`retire_orphan`] then retires it, but only when the mob
//! still lists that member under the `delegate` profile: a member a
//! `delegate` call never spawned (a `spawn_member` member whose id the
//! call reused, which fails the call) is left alone.
//!
//! Goes when the engine retires the helper itself (lukacf/meerkat#2206).

use std::collections::HashMap;

use meerkat_core::{AgentEvent, SessionId};
use meerkat_mob::{AgentIdentity, MobId};
use meerkat_mob_mcp::MobMcpState;

use super::HelperKey;

/// The engine's delegation tool.
pub const DELEGATE: &str = "delegate";

/// The profile the engine spawns a `delegate` helper from.
const DELEGATE_PROFILE: &str = "delegate";

/// The mob a `delegate` call of `session` spawns its helper into.
#[must_use]
pub fn implicit_mob(session: &SessionId) -> MobId {
    MobId::from(format!("implicit-{session}"))
}

/// How a `delegate` call ended, as its caller's events show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallEnded {
    /// It completed with the helper's result: the caller has the answer.
    Returned,
    /// The engine's tool timeout dropped it (`ToolExecutionTimedOut`, then
    /// its completion as an error): its helper is left seated.
    TimedOut,
    /// It completed as an error otherwise (the helper's turn failed, its
    /// spawn was refused): the engine retired a helper it seated.
    Failed,
    /// The caller's run ended while it was open (an interrupt, or the run
    /// failing): the call's future was dropped with the run, its helper
    /// keeps running, and nothing will return its answer.
    CutOff,
}

/// The `delegate` calls one session has open (by tool call id), each with
/// the helper it names.
#[derive(Debug, Default)]
pub struct DelegateCalls {
    open: HashMap<String, HelperKey>,
    /// The open calls the engine reported timed out (their completion
    /// follows).
    timed_out: std::collections::HashSet<String>,
}

impl DelegateCalls {
    /// Follow `event` of `session` (`None`: unknown, and its calls are not
    /// followed); the helpers whose calls just ended, each with how.
    ///
    /// A timed-out call is told apart from a cut-off one by the engine's
    /// own events: the timeout emits `ToolExecutionTimedOut` and completes
    /// the call with its error (meerkat-core `agent/state.rs`), while a
    /// cancelled run drops its in-flight calls with the run future, so
    /// they never complete (meerkat-session `ephemeral.rs`, the interrupt
    /// race). A helper another open call still names is not returned: that
    /// call may be the one that spawned it.
    pub fn observe(
        &mut self,
        session: Option<&SessionId>,
        event: &AgentEvent,
    ) -> Vec<(HelperKey, CallEnded)> {
        let ended: Vec<(HelperKey, CallEnded)> = match event {
            AgentEvent::ToolCallRequested { id, name, args } if name == DELEGATE => {
                if let (Some(session), Some(member)) = (
                    session,
                    args.as_value()
                        .get("member_id")
                        .and_then(serde_json::Value::as_str),
                ) {
                    self.open.insert(
                        id.clone(),
                        HelperKey::new(implicit_mob(session), AgentIdentity::from(member)),
                    );
                }
                Vec::new()
            }
            AgentEvent::ToolExecutionTimedOut { id, name, .. } if name == DELEGATE => {
                if self.open.contains_key(id) {
                    self.timed_out.insert(id.clone());
                }
                Vec::new()
            }
            AgentEvent::ToolExecutionCompleted {
                id, name, is_error, ..
            } if name == DELEGATE => {
                let timed_out = self.timed_out.remove(id);
                match self.open.remove(id) {
                    Some(key) if timed_out => vec![(key, CallEnded::TimedOut)],
                    Some(key) if *is_error => vec![(key, CallEnded::Failed)],
                    Some(key) => vec![(key, CallEnded::Returned)],
                    None => Vec::new(),
                }
            }
            AgentEvent::RunCompleted { .. } | AgentEvent::RunFailed { .. } => {
                self.timed_out.clear();
                self.open
                    .drain()
                    .map(|(_, key)| (key, CallEnded::CutOff))
                    .collect()
            }
            _ => Vec::new(),
        };
        let mut out: Vec<(HelperKey, CallEnded)> = Vec::new();
        for (key, how) in ended {
            if !out.iter().any(|(seen, _)| seen == &key)
                && !self.open.values().any(|open| open == &key)
            {
                out.push((key, how));
            }
        }
        out
    }
}

/// Retire `key`, a helper whose `delegate` call ended without its result,
/// when its mob still lists it as a `delegate` helper: force-cancel its
/// run, then retire it. Anything else (gone already, the engine retired it,
/// another profile's member) is left as it is.
pub async fn retire_orphan(state: &MobMcpState, key: &HelperKey) {
    let Ok(handle) = state.handle_for(&key.mob).await else {
        return;
    };
    let seated = handle.list_members().await.into_iter().any(|member| {
        member.agent_identity == key.member && member.role.as_str() == DELEGATE_PROFILE
    });
    if !seated {
        return;
    }
    tracing::info!(helper = %key, "retiring a helper whose delegate call ended without its result");
    super::stop_helper(&handle, key).await;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use meerkat_core::ToolCallArguments;

    fn requested(id: &str, name: &str, member: &str) -> AgentEvent {
        AgentEvent::ToolCallRequested {
            id: id.into(),
            name: name.into(),
            args: ToolCallArguments::from_value(serde_json::json!({ "member_id": member }))
                .unwrap(),
        }
    }

    fn completed(id: &str, is_error: bool) -> AgentEvent {
        AgentEvent::ToolExecutionCompleted {
            id: id.into(),
            name: DELEGATE.into(),
            is_error,
            duration_ms: 1,
        }
    }

    fn key(session: &SessionId, member: &str) -> HelperKey {
        HelperKey::new(implicit_mob(session), AgentIdentity::from(member))
    }

    #[test]
    fn each_call_says_how_it_ended() {
        let session = SessionId::new();
        let mut calls = DelegateCalls::default();
        for (id, member) in [("d1", "a"), ("d2", "b"), ("d3", "c")] {
            assert!(
                calls
                    .observe(Some(&session), &requested(id, DELEGATE, member))
                    .is_empty()
            );
        }
        assert_eq!(
            calls.observe(Some(&session), &completed("d1", false)),
            vec![(key(&session, "a"), CallEnded::Returned)]
        );
        assert_eq!(
            calls.observe(Some(&session), &completed("d2", true)),
            vec![(key(&session, "b"), CallEnded::Failed)]
        );
        // The engine's timeout comes before the call's error completion.
        let timed_out = AgentEvent::ToolExecutionTimedOut {
            id: "d3".into(),
            name: DELEGATE.into(),
            timeout_ms: 3000,
        };
        assert!(calls.observe(Some(&session), &timed_out).is_empty());
        assert_eq!(
            calls.observe(Some(&session), &completed("d3", true)),
            vec![(key(&session, "c"), CallEnded::TimedOut)]
        );
        // Another tool's calls are not followed.
        calls.observe(Some(&session), &requested("s1", "shell", "c"));
        assert!(calls.open.is_empty() && calls.timed_out.is_empty());
    }

    #[test]
    fn a_run_that_ends_with_a_call_open_cuts_it_off_once() {
        let session = SessionId::new();
        let mut calls = DelegateCalls::default();
        calls.observe(Some(&session), &requested("d1", DELEGATE, "a"));
        calls.observe(Some(&session), &requested("d2", DELEGATE, "a"));
        // `d1` failed (`a` taken by `d2`): `d2` may be the one that spawned it.
        assert!(
            calls
                .observe(Some(&session), &completed("d1", true))
                .is_empty()
        );
        let ended = AgentEvent::RunCompleted {
            session_id: session.clone(),
            identity: meerkat_core::types::TranscriptMessageIdentity::default(),
            result: String::new(),
            structured_output: None,
            extraction_required: false,
            usage: Default::default(),
            terminal_cause_kind: None,
            assistant_message_id: None,
        };
        assert_eq!(
            calls.observe(Some(&session), &ended),
            vec![(key(&session, "a"), CallEnded::CutOff)]
        );
        assert!(calls.open.is_empty());
    }

    #[test]
    fn a_call_of_an_unknown_session_is_not_followed() {
        let mut calls = DelegateCalls::default();
        calls.observe(None, &requested("d1", DELEGATE, "a"));
        assert!(calls.observe(None, &completed("d1", true)).is_empty());
    }
}
