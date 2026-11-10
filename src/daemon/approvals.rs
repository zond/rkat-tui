//! The approval gate on the daemon side (P3 of `docs/mob-split.md`).
//!
//! The daemon runs the [`ApprovalEngine`] (tiers, rules, classifier,
//! permission modes, escalation timeouts), built by the shared bootstrap;
//! the durable decisions are the record of its escalations. Three parts
//! are the daemon's own, all here:
//!
//! - [`RootGate`]: the orchestrator's `ToolDispatchAdmission`. It is the
//!   shared [`ApprovalGate`], resolved per call to the orchestrator's
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
//!   before its first call; a fresh conversation) is judged by the TUI's
//!   own file and note tools plus the engine-native tools of the catalogs
//!   read so far: every session of the daemon is built by the same factory,
//!   so its native `shell` is the orchestrator's. Everything else is judged
//!   as a foreign tool (the classifier or the human decides it).
//!
//! [`watch`] follows the orchestrator's events: a run start lifts a Stop,
//! a run end refreshes
//! the tool catalog. [`watch_helpers`] reads a helper's catalog after its
//! first run, from the helper events the follower forwards; a helper that
//! stays seated (`spawn_member`) is judged by its own catalog from its
//! second run on, one retired after its run (`delegate`) never is.

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

/// How long the event watcher waits before subscribing again.
const RESUBSCRIBE: Duration = Duration::from_secs(1);

/// The orchestrator's approval gate: the shared [`ApprovalGate`], for the
/// session the orchestrator runs as.
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
                "the orchestrator's session is not known to the approval gate yet".to_string(),
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

/// [`ToolOrigins`] for the daemon's sessions: catalogs read while their
/// session is idle, the TUI's own tools before that.
pub struct DaemonToolOrigins {
    service: Arc<dyn MobSessionService>,
    cache: RwLock<HashMap<SessionId, ToolCatalog>>,
    /// The TUI's own tools, and the engine-native tools of every catalog
    /// read: what a session not read yet is judged by.
    known: RwLock<ToolCatalog>,
}

impl std::fmt::Debug for DaemonToolOrigins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonToolOrigins")
            .field("cached", &self.cache.read().len())
            .finish_non_exhaustive()
    }
}

impl DaemonToolOrigins {
    /// Origins over `service`; `own` are the TUI's tools every session of
    /// the daemon is given (their definitions name their source).
    #[must_use]
    pub fn new(service: Arc<dyn MobSessionService>, own: &[Arc<dyn AgentToolDispatcher>]) -> Self {
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
            cache: RwLock::new(HashMap::new()),
            known: RwLock::new(ToolCatalog::from_defs(&defs)),
        }
    }

    /// Read `session`'s tool definitions (it must be idle to answer) and
    /// cache them. Returns whether the read succeeded.
    pub async fn refresh(&self, session: &SessionId) -> bool {
        let read =
            tokio::time::timeout(CATALOG_READ, self.service.live_visible_tool_defs(session)).await;
        match read {
            Ok(Ok(Some(defs))) => {
                let catalog = ToolCatalog::from_defs(&defs);
                self.known.write().add_missing_natives(&catalog);
                self.cache.write().insert(session.clone(), catalog);
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

    /// Whether `session`'s catalog was read.
    #[must_use]
    pub fn has(&self, session: &SessionId) -> bool {
        self.cache.read().contains_key(session)
    }

    /// The origin of `name` for `session` (diagnostics and tests).
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn origin(
        &self,
        session: &SessionId,
        name: &str,
    ) -> Option<crate::approval::catalog::ToolOrigin> {
        match self.cache.read().get(session) {
            Some(catalog) => catalog.get(name).cloned(),
            None => self.known.read().get(name).cloned(),
        }
    }
}

#[async_trait]
impl ToolOrigins for DaemonToolOrigins {
    async fn catalog(&self, session_id: &SessionId) -> Option<ToolCatalog> {
        Some(
            self.cache
                .read()
                .get(session_id)
                .cloned()
                .unwrap_or_else(|| self.known.read().clone()),
        )
    }
}

/// Follow an agent's events until the task is aborted: every run start
/// goes to the engine (it lifts a Stop given to an earlier run) and makes
/// the agent the most recently active one (`agents`), every run end
/// refreshes the session's tool catalog. After a conversation switch
/// (`current` names another session) it subscribes again: the member's old
/// stream does not end by itself.
pub fn watch(
    state: Arc<MobMcpState>,
    mob: MobId,
    identity: AgentIdentity,
    engine: Arc<ApprovalEngine>,
    origins: Arc<DaemonToolOrigins>,
    current: Arc<super::conversations::Conversations>,
    agents: std::sync::Weak<super::agents::Agents>,
) -> tokio::task::JoinHandle<()> {
    use futures::StreamExt as _;
    tokio::spawn(async move {
        let name = identity.to_string();
        loop {
            let following = current.current(&name);
            let subscribed = match state.handle_for(&mob).await {
                Ok(handle) => handle.subscribe_agent_events(&identity).await,
                Err(err) => Err(err),
            };
            let mut stream = match subscribed {
                Ok(stream) => stream,
                Err(err) => {
                    tracing::debug!(error = %err, agent = %name, "cannot follow the agent's events yet");
                    tokio::time::sleep(RESUBSCRIBE).await;
                    continue;
                }
            };
            let mut check = tokio::time::interval(RESUBSCRIBE);
            check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                let envelope = tokio::select! {
                    envelope = stream.next() => match envelope {
                        Some(envelope) => envelope,
                        None => break,
                    },
                    _ = check.tick() => {
                        if current.current(&name) != following {
                            break;
                        }
                        continue;
                    }
                };
                match &envelope.payload {
                    AgentEvent::RunStarted {
                        session_id,
                        identity,
                        ..
                    } => {
                        engine.run_started(session_id, identity.run_id.as_ref());
                        if let Some(agents) = agents.upgrade() {
                            agents.touch(&name);
                        }
                    }
                    AgentEvent::RunCompleted { session_id, .. }
                    | AgentEvent::RunFailed { session_id, .. } => {
                        let origins = Arc::clone(&origins);
                        let session = session_id.clone();
                        tokio::spawn(async move {
                            origins.refresh(&session).await;
                        });
                    }
                    _ => {}
                }
            }
            tokio::time::sleep(RESUBSCRIBE).await;
        }
    })
}

/// Read a helper's tool catalog the first time its session is idle: every
/// helper event the follower forwards (`updates`, the helper registry's
/// channel) is looked at, and a run end of a session whose catalog is not
/// read yet reads it (the read waits behind a run still closing). A helper
/// that leaves its mob after its one run (`delegate`) is gone before the
/// read answers; it stays judged by the known tools. Runs until `updates`
/// closes.
pub fn watch_helpers(
    updates: tokio::sync::mpsc::UnboundedReceiver<crate::mobs::HelperUpdate>,
    origins: Arc<DaemonToolOrigins>,
) -> tokio::task::JoinHandle<()> {
    let has = {
        let origins = Arc::clone(&origins);
        move |session: &SessionId| origins.has(session)
    };
    let refresh = move |session: SessionId| {
        let origins = Arc::clone(&origins);
        async move {
            let read = origins.refresh(&session).await;
            if !read {
                tracing::debug!(%session, "a helper's tool catalog was not readable after its run; the next run end tries again");
            }
        }
    };
    tokio::spawn(follow_helper_runs(updates, has, refresh))
}

/// [`watch_helpers`]'s loop over `has` (the catalog was read) and
/// `refresh` (read it, recording it on success). One read per session at a
/// time; a session leaves the in-flight set when its read ends, so a read
/// that failed (the session was busy again, the helper's next turn
/// already running) is tried again at its next run end, and the set holds
/// only reads still running.
pub async fn follow_helper_runs<H, R, F>(
    mut updates: tokio::sync::mpsc::UnboundedReceiver<crate::mobs::HelperUpdate>,
    has: H,
    refresh: R,
) where
    H: Fn(&SessionId) -> bool,
    R: Fn(SessionId) -> F,
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let reading = Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new()));
    while let Some(update) = updates.recv().await {
        let crate::mobs::HelperUpdate::Event(scoped) = update;
        let (AgentEvent::RunCompleted { session_id, .. }
        | AgentEvent::RunFailed { session_id, .. }) = &scoped.event
        else {
            continue;
        };
        if has(session_id) || !reading.lock().insert(session_id.clone()) {
            continue;
        }
        let read = refresh(session_id.clone());
        let reading = Arc::clone(&reading);
        let session = session_id.clone();
        tokio::spawn(async move {
            read.await;
            reading.lock().remove(&session);
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::approval::desk::RemoteAnswer;
    use crate::approval::escalation::{EscalationPrompt, RecordedPrompt};

    fn run_completed(session: &SessionId) -> crate::mobs::HelperUpdate {
        let usage = serde_json::to_value(meerkat_core::CumulativeUsage::default()).unwrap();
        let event: AgentEvent = serde_json::from_value(serde_json::json!({
            "type": "run_completed",
            "session_id": session,
            "result": "done",
            "usage": usage,
        }))
        .unwrap();
        crate::mobs::HelperUpdate::Event(meerkat_core::ScopedAgentEvent::primary(
            session.to_string(),
            event,
        ))
    }

    /// A helper whose catalog could not be read after one run (it was
    /// busy again) is read again after a later run end, and a read session
    /// leaves the in-flight set.
    #[tokio::test]
    async fn a_failed_helper_catalog_read_is_tried_again_at_the_next_run_end() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let session = SessionId::new();
        let read = Arc::new(Mutex::new(std::collections::HashSet::<SessionId>::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(follow_helper_runs(
            rx,
            {
                let read = Arc::clone(&read);
                move |s: &SessionId| read.lock().contains(s)
            },
            {
                let read = Arc::clone(&read);
                let calls = Arc::clone(&calls);
                move |s: SessionId| {
                    let read = Arc::clone(&read);
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        // The first read finds the session busy.
                        if n > 0 {
                            read.lock().insert(s);
                        }
                    }
                }
            },
        ));
        for _ in 0..200 {
            if read.lock().contains(&session) {
                break;
            }
            tx.send(run_completed(&session)).unwrap();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(read.lock().contains(&session), "the catalog was read again");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // Once read, later run ends read nothing.
        tx.send(run_completed(&session)).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        drop(tx);
        task.await.unwrap();
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
