//! Helpers (sub-agents) as Meerkat mob members.
//!
//! The agent delegates with upstream's mob tools (`delegate`: an ephemeral
//! helper in the session's implicit mob, which runs one turn and returns
//! its bounded result; the `mob_*` tools for longer-lived teams). The
//! daemon hosts them in its [`MobMcpState`] (mob state durable under the
//! realm, see `crate::daemon::engine`) and adds what upstream leaves to the
//! host:
//!
//! - **The approval gate.** [`customizer::HelperCustomizer`] (the state's
//!   child spawn-member customizer, asked for every member of every mob the
//!   state creates or restores) puts an [`crate::approval::ApprovalGate`] for
//!   the helper's owner on the spawn spec's `tool_dispatch_admission`: the
//!   helper's calls are judged against the owner's transcript of the user's
//!   words (the helper's task is assistant-authored and never recorded as
//!   user intent), share its allow cache, Stop and the denial counters, and
//!   the escalation dialog names the helper.
//! - **The model.** A `delegate` helper runs on its delegator's model (the
//!   implicit mob's profile); the customizer adds the owner's auth binding,
//!   so a helper of a configured endpoint's session reaches the same
//!   endpoint. The engine names the provider route the session's build
//!   resolved for its model in the implicit mob's profile, so a model only
//!   an endpoint lists (a realm `[models.<id>]` profile) delegates too.
//! - **Tools and instructions.** The TUI's file tools and
//!   [`crate::bootstrap::HELPER_PERSONA`].
//! - **Observation.** [`follow`] subscribes to each helper and forwards its
//!   events, scoped to it, as [`HelperUpdate`]s (the daemon reads a
//!   helper's tool catalog when its run ends); clients follow the helpers
//!   on the mobs' own event streams (`crate::client::helpers`).
//! - **Ownership.** A helper works for the session its spawner works for,
//!   or else for the bridge session that created its mob (the mob host's
//!   create hook records it); a spawn that resolves to no session open here
//!   is refused. The bindings persist under the realm
//!   ([`bindings::BINDINGS_FILE`]), so a member restored after a restart is
//!   gated for its own owner
//!   ([`customizer::HelperCustomizer`]'s `customize_resume`). A retired
//!   member or a destroyed mob is dropped from them; a forgotten session
//!   ([`crate::approval::ApprovalEngine::forget_session`]) is dropped from
//!   memory.
//! - **Interrupts.** [`cancel_helpers`] force-cancels the running helpers of
//!   a session when its turn is interrupted or its conversation is left.

pub mod registry;

pub mod bindings;
pub mod customizer;
pub(crate) mod follow;

use std::sync::Arc;

use meerkat_core::SessionId;

pub use registry::{HelperKey, HelperRegistry};
pub use registry::{HelperStatus, HelperTag, HelperUpdate};

use meerkat_mob_mcp::MobMcpState;

/// Stop every running helper `registry` lists for `owner` in `state`'s mobs:
/// force-cancel its run, then retire it. The `delegate` call that would
/// have retired it is abandoned with the owner's run, so without the retire
/// the helper would stay seated. Failures are logged: the gate already
/// denies the helpers' reviewed calls once the owner is stopped. The daemon
/// calls it when the orchestrator's run is interrupted and before it
/// switches conversations.
pub(crate) async fn cancel_helpers(
    state: &MobMcpState,
    registry: &HelperRegistry,
    owner: &SessionId,
) {
    for key in registry.live_for(owner) {
        let handle = match state.handle_for(&key.mob).await {
            Ok(handle) => handle,
            Err(err) => {
                tracing::warn!(helper = %key, error = %err, "cannot reach helper's mob");
                continue;
            }
        };
        if let Err(err) = handle.force_cancel_member(key.member.clone()).await {
            tracing::debug!(helper = %key, error = %err, "helper run not cancelled");
        }
        if let Err(err) = handle.retire(key.member.clone()).await {
            tracing::warn!(helper = %key, error = %err, "cannot retire helper");
        }
    }
}

/// Record the creator of every mob the host creates (helpers spawned into
/// it work for that session) and drop a destroyed mob's helpers.
///
/// The state is fresh, so the hooks bind; a failure is logged and fails
/// closed (no mob's creator is recorded, so every helper spawn that no
/// helper makes is refused).
pub(crate) fn install_mob_hooks(state: &Arc<MobMcpState>, registry: &Arc<HelperRegistry>) {
    let created = Arc::downgrade(registry);
    let created_hook = state.set_before_create(Arc::new(
        move |request: meerkat_mob_mcp::MobCreateRequest| -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), String>> + Send>,
        > {
            // A host-made mob has no creator session: nothing to record, and
            // its spawns are refused unless a helper spawns them.
            let recorded = match (created.upgrade(), request.owner_bridge_session_id.as_ref()) {
                (Some(registry), Some(owner)) => registry
                    .record_mob(&request.mob_id, owner)
                    .map_err(|err| format!("rkat-tui cannot record the mob's owner: {err}")),
                _ => Ok(()),
            };
            Box::pin(async move { recorded })
        },
    ));
    if let Err(err) = created_hook {
        tracing::error!(error = %err, "cannot record mob creators: helper spawns are refused");
    }
    let destroyed = Arc::downgrade(registry);
    let destroyed_hook = state.set_after_destroy(Arc::new(move |mob: &meerkat_mob::MobId| {
        if let Some(registry) = destroyed.upgrade() {
            registry.mob_destroyed(mob);
        }
    }));
    if let Err(err) = destroyed_hook {
        tracing::warn!(error = %err, "destroyed mobs keep their helper bindings");
    }
}
