//! Following helpers in the daemon: every helper the customizer saw
//! spawning is subscribed through its mob (from its earliest retained
//! event, so a fast helper's whole run is seen), its session is bound in
//! the registry (for nesting), what the daemon needs of its events is
//! handed to a [`HelperWatch`] (each model call's usage, each run's end)
//! until it leaves its mob, and its runs' starts and ends and its calls go
//! to [`super::endings`]. The mob's own structural events drive the
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
    /// The helper `tag` left its mob for good (it was retired): the clients
    /// show it retired, not idle.
    fn retired(&self, tag: &HelperTag);
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
    let Spawning {
        key,
        tag,
        mut wired,
    } = helper;
    // Tests: the fork's wire is made as its spawn is announced, before it
    // is seated (what a slow spawn does to the normal order).
    #[cfg(any(test, feature = "test-support"))]
    let wiring_early = tag.kind.is_fork() && registry.wires_forks_early();
    #[cfg(not(any(test, feature = "test-support")))]
    let wiring_early = false;
    if wiring_early {
        tokio::spawn(wire_fork(
            Weak::clone(&state),
            Arc::clone(&registry),
            key.clone(),
            tag.clone(),
            wired.take(),
        ));
    }
    let retired = match subscribe(&state, &key.mob, &key.member).await {
        Ok(Followed {
            handle,
            mut stream,
            session,
            mut mob_events,
        }) => {
            observer.mob_hosts(&key.mob);
            // It is in its mob's roster now: its owner's clients list it.
            registry.seated_changed(&tag.owner);
            // A fresh fork, and one restored (the mob keeps the edge
            // across a restart; one it lost is made again): wired once it
            // is seated, off this task, which reads its events meanwhile.
            if tag.kind.is_fork() && !wiring_early {
                tokio::spawn(wire_fork(
                    Weak::clone(&state),
                    Arc::clone(&registry),
                    key.clone(),
                    tag.clone(),
                    wired.take(),
                ));
            }
            // A single-use fork restored after a restart whose outcome the
            // engine recorded before the stop is done: retired once idle.
            // The record is of its own job: an earlier fork of the same
            // name (ended, its record kept for the epoch) is another job.
            let mut fork_job: Option<String> = None;
            // A helper of any kind whose retire was decided before the
            // last stop, and did not reach the engine, is done: retired now.
            if registry.retire_owed(&key) {
                registry.endings().retire_owed(&key);
            } else if tag.kind.is_fork() && tag.kind.life() == Some(super::HelperLife::Single) {
                let recorded = outcome_on_record(&handle, &key.member).await;
                tracing::debug!(helper = %key, recorded, "a single-use fork followed: its outcome on record");
                if recorded {
                    registry.endings().fork_outcome_recorded(&key, true);
                }
            }
            // Nothing else waits for an edge (dropped on any other path).
            drop(wired);
            if let Some(session) = &session {
                registry.bind_session(&key, &tag, session);
            }
            let mut retired = None;
            let mut seen = RunSeen::default();
            let follower = Follower {
                state: &state,
                registry: &registry,
                observer: &*observer,
                key: &key,
                tag: &tag,
            };
            loop {
                tokio::select! {
                    envelope = stream.next() => {
                        let Some(envelope) = envelope else { break };
                        follower.on_event(&mut seen, &envelope).await;
                    }
                    event = recv(&mut mob_events) => {
                        if let Some(job) = event.as_ref().and_then(|event| completed_job(&event.kind, &key.member)) {
                            // Its own job, read once it is needed (it is
                            // seated by then: its job ran).
                            if fork_job.is_none() {
                                fork_job = fork_job_of(&handle, &key.member).await;
                            }
                            if own_outcome(job, fork_job.as_deref()) {
                                registry.endings().fork_outcome_recorded(&key, false);
                            } else {
                                tracing::debug!(helper = %key, job, "an outcome of an earlier fork of this name: not this one's");
                            }
                        }
                        match event.map(|event| left(&event.kind, &key.member)) {
                        Some(Some(gone)) => {
                            // Deliver what it published before it left,
                            // each event as the loop above does (review 7,
                            // M9: only run ends were read here).
                            while let Some(Some(envelope)) = futures::FutureExt::now_or_never(stream.next()) {
                                follower.on_event(&mut seen, &envelope).await;
                            }
                            retired = Some(gone);
                            break;
                        }
                        Some(None) => {}
                        None => {
                            tracing::debug!(helper = %key, "the mob's events ended; following the helper's own stream to its end");
                            mob_events = None;
                        }
                        }
                    }
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
        observer.retired(&tag);
    } else {
        registry.finished(&key);
    }
}

/// `terminal` with the `steps` its run made, when it reached its limit.
/// What the follower keeps of the helper's current run.
#[derive(Debug, Default)]
struct RunSeen {
    /// The model calls of its current run (a checkpoint names them).
    steps: u64,
    /// Its parent's peer id, once a message of it needs it: a run whose
    /// last call messaged the parent tells the parent nothing more when it
    /// goes idle.
    parent_peer: Option<String>,
}

/// One helper as its follower reads its events.
struct Follower<'a> {
    state: &'a Weak<MobMcpState>,
    registry: &'a Arc<HelperRegistry>,
    observer: &'a dyn HelperWatch,
    key: &'a super::registry::HelperKey,
    tag: &'a HelperTag,
}

impl Follower<'_> {
    /// What one event of the helper means for the registry, its endings and
    /// the observer. The live loop and the drain when the helper leaves its
    /// mob both read every event through here.
    async fn on_event(
        &self,
        seen: &mut RunSeen,
        envelope: &meerkat_core::event::EventEnvelope<AgentEvent>,
    ) {
        let (registry, key, tag) = (self.registry, self.key, self.tag);
        if let EventSourceIdentity::Session { session_id } = &envelope.source {
            registry.bind_session(key, tag, session_id);
        }
        match &envelope.payload {
            AgentEvent::RunStarted { .. } => {
                seen.steps = 0;
                registry.ran(key, true);
                registry.endings().run_started(key);
            }
            AgentEvent::TurnCompleted { .. } => seen.steps += 1,
            AgentEvent::ToolCallRequested { name, args, .. } => {
                let to_parent = if name == "send_message" {
                    if seen.parent_peer.is_none() {
                        seen.parent_peer = peer_of_parent(self.state, registry, tag).await;
                    }
                    let peer = args
                        .as_value()
                        .get("peer_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                    peer.is_some() && peer == seen.parent_peer
                } else {
                    false
                };
                registry.endings().called(key, to_parent);
            }
            _ => {}
        }
        if let Some(terminal) = super::endings::Terminal::of(&envelope.payload) {
            registry.ran(key, false);
            registry
                .endings()
                .run_ended(key, counted(terminal, seen.steps));
        }
        observe(self.observer, tag, &envelope.payload).await;
    }
}

fn counted(terminal: super::endings::Terminal, steps: u64) -> super::endings::Terminal {
    match terminal {
        super::endings::Terminal::Limit { .. } => super::endings::Terminal::Limit { steps },
        other => other,
    }
}

/// The member that started the helper `tag`: the helper above it on its
/// path, else the agent its owner session runs as (`None`: neither is
/// seated here).
fn parent_of(registry: &HelperRegistry, tag: &HelperTag) -> Option<super::HelperKey> {
    if tag.parent_path().is_empty() {
        registry.root_of_session(&tag.owner)
    } else {
        registry
            .live_by_path(&tag.owner, tag.parent_path())
            .map(|(key, _)| key)
    }
}

/// How long a fork's wire waits for the fork to be seated in its mob's
/// roster: inside the bound its first calls wait for the edge
/// ([`crate::approval::admission::WIRE_WAIT`]).
const SEAT_WAIT: Duration = Duration::from_secs(12);
/// The first pause before looking at the roster again; each next doubles.
const SEAT_RECHECK_FIRST: Duration = Duration::from_millis(25);
/// The longest pause between two looks.
const SEAT_RECHECK_MAX: Duration = Duration::from_millis(500);

/// Wire the fork `key` to its parent ([`wire_to_parent`]), then open its
/// calls' latch (`wired`): when the edge is in place. When it cannot be
/// made the latch is dropped, which opens it too.
async fn wire_fork(
    state: Weak<MobMcpState>,
    registry: Arc<HelperRegistry>,
    key: super::HelperKey,
    tag: HelperTag,
    wired: Option<Arc<tokio::sync::watch::Sender<bool>>>,
) {
    let edge = wire_to_parent(&state, &registry, &key, &tag).await;
    if edge && let Some(wired) = wired {
        wired.send_replace(true);
    }
}

/// Wire the fork child `key` to its parent, as the daemon wires its agents
/// (the host's `mob_wire_members_batch`; an edge that exists stays), once
/// it is seated. The engine seats a fork with no edge, and nothing can be
/// set at its spawn (lukacf/meerkat#2232); the host can wire it after
/// (`agents::a_fork_and_its_parent_talk_both_ways_and_its_later_runs_are_told`).
/// Its spawn may still be building when the follower hears of it (its
/// stream opens before the roster lists it; a slow build took 1.6 s on a
/// hosted runner): the wire is made when the roster lists it, looked at on
/// its `MemberSpawned` and else after a short pause that doubles, and a
/// "member not found" is tried again, all within [`SEAT_WAIT`]. Whether
/// the edge is in place. A child seated in another mob than its parent's
/// cannot be wired (an edge is within one mob): logged, and it reaches no
/// one.
async fn wire_to_parent(
    state: &Weak<MobMcpState>,
    registry: &HelperRegistry,
    key: &super::HelperKey,
    tag: &HelperTag,
) -> bool {
    let Some(parent) = parent_of(registry, tag) else {
        tracing::warn!(helper = %key, "a fork's parent is not seated here; it is wired to no one");
        return false;
    };
    if parent.mob != key.mob {
        tracing::warn!(helper = %key, parent = %parent, "a fork seated outside its parent's mob is wired to no one");
        return false;
    }
    let handle = match mob_handle(state, &key.mob).await {
        Ok(handle) => handle,
        Err(err) => {
            tracing::warn!(helper = %key, error = %err, "cannot wire a fork to its parent: its mob is not reachable");
            return false;
        }
    };
    // Its seat shows on the mob's structural events.
    let mut events = handle.events().subscribe().await.ok();
    let deadline = tokio::time::Instant::now() + SEAT_WAIT;
    let mut pause = SEAT_RECHECK_FIRST;
    loop {
        if is_member(&handle, &key.member).await {
            let Some(state) = state.upgrade() else {
                return false;
            };
            match state
                .mob_wire_members_batch(&key.mob, vec![(parent.member.clone(), key.member.clone())])
                .await
            {
                Ok(report) => {
                    tracing::debug!(
                        helper = %key,
                        wired = report.wired.len(),
                        already = report.already_wired.len(),
                        "wired a fork to its parent"
                    );
                    return true;
                }
                // Listed, but not seated for the wire yet: again.
                Err(MobError::MemberNotFound(_)) => {}
                Err(err) => {
                    tracing::warn!(helper = %key, error = %err, "cannot wire a fork to its parent");
                    return false;
                }
            }
        }
        #[cfg(any(test, feature = "test-support"))]
        registry.fork_not_seated();
        let now = tokio::time::Instant::now();
        if now >= deadline {
            tracing::warn!(helper = %key, waited_secs = SEAT_WAIT.as_secs(), "cannot wire a fork to its parent: it was not seated in time");
            return false;
        }
        let _ =
            tokio::time::timeout(pause.min(deadline - now), seated(&mut events, &key.member)).await;
        pause = (pause * 2).min(SEAT_RECHECK_MAX);
    }
}

/// Wait for `identity`'s `MemberSpawned` on `events` (never, when they
/// are not followed or ended).
async fn seated(events: &mut Option<MobEventsSubscription>, identity: &AgentIdentity) {
    loop {
        match recv(events).await.map(|event| event.kind) {
            Some(MobEventKind::MemberSpawned(spawned)) if &spawned.agent_identity == identity => {
                return;
            }
            Some(_) => {}
            None => {
                *events = None;
                std::future::pending::<()>().await;
            }
        }
    }
}

/// The comms peer id of the member that started the helper `tag`: the
/// helper above it on its path, else the agent its owner session runs as
/// (`None`: neither is seated, or its endpoint cannot be read).
async fn peer_of_parent(
    state: &Weak<MobMcpState>,
    registry: &HelperRegistry,
    tag: &HelperTag,
) -> Option<String> {
    let parent = parent_of(registry, tag)?;
    let handle = state.upgrade()?.handle_for(&parent.mob).await.ok()?;
    let endpoint = handle.member_peer_endpoint(&parent.member).await.ok()??;
    Some(endpoint.descriptor.peer_id.to_string())
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

/// The job of the engine's record of a completed `fork_off` outcome that
/// `kind` is, when it names the member `identity` (`ForkJobTerminal`,
/// written before the outcome is handed to its parent, which a restart
/// hands over again from it). The record is the job's: a name used again
/// for a later fork is another job ([`own_outcome`]).
fn completed_job<'a>(kind: &'a MobEventKind, identity: &AgentIdentity) -> Option<&'a str> {
    match kind {
        MobEventKind::ForkJobTerminal(terminal)
            if &terminal.child == identity
                && terminal.status
                    == meerkat_core::event::BackgroundJobTerminalStatus::Completed =>
        {
            Some(terminal.job_id.as_str())
        }
        _ => None,
    }
}

/// Whether the completed outcome of `recorded` (a job, [`completed_job`]) is
/// the one the fork seated now runs (`own`, its roster entry's job): the
/// engine keys an outcome by its job and member, and a name an ended fork
/// had may be given to a new fork, whose job is another.
fn own_outcome(recorded: &str, own: Option<&str>) -> bool {
    own == Some(recorded)
}

/// The `fork_off` job the member `identity` of `handle`'s mob was seated to
/// run (its roster entry's record; `None`: no fork, or not listed).
async fn fork_job_of(handle: &MobHandle, identity: &AgentIdentity) -> Option<String> {
    handle
        .roster()
        .await
        .get(identity)
        .and_then(|entry| entry.fork_job.as_ref())
        .map(|job| job.job_id.clone())
}

/// Whether `handle`'s mob holds the completed `fork_off` outcome of the job
/// the fork `identity` seated now runs (its `ForkJobTerminal` of the
/// current epoch; an earlier fork's of the same name is not its).
async fn outcome_on_record(handle: &MobHandle, identity: &AgentIdentity) -> bool {
    let Some(job) = fork_job_of(handle, identity).await else {
        return false;
    };
    match handle.fork_job_terminal(&job, identity).await {
        Ok(terminal) => terminal.is_some_and(|terminal| {
            terminal.status == meerkat_core::event::BackgroundJobTerminalStatus::Completed
        }),
        Err(err) => {
            tracing::debug!(helper = %identity, error = %err, "cannot read the mob's fork outcomes");
            false
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use meerkat_core::event::BackgroundJobTerminalStatus;

    fn terminal(job: &str, child: &str, status: BackgroundJobTerminalStatus) -> MobEventKind {
        let outcome = serde_json::json!({"text": "done"});
        MobEventKind::ForkJobTerminal(meerkat_mob::event::ForkJobTerminalEvent {
            job_id: job.to_string(),
            child: AgentIdentity::from(child),
            owner_session_id: SessionId::new(),
            retained_work: None,
            status,
            result_digest: meerkat_mob::event::detached_outcome_digest(&outcome),
            outcome,
        })
    }

    struct Watch(parking_lot::Mutex<usize>);

    #[async_trait::async_trait]
    impl HelperWatch for Watch {
        async fn turn(&self, _tag: &HelperTag, _usage: &TurnUsage) {}
        fn run_ended(&self, _session: &SessionId) {
            *self.0.lock() += 1;
        }
        fn mob_hosts(&self, _mob: &MobId) {}
        fn retired(&self, _tag: &HelperTag) {}
    }

    /// Review 7, M9: an event read while draining a helper that left its
    /// mob means what it means in the live loop: a run's start marks it
    /// running and a run's end idle again (the drain read run ends only, so
    /// a start drained there was never counted), and the observer hears
    /// the end.
    #[tokio::test]
    async fn every_event_a_follower_reads_is_handled_alike() {
        use meerkat_core::types::{ContentInput, RunInput, TranscriptMessageIdentity};
        let registry = Arc::new(HelperRegistry::in_memory());
        let key =
            super::super::registry::HelperKey::new(MobId::from("m"), AgentIdentity::from("x"));
        let tag = HelperTag {
            owner: SessionId::new(),
            path: vec!["x".into()],
            model: "m".into(),
            kind: crate::mobs::HelperKind::Other,
        };
        let watch = Watch(parking_lot::Mutex::new(0));
        let state = Weak::new();
        let follower = Follower {
            state: &state,
            registry: &registry,
            observer: &watch,
            key: &key,
            tag: &tag,
        };
        let mut seen = RunSeen::default();
        let event = |payload| meerkat_core::event::EventEnvelope::new("s", 0, None, payload);
        follower
            .on_event(
                &mut seen,
                &event(AgentEvent::RunStarted {
                    session_id: SessionId::new(),
                    identity: TranscriptMessageIdentity::default(),
                    input: RunInput::Content {
                        content: ContentInput::Text("go".into()),
                    },
                    request_reasoning: None,
                    reasoning_effort: None,
                }),
            )
            .await;
        assert!(matches!(registry.activity(&key), Some((true, _))));
        follower
            .on_event(
                &mut seen,
                &event(AgentEvent::RunCompleted {
                    session_id: SessionId::new(),
                    identity: TranscriptMessageIdentity::default(),
                    result: "ok".into(),
                    structured_output: None,
                    extraction_required: false,
                    usage: Default::default(),
                    terminal_cause_kind: None,
                    assistant_message_id: None,
                }),
            )
            .await;
        assert!(matches!(registry.activity(&key), Some((false, _))));
        assert_eq!(*watch.0.lock(), 1);
    }

    /// A fork's outcome is its job's: an earlier fork's record under the
    /// same name is not the fork's seated now, and only a completed one
    /// counts.
    #[test]
    fn an_outcome_is_the_fork_s_own_only_by_its_job() {
        let x = AgentIdentity::from("x");
        let first = terminal("job-1", "x", BackgroundJobTerminalStatus::Completed);
        assert_eq!(completed_job(&first, &x), Some("job-1"));
        assert!(own_outcome("job-1", Some("job-1")));
        // The second `x` runs `job-2`: the first's record is not its own.
        assert!(!own_outcome("job-1", Some("job-2")));
        // A member whose job is not known yet takes no record for its own.
        assert!(!own_outcome("job-1", None));
        // Another member's, or a failed one, is no completed outcome of `x`.
        let other = terminal("job-1", "y", BackgroundJobTerminalStatus::Completed);
        assert_eq!(completed_job(&other, &x), None);
        let failed = terminal("job-1", "x", BackgroundJobTerminalStatus::Failed);
        assert_eq!(completed_job(&failed, &x), None);
    }
}
