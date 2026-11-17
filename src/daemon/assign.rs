//! `assign` in the daemon: seat a single-use helper beside its caller and
//! answer at once.
//!
//! - **Where.** In the caller's own mob (an agent's is the workspace's
//!   mob `tui`; a helper's is the one it is seated in), under the caller's
//!   own profile, through the mob host's spawn with the caller's session
//!   as the owner context: the engine finds the caller as the spawner
//!   (`spawner_for_bridge_session`) and the customizer shapes the helper
//!   as every helper (gate, tools, persona, scratch directory, model rule,
//!   bookkeeping) and wires it to its spawner both ways
//!   (`auto_wire_parent`), exactly as a `spawn_member` the caller made
//!   itself. Not an implicit mob, as the engine's withdrawn `delegate`
//!   used: the daemon cannot address a member there (`docs/evidence.md`).
//! - **When it ends.** Its parent is told when it finishes (its run ended
//!   and the runtime holds no unfinished input of it), with its answer,
//!   and it is retired then ([`crate::mobs::endings`]). A message from its
//!   parent before that makes it go on.

use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use meerkat_core::SessionId;
use meerkat_mob::{AgentIdentity, SpawnMemberSpec};
use meerkat_mob_mcp::MobMcpState;

use crate::mobs::{ASSIGN_LABEL, HelperKey, HelperRegistry};
use crate::tools::{AssignArgs, AssignHost};

/// The longest name a helper gets.
pub const MAX_NAME: usize = 32;

/// What a well-formed name is, for the refusal of one that is not.
const NAME_RULE: &str =
    "a name is 1 to 32 lowercase letters, digits and '-', starting with a letter or digit";

/// Why `name` is no helper's name (`None`: it is one).
#[must_use]
pub fn name_refusal(name: &str) -> Option<String> {
    let well_formed = !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-');
    if !well_formed {
        return Some(format!(
            "`{name}` is not a helper name: {NAME_RULE}; call `assign` again with one"
        ));
    }
    if super::agents::is_agent(name) {
        return Some(format!(
            "`{name}` is reserved for the workspace's agents (`main-` and four characters); call `assign` again with another name"
        ));
    }
    None
}

/// The answer to an `assign` that seated `name`.
#[must_use]
pub fn started_text(name: &str, peer: &str) -> String {
    format!(
        "Helper `{name}` is running. You will be told in a note when it finishes, with its answer, and it is then retired; do not wait or poll for it. Meanwhile you can message it with `send_message` (it is `{peer}` in `peers`), and it can message you."
    )
}

/// The daemon's `assign` ([`AssignHost`]).
pub struct Assigner {
    registry: Arc<HelperRegistry>,
    state: OnceLock<Weak<MobMcpState>>,
    /// `connect_helpers` ([`super::connect`]).
    connector: super::connect::Connector,
}

impl std::fmt::Debug for Assigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Assigner").finish_non_exhaustive()
    }
}

impl Assigner {
    /// Seat helpers for the sessions `registry` knows.
    #[must_use]
    pub fn new(registry: Arc<HelperRegistry>) -> Self {
        Self {
            connector: super::connect::Connector::new(Arc::clone(&registry)),
            registry,
            state: OnceLock::new(),
        }
    }

    /// Seat and connect them through `state` (once; it is made after the
    /// tools).
    pub fn bind(&self, state: &Arc<MobMcpState>, links: &Arc<super::links::Links>) {
        let _ = self.state.set(Arc::downgrade(state));
        self.connector.bind(state, links);
    }

    /// The member running as `caller`: an agent, or a helper.
    fn caller_key(&self, caller: &SessionId) -> Option<HelperKey> {
        self.registry
            .root_of_session(caller)
            .or_else(|| self.registry.member_key_of_session(caller))
    }

    async fn seat(&self, caller: Option<&SessionId>, args: AssignArgs) -> Result<String, String> {
        let AssignArgs { name, task, model } = args;
        if let Some(refused) = name_refusal(&name) {
            return Err(refused);
        }
        if task.trim().is_empty() {
            return Err(
                "`task` is empty: give the helper its whole task and the context it needs"
                    .to_string(),
            );
        }
        let not_here = "`assign` is not available to this caller (it is no agent or helper of this workspace's daemon)";
        let caller = caller.ok_or(not_here)?;
        let parent = self.caller_key(caller).ok_or(not_here)?;
        let state = self
            .state
            .get()
            .and_then(Weak::upgrade)
            .ok_or("the daemon is stopping: no helper can be started")?;
        let handle = state.handle_for(&parent.mob).await.map_err(|err| {
            format!("your mob cannot be reached, so no helper was started: {err}")
        })?;
        let members = handle.list_members().await;
        if members
            .iter()
            .any(|member| member.agent_identity.as_str() == name)
        {
            return Err(format!(
                "the name `{name}` is taken by another member (`list_members` shows them); call `assign` again with another name"
            ));
        }
        let role = members
            .iter()
            .find(|member| member.agent_identity == parent.member)
            .map(|member| member.role.clone())
            .ok_or(not_here)?;
        let peer = meerkat_core::MemberCommsName::new(parent.mob.as_str(), role.as_str(), &name)
            .map_or_else(|_| name.clone(), |peer| peer.to_string());
        let mut spec = SpawnMemberSpec::new(role, AgentIdentity::from(name.as_str()))
            .with_initial_message(task)
            .with_labels(std::iter::once((ASSIGN_LABEL.to_string(), "1".to_string())).collect())
            .with_auto_wire_parent(true);
        spec.model_override = model;
        let witness = meerkat_mob::CreationSourceCapture::classify(
            handle.capture_member_creation_source(caller).await,
        )
        .into_admitted_witness(crate::tools::ASSIGN);
        spec = spec.with_creation_source(witness);
        handle
            .spawn_spec_with_generated_owner_context(spec, caller.clone())
            .await
            .map_err(|err| format!("the helper `{name}` was not started: {err}"))?;
        Ok(started_text(&name, &peer))
    }
}

#[async_trait]
impl AssignHost for Assigner {
    async fn assign(&self, caller: Option<&SessionId>, args: AssignArgs) -> (String, bool) {
        match self.seat(caller, args).await {
            Ok(text) => (text, false),
            Err(text) => (text, true),
        }
    }

    async fn connect(
        &self,
        caller: Option<&SessionId>,
        args: crate::tools::ConnectArgs,
    ) -> (String, bool) {
        match self.connector.connect(caller, &args.helpers).await {
            Ok(text) => (text, false),
            Err(text) => (text, true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_short_lowercase_and_no_agent_s() {
        for good in ["scout", "a", "test-runner-2", "9lives"] {
            assert_eq!(name_refusal(good), None, "{good}");
        }
        for bad in ["", "Scout", "has space", "-lead", "x_y", &"a".repeat(33)] {
            let refused = name_refusal(bad).unwrap_or_default();
            assert!(
                refused.contains("1 to 32 lowercase letters")
                    && refused.contains("call `assign` again"),
                "{bad}: {refused}"
            );
        }
        assert!(
            name_refusal("main-ab12")
                .is_some_and(|refused| refused.contains("reserved for the workspace's agents"))
        );
    }
}
