//! The agents: durable, turn-driven, host-root members of the
//! workspace's mob, one per agent ([`super::agents`]), all seated with the
//! one profile `main` this module defines.
//!
//! Clients find their agent's session with `mob/member_status`
//! (`current_session_id`).

use std::sync::Arc;

use meerkat_mob::{
    AgentIdentity, MobDefinition, MobId, MobRuntimeMode, ProfileBinding, ProfileName,
};
use meerkat_mob_mcp::MobMcpState;

use super::engine::RootPlan;
use super::{AGENT_PROFILE, DaemonError, MOB_ID};

/// Where the agent the daemon started with lives (the most recent one it
/// restored, else the one it seated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirstAgent {
    /// Its mob.
    pub mob_id: MobId,
    /// Its member identity.
    pub identity: AgentIdentity,
    /// The session it runs as.
    pub session_id: meerkat_core::SessionId,
    /// Whether this start seated it (else it was restored).
    pub created: bool,
}

/// The mob definition: one durable, turn-driven `main` profile on the
/// start-up model, with the realm's tools, mob tools for helpers, and the
/// MCP servers.
fn definition(plan: &RootPlan) -> MobDefinition {
    let mut definition =
        MobDefinition::implicit_with_provider(MOB_ID, &plan.model, Some(plan.provider), None);
    definition.id = MobId::from(MOB_ID);
    definition.profiles.clear();
    if let Some(profile) = main_profile(plan) {
        definition.profiles.insert(
            ProfileName::from(AGENT_PROFILE),
            ProfileBinding::Inline(Box::new(profile)),
        );
    }
    definition
}

/// The `main` profile of this start: what every agent is built on, fresh
/// or rebuilt. The mob keeps the definition it was created with on the
/// realm's first start, and the engine updates a stored definition only
/// while no handle of the mob is live
/// (`meerkat_mob::MobStorage::update_definition`), which `MobMcpState`
/// does not expose (lukacf/meerkat#2184 asks to update a stored mob
/// definition; this override goes when it ships); so the daemon puts this
/// profile on every agent's spawn as its override profile
/// (`crate::mobs::RootMember::current_profile`),
/// and the MCP servers, the memory tools and the compaction threshold of
/// the current start reach the agents of an existing realm. A rebuilt
/// agent keeps its session's model, provider and provider parameters (the
/// engine keeps a resumed session's identity).
#[must_use]
pub fn main_profile(plan: &RootPlan) -> Option<meerkat_mob::Profile> {
    let mut definition =
        MobDefinition::implicit_with_provider(MOB_ID, &plan.model, Some(plan.provider), None);
    let mut main = None;
    for (_, binding) in std::mem::take(&mut definition.profiles) {
        if let ProfileBinding::Inline(mut profile) = binding {
            profile
                .self_hosted_server_id
                .clone_from(&plan.self_hosted_server);
            profile.durable = true;
            profile.runtime_mode = MobRuntimeMode::TurnDriven;
            profile.peer_description = "rkat-tui orchestrator".to_string();
            // Clients reach it with `mob/member_send`, whose run outlives
            // the client.
            profile.external_addressable = true;
            profile.tools.builtins = true;
            profile.tools.shell = true;
            profile.tools.comms = true;
            profile.tools.mob = true;
            // The schedule tools come with the mob tools, per session
            // (`super::deferral::AgentTools`): the engine's category would
            // compose them from the runtime's slot every agent shares.
            profile.tools.schedule = false;
            profile.tools.memory = plan.memory;
            profile.tools.mcp_servers.clone_from(&plan.mcp_servers);
            // The `[thinking]` of the start-up model; a rebuild re-applies
            // the setting of the session's model (the customizer).
            profile.provider_params = crate::thinking::params_for(plan.thinking.as_ref());
            // The `[auto]` compaction policy of this start.
            profile.auto_compact_threshold = plan.compact_threshold;
            main = Some(*profile);
        }
    }
    main
}

/// Create the mob, or restore it (and resume it when it was stopped). Its
/// agents are restored or seated by [`super::agents::Agents::start`].
///
/// # Errors
///
/// [`DaemonError::Mob`] when the mob cannot be created or resumed.
pub async fn ensure_mob(state: &Arc<MobMcpState>, plan: &RootPlan) -> Result<(), DaemonError> {
    let mob_id = MobId::from(MOB_ID);
    let mob_err = |err| DaemonError::mob(&MobId::from(MOB_ID), err);
    // Listing restores the persisted mobs first.
    let listed = state.mob_list().await.map_err(mob_err)?;
    if listed.iter().any(|(id, _)| id == &mob_id) {
        if state.mob_status(&mob_id).await.map_err(mob_err)? == meerkat_mob::MobState::Stopped {
            state.mob_resume(&mob_id).await.map_err(mob_err)?;
        }
    } else {
        state
            .mob_create_definition(definition(plan))
            .await
            .map_err(mob_err)?;
    }
    Ok(())
}
