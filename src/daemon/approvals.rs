//! The approval gate on the daemon side.
//!
//! The daemon runs the [`ApprovalEngine`] (tiers, rules, classifier,
//! permission modes, escalation timeouts), built by the shared bootstrap;
//! the durable decisions are the record of its escalations. Three parts
//! are the daemon's own, all here:
//!
//! - [`RootGate`]: the agent's `ToolDispatchAdmission`. It is the
//!   shared [`ApprovalGate`], resolved per call to the agent's
//!   session (unknown until its first spawn: the helper registry knows it).
//! - [`Desk`]: where an escalation goes. The engine's broker hands it to
//!   the desk, which holds a durable
//!   decision ([`super::decisions`]), which every attached client is shown
//!   as it opens and decides with `decision/decide`, and hands the engine
//!   the [`crate::approval::escalation::HumanDecision`] the decision
//!   carries ([`crate::approval::desk::answered`]). The engine keeps its
//!   own timeout and serialisation.
//! - [`DaemonToolOrigins`]: where a session's tools come from. The engine's
//!   published tool definitions are not reachable through the RPC
//!   runtime's public API (only the command-based read, which answers
//!   nothing while a turn runs), so the catalog is read when the session is
//!   idle (at start and after every run) and cached. A session not read
//!   yet (a helper, which runs from its spawn on and so is never idle
//!   before its first call; a fresh conversation) is judged by rkat-tui's
//!   own file and note tools plus the engine-native tools of the catalogs
//!   read so far: every session of the daemon is built by the same factory,
//!   so its native `shell` is the agent's. Everything else is judged
//!   as a foreign tool (the classifier or the human decides it).
//!
//! [`watch`] follows the agent's events: a run start lifts a Stop,
//! a model call is counted ([`super::usage`]), a run end refreshes the
//! tool catalog, and the helper of a `delegate` call that ended without its
//! result is retired ([`crate::mobs::delegates`]). [`HelperEvents`] does the same for the helpers, from the
//! follower's one subscription to each ([`crate::mobs::follow`]): their
//! model calls are counted to their owner's conversation, and a helper's
//! catalog is read after its first run; a helper that stays seated
//! (`spawn_member`) is judged by its own catalog from its second run on,
//! one retired after its run (`delegate`) never is.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use meerkat_core::approval::decision::DecisionPhase;
use meerkat_core::types::ToolCallView;
use meerkat_core::{
    AgentEvent, AgentToolDispatcher, LiveBridgeEffectKind, LiveBridgeEffectOutcome, SessionId,
    ToolDispatchAdmission, ToolDispatchContext, ToolError,
};
use meerkat_mob::{AgentIdentity, MobId, MobSessionService};
use meerkat_mob_mcp::MobMcpState;
use parking_lot::{Mutex, RwLock};
use tokio::sync::{mpsc, oneshot};

use crate::approval::admission::denial_error;
use crate::approval::catalog::{ToolCatalog, ToolOrigins};
use crate::approval::desk::{EscalationAnswer, answered};
use crate::approval::escalation::{EscalationRequest, HumanDecision};
use crate::approval::{ApprovalEngine, ApprovalGate, DenyReason};
use crate::mobs::{HelperKey, HelperRegistry};

use super::decisions::{Decisions, Settled};

/// How long one read of a session's tool definitions may take.
const CATALOG_READ: Duration = Duration::from_secs(30);

/// How long the event watcher waits before subscribing again after its
/// subscription failed or its stream ended (a new seat resubscribes at
/// once).
const RESUBSCRIBE: Duration = Duration::from_secs(1);

/// The agent's approval gate: the shared [`ApprovalGate`], for the
/// session the agent runs as.
pub struct RootGate {
    engine: Arc<ApprovalEngine>,
    registry: Arc<HelperRegistry>,
    key: HelperKey,
}

impl std::fmt::Debug for RootGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RootGate")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl RootGate {
    /// The gate of the root member `key` (its session from `registry`).
    #[must_use]
    pub fn new(engine: Arc<ApprovalEngine>, registry: Arc<HelperRegistry>, key: HelperKey) -> Self {
        Self {
            engine,
            registry,
            key,
        }
    }

    fn gate(&self, context: Option<&ToolDispatchContext>) -> Option<ApprovalGate> {
        let session = context
            .and_then(ToolDispatchContext::origin_session_id)
            .cloned()
            .or_else(|| self.registry.root_session(&self.key))?;
        Some(ApprovalGate::new(Arc::clone(&self.engine), session))
    }
}

#[async_trait]
impl ToolDispatchAdmission for RootGate {
    async fn await_dispatch_admission(
        &self,
        call: ToolCallView<'_>,
        context: Option<&ToolDispatchContext>,
        effect_kind: LiveBridgeEffectKind,
    ) -> Result<(), ToolError> {
        match self.gate(context) {
            Some(gate) => {
                gate.await_dispatch_admission(call, context, effect_kind)
                    .await
            }
            // Fail closed: a call nobody can attribute is not run.
            None => Err(denial_error(&DenyReason::ClassifierUnavailable(
                "the agent's session is not known to the approval gate yet".to_string(),
            ))),
        }
    }

    async fn record_dispatch_outcome(
        &self,
        _call: ToolCallView<'_>,
        _context: Option<&ToolDispatchContext>,
        _effect_kind: LiveBridgeEffectKind,
        _outcome: LiveBridgeEffectOutcome,
    ) -> Result<(), ToolError> {
        Ok(())
    }
}

/// One escalation the desk waits on.
struct Waiting {
    session: SessionId,
    stop: Option<oneshot::Sender<()>>,
}

/// How a held decision ended for the desk.
enum Ending {
    /// A client decided it.
    Decided(Settled),
    /// The human interrupted the session's turn.
    Stopped,
    /// The engine gave up on the escalation (its timeout, an abandoned
    /// call).
    Abandoned,
}

/// Serves the engine's escalations as durable decisions
/// ([`super::decisions`]).
pub struct Desk {
    decisions: Arc<Decisions>,
    /// The engine's escalation timeout (each decision's deadline).
    timeout: Duration,
    waiting: Mutex<HashMap<u64, Waiting>>,
    /// The engine told who answered each escalation (the audit log).
    engine: Mutex<std::sync::Weak<ApprovalEngine>>,
}

impl std::fmt::Debug for Desk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Desk")
            .field("waiting", &self.waiting.lock().len())
            .finish_non_exhaustive()
    }
}

impl Desk {
    /// A desk holding a decision through `decisions` for each escalation
    /// `requests` delivers (open for `timeout`, the engine's escalation
    /// timeout), until the engine drops its broker. Returns the desk and
    /// its task.
    #[must_use]
    pub fn spawn(
        decisions: Arc<Decisions>,
        timeout: Duration,
        mut requests: mpsc::Receiver<EscalationRequest>,
    ) -> (Arc<Self>, tokio::task::JoinHandle<()>) {
        let desk = Arc::new(Self {
            decisions,
            timeout,
            waiting: Mutex::new(HashMap::new()),
            engine: Mutex::new(std::sync::Weak::new()),
        });
        let task = tokio::spawn({
            let desk = Arc::clone(&desk);
            async move {
                while let Some(request) = requests.recv().await {
                    tokio::spawn(Arc::clone(&desk).wait(request));
                }
            }
        });
        (desk, task)
    }

    /// Tell `engine` who answers each escalation from now on (held
    /// weakly: the engine owns the broker this desk serves).
    pub fn report_to(&self, engine: &Arc<ApprovalEngine>) {
        *self.engine.lock() = Arc::downgrade(engine);
    }

    /// The human interrupted `session`'s turn: its open escalations are
    /// answered with Stop (the engine denies them and stops the session).
    pub fn stop(&self, session: &SessionId) {
        for waiting in self.waiting.lock().values_mut() {
            if &waiting.session == session
                && let Some(stop) = waiting.stop.take()
            {
                let _ = stop.send(());
            }
        }
    }

    /// How many escalations wait for a decision.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn waiting(&self) -> usize {
        self.waiting.lock().len()
    }

    /// Hold `request`'s decision and wait for a client to decide it (or
    /// the engine to give up on it: its timeout, an abandoned call; or the
    /// human to interrupt), then answer it and tell the clients shown it
    /// that it ended.
    async fn wait(self: Arc<Self>, request: EscalationRequest) {
        let EscalationRequest {
            prompt,
            mut respond,
        } = request;
        let hold = match self.decisions.hold_request(&prompt, self.timeout) {
            Ok(hold) => hold,
            Err(err) => {
                // Nothing a client could decide: fail closed.
                tracing::warn!(error = %err, "an escalation could not be held as a decision; denied");
                let _ = respond.send(HumanDecision::Deny { comment: None });
                return;
            }
        };
        let (settled_tx, mut settled_rx) = oneshot::channel();
        let id = match self.decisions.hold(hold, settled_tx).await {
            Ok(id) => id,
            Err(err) => {
                tracing::warn!(error = %err, "an escalation's decision was not held; denied");
                let _ = respond.send(HumanDecision::Deny { comment: None });
                return;
            }
        };
        let (stop_tx, mut stop_rx) = oneshot::channel();
        self.waiting.lock().insert(
            prompt.id,
            Waiting {
                session: prompt.session_id.clone(),
                stop: Some(stop_tx),
            },
        );
        let mut stoppable = true;
        let ending = loop {
            tokio::select! {
                () = respond.closed() => break Ending::Abandoned,
                stopped = &mut stop_rx, if stoppable => {
                    if stopped.is_ok() {
                        break Ending::Stopped;
                    }
                    stoppable = false;
                }
                settled = &mut settled_rx => match settled {
                    Ok(settled) => break Ending::Decided(settled),
                    // The waiter was dropped without a decision: withdrawn.
                    Err(_) => break Ending::Abandoned,
                },
            }
        };
        self.waiting.lock().remove(&prompt.id);
        match ending {
            Ending::Decided(settled) => {
                let phase = if settled.approve {
                    DecisionPhase::Approved
                } else {
                    DecisionPhase::Rejected
                };
                self.decisions.ended(&id, Some(phase)).await;
                if let (Some(by), Some(engine)) = (settled.by, self.engine.lock().upgrade()) {
                    engine.answered_by(prompt.id, by);
                }
                let answer = EscalationAnswer::from_decision(
                    settled.approve,
                    settled.answer,
                    settled.reason.as_deref(),
                );
                let (decision, no_rule) = answered(answer, &prompt.shown);
                if let Some(err) = no_rule {
                    tracing::warn!(error = %err, "allow rule not added; allowing once");
                }
                let _ = respond.send(decision);
            }
            Ending::Stopped => {
                let phase = self.decisions.withdraw(&id).await;
                self.decisions.ended(&id, phase).await;
                let _ = respond.send(HumanDecision::Stop);
            }
            Ending::Abandoned => {
                let phase = self.decisions.withdraw(&id).await;
                self.decisions.ended(&id, phase).await;
            }
        }
    }
}

/// How many sessions' tool catalogs are kept; past it the one read
/// longest ago goes (its session is judged by the known tools until its
/// catalog is read again, at its next run end).
pub const MAX_CATALOGS: usize = 256;

/// [`ToolOrigins`] for the daemon's sessions: catalogs read while their
/// session is idle, rkat-tui's own tools before that.
pub struct DaemonToolOrigins {
    service: Arc<dyn MobSessionService>,
    /// By session, the one read longest ago first (at most
    /// [`MAX_CATALOGS`]; a forgotten session's goes).
    cache: RwLock<indexmap::IndexMap<SessionId, ToolCatalog>>,
    /// The definitions' sizes by family each session's model requests
    /// carry, as last read (`/context`; as many as [`MAX_CATALOGS`]).
    sizes: RwLock<indexmap::IndexMap<SessionId, Vec<super::context::FamilySize>>>,
    /// rkat-tui's own tools, and the engine-native tools of every catalog
    /// read: what a session not read yet is judged by.
    known: RwLock<ToolCatalog>,
    /// The deferrable tools an agent may reveal mid-run: judged as what
    /// they are before the catalog read at the run's end names them.
    deferrable: Arc<super::deferral::Deferrable>,
}

impl std::fmt::Debug for DaemonToolOrigins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonToolOrigins")
            .field("cached", &self.cache.read().len())
            .finish_non_exhaustive()
    }
}

impl DaemonToolOrigins {
    /// Origins over `service`; `own` are rkat-tui's tools every session of
    /// the daemon is given (their definitions name their source).
    /// `deferrable` are the tools an agent may reveal mid-run.
    #[must_use]
    pub fn new(
        service: Arc<dyn MobSessionService>,
        own: &[Arc<dyn AgentToolDispatcher>],
        deferrable: Arc<super::deferral::Deferrable>,
    ) -> Self {
        let defs: Vec<_> = own
            .iter()
            .flat_map(|dispatcher| {
                dispatcher
                    .tools()
                    .iter()
                    .map(|def| (**def).clone())
                    .collect::<Vec<_>>()
            })
            .collect();
        Self {
            service,
            cache: RwLock::new(indexmap::IndexMap::new()),
            sizes: RwLock::new(indexmap::IndexMap::new()),
            known: RwLock::new(ToolCatalog::from_defs(&defs)),
            deferrable,
        }
    }

    /// Read `session`'s tool definitions (it must be idle to answer) and
    /// cache them. Returns whether the read succeeded.
    pub async fn refresh(&self, session: &SessionId) -> bool {
        let read =
            tokio::time::timeout(CATALOG_READ, self.service.live_visible_tool_defs(session)).await;
        match read {
            Ok(Ok(Some(defs))) => {
                crate::tools::keep_newest(
                    &mut self.sizes.write(),
                    session.clone(),
                    super::context::family_sizes(&defs),
                    MAX_CATALOGS,
                );
                let mut catalog = ToolCatalog::from_defs(&defs);
                // The tools kept behind the deferred catalog, which the
                // visible definitions leave out until they are loaded.
                if let Ok(Ok(Some(view))) =
                    tokio::time::timeout(CATALOG_READ, self.service.tool_catalog_view(session))
                        .await
                {
                    catalog.add_deferred(&view);
                }
                self.known.write().add_missing_natives(&catalog);
                self.keep(session.clone(), catalog);
                true
            }
            Ok(Ok(None)) => false,
            Ok(Err(err)) => {
                tracing::debug!(error = %err, %session, "tool definitions unavailable");
                false
            }
            Err(_) => {
                tracing::debug!(%session, "tool definitions not read in time");
                false
            }
        }
    }

    /// Cache `catalog` as `session`'s, newest last, dropping the oldest
    /// past [`MAX_CATALOGS`].
    fn keep(&self, session: SessionId, catalog: ToolCatalog) {
        crate::tools::keep_newest(&mut self.cache.write(), session, catalog, MAX_CATALOGS);
    }

    /// `session` is gone: its catalog goes with it.
    pub fn forget(&self, session: &SessionId) {
        self.cache.write().shift_remove(session);
        self.sizes.write().shift_remove(session);
    }

    /// The tool definitions `session`'s model requests carry, by family,
    /// as last read (`None`: not read yet).
    #[must_use]
    pub fn sizes(&self, session: &SessionId) -> Option<Vec<super::context::FamilySize>> {
        self.sizes.read().get(session).cloned()
    }

    /// Whether `session`'s catalog was read.
    #[must_use]
    pub fn has(&self, session: &SessionId) -> bool {
        self.cache.read().contains_key(session)
    }

    /// How `session`'s next model request projects its tool catalog
    /// (tests: which tools are deferred).
    #[cfg(any(test, feature = "test-support"))]
    pub async fn view(&self, session: &SessionId) -> Option<meerkat_core::ToolCatalogView> {
        self.service.tool_catalog_view(session).await.ok().flatten()
    }

    /// The origin of `name` for `session` (diagnostics and tests).
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn origin(
        &self,
        session: &SessionId,
        name: &str,
    ) -> Option<crate::approval::catalog::ToolOrigin> {
        let known = match self.cache.read().get(session) {
            Some(catalog) => catalog.get(name).cloned(),
            None => self.known.read().get(name).cloned(),
        };
        known.or_else(|| {
            let mut deferrable = ToolCatalog::default();
            self.deferrable.add_to(&mut deferrable);
            deferrable.get(name).cloned()
        })
    }
}

#[async_trait]
impl ToolOrigins for DaemonToolOrigins {
    async fn catalog(&self, session_id: &SessionId) -> Option<ToolCatalog> {
        let mut catalog = self
            .cache
            .read()
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| self.known.read().clone());
        // A tool revealed in this run is in no catalog read yet.
        self.deferrable.add_to(&mut catalog);
        Some(catalog)
    }
}

/// What [`watch`] follows an agent with.
pub struct Watched {
    /// The mob host.
    pub state: Arc<MobMcpState>,
    /// The agent's mob.
    pub mob: MobId,
    /// The agent.
    pub identity: AgentIdentity,
    /// The approval engine (a run start lifts a Stop).
    pub engine: Arc<ApprovalEngine>,
    /// The tool catalogs (read at a run's end).
    pub origins: Arc<DaemonToolOrigins>,
    /// The agents' conversations (which one the agent runs as).
    pub current: Arc<super::conversations::Conversations>,
    /// Where its model calls are counted.
    pub ledger: Arc<super::usage::Ledger>,
    /// The live agents.
    pub agents: std::sync::Weak<super::agents::Agents>,
    /// When the agent is idle (told each run's start and end).
    pub activity: Arc<super::activity::Activity>,
    /// Set when the daemon stops: the watcher counts the events its
    /// stream already holds, then ends.
    pub draining: tokio::sync::watch::Receiver<bool>,
}

/// Follow an agent's events until the task is aborted: every run start
/// goes to the engine (it lifts a Stop given to an earlier run) and makes
/// the agent the most recently active one (`agents`), every run end
/// refreshes the session's tool catalog, every model call and compaction
/// is counted ([`super::usage`]), each run's start and end tell
/// [`super::activity`] when the agent is idle, and a `delegate` call that
/// ends without its helper's result has the helper retired. When the agent is seated
/// again (a conversation switch, a reseat:
/// [`super::conversations::Conversations::seatings`]) it subscribes again,
/// from the new incarnation's first event: the member's old stream does
/// not end by itself, and the new one's events were seen by nobody yet. A
/// stream that ends otherwise is resumed after the last event read.
pub fn watch(watched: Watched) -> tokio::task::JoinHandle<()> {
    let watcher = Watcher {
        name: watched.identity.to_string(),
        seatings: watched.current.seatings(),
        read_to: None,
        watched,
    };
    tokio::spawn(watcher.run())
}

/// Where a watcher read an agent's stream to.
#[derive(Debug, Clone, Copy)]
struct Position {
    /// The agent's incarnation ([`super::conversations::Conversations::seat_of`]).
    seat: u64,
    /// The stream's sequence space (`None`: unknown, or refused).
    epoch: Option<meerkat_core::comms::SessionEventEpoch>,
    /// The last event read.
    seq: u64,
}

/// How following one subscription ended.
enum Ended {
    /// The daemon stops.
    Stop,
    /// The agent was seated again: subscribe to the new incarnation now.
    Reseated,
    /// The stream ended: resume it after a pause.
    Closed,
}

/// What one subscription's events carry over from one to the next.
#[derive(Default)]
struct Following {
    /// The session subscribed to (an event that names none is its).
    session: Option<SessionId>,
    /// The run the events belong to now.
    run: Option<meerkat_core::RunId>,
    /// A compaction's history estimate, until it completes.
    compacting: Option<u64>,
    /// The agent's open `delegate` calls: a helper whose call ends
    /// without its result is retired.
    delegates: crate::mobs::delegates::DelegateCalls,
}

/// One agent's event watcher ([`watch`]).
struct Watcher {
    name: String,
    watched: Watched,
    seatings: tokio::sync::watch::Receiver<u64>,
    /// Where the stream was read to.
    read_to: Option<Position>,
}

impl Watcher {
    async fn run(mut self) {
        loop {
            if *self.watched.draining.borrow() {
                return;
            }
            let seat = self.watched.current.seat_of(&self.name);
            let Some(subscription) = self.subscribe(seat).await else {
                tokio::time::sleep(RESUBSCRIBE).await;
                continue;
            };
            match self.follow(subscription, seat).await {
                Ended::Stop => return,
                Ended::Reseated => {}
                Ended::Closed => tokio::time::sleep(RESUBSCRIBE).await,
            }
        }
    }

    /// Where to read incarnation `seat` from: a new incarnation (a switch,
    /// a reseat) from its first event, the same one again from where it
    /// was left.
    fn cursor(&self, seat: u64) -> meerkat_core::comms::SessionEventCursor {
        use meerkat_core::comms::SessionEventCursor;
        match self.read_to {
            Some(Position {
                seat: seen,
                epoch: Some(epoch),
                seq,
            }) if seen == seat => SessionEventCursor::After { epoch, seq },
            Some(Position {
                seat: seen,
                epoch: None,
                ..
            }) if seen == seat => SessionEventCursor::Live,
            _ => SessionEventCursor::Earliest,
        }
    }

    /// Subscribe to incarnation `seat`'s events; `None` (logged) when the
    /// stream cannot be opened yet (a cursor it refuses is not tried
    /// again).
    async fn subscribe(&mut self, seat: u64) -> Option<meerkat_mob::AgentEventSubscription> {
        let cursor = self.cursor(seat);
        if matches!(cursor, meerkat_core::comms::SessionEventCursor::Earliest) {
            // A new incarnation: the old one's open runs never end.
            self.watched.activity.changed(&self.name, true);
        }
        let Watched {
            state,
            mob,
            identity,
            ..
        } = &self.watched;
        let subscribed = match state.handle_for(mob).await {
            Ok(handle) => handle.subscribe_agent_events_from(identity, cursor).await,
            Err(err) => Err(err),
        };
        match subscribed {
            Ok(subscription) => Some(subscription),
            Err(err) => {
                tracing::debug!(error = %err, agent = %self.name, "cannot follow the agent's events yet");
                if let Some(position) = &mut self.read_to
                    && position.seat == seat
                {
                    position.epoch = None;
                }
                None
            }
        }
    }

    /// Read `subscription` (incarnation `seat`) until it ends, the agent
    /// is seated again or the daemon stops.
    async fn follow(
        &mut self,
        subscription: meerkat_mob::AgentEventSubscription,
        seat: u64,
    ) -> Ended {
        use futures::StreamExt as _;
        let epoch = subscription.epoch;
        let attached = subscription.attached_epoch.clone();
        let mut stream = subscription.stream;
        let mut following = Following {
            session: self.watched.current.current(&self.name),
            ..Following::default()
        };
        loop {
            if *self.watched.draining.borrow() {
                Self::drain(&mut stream, &following, &self.watched).await;
                return Ended::Stop;
            }
            let envelope = tokio::select! {
                envelope = stream.next() => match envelope {
                    Some(envelope) => envelope,
                    None => return Ended::Closed,
                },
                changed = self.watched.draining.changed() => {
                    if changed.is_err() {
                        return Ended::Stop;
                    }
                    continue;
                }
                changed = self.seatings.changed() => {
                    if changed.is_err() {
                        return Ended::Stop;
                    }
                    // The member was seated again: its old stream does not
                    // end by itself.
                    if self.watched.current.seat_of(&self.name) != seat {
                        return Ended::Reseated;
                    }
                    continue;
                }
            };
            self.read_to = Some(Position {
                seat,
                epoch: epoch.or_else(|| {
                    attached
                        .as_ref()
                        .and_then(|attached| attached.get().copied().flatten())
                }),
                seq: envelope.seq,
            });
            self.on_event(&envelope, &mut following).await;
        }
    }

    /// Stopping: count the model calls the stream holds already, without
    /// waiting for more; the ledger is flushed after every watcher ended.
    async fn drain(
        stream: &mut meerkat_core::EventStream,
        following: &Following,
        watched: &Watched,
    ) {
        use futures::{FutureExt as _, StreamExt as _};
        while let Some(Some(envelope)) = stream.next().now_or_never() {
            if let AgentEvent::TurnCompleted {
                usage: Some(usage), ..
            } = &envelope.payload
                && let Some(session) = event_session(&envelope.source, following.session.as_ref())
            {
                let model = model_of(usage, &watched.current, &session);
                watched.ledger.turn(&session, &model, usage).await;
            }
        }
    }

    /// What one event of the agent means for the daemon.
    async fn on_event(
        &self,
        envelope: &meerkat_core::event::EventEnvelope<AgentEvent>,
        following: &mut Following,
    ) {
        let session = || event_session(&envelope.source, following.session.as_ref());
        for orphan in following
            .delegates
            .observe(session().as_ref(), &envelope.payload)
        {
            let state = Arc::clone(&self.watched.state);
            tokio::spawn(async move {
                crate::mobs::delegates::retire_orphan(&state, &orphan).await;
            });
        }
        match &envelope.payload {
            AgentEvent::RunStarted {
                session_id,
                identity,
                input,
                ..
            } => {
                following.run.clone_from(&identity.run_id);
                self.run_started(session_id, identity.run_id.as_ref(), input.prompt_text())
                    .await;
            }
            AgentEvent::ToolCallRequested {
                name: tool, args, ..
            } => {
                if let Some(agents) = self.watched.agents.upgrade() {
                    agents.tool_requested(&self.name, tool, args.as_value());
                }
            }
            AgentEvent::ToolExecutionCompleted {
                name,
                is_error: false,
                ..
            } if name == SCHEDULE_DELETE => {
                // The agent deleted one of its schedules: its usage record
                // goes (the event names no schedule; the service's list
                // says which are gone).
                if let Some(agents) = self.watched.agents.upgrade() {
                    let schedules = Arc::clone(agents.schedules());
                    tokio::spawn(async move { schedules.reconcile().await });
                }
            }
            AgentEvent::TurnCompleted {
                usage: Some(usage), ..
            } => {
                if let Some(session) = session() {
                    self.model_call(&session, following.run.as_ref(), usage)
                        .await;
                }
            }
            AgentEvent::CompactionStarted {
                estimated_history_tokens,
                ..
            } => following.compacting = Some(*estimated_history_tokens),
            AgentEvent::CompactionCompleted {
                summary_tokens,
                estimated_history_tokens_after,
                summary_source,
                ..
            } => {
                let estimate = following.compacting.take();
                if let Some(session) = session() {
                    // Only a provider call costs anything; its input is
                    // the history estimate, its output the summary.
                    let summarized = summary_source
                        .as_ref()
                        .is_none_or(|source| {
                            matches!(
                                source,
                                meerkat_core::CompactionSummarySource::ProviderCall { .. }
                            )
                        })
                        .then_some(super::usage::Summarized {
                            input: estimate.unwrap_or(0),
                            output: *summary_tokens,
                        });
                    let model = self.watched.current.model_of(&session).unwrap_or_default();
                    self.watched
                        .ledger
                        .compaction(
                            &session,
                            &model,
                            summarized,
                            estimated_history_tokens_after.unwrap_or(*summary_tokens),
                        )
                        .await;
                }
            }
            AgentEvent::RunCompleted {
                session_id,
                identity,
                ..
            }
            | AgentEvent::RunFailed {
                session_id,
                identity,
                ..
            } => self.run_ended(session_id, identity.run_id.as_ref()),
            _ => {}
        }
    }

    /// A run started: the engine lifts a Stop given to an earlier run,
    /// the agent is busy and the most recently active one, and a run no
    /// client's input started (a schedule's, a watch's) is attributed.
    async fn run_started(
        &self,
        session: &SessionId,
        run: Option<&meerkat_core::RunId>,
        prompt: Option<String>,
    ) {
        let Watched {
            engine,
            activity,
            agents,
            ..
        } = &self.watched;
        engine.run_started(session, run);
        activity.run_started(&self.name, run);
        let Some(agents) = agents.upgrade() else {
            return;
        };
        agents.touch(&self.name);
        if let (Some(run), Some(prompt)) = (run, prompt)
            && !engine.human_run(session, run)
        {
            agents
                .unprompted_run(&self.name, session, run, &prompt)
                .await;
        }
    }

    /// A model call of `session` in `run`: counted to the conversation, and
    /// to the schedule that fired the run.
    async fn model_call(
        &self,
        session: &SessionId,
        run: Option<&meerkat_core::RunId>,
        usage: &meerkat_core::TurnUsage,
    ) {
        let Watched {
            current,
            ledger,
            agents,
            ..
        } = &self.watched;
        let model = model_of(usage, current, session);
        ledger.turn(session, &model, usage).await;
        let fired = run.and_then(|run| {
            agents
                .upgrade()
                .and_then(|agents| agents.schedules().schedule_of(run))
        });
        if let Some(schedule) = fired {
            ledger.scheduled(&schedule, &model, usage).await;
        }
    }

    /// A run ended: the agent may be idle, its turn's watches and the
    /// run's schedule bookkeeping end, the counts are written behind and
    /// the session's tool catalog is read again.
    fn run_ended(&self, session: &SessionId, run: Option<&meerkat_core::RunId>) {
        let Watched {
            activity,
            agents,
            ledger,
            origins,
            ..
        } = &self.watched;
        activity.run_ended(&self.name, run);
        if let Some(agents) = agents.upgrade() {
            agents.run_ended(&self.name);
            if let Some(run) = run {
                agents.schedules().run_ended(run);
            }
        }
        ledger.persist();
        let origins = Arc::clone(origins);
        let session = session.clone();
        tokio::spawn(async move {
            origins.refresh(&session).await;
        });
    }
}

/// The engine's tool that deletes a schedule.
const SCHEDULE_DELETE: &str = "meerkat_schedule_delete";

/// The session an event of an agent belongs to: its source's, else the
/// session the watcher subscribed to.
fn event_session(
    source: &meerkat_core::EventSourceIdentity,
    following: Option<&SessionId>,
) -> Option<SessionId> {
    match source {
        meerkat_core::EventSourceIdentity::Session { session_id } => Some(session_id.clone()),
        _ => following.cloned(),
    }
}

/// The model a call's usage counts for: the one the provider accounted it
/// to, else the session's.
fn model_of(
    usage: &meerkat_core::TurnUsage,
    current: &super::conversations::Conversations,
    session: &SessionId,
) -> String {
    let accounted = usage.accounting().model.trim();
    if accounted.is_empty() {
        current.model_of(session).unwrap_or_default()
    } else {
        accounted.to_string()
    }
}

/// Is a catalog read (true when `session`'s catalog was read).
type HasCatalog = Box<dyn Fn(&SessionId) -> bool + Send + Sync>;
/// Reads `session`'s catalog, recording it on success.
type ReadCatalog = Box<dyn Fn(SessionId) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// Read a helper's tool catalog the first time its session is idle: a run
/// end of a session whose catalog is not read yet reads it (the read waits
/// behind a run still closing). A helper that leaves its mob after its one
/// run (`delegate`) is gone before the read answers; it stays judged by
/// the known tools. One read per session at a time; a session leaves the
/// in-flight set when its read ends, so a read that failed (the session
/// was busy again, the helper's next turn already running) is tried again
/// at its next run end, and the set holds only reads still running.
pub struct CatalogReads {
    has: HasCatalog,
    read: ReadCatalog,
    reading: Arc<Mutex<std::collections::HashSet<SessionId>>>,
}

impl CatalogReads {
    /// Reads over `has` and `read`.
    #[must_use]
    pub fn new(has: HasCatalog, read: ReadCatalog) -> Self {
        Self {
            has,
            read,
            reading: Arc::default(),
        }
    }

    /// Over `origins`.
    #[must_use]
    pub fn of(origins: &Arc<DaemonToolOrigins>) -> Self {
        let has = Arc::clone(origins);
        let read = Arc::clone(origins);
        Self::new(
            Box::new(move |session| has.has(session)),
            Box::new(move |session| {
                let origins = Arc::clone(&read);
                Box::pin(async move {
                    if !origins.refresh(&session).await {
                        tracing::debug!(%session, "a helper's tool catalog was not readable after its run; the next run end tries again");
                    }
                })
            }),
        )
    }

    /// A run of `session` ended.
    pub fn run_ended(&self, session: &SessionId) {
        if (self.has)(session) || !self.reading.lock().insert(session.clone()) {
            return;
        }
        let read = (self.read)(session.clone());
        let reading = Arc::clone(&self.reading);
        let session = session.clone();
        tokio::spawn(async move {
            read.await;
            reading.lock().remove(&session);
        });
    }
}

/// What the daemon does with its helpers' events
/// ([`crate::mobs::follow::HelperWatch`]): each model call is counted to
/// the conversation the helper works for, under the helper's label and
/// the model the provider accounted it to (else the helper's); a run's
/// end reads its catalog ([`CatalogReads`]) and writes the counts.
pub struct HelperEvents {
    catalogs: CatalogReads,
    ledger: Arc<super::usage::Ledger>,
    /// Tells the clients a mob hosts helpers or was destroyed.
    announce: Box<dyn Fn(&MobId, super::protocol::MobChange) + Send + Sync>,
    /// The mobs announced as hosting helpers (each once, until destroyed).
    hosting: Mutex<std::collections::HashSet<MobId>>,
}

impl HelperEvents {
    /// Over `catalogs` and `ledger`; `announce` tells the clients of mobs.
    #[must_use]
    pub fn new(
        catalogs: CatalogReads,
        ledger: Arc<super::usage::Ledger>,
        announce: impl Fn(&MobId, super::protocol::MobChange) + Send + Sync + 'static,
    ) -> Self {
        Self {
            catalogs,
            ledger,
            announce: Box::new(announce),
            hosting: Mutex::default(),
        }
    }

    /// `mob` was destroyed: the clients are told, and a mob made again
    /// under its id is announced again.
    pub fn mob_destroyed(&self, mob: &MobId) {
        if self.hosting.lock().remove(mob) {
            (self.announce)(mob, super::protocol::MobChange::Destroyed);
        }
    }
}

#[async_trait]
impl crate::mobs::follow::HelperWatch for HelperEvents {
    async fn turn(&self, tag: &crate::mobs::HelperTag, usage: &meerkat_core::TurnUsage) {
        let accounted = usage.accounting().model.trim();
        let model = if accounted.is_empty() {
            tag.model.as_str()
        } else {
            accounted
        };
        self.ledger
            .helper_turn(&tag.owner, tag.label(), model, usage)
            .await;
    }

    fn run_ended(&self, session: &SessionId) {
        self.catalogs.run_ended(session);
        self.ledger.persist();
    }

    fn mob_hosts(&self, mob: &MobId) {
        if self.hosting.lock().insert(mob.clone()) {
            (self.announce)(mob, super::protocol::MobChange::Hosts);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::approval::desk::RemoteAnswer;
    use crate::approval::escalation::{EscalationPrompt, RecordedPrompt};

    /// A helper whose catalog could not be read after one run (it was
    /// busy again) is read again after a later run end, and a read session
    /// leaves the in-flight set.
    #[tokio::test]
    async fn a_failed_helper_catalog_read_is_tried_again_at_the_next_run_end() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let session = SessionId::new();
        let read = Arc::new(Mutex::new(std::collections::HashSet::<SessionId>::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let reads = CatalogReads::new(
            {
                let read = Arc::clone(&read);
                Box::new(move |s: &SessionId| read.lock().contains(s))
            },
            {
                let read = Arc::clone(&read);
                let calls = Arc::clone(&calls);
                Box::new(move |s: SessionId| {
                    let read = Arc::clone(&read);
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async move {
                        // The first read finds the session busy.
                        if n > 0 {
                            read.lock().insert(s);
                        }
                    })
                })
            },
        );
        for _ in 0..200 {
            if read.lock().contains(&session) {
                break;
            }
            reads.run_ended(&session);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(read.lock().contains(&session), "the catalog was read again");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // Once read, later run ends read nothing.
        reads.run_ended(&session);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    fn desk() -> (Arc<Desk>, Arc<Decisions>, mpsc::Sender<EscalationRequest>) {
        use meerkat_core::approval::decision::InMemoryDecisionStore;
        let service = meerkat_runtime::decision::DurableDecisionService::new(Arc::new(
            InMemoryDecisionStore::new(),
        ));
        let decisions = Decisions::new(
            service,
            super::super::decisions::Principal::new("alice").unwrap(),
            None,
        )
        .unwrap();
        let (tx, rx) = mpsc::channel(4);
        let (desk, _task) = Desk::spawn(Arc::clone(&decisions), Duration::from_mins(1), rx);
        (desk, decisions, tx)
    }

    fn prompt(id: u64, session: &SessionId) -> EscalationPrompt {
        EscalationPrompt {
            id,
            session_id: session.clone(),
            shown: RecordedPrompt {
                tool: "shell".into(),
                args_preview: "ls".into(),
                subjects: vec!["ls".into()],
                ..RecordedPrompt::default()
            },
        }
    }

    async fn ask(
        tx: &mpsc::Sender<EscalationRequest>,
        prompt: EscalationPrompt,
    ) -> oneshot::Receiver<HumanDecision> {
        let (respond, answer) = oneshot::channel();
        tx.send(EscalationRequest { prompt, respond })
            .await
            .unwrap();
        answer
    }

    /// The open decisions of the desk's hub.
    fn open(decisions: &Decisions) -> Vec<meerkat_core::approval::decision::StoredDecision> {
        decisions
            .service()
            .list(
                &meerkat_core::approval::decision::DecisionListFilter::default()
                    .with_phase(DecisionPhase::Open),
                None,
                100,
            )
            .unwrap()
            .decisions
    }

    async fn until(mut done: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("in time");
    }

    #[tokio::test]
    async fn the_desk_hands_the_engine_what_the_decision_was_decided() {
        let (desk, decisions, tx) = desk();
        let session = SessionId::new();
        let answer = ask(&tx, prompt(1, &session)).await;
        until(|| open(&decisions).len() == 1).await;
        let id = open(&decisions)[0].decision_id.clone();
        decisions.settled(
            &id,
            Settled {
                approve: false,
                answer: Some(RemoteAnswer::Deny),
                reason: Some("not that".into()),
                by: None,
            },
        );
        assert_eq!(
            answer.await.unwrap(),
            HumanDecision::Deny {
                comment: Some("not that".into())
            }
        );
        let first = id;
        let answer = ask(&tx, prompt(2, &session)).await;
        // The first stays open in the store: only the hub heard it settled.
        until(|| open(&decisions).len() == 2).await;
        let id = open(&decisions)
            .into_iter()
            .map(|row| row.decision_id)
            .find(|id| *id != first)
            .unwrap();
        decisions.settled(
            &id,
            Settled {
                approve: true,
                answer: Some(RemoteAnswer::AllowAndAddRule),
                reason: None,
                by: None,
            },
        );
        assert!(matches!(
            answer.await.unwrap(),
            HumanDecision::AllowAndAddRule(_)
        ));
        until(|| desk.waiting() == 0).await;
    }

    #[tokio::test]
    async fn an_interrupt_stops_the_sessions_waiting_escalations_only() {
        let (desk, decisions, tx) = desk();
        let (mine, other) = (SessionId::new(), SessionId::new());
        let stopped = ask(&tx, prompt(1, &mine)).await;
        let kept = ask(&tx, prompt(2, &other)).await;
        until(|| desk.waiting() == 2).await;
        desk.stop(&mine);
        assert_eq!(stopped.await.unwrap(), HumanDecision::Stop);
        assert_eq!(desk.waiting(), 1);
        // The stopped one is withdrawn; the other stays open.
        until(|| open(&decisions).len() == 1).await;
        let id = open(&decisions)[0].decision_id.clone();
        decisions.settled(
            &id,
            Settled {
                approve: true,
                answer: None,
                reason: None,
                by: None,
            },
        );
        assert_eq!(kept.await.unwrap(), HumanDecision::AllowOnce);
    }

    #[tokio::test]
    async fn an_escalation_the_engine_gave_up_on_is_withdrawn() {
        let (desk, decisions, tx) = desk();
        let session = SessionId::new();
        let answer = ask(&tx, prompt(1, &session)).await;
        until(|| desk.waiting() == 1).await;
        // The engine's timeout dropped the responder.
        drop(answer);
        until(|| desk.waiting() == 0).await;
        until(|| open(&decisions).is_empty()).await;
    }
}
