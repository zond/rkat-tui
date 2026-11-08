//! The approval gate on the daemon side (P3 of `docs/mob-split.md`).
//!
//! The daemon runs the same [`ApprovalEngine`] the in-process TUI does
//! (tiers, rules, classifier, permission modes, escalation timeouts, the
//! ledger), built by the shared bootstrap. Three things differ, all here:
//!
//! - [`RootGate`]: the orchestrator's `ToolDispatchAdmission`. It is the
//!   in-process [`ApprovalGate`], resolved per call to the orchestrator's
//!   session (unknown until its first spawn: the helper registry knows it).
//! - [`Desk`]: where an escalation goes. In-process the broker hands it to
//!   the runner and the UI answers on a channel; here it waits for the
//!   escalation's record in the realm's approval store to be decided over
//!   the engine's `approval/decide` (the record is the one the ledger
//!   already writes for every escalation) and hands the engine the
//!   [`crate::approval::escalation::HumanDecision`] it carries
//!   ([`crate::approval::remote::human_decision`]). The engine keeps its
//!   own timeout and serialisation; a record decided by a client is
//!   confirmed, not decided twice ([`crate::approval::ledger::EscalationLedger::settle`]).
//! - [`DaemonToolOrigins`]: where a session's tools come from. The engine's
//!   published tool definitions are not reachable through the RPC
//!   runtime's public API (only the command-based read, which answers
//!   nothing while a turn runs), so the catalog is read when the session is
//!   idle (at start and after every run) and cached; until the first read
//!   the TUI's own file and note tools are known and everything else is
//!   judged as a foreign tool (the classifier or the human decides it).
//!
//! [`watch`] follows the orchestrator's events: a run start lifts a Stop
//! (as the in-process runner reports `RunStarted`), a run end refreshes
//! the tool catalog.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use meerkat_core::types::ToolCallView;
use meerkat_core::{
    AgentEvent, AgentToolDispatcher, ApprovalService, ApprovalStatus, LiveBridgeEffectKind,
    LiveBridgeEffectOutcome, SessionId, ToolDispatchAdmission, ToolDispatchContext, ToolError,
};
use meerkat_mob::{AgentIdentity, MobId, MobSessionService};
use meerkat_mob_mcp::MobMcpState;
use parking_lot::{Mutex, RwLock};
use tokio::sync::{mpsc, oneshot};

use crate::approval::admission::denial_error;
use crate::approval::catalog::{ToolCatalog, ToolOrigin, ToolOrigins};
use crate::approval::escalation::{EscalationRequest, HumanDecision};
use crate::approval::remote::human_decision;
use crate::approval::{ApprovalEngine, ApprovalGate, DenyReason};
use crate::mobs::{HelperKey, HelperRegistry};

/// How often a waiting escalation's record is read.
pub const DECISION_POLL: Duration = Duration::from_millis(100);

/// How long one read of a session's tool definitions may take.
const CATALOG_READ: Duration = Duration::from_secs(30);

/// How long the event watcher waits before subscribing again.
const RESUBSCRIBE: Duration = Duration::from_secs(1);

/// The orchestrator's approval gate: the in-process gate, for the session
/// the orchestrator runs as.
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

/// Serves the engine's escalations through the realm's approval store.
pub struct Desk {
    service: ApprovalService,
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
    /// A desk over `service` (the runtime's, which `approval/*` serves)
    /// answering the escalations `requests` delivers, until the engine
    /// drops its broker. Returns the desk and its task.
    #[must_use]
    pub fn spawn(
        service: ApprovalService,
        mut requests: mpsc::Receiver<EscalationRequest>,
    ) -> (Arc<Self>, tokio::task::JoinHandle<()>) {
        let desk = Arc::new(Self {
            service,
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
    /// answered with Stop (the engine denies them and stops the session,
    /// as the in-process runner does).
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
    pub fn waiting(&self) -> usize {
        self.waiting.lock().len()
    }

    /// Wait for `request`'s record to be decided (or the engine to give up
    /// on it: its timeout, an abandoned call) and answer it.
    async fn wait(self: Arc<Self>, request: EscalationRequest) {
        let EscalationRequest {
            prompt,
            mut respond,
        } = request;
        let Some(record) = prompt.record.clone() else {
            // No record nobody can decide: fail closed.
            let _ = respond.send(HumanDecision::Deny { comment: None });
            return;
        };
        let (stop_tx, mut stop_rx) = oneshot::channel();
        self.waiting.lock().insert(
            prompt.id,
            Waiting {
                session: prompt.session_id.clone(),
                stop: Some(stop_tx),
            },
        );
        let shown = prompt.recorded();
        let mut tick = tokio::time::interval(DECISION_POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let decision = loop {
            tokio::select! {
                () = respond.closed() => break None,
                stopped = &mut stop_rx => {
                    if stopped.is_ok() {
                        break Some(HumanDecision::Stop);
                    }
                }
                _ = tick.tick() => {}
            }
            match self.service.get(&record) {
                Ok(current) if current.status != ApprovalStatus::Pending => {
                    break human_decision(&current, &shown);
                }
                Ok(_) => {}
                Err(meerkat_core::ApprovalError::NotFound { .. }) => {
                    break Some(HumanDecision::Deny { comment: None });
                }
                Err(err) => tracing::debug!(error = %err, "cannot read an escalation record"),
            }
        };
        self.waiting.lock().remove(&prompt.id);
        if let Some(decision) = decision {
            let _ = respond.send(decision);
        }
    }
}

/// [`ToolOrigins`] for the daemon's sessions: catalogs read while their
/// session is idle, the TUI's own tools before that.
pub struct DaemonToolOrigins {
    service: Arc<dyn MobSessionService>,
    cache: RwLock<HashMap<SessionId, ToolCatalog>>,
    known: ToolCatalog,
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
            known: ToolCatalog::from_defs(&defs),
        }
    }

    /// Read `session`'s tool definitions (it must be idle to answer) and
    /// cache them. Returns whether the read succeeded.
    pub async fn refresh(&self, session: &SessionId) -> bool {
        let read =
            tokio::time::timeout(CATALOG_READ, self.service.live_visible_tool_defs(session)).await;
        match read {
            Ok(Ok(Some(defs))) => {
                self.cache
                    .write()
                    .insert(session.clone(), ToolCatalog::from_defs(&defs));
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
    pub fn origin(&self, session: &SessionId, name: &str) -> Option<ToolOrigin> {
        self.cache
            .read()
            .get(session)
            .unwrap_or(&self.known)
            .get(name)
            .cloned()
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
                .unwrap_or_else(|| self.known.clone()),
        )
    }
}

/// Follow the orchestrator's events until the task is aborted: every run
/// start goes to the engine (it lifts a Stop given to an earlier run),
/// every run end refreshes the session's tool catalog.
pub fn watch(
    state: Arc<MobMcpState>,
    mob: MobId,
    identity: AgentIdentity,
    engine: Arc<ApprovalEngine>,
    origins: Arc<DaemonToolOrigins>,
) -> tokio::task::JoinHandle<()> {
    use futures::StreamExt as _;
    tokio::spawn(async move {
        loop {
            let subscribed = match state.handle_for(&mob).await {
                Ok(handle) => handle.subscribe_agent_events(&identity).await,
                Err(err) => Err(err),
            };
            let mut stream = match subscribed {
                Ok(stream) => stream,
                Err(err) => {
                    tracing::debug!(error = %err, "cannot follow the orchestrator's events yet");
                    tokio::time::sleep(RESUBSCRIBE).await;
                    continue;
                }
            };
            while let Some(envelope) = stream.next().await {
                match &envelope.payload {
                    AgentEvent::RunStarted {
                        session_id,
                        identity,
                        ..
                    } => engine.run_started(session_id, identity.run_id.as_ref()),
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::approval::escalation::EscalationPrompt;
    use crate::approval::remote::RemoteAnswer;
    use meerkat_core::{
        ApprovalActionKind, ApprovalDecision, ApprovalOwnerRef, ApprovalPrincipalId,
        ApprovalProposedAction, ApprovalRequest, ApprovalResourceId, ApprovalResourceKind,
        ApprovalResourceRef, ApprovalRisk, SurfaceMetadata,
    };

    fn record(service: &ApprovalService, session: &SessionId) -> meerkat_core::ApprovalId {
        service
            .request(ApprovalRequest {
                requester: ApprovalPrincipalId::new(crate::approval::ledger::REQUESTER).unwrap(),
                owner: ApprovalOwnerRef::Session {
                    session_id: session.clone(),
                },
                resource: ApprovalResourceRef {
                    kind: ApprovalResourceKind::ToolCall,
                    id: ApprovalResourceId::new("call"),
                },
                proposed_action: ApprovalProposedAction {
                    kind: ApprovalActionKind::ToolCall,
                    summary: "Run x".into(),
                    body: None,
                },
                risk: ApprovalRisk::Medium,
                request_body: None,
                allowed_decisions: [ApprovalDecision::Approve, ApprovalDecision::Deny]
                    .into_iter()
                    .collect(),
                expires_at: None,
                metadata: SurfaceMetadata::default(),
                request_provenance: None,
            })
            .unwrap()
            .approval_id
    }

    fn prompt(
        id: u64,
        session: &SessionId,
        record: Option<meerkat_core::ApprovalId>,
    ) -> EscalationPrompt {
        EscalationPrompt {
            id,
            session_id: session.clone(),
            agent: None,
            tool: "shell".into(),
            args_preview: "ls".into(),
            subjects: vec!["ls".into()],
            classifier_hint: None,
            policy_reason: None,
            record,
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

    #[tokio::test]
    async fn the_desk_hands_the_engine_what_the_record_was_decided() {
        let service = ApprovalService::new();
        let (tx, rx) = mpsc::channel(4);
        let (desk, _task) = Desk::spawn(service.clone(), rx);
        let session = SessionId::new();
        let id = record(&service, &session);
        let answer = ask(&tx, prompt(1, &session, Some(id.clone()))).await;
        service
            .decide(
                &id,
                ApprovalDecision::Deny,
                ApprovalPrincipalId::new("someone").unwrap(),
                Some("not that".into()),
                Some(RemoteAnswer::Deny.provenance(Some("not that"))),
            )
            .unwrap();
        assert_eq!(
            answer.await.unwrap(),
            HumanDecision::Deny {
                comment: Some("not that".into())
            }
        );
        // No record: denied at once (fail closed).
        let answer = ask(&tx, prompt(2, &session, None)).await;
        assert_eq!(answer.await.unwrap(), HumanDecision::Deny { comment: None });
        assert_eq!(desk.waiting(), 0);
    }

    #[tokio::test]
    async fn an_interrupt_stops_the_sessions_waiting_escalations_only() {
        let service = ApprovalService::new();
        let (tx, rx) = mpsc::channel(4);
        let (desk, _task) = Desk::spawn(service.clone(), rx);
        let (mine, other) = (SessionId::new(), SessionId::new());
        let stopped = ask(&tx, prompt(1, &mine, Some(record(&service, &mine)))).await;
        let kept_id = record(&service, &other);
        let kept = ask(&tx, prompt(2, &other, Some(kept_id.clone()))).await;
        while desk.waiting() < 2 {
            tokio::task::yield_now().await;
        }
        desk.stop(&mine);
        assert_eq!(stopped.await.unwrap(), HumanDecision::Stop);
        assert_eq!(desk.waiting(), 1);
        service
            .decide(
                &kept_id,
                ApprovalDecision::Approve,
                ApprovalPrincipalId::new("someone").unwrap(),
                None,
                None,
            )
            .unwrap();
        assert_eq!(kept.await.unwrap(), HumanDecision::AllowOnce);
    }

    #[tokio::test]
    async fn an_escalation_the_engine_gave_up_on_stops_waiting() {
        let service = ApprovalService::new();
        let (tx, rx) = mpsc::channel(4);
        let (desk, _task) = Desk::spawn(service.clone(), rx);
        let session = SessionId::new();
        let answer = ask(&tx, prompt(1, &session, Some(record(&service, &session)))).await;
        while desk.waiting() < 1 {
            tokio::task::yield_now().await;
        }
        // The engine's timeout dropped the responder.
        drop(answer);
        tokio::time::timeout(Duration::from_secs(5), async {
            while desk.waiting() > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the desk stopped waiting");
    }
}
