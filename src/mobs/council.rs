//! Councils: the participants of a council an agent (or a helper)
//! convenes work for whoever convened it.
//!
//! The engine names no convener on a council's mob (lukacf/meerkat#2183);
//! the derivation below stands in for it until that ships.
//!
//! The engine seats a council's participants (forks of the members named in
//! the `council` call) in a temporary mob it creates for an owner session
//! of its own, a fresh id no session of this daemon has, with
//! `SpawnSource::AttachedForkedParticipant` and no spawner
//! (`docs/evidence.md`, "Councils"). Nothing in the create request or the
//! spawn names the convener, so [`Convening`] does: when the gate admits a
//! `council` call, the mob id the engine will create for it (derived from
//! the call as the engine derives it, [`council_mob`]) is recorded as
//! convened by the calling session ([`HelperRegistry::expect_council`]),
//! and the create hook records that session as the mob's owner instead of
//! the engine's fresh one. The participants are then helpers of the
//! convener like any other: gated for its owner, followed, their catalogs
//! read. A council mob nobody convened through the gate keeps the engine's
//! owner, which no session here has, and its participants are refused.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use meerkat_core::{
    LiveBridgeEffectKind, LiveBridgeEffectOutcome, SessionId, ToolCallView, ToolDispatchAdmission,
    ToolDispatchContext, ToolError,
};
use meerkat_mob::MobId;
use meerkat_mob::temporary_council::TemporaryCouncilId;

use super::HelperRegistry;

/// The tool that convenes a council.
pub const COUNCIL: &str = "council";

/// The namespace of an agent-derived council id: the engine's
/// (`meerkat_mob_mcp::agent_tools::AGENT_COUNCIL_NAMESPACE`, private there;
/// `tests/it/councils.rs` fails if it drifts).
const AGENT_COUNCIL_NAMESPACE: uuid::Uuid =
    uuid::Uuid::from_u128(0x312d_5af0_861a_4ba8_a35d_d6b4_734a_25b9);

/// The temporary mob the engine creates for the `council` call `call_id`
/// of `session` with `args`: the call's own `council_id` when it names
/// one, else the id derived from the session and the call. `None` when the
/// arguments name an id the engine refuses (it then creates no mob).
#[must_use]
pub fn council_mob(session: &SessionId, call_id: &str, args: &serde_json::Value) -> Option<MobId> {
    let id = match args.get("council_id").and_then(serde_json::Value::as_str) {
        Some(named) => TemporaryCouncilId::new(named).ok()?,
        None => {
            let seed = format!("{session}:{call_id}");
            let id = uuid::Uuid::new_v5(&AGENT_COUNCIL_NAMESPACE, seed.as_bytes());
            TemporaryCouncilId::new(format!("agent-{}", id.simple())).ok()?
        }
    };
    Some(id.temporary_mob_id())
}

/// An admission that records the council a call it admits convenes, for
/// the session that makes it (the dispatching session, else `session`),
/// after `inner` admitted it.
pub struct Convening {
    inner: Arc<dyn ToolDispatchAdmission>,
    registry: Weak<HelperRegistry>,
    session: Option<SessionId>,
}

impl Convening {
    /// `inner`, recording councils in `registry` for the dispatching
    /// session or `session`.
    #[must_use]
    pub fn new(
        inner: Arc<dyn ToolDispatchAdmission>,
        registry: &Arc<HelperRegistry>,
        session: Option<SessionId>,
    ) -> Self {
        Self {
            inner,
            registry: Arc::downgrade(registry),
            session,
        }
    }
}

impl std::fmt::Debug for Convening {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Convening")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ToolDispatchAdmission for Convening {
    async fn await_dispatch_admission(
        &self,
        call: ToolCallView<'_>,
        context: Option<&ToolDispatchContext>,
        effect_kind: LiveBridgeEffectKind,
    ) -> Result<(), ToolError> {
        self.inner
            .await_dispatch_admission(call, context, effect_kind)
            .await?;
        if call.name == COUNCIL {
            let session = context
                .and_then(ToolDispatchContext::origin_session_id)
                .or(self.session.as_ref());
            let args = serde_json::from_str(call.args.get()).unwrap_or_default();
            if let (Some(session), Some(registry)) = (session, self.registry.upgrade())
                && let Some(mob) = council_mob(session, call.id, &args)
            {
                registry.expect_council(mob, session.clone());
            }
        }
        Ok(())
    }

    async fn record_dispatch_outcome(
        &self,
        call: ToolCallView<'_>,
        context: Option<&ToolDispatchContext>,
        effect_kind: LiveBridgeEffectKind,
        outcome: LiveBridgeEffectOutcome,
    ) -> Result<(), ToolError> {
        if call.name == COUNCIL && outcome == LiveBridgeEffectOutcome::Failed {
            let session = context
                .and_then(ToolDispatchContext::origin_session_id)
                .or(self.session.as_ref());
            let args = serde_json::from_str(call.args.get()).unwrap_or_default();
            if let (Some(session), Some(registry)) = (session, self.registry.upgrade())
                && let Some(mob) = council_mob(session, call.id, &args)
            {
                registry.council_failed(&mob, session);
            }
        }
        self.inner
            .record_dispatch_outcome(call, context, effect_kind, outcome)
            .await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_council_mob_is_the_named_id_or_the_one_derived_from_the_call() {
        let session = SessionId::new();
        let named = council_mob(&session, "c1", &serde_json::json!({"council_id": "review"}));
        assert_eq!(named.unwrap().as_str(), "council--review");
        let derived = council_mob(&session, "c1", &serde_json::json!({})).unwrap();
        assert!(derived.as_str().starts_with("council--agent-"), "{derived}");
        assert_eq!(
            council_mob(&session, "c1", &serde_json::json!({})),
            Some(derived.clone())
        );
        assert_ne!(
            council_mob(&session, "c2", &serde_json::json!({})),
            Some(derived)
        );
        // An id the engine refuses creates no mob.
        assert_eq!(
            council_mob(&session, "c1", &serde_json::json!({"council_id": "a b"})),
            None
        );
    }
}
