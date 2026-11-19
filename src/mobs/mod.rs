//! Helpers (sub-agents) as Meerkat mob members.
//!
//! The agent starts every helper with rkat-tui's `assign`
//! (`crate::tools::assign`, `crate::daemon::assign`), beside it in its own
//! mob, with two choices: from a fresh context or a fork of its
//! conversation, single-use or kept ([`HelperKind`]); the `mob_*` tools
//! make teams of their own. The engine's `delegate`, `fork_off` and
//! `spawn_member` are not offered ([`crate::approval::policy::NOT_OFFERED`]);
//! a realm of an earlier build may still hold `delegate` helpers
//! ([`delegates`]). The
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
//!   user intent), share its allow cache and the denial counters (not its
//!   Stop: an interrupt stops the agent's own calls only), and the
//!   escalation dialog names the helper; Stop on that dialog stops the
//!   helper.
//! - **The model.** The customizer chooses a fresh helper's model (the
//!   one its spawn names, else `helpers.model`, else its owner's; a fork
//!   its owner's) and adds the owner's auth binding, so a helper of a
//!   configured endpoint's session reaches the same endpoint, on a
//!   self-hosted one as its alias.
//! - **Tools and instructions.** rkat-tui's file tools and
//!   [`crate::bootstrap::HELPER_PERSONA`].
//! - **Observation.** [`follow`] subscribes to each helper once and hands
//!   the daemon what it needs of the helper's events
//!   ([`follow::HelperWatch`]: each model call's usage, counted to the
//!   owner's conversation, and each run's end, when the helper's tool
//!   catalog is read); clients follow the helpers on the mobs' own event
//!   streams (`crate::client::helpers`).
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
//! - **Interrupts.** An interrupt stops the agent's own run only: its
//!   helpers keep running (the owner's decision of 2026-11-15). Leaving a
//!   conversation ([`cancel_helpers`]) and the daemon's stop still end them;
//!   the user stops one helper on its own ([`endings::HelperEndings::stop`]).
//! - **No orphans.** However a helper is retired, its own helpers (theirs
//!   too) are stopped and retired with it, deepest first
//!   ([`endings::HelperEndings::take_below`]), and its parent's note or
//!   `retire_helper`'s answer names them.
//! - **Endings.** An `assign` helper's answer goes to its parent as a note
//!   when it finishes, and it is retired; a kept member's parent is told
//!   each time it goes idle; every ending is told to the parent once
//!   ([`endings`]).

pub mod registry;

pub mod bindings;
pub mod council;
pub mod customizer;
pub mod delegates;
pub mod endings;
pub(crate) mod follow;
pub mod profiles;

use std::sync::Arc;

use meerkat_core::SessionId;

pub use registry::{Gone, HelperKey, HelperRegistry, Refused, SeatedHook};
pub use registry::{HelperContext, HelperKind, HelperLife, HelperStatus, HelperTag};

use meerkat_mob_mcp::MobMcpState;

/// The helpers seated now for `owner` (its whole tree, nested ones too):
/// every helper bound to it that its mob's roster lists, a helper the
/// engine restored and has not rebuilt yet (an idle kept one after a
/// restart) too, by mob and name. The one source of what an
/// agent holds: the clients' live helpers, `contacts`, the reminder after
/// a compaction and a conversation's end read it.
pub async fn seated_for(
    state: &MobMcpState,
    registry: &HelperRegistry,
    owner: &SessionId,
) -> Vec<(HelperKey, HelperTag)> {
    let bound: Vec<(HelperKey, HelperTag)> = registry
        .bound_members()
        .into_iter()
        .filter(|(_, tag)| &tag.owner == owner)
        .collect();
    let mut rosters: std::collections::HashMap<
        meerkat_mob::MobId,
        Vec<meerkat_mob::AgentIdentity>,
    > = std::collections::HashMap::new();
    for (key, _) in &bound {
        if rosters.contains_key(&key.mob) {
            continue;
        }
        let roster = match state.handle_for(&key.mob).await {
            Ok(handle) => handle
                .list_members()
                .await
                .into_iter()
                .map(|member| member.agent_identity)
                .collect(),
            Err(_) => Vec::new(),
        };
        rosters.insert(key.mob.clone(), roster);
    }
    bound
        .into_iter()
        .filter(|(key, _)| {
            rosters
                .get(&key.mob)
                .is_some_and(|roster| roster.contains(&key.member))
        })
        .collect()
}

/// The helpers among `seated` (one owner's, [`seated_for`]) that count
/// against the `assign` limit of the parent at `path` of that tree, which
/// sits in `mob`: the ones one level below it in its own mob, the mob
/// `assign` seats in. A member of a mob it made with `mob_create` is not
/// counted.
#[must_use]
pub fn held_by(
    seated: Vec<(HelperKey, HelperTag)>,
    mob: &meerkat_mob::MobId,
    path: &[String],
) -> Vec<(HelperKey, HelperTag)> {
    seated
        .into_iter()
        .filter(|(key, tag)| {
            &key.mob == mob && tag.path.len() == path.len() + 1 && tag.path.starts_with(path)
        })
        .collect()
}

/// Stop every running helper `registry` lists for `owner` in `state`'s mobs:
/// force-cancel its run, then retire it. Failures are logged. The daemon
/// calls it before an agent switches conversations (the conversation its
/// helpers worked for is left, and nobody is told of their ending); an
/// interrupt does not.
pub(crate) async fn cancel_helpers(
    state: &MobMcpState,
    registry: &HelperRegistry,
    owner: &SessionId,
) {
    // Deepest first: a helper's own helpers before it.
    let mut seated = seated_for(state, registry, owner).await;
    seated.sort_by_key(|(_, tag)| std::cmp::Reverse(tag.path.len()));
    for (key, _) in seated {
        // Left with its conversation: no parent is told. Owed until the
        // engine retired it, so one the engine refuses goes at the next
        // start.
        registry.endings().left(&key);
        registry.owe_retire(&key);
        match state.handle_for(&key.mob).await {
            Ok(handle) => {
                if let Err(err) = registry
                    .endings()
                    .retirements()
                    .stop_helper(&handle, &key)
                    .await
                {
                    tracing::warn!(helper = %key, error = %err, "a helper of the conversation left stays seated: it is retired at the next start");
                }
            }
            Err(err) => tracing::warn!(helper = %key, error = %err, "cannot reach helper's mob"),
        }
    }
}

/// How many times [`stop_helper`] asks the engine to retire a helper.
const RETIRE_TRIES: u32 = 3;
/// The wait before [`stop_helper`]'s second try; each next one doubles.
const RETIRE_BACKOFF: std::time::Duration = std::time::Duration::from_millis(250);

/// Force-cancel the helper `key`'s run in its mob `handle`, then retire it.
/// A refused retire (the engine answers some as retryable, e.g. one that
/// meets a spawn of the same name) is tried again, [`RETIRE_TRIES`] times
/// in all with a growing wait. `Ok` once the engine retired it, or its
/// roster lists it no more; else the engine's last refusal, in words
/// (the helper is still seated).
///
/// # Errors
///
/// The engine's last refusal when every try failed and the roster still
/// lists the helper.
pub(crate) async fn stop_helper(
    handle: &meerkat_mob::MobHandle,
    key: &HelperKey,
) -> Result<(), String> {
    let mut wait = RETIRE_BACKOFF;
    let mut last = String::new();
    for attempt in 1..=RETIRE_TRIES {
        if let Err(err) = handle.force_cancel_member(key.member.clone()).await {
            tracing::debug!(helper = %key, error = %err, "helper run not cancelled");
        }
        match handle.retire(key.member.clone()).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                tracing::warn!(helper = %key, attempt, error = %err, "the engine did not retire the helper");
                last = err.to_string();
            }
        }
        if !roster_lists(handle, key).await {
            return Ok(());
        }
        if attempt < RETIRE_TRIES {
            tokio::time::sleep(wait).await;
            wait *= 2;
        }
    }
    Err(last)
}

/// Whether `handle`'s roster lists the helper `key`.
async fn roster_lists(handle: &meerkat_mob::MobHandle, key: &HelperKey) -> bool {
    handle
        .list_members()
        .await
        .iter()
        .any(|member| member.agent_identity == key.member)
}

/// Record the creator of every mob the host creates (helpers spawned into
/// it work for that session) and drop a destroyed mob's helpers.
///
/// The state is fresh, so the hooks bind; a failure is logged and fails
/// closed (no mob's creator is recorded, so every helper spawn that no
/// helper makes is refused).
pub(crate) fn install_mob_hooks(state: &Arc<MobMcpState>, registry: &Arc<HelperRegistry>) {
    registry.endings().bind(state);
    registry.endings().owe_to(registry);
    let created = Arc::downgrade(registry);
    let created_hook = state.set_before_create(Arc::new(
        move |request: meerkat_mob_mcp::MobCreateRequest| -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), String>> + Send>,
        > {
            // A council's mob is created for a session of the council's own:
            // its participants work for the session that convened it, which
            // the engine names. A council nobody convened (a host ran it)
            // keeps the council's session, which no session here has, so
            // its participants are refused. A host-made mob has no creator
            // session: nothing to record, and its spawns are refused unless
            // a helper spawns them.
            let creator = request
                .council
                .as_ref()
                .and_then(|council| council.convener_session_id.as_ref())
                .or(request.owner_bridge_session_id.as_ref());
            let recorded = match (created.upgrade(), creator) {
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
