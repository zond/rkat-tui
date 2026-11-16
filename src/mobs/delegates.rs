//! The `delegate` helpers a realm of an earlier build holds.
//!
//! `delegate`, the engine's tool, is withdrawn
//! ([`crate::approval::policy::WITHDRAWN_TOOLS`]; `assign` replaces it), so
//! no helper of it is seated any more. A realm an earlier build wrote may
//! still hold one: a member of its caller's implicit mob
//! (`implicit-<session>`), seated by the mob's `delegate` profile, bound to its
//! caller's session, whose call ended with that build's daemon. The engine
//! restores it at the next start and runs it on; its parent is told when
//! it finishes ([`crate::mobs::endings`]), and [`retire_left_helper`] then
//! retires it, which the engine did only when the call returned.

use meerkat_mob_mcp::MobMcpState;

use super::HelperKey;

/// The profile the engine seated a `delegate` helper with.
const DELEGATE_PROFILE: &str = "delegate";

/// The mob a `delegate` call of `session` seated its helper in (tests:
/// the daemon only recognises such a mob by its `implicit-` prefix).
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn implicit_mob(session: &meerkat_core::SessionId) -> meerkat_mob::MobId {
    meerkat_mob::MobId::from(format!("implicit-{session}"))
}

/// Retire `key`, a `delegate` helper an earlier build left that finished,
/// when its mob still lists it under the `delegate` profile: force-cancel
/// its run, then retire it. Anything else (gone already, another profile's
/// member under the same id) is left as it is.
pub async fn retire_left_helper(state: &MobMcpState, key: &HelperKey) {
    let Ok(handle) = state.handle_for(&key.mob).await else {
        return;
    };
    let seated = handle.list_members().await.into_iter().any(|member| {
        member.agent_identity == key.member && member.role.as_str() == DELEGATE_PROFILE
    });
    if !seated {
        return;
    }
    tracing::info!(helper = %key, "retiring a finished delegate helper an earlier build left");
    super::stop_helper(&handle, key).await;
}
