//! The orchestrators: durable, turn-driven, host-root members of the
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
use super::{DaemonError, MOB_ID, ORCHESTRATOR};

/// Where the agent the daemon started with lives (the most recent one it
/// restored, else the one it seated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orchestrator {
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
    let profiles = std::mem::take(&mut definition.profiles);
    for (_, binding) in profiles {
        if let ProfileBinding::Inline(mut profile) = binding {
            profile.durable = true;
            profile.runtime_mode = MobRuntimeMode::TurnDriven;
            profile.peer_description = "rkat-tui orchestrator".to_string();
            // Clients reach it with `mob/member_send`, whose run outlives
            // the client (a `mob/turn_start` run is its request's).
            profile.external_addressable = true;
            profile.tools.builtins = true;
            profile.tools.shell = true;
            profile.tools.comms = true;
            profile.tools.mob = true;
            profile.tools.memory = plan.memory;
            profile.tools.mcp_servers.clone_from(&plan.mcp_servers);
            // The `[thinking]` of the start-up model; a rebuild re-applies
            // the setting of the session's model (the customizer).
            profile.provider_params = crate::thinking::params_for(plan.thinking.as_ref());
            // The `[auto]` compaction policy (the mob keeps the definition
            // it was created with: a policy changed later reaches the
            // orchestrator once its mob is created again).
            profile.auto_compact_threshold = plan.compact_threshold;
            definition.profiles.insert(
                ProfileName::from(ORCHESTRATOR),
                ProfileBinding::Inline(profile),
            );
        }
    }
    definition
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
