//! The orchestrator: one durable, turn-driven, host-root member of the
//! workspace's mob, created on first start and restored after.
//!
//! Clients find it with `mob/members {mob_id: "tui"}` (identity `main`)
//! and its session with `mob/member_status` (`current_session_id`).

use std::sync::Arc;

use meerkat_mob::{
    AgentIdentity, MobDefinition, MobId, MobRuntimeMode, ProfileBinding, ProfileName,
};
use meerkat_mob_mcp::MobMcpState;

use super::engine::RootPlan;
use super::{DaemonError, MOB_ID, ORCHESTRATOR};
use crate::mobs::HelperKey;

/// Where the orchestrator lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orchestrator {
    /// Its mob.
    pub mob_id: MobId,
    /// Its member identity.
    pub identity: AgentIdentity,
    /// The session it runs as.
    pub session_id: meerkat_core::SessionId,
    /// Whether this start created it (else it was restored).
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
            definition.profiles.insert(
                ProfileName::from(ORCHESTRATOR),
                ProfileBinding::Inline(profile),
            );
        }
    }
    definition
}

/// Create or restore the mob and seat the orchestrator if it is missing.
///
/// # Errors
///
/// [`DaemonError::Mob`] when the mob cannot be created, resumed or spawned
/// into; [`DaemonError::NoSession`] when the member has no session.
pub async fn ensure(
    state: &Arc<MobMcpState>,
    plan: &RootPlan,
) -> Result<Orchestrator, DaemonError> {
    let mob_id = MobId::from(MOB_ID);
    let identity = AgentIdentity::from(ORCHESTRATOR);
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
    let members = state.mob_list_members(&mob_id).await.map_err(mob_err)?;
    let created = !members
        .iter()
        .any(|member| member.agent_identity == identity);
    if created {
        let mut spec = meerkat_mob::SpawnMemberSpec::host_root(ORCHESTRATOR, ORCHESTRATOR);
        spec.runtime_mode = Some(MobRuntimeMode::TurnDriven);
        state.mob_spawn_spec(&mob_id, spec).await.map_err(mob_err)?;
    }
    let snapshot = state
        .mob_member_status(&mob_id, &identity)
        .await
        .map_err(mob_err)?;
    let session_id = snapshot
        .current_session_id
        .ok_or_else(|| DaemonError::NoSession(ORCHESTRATOR.to_string()))?;
    // A restored orchestrator registered itself in its customizer's resume;
    // a fresh one registers here, before its first turn can delegate.
    let key = HelperKey::new(mob_id.clone(), identity.clone());
    if plan.registry.root_session(&key).as_ref() != Some(&session_id) {
        plan.registry
            .register_root(key, &session_id, &plan.model, plan.auth_binding.as_ref());
    }
    Ok(Orchestrator {
        mob_id,
        identity,
        session_id,
        created,
    })
}
