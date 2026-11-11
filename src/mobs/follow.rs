//! Following helpers in the daemon: every helper the customizer saw
//! spawning is subscribed through its mob (from its earliest retained
//! event, so a fast helper's whole run is seen), its session is bound in
//! the registry (for nesting), and what the daemon needs of its events is
//! handed to a [`HelperWatch`] (each model call's usage, each run's end)
//! until it leaves its mob. The mob's own structural events drive the
//! rest (`MobHandle::events().subscribe()`, taken before the member's stream): a
//! helper that is not a member yet is subscribed once its `MemberSpawned`
//! arrives, and its `MemberRetired` (or the mob's destroy, reset or
//! completion) ends the follow, since a retired member's session stream
//! does not end by itself.
//!
//! A helper that left the roster of a running mob retired: it is dropped
//! from the registry and its binding. One whose mob stopped (the daemon is
//! quitting) only stops being listed as running; its binding stays, so the
//! member is gated for its owner when the mob is restored.

use std::sync::{Arc, Weak};
use std::time::Duration;

use futures::StreamExt;
use meerkat_core::comms::{EventStream, SessionEventCursor};
use meerkat_core::{AgentEvent, EventSourceIdentity, SessionId, TurnUsage};
use meerkat_mob::{
    AgentIdentity, MobError, MobEvent, MobEventKind, MobEventsSubscription, MobHandle, MobId,
    MobState,
};
use meerkat_mob_mcp::MobMcpState;
use tokio::sync::mpsc;

use super::customizer::Spawning;
use super::registry::{HelperRegistry, HelperTag};

/// What the daemon does with a followed helper's events.
#[async_trait::async_trait]
pub trait HelperWatch: Send + Sync {
    /// One model call of the helper `tag` used `usage`.
    async fn turn(&self, tag: &HelperTag, usage: &TurnUsage);
    /// A run of the helper running as `session` ended.
    fn run_ended(&self, session: &SessionId);
    /// `mob` hosts a helper the follower subscribed to (its stream can be
    /// opened now).
    fn mob_hosts(&self, mob: &MobId);
}

/// Hand `event` of the helper `tag` to `watch`, if it wants it.
async fn observe(watch: &dyn HelperWatch, tag: &HelperTag, event: &AgentEvent) {
    match event {
        AgentEvent::TurnCompleted {
            usage: Some(usage), ..
        } => watch.turn(tag, usage).await,
        AgentEvent::RunCompleted { session_id, .. } | AgentEvent::RunFailed { session_id, .. } => {
            watch.run_ended(session_id);
        }
        _ => {}
    }
}

/// How long a helper the customizer announced may take to join its mob
/// (its `MemberSpawned`); a spawn refused after the customizer saw it never
/// joins.
const JOIN_WAIT: Duration = Duration::from_secs(10);

/// How long the follower waits for the helper's mob to be reachable
/// (`MobMcpState::handle_for`), and how often it tries. The engine's create
/// hook runs before the mob exists and it has no hook or event for "the
/// mob is managed now" (`docs/evidence.md`, the engine asks), so this one
/// wait stays a bounded retry; a member's spawn normally finds its mob
/// managed already.
const MOB_WAIT: Duration = Duration::from_secs(2);
const MOB_RETRY: Duration = Duration::from_millis(20);

/// Follow every helper announced on `spawning` until the channel closes.
pub async fn watch(
    state: Weak<MobMcpState>,
    registry: Arc<HelperRegistry>,
    observer: Arc<dyn HelperWatch>,
    mut spawning: mpsc::UnboundedReceiver<Spawning>,
) {
    while let Some(helper) = spawning.recv().await {
        tokio::spawn(follow(
            Weak::clone(&state),
            Arc::clone(&registry),
            Arc::clone(&observer),
            helper,
        ));
    }
}

/// Follow one helper's events until it leaves its mob.
async fn follow(
    state: Weak<MobMcpState>,
    registry: Arc<HelperRegistry>,
    observer: Arc<dyn HelperWatch>,
    helper: Spawning,
) {
    let Spawning { key, tag } = helper;
    let retired = match subscribe(&state, &key.mob, &key.member).await {
        Ok(Followed {
            handle,
            mut stream,
            session,
            mut mob_events,
        }) => {
            observer.mob_hosts(&key.mob);
            if let Some(session) = session {
                registry.bind_session(&key, &tag, &session);
            }
            let mut retired = None;
            loop {
                tokio::select! {
                    envelope = stream.next() => {
                        let Some(envelope) = envelope else { break };
                        if let EventSourceIdentity::Session { session_id } = &envelope.source {
                            registry.bind_session(&key, &tag, session_id);
                        }
                        observe(&*observer, &tag, &envelope.payload).await;
                    }
                    event = recv(&mut mob_events) => match event.map(|event| left(&event.kind, &key.member)) {
                        Some(Some(gone)) => {
                            // Deliver what it published before it left.
                            while let Some(Some(envelope)) = futures::FutureExt::now_or_never(stream.next()) {
                                observe(&*observer, &tag, &envelope.payload).await;
                            }
                            retired = Some(gone);
                            break;
                        }
                        Some(None) => {}
                        None => {
                            tracing::debug!(helper = %key, "the mob's events ended; following the helper's own stream to its end");
                            mob_events = None;
                        }
                    },
                }
            }
            match retired {
                Some(retired) => retired,
                // Its stream ended first: one look at the roster says why.
                None => has_retired(&handle, &key.member).await,
            }
        }
        Err(err) => {
            tracing::warn!(helper = %key, error = %err, "cannot follow helper");
            registry.follow_failed(&tag);
            // A spawn refused after the customizer bound it never joined.
            match state.upgrade() {
                Some(state) => match state.handle_for(&key.mob).await {
                    Ok(handle) => has_retired(&handle, &key.member).await,
                    Err(_) => false,
                },
                None => false,
            }
        }
    };
    if retired {
        registry.retired(&key);
    } else {
        registry.finished(&key);
    }
}

/// The next structural event of a mob (`None`: they ended, or none are
/// followed: then it never answers).
async fn recv(events: &mut Option<MobEventsSubscription>) -> Option<MobEvent> {
    match events {
        Some(events) => events.event_rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Whether `kind` ends the follow of `identity`: `Some(true)` when the
/// member retired, `Some(false)` when its mob ended (destroyed, reset or
/// completed: its bindings are the destroy hook's), `None` otherwise.
fn left(kind: &MobEventKind, identity: &AgentIdentity) -> Option<bool> {
    match kind {
        MobEventKind::MemberRetired { agent_identity, .. } if agent_identity == identity => {
            Some(true)
        }
        MobEventKind::MobDestroying | MobEventKind::MobReset | MobEventKind::MobCompleted => {
            Some(false)
        }
        _ => None,
    }
}

/// Whether `identity` left `handle`'s mob for good: the mob still runs and
/// its roster no longer lists the member. A stopped mob (the host is
/// quitting) proves nothing; a destroyed one is dropped by the host's
/// destroy hook.
async fn has_retired(handle: &MobHandle, identity: &AgentIdentity) -> bool {
    handle.status_observation_snapshot() == MobState::Running && !is_member(handle, identity).await
}

/// Whether `identity` is still a member of `handle`'s mob (retiring counts
/// as gone).
async fn is_member(handle: &MobHandle, identity: &AgentIdentity) -> bool {
    handle
        .list_members()
        .await
        .iter()
        .any(|member| &member.agent_identity == identity)
}

/// A helper followed: its mob, its events from the earliest retained one,
/// its session when the mob names it, and the mob's structural events
/// (`None`: they could not be followed).
struct Followed {
    handle: MobHandle,
    stream: EventStream,
    session: Option<meerkat_core::SessionId>,
    mob_events: Option<MobEventsSubscription>,
}

/// Subscribe to `identity` in `mob_id` from its earliest retained event.
/// The mob's structural events are subscribed first, so a helper that is
/// not a member yet is subscribed at its `MemberSpawned` (within
/// [`JOIN_WAIT`]), and its retirement is not missed.
async fn subscribe(
    state: &Weak<MobMcpState>,
    mob_id: &MobId,
    identity: &AgentIdentity,
) -> Result<Followed, MobError> {
    let handle = mob_handle(state, mob_id).await?;
    let mut mob_events = match handle.events().subscribe().await {
        Ok(events) => Some(events),
        Err(err) => {
            tracing::warn!(mob = %mob_id, error = %err, "cannot follow the mob's events; a helper's retirement shows when its stream ends");
            None
        }
    };
    let subscribed = match handle
        .subscribe_agent_events_from(identity, SessionEventCursor::Earliest)
        .await
    {
        Err(MobError::MemberNotFound(_)) => {
            joined(&mut mob_events, identity).await?;
            handle
                .subscribe_agent_events_from(identity, SessionEventCursor::Earliest)
                .await?
        }
        other => other?,
    };
    let session = handle.resolve_bridge_session_id(identity).await;
    Ok(Followed {
        handle,
        stream: subscribed.stream,
        session,
        mob_events,
    })
}

/// Wait for `identity`'s `MemberSpawned` on `events` (at most
/// [`JOIN_WAIT`]).
async fn joined(
    events: &mut Option<MobEventsSubscription>,
    identity: &AgentIdentity,
) -> Result<(), MobError> {
    let never = || MobError::Internal("helper never appeared in its mob".to_string());
    tokio::time::timeout(JOIN_WAIT, async {
        loop {
            match recv(events).await.map(|event| event.kind) {
                Some(MobEventKind::MemberSpawned(spawned))
                    if &spawned.agent_identity == identity =>
                {
                    return Ok(());
                }
                Some(kind) if left(&kind, identity).is_some() => return Err(never()),
                Some(_) => {}
                None => return Err(never()),
            }
        }
    })
    .await
    .map_err(|_| never())?
}

/// `mob_id`'s handle, waiting (within [`MOB_WAIT`]) for it to be managed.
async fn mob_handle(state: &Weak<MobMcpState>, mob_id: &MobId) -> Result<MobHandle, MobError> {
    let deadline = tokio::time::Instant::now() + MOB_WAIT;
    loop {
        let state = state
            .upgrade()
            .ok_or_else(|| MobError::Internal("the mob host is gone".to_string()))?;
        match state.handle_for(mob_id).await {
            Ok(handle) => return Ok(handle),
            Err(err) if tokio::time::Instant::now() >= deadline => return Err(err),
            Err(_) => {}
        }
        drop(state);
        tokio::time::sleep(MOB_RETRY).await;
    }
}
