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

/// The `delegate` calls one session has open (by tool call id), each with
/// the helper it names.
#[derive(Debug, Default)]
pub struct DelegateCalls {
    open: HashMap<String, HelperKey>,
}

impl DelegateCalls {
    /// Follow `event` of `session` (`None`: unknown, and its calls are not
    /// followed); the helpers whose calls just ended without their
    /// result, to retire.
    ///
    /// A call ends without its result when it completes as an error (a
    /// timed-out call completes with its timeout error) or when the run
    /// ends while it is still open. A helper another open call still names
    /// is not returned: that call may be the one that spawned it.
    pub fn observe(&mut self, session: Option<&SessionId>, event: &AgentEvent) -> Vec<HelperKey> {
        let ended: Vec<HelperKey> = match event {
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
            AgentEvent::ToolExecutionCompleted {
                id, name, is_error, ..
            } if name == DELEGATE => match self.open.remove(id) {
                Some(key) if *is_error => vec![key],
                _ => Vec::new(),
            },
            AgentEvent::RunCompleted { .. } | AgentEvent::RunFailed { .. } => {
                self.open.drain().map(|(_, key)| key).collect()
            }
            _ => Vec::new(),
        };
        let mut orphans: Vec<HelperKey> = Vec::new();
        for key in ended {
            if !orphans.contains(&key) && !self.open.values().any(|open| open == &key) {
                orphans.push(key);
            }
        }
        orphans
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
    fn a_failed_call_names_its_helper_and_a_completed_one_does_not() {
        let session = SessionId::new();
        let mut calls = DelegateCalls::default();
        assert!(
            calls
                .observe(Some(&session), &requested("d1", DELEGATE, "a"))
                .is_empty()
        );
        assert!(
            calls
                .observe(Some(&session), &requested("d2", DELEGATE, "b"))
                .is_empty()
        );
        assert!(
            calls
                .observe(Some(&session), &completed("d1", false))
                .is_empty()
        );
        assert_eq!(
            calls.observe(Some(&session), &completed("d2", true)),
            vec![key(&session, "b")]
        );
        // Another tool's calls are not followed.
        calls.observe(Some(&session), &requested("s1", "shell", "c"));
        assert!(calls.open.is_empty());
    }

    #[test]
    fn a_run_that_ends_with_a_call_open_names_its_helper_once() {
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
            vec![key(&session, "a")]
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
