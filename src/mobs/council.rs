//! Councils: the participants of a council an agent (or a helper)
//! convenes work for whoever convened it.
//!
//! The engine seats a council's participants (forks of the members named in
//! the `council` call) in a temporary mob it creates for an owner session
//! of its own, a fresh id no session of this daemon has, with
//! `SpawnSource::AttachedForkedParticipant` and no spawner
//! (`docs/evidence.md`, "Councils"). Its create request names the session
//! that convened it (`MobCreateRequest::council`, Meerkat 0.9.15), and the
//! create hook records that session as the mob's owner
//! ([`super::install_mob_hooks`]). The participants are then helpers of the
//! convener like any other: gated for its owner, followed, their catalogs
//! read. A council nobody convened (a host ran it) keeps the engine's
//! owner, which no session here has, and its participants are refused.
//!
//! The client never sees the create request; it follows a council's
//! participants from the agent's `council` call, by the mob id the engine
//! creates for it ([`council_mob`], `crate::client::helpers`).

use meerkat_core::SessionId;
use meerkat_mob::MobId;
use meerkat_mob::temporary_council::TemporaryCouncilId;

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
