//! Consent through the engine's durable decisions.
//!
//! When the approval engine needs the human, the daemon's desk
//! ([`super::approvals::Desk`]) holds one durable decision through the
//! runtime's decision owner (`SessionRuntime::decision_service()`): kind
//! [`KIND`], the escalation as its subject, the dialog's content as its
//! rendering ([`DialogRendering`], JSON a client renders as the approval
//! dialog), the gate as its requester and the daemon's [`Principal`] as its
//! approver, until the engine's escalation timeout (and [`DEADLINE_SLACK`]).
//! [`Decisions`] is the daemon's side of the consent surface:
//!
//! - **Push.** It subscribes to the owner's openings when it is made, before
//!   any client lists anything; each decision that opens is offered to every
//!   attached client authorized for it ([`authorized`]: with one principal
//!   per daemon, every client) as a `decision/notice` carrying the whole
//!   rendering, and the presentation is recorded (to the approver, on the
//!   client's channel, under the notice's id) once the notice is written to
//!   the client's socket. A client that attaches later is offered the open
//!   decisions then (the catch-up); a subscription that fell behind
//!   ([`DecisionOpening::Missed`]) offers the open decisions to every client.
//!   Each client is offered a decision once (the dedup is per connection).
//! - **Answer.** A client's `decision/decide` never reaches the router as
//!   it is: the proxy hands it to [`Decisions::decide`], which parses it as a
//!   closed request (a field the engine's request does not have, such as an
//!   `actor` or an `approver`, is refused; the one extension is the dialog's
//!   [`RemoteAnswer`] tag, stripped before dispatch), builds an
//!   `AuthenticatedDecisionEvent` whose approver is the daemon's fixed
//!   principal (bound to the attach token by the host's configuration,
//!   `[daemon] principal` in the user's `tui.toml`, never anything the
//!   client names) and calls `MethodRouter::dispatch_with_decision_event`.
//!   The event is eligible only for a decision presented on that connection
//!   while it is open. The first valid decision wins; a later one gets the
//!   engine's own `not_open` refusal.
//! - **Ending.** The desk ends every decision it held (decided, withdrawn
//!   when the engine gave up or the human interrupted) and the clients that
//!   were shown it are told with an `rkat/decision_ended` notification, so
//!   the dialogs of the clients that did not answer close.
//!
//! A decision's waiting dispatch lives in this process: a decision a
//! previous daemon left open is withdrawn at start
//! ([`Decisions::withdraw_leftovers`]). The decisions are the daemon's only
//! record of its escalations: the approval engine keeps none of its own.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use meerkat_contracts::{
    WireDecisionNotice, WireDecisionNoticeEvent, WireDecisionNoticePresentation,
};
use meerkat_core::approval::ApprovalAssurance;
use meerkat_core::approval::consent::{
    AuthenticatedApprover, ConsentEligibility, ConsentIneligible, HostAuthenticationScope,
};
use meerkat_core::approval::decision::{
    ApproverId, AttestedDecision, AuthenticatedDecisionEvent, DECISION_PAGE_MAX,
    DecisionContractError, DecisionHoldRequest, DecisionId, DecisionIngress, DecisionKind,
    DecisionListFilter, DecisionOperationId, DecisionPhase, DecisionPrincipalKey,
    DecisionReference, DecisionRenderer, DecisionRendering, DecisionRequestBody, DecisionStage,
    DecisionSubjectKey, PresentationAudience, PresentationRecord, Sha256Digest, StoredDecision,
    canonical_parts,
};
use meerkat_core::{PrincipalKind, PrincipalRef, SessionId, TrustDomainId};
use meerkat_rpc::protocol::RpcRequest;
use meerkat_rpc::router::MethodRouter;
use meerkat_runtime::decision::wire::{wire_canonical_part, wire_decision_phase};
use meerkat_runtime::decision::{
    DecisionClock, DecisionClockError, DecisionError, DecisionOpening, DecisionOpenings,
    DecisionWithdrawer, DurableDecisionService, SystemDecisionClock,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::protocol::{self, DecideParams, DecisionEnded, INTERNAL_ERROR, INVALID_PARAMS};
use super::serve::Outgoing;
use crate::approval::desk::RemoteAnswer;
use crate::approval::escalation::{EscalationPrompt, RecordedPrompt};
use meerkat_rpc::protocol::RpcId;

/// The kind of every decision the daemon's gate holds.
pub const KIND: &str = "rkat-tui.tool-call";

/// The renderer token of [`DialogRendering`].
pub const RENDERER: &str = "rkat-tui.dialog-v1";

/// How a client's decision reaches the host.
pub const INGRESS: &str = "rkat-tui-mob";

/// The trust domain of the daemon's principals.
pub const TRUST_DOMAIN: &str = "rkat-tui.local";

/// The requester of every decision: rkat-tui's approval gate.
pub const GATE: &str = "rkat-tui-approval-gate";

/// The notification that tells a client a decision it was shown ended.
pub const ENDED: &str = protocol::DECISION_ENDED;

/// The notification that offers a client a decision.
pub const NOTICE: &str = protocol::DECISION_NOTICE;

/// The method a client decides with.
pub const DECIDE: &str = protocol::DECISION_DECIDE;

/// How long a decision outlives the engine's escalation timeout: the engine
/// gives up first (and the desk withdraws the decision), so a decision
/// never expires under a dispatch still waiting for it.
pub const DEADLINE_SLACK: Duration = Duration::from_secs(5);

/// How long a notice may take to reach a client's socket before the
/// presentation is given up.
const NOTICE_WRITE: Duration = Duration::from_secs(5);

/// How often one call tries the store again while it is busy with another
/// (a SQLite decision store never waits for another writer), and the wait
/// before the first retry (each later one waits once more as long).
const STORE_RETRIES: u32 = 5;
const STORE_BACKOFF: Duration = Duration::from_millis(20);

/// The default principal name when `[daemon] principal` names none and the
/// environment has no login name.
pub const DEFAULT_PRINCIPAL: &str = "owner";

/// What a decision's rendering holds: the dialog a client shows,
/// and the session the call belongs to (a helper's call belongs to the
/// session it works for). Serialized as JSON; that text is the rendering
/// the human is shown and whose digest a decision binds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DialogRendering {
    /// The session the call belongs to.
    pub session_id: SessionId,
    /// The dialog's content.
    pub prompt: RecordedPrompt,
}

impl DialogRendering {
    /// The rendering of a decision's text, when it is one of these.
    #[must_use]
    pub fn parse(rendering: &str) -> Option<Self> {
        serde_json::from_str(rendering).ok()
    }
}

/// The person every attached client decides as: the daemon binds its
/// attach token to this one principal (the daemon serves one person).
#[derive(Debug, Clone)]
pub struct Principal {
    person: PrincipalRef,
    approver_id: ApproverId,
}

impl Principal {
    /// The principal `name` (a login name: no whitespace or control
    /// characters).
    ///
    /// # Errors
    ///
    /// Why `name` cannot name a principal.
    pub fn new(name: &str) -> Result<Self, String> {
        let domain = TrustDomainId::new(TRUST_DOMAIN).map_err(|err| err.to_string())?;
        let person = PrincipalRef::in_domain(PrincipalKind::Human, name, domain)
            .map_err(|err| format!("`{name}` cannot name the daemon's principal: {err}"))?;
        let approver_id = ApproverId::new(format!("person:{name}"))
            .map_err(|err| format!("`{name}` cannot name the daemon's principal: {err}"))?;
        // An approver must be a qualified human: checked once here.
        AuthenticatedApprover::new(person.clone(), ApprovalAssurance::DeviceTrusted)
            .map_err(|err| format!("`{name}` cannot approve: {err}"))?;
        Ok(Self {
            person,
            approver_id,
        })
    }

    /// `configured` (`[daemon] principal`), else the login name
    /// (`$USER`, `$LOGNAME`), else [`DEFAULT_PRINCIPAL`].
    ///
    /// # Errors
    ///
    /// Why the configured name cannot name a principal.
    pub fn from_config(configured: Option<&str>) -> Result<Self, String> {
        if let Some(name) = configured {
            return Self::new(name);
        }
        let login = ["USER", "LOGNAME"]
            .into_iter()
            .filter_map(|var| std::env::var(var).ok())
            .find(|name| Self::new(name).is_ok());
        Self::new(login.as_deref().unwrap_or(DEFAULT_PRINCIPAL))
    }

    /// The approver id every decision of the daemon is held for.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn approver_id(&self) -> &ApproverId {
        &self.approver_id
    }

    /// The principal, authenticated on one connection (the token is a
    /// device credential: device trusted).
    fn approver(&self, scope: HostAuthenticationScope) -> Option<AuthenticatedApprover> {
        AuthenticatedApprover::new(self.person.clone(), ApprovalAssurance::DeviceTrusted)
            .ok()
            .map(|approver| approver.with_authentication_scope(scope))
    }
}

/// The requester key of every decision: the gate, a service of the host.
fn requester() -> Result<DecisionPrincipalKey, String> {
    let domain = TrustDomainId::new(TRUST_DOMAIN).map_err(|err| err.to_string())?;
    let gate = PrincipalRef::in_domain(PrincipalKind::ServiceAccount, GATE, domain)
        .map_err(|err| err.to_string())?;
    DecisionPrincipalKey::of(&gate).map_err(|err| err.to_string())
}

/// Why an escalation cannot be held as a decision ([`Decisions::hold_request`]).
#[derive(Debug, thiserror::Error)]
pub enum HoldError {
    /// The dialog's rendering could not be written.
    #[error("the dialog cannot be rendered: {0}")]
    Render(#[from] serde_json::Error),
    /// The host's clock gave no representable time.
    #[error(transparent)]
    Clock(#[from] DecisionClockError),
    /// The decision contract refuses a value (kind, subject, body, …).
    #[error("the decision contract refuses it: {0}")]
    Contract(#[from] DecisionContractError),
}

/// How a held decision was decided by a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    /// Approved (else rejected).
    pub approve: bool,
    /// The dialog's answer, when the client named it.
    pub answer: Option<RemoteAnswer>,
    /// The decider's reason (the dialog's comment).
    pub reason: Option<String>,
    /// Who decided: `human:<connection scope>` (the audit log's decider).
    pub by: Option<String>,
}

/// One attached client.
struct Client {
    out: super::serve::ClientOut,
    /// The principal the client's connection authenticated as.
    approver_id: ApproverId,
    /// The connection's own authentication scope.
    scope: HostAuthenticationScope,
    /// Cleared when the connection ends: no decide on it is eligible then.
    open: Arc<AtomicBool>,
    /// The decisions presented on this connection (shown, and the
    /// presentation recorded). Its lock is a presentation pass: a decide
    /// waits for one in flight.
    presented: tokio::sync::Mutex<HashSet<DecisionId>>,
    /// The decisions whose notice reached this connection, recorded or not
    /// (a decision another client decided first refuses the record): each is
    /// offered once, and the client hears how it ended.
    shown: Mutex<HashSet<DecisionId>>,
}

/// Whether a client authenticated as `approver_id` may be shown, and may
/// decide, `row`: the decision is held for that approver. A daemon serves
/// one principal, so every client of it is; a daemon serving several
/// persons would bind one per connection and this filters per decision.
#[must_use]
pub fn authorized(approver_id: &ApproverId, row: &StoredDecision) -> bool {
    &row.descriptor.approver_id == approver_id
}

/// Whether `row` is one of the gate's decisions waiting for a human now.
fn awaits_a_human(row: &StoredDecision) -> bool {
    row.descriptor.kind.as_str() == KIND
        && row.state.phase == DecisionPhase::Open
        && row.state.stage == DecisionStage::Human
}

/// The daemon's consent surface over the decision owner (see the module
/// docs).
pub struct Decisions {
    service: DurableDecisionService,
    principal: Principal,
    requester: DecisionPrincipalKey,
    /// Decides a client's `decision/decide` (`None` in unit tests).
    router: Option<Arc<MethodRouter>>,
    /// The subscription made at construction, until [`Self::follow`] takes
    /// it.
    openings: Mutex<Option<DecisionOpenings>>,
    clients: Mutex<HashMap<u64, Arc<Client>>>,
    waiters: Mutex<HashMap<DecisionId, oneshot::Sender<Settled>>>,
    next_client: AtomicU64,
    next_notice: AtomicU64,
}

impl std::fmt::Debug for Decisions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decisions")
            .field("principal", &self.principal.approver_id)
            .field("clients", &self.clients.lock().len())
            .field("waiting", &self.waiters.lock().len())
            .finish_non_exhaustive()
    }
}

/// A client's registration; dropping it ends the client (nothing more is
/// offered to it and no decide of it is eligible any more).
pub struct ClientGuard {
    hub: Arc<Decisions>,
    id: u64,
}

impl ClientGuard {
    /// The client's id ([`Decisions::decide`] takes it).
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        if let Some(client) = self.hub.clients.lock().remove(&self.id) {
            client.open.store(false, Ordering::SeqCst);
        }
    }
}

impl std::fmt::Debug for ClientGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientGuard")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Decisions {
    /// The consent surface over `service` for `principal`, deciding through
    /// `router`. Subscribes to the owner's openings now: before any client
    /// attaches and lists the open decisions.
    ///
    /// # Errors
    ///
    /// When the gate's requester key cannot be made.
    pub fn new(
        service: DurableDecisionService,
        principal: Principal,
        router: Option<Arc<MethodRouter>>,
    ) -> Result<Arc<Self>, String> {
        let openings = service.subscribe_openings();
        Ok(Arc::new(Self {
            service,
            principal,
            requester: requester()?,
            router,
            openings: Mutex::new(Some(openings)),
            clients: Mutex::new(HashMap::new()),
            waiters: Mutex::new(HashMap::new()),
            next_client: AtomicU64::new(1),
            next_notice: AtomicU64::new(1),
        }))
    }

    /// The decision owner.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn service(&self) -> &DurableDecisionService {
        &self.service
    }

    /// The daemon's principal.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Offer every decision that opens to the attached clients, until the
    /// owner is gone. Takes the subscription made at construction (a second
    /// call follows nothing).
    pub fn follow(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let openings = self.openings.lock().take();
        let hub = Arc::clone(self);
        tokio::spawn(async move {
            let Some(mut openings) = openings else {
                return;
            };
            while let Some(opening) = openings.next().await {
                hub.on_opening(opening).await;
            }
        })
    }

    /// One reading of the openings subscription.
    pub async fn on_opening(&self, opening: DecisionOpening) {
        match opening {
            DecisionOpening::Opened(row) => self.offer_all(&row).await,
            DecisionOpening::Missed { missed } => {
                tracing::warn!(
                    missed,
                    "the daemon fell behind the decisions it opened; it offers the open decisions again"
                );
                for row in self.open_rows().await {
                    self.offer_all(&row).await;
                }
            }
            // An opening of a kind this version does not know offers
            // nothing.
            _ => {}
        }
    }

    /// A client attached: decisions are offered to it through `out` from
    /// now on, and the decisions open now are offered to it (the
    /// catch-up). The guard ends it.
    pub fn connect(self: &Arc<Self>, out: super::serve::ClientOut) -> ClientGuard {
        let id = self.next_client.fetch_add(1, Ordering::Relaxed);
        let client = Arc::new(Client {
            out,
            approver_id: self.principal.approver_id.clone(),
            scope: HostAuthenticationScope::new(format!("{INGRESS}:{}:{id}", std::process::id())),
            open: Arc::new(AtomicBool::new(true)),
            presented: tokio::sync::Mutex::new(HashSet::new()),
            shown: Mutex::new(HashSet::new()),
        });
        self.clients.lock().insert(id, Arc::clone(&client));
        let hub = Arc::clone(self);
        tokio::spawn(async move {
            for row in hub.open_rows().await {
                hub.offer(&client, &row).await;
            }
        });
        ClientGuard {
            hub: Arc::clone(self),
            id,
        }
    }

    /// Push the notification `method` with `params` to every attached
    /// client (the approval engine's notes, [`protocol::APPROVAL_NOTE`]).
    pub fn broadcast(&self, method: &str, params: &impl serde::Serialize) {
        let line = protocol::notification_line(method, params);
        for client in self.clients.lock().values() {
            if client.open.load(Ordering::Relaxed) {
                let _ = client.out.send(super::serve::Outgoing::line(line.clone()));
            }
        }
    }

    /// How many clients are attached.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn clients(&self) -> usize {
        self.clients.lock().len()
    }

    /// The gate's open decisions for the daemon's principal, every page.
    async fn open_rows(&self) -> Vec<StoredDecision> {
        let Ok(kind) = DecisionKind::new(KIND) else {
            return Vec::new();
        };
        let filter = DecisionListFilter::default()
            .with_kind(kind)
            .with_phase(DecisionPhase::Open)
            .with_approver_id(self.principal.approver_id.clone());
        let mut rows = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let service = self.service.clone();
            let filter = filter.clone();
            let from = after.clone();
            let page =
                blocking(move || service.list(&filter, from.as_deref(), DECISION_PAGE_MAX)).await;
            let page = match page {
                Ok(page) => page,
                Err(err) => {
                    tracing::warn!(error = %err, "the open decisions could not be listed");
                    return rows;
                }
            };
            rows.extend(page.decisions);
            match page
                .next_after
                .filter(|next| after.as_deref() != Some(next.as_str()))
            {
                Some(next) => after = Some(next),
                None => return rows,
            }
        }
    }

    async fn offer_all(&self, row: &StoredDecision) {
        let clients: Vec<Arc<Client>> = self.clients.lock().values().cloned().collect();
        for client in clients {
            self.offer(&client, row).await;
        }
    }

    /// Show `row` to `client` once, and record that it was shown: the
    /// clock is read first, the notice is written to the client's socket,
    /// and only then is the presentation recorded at that reading. A notice
    /// that did not reach the socket records nothing (a decide then is
    /// refused); never the reverse.
    async fn offer(&self, client: &Client, row: &StoredDecision) {
        if !client.open.load(Ordering::SeqCst)
            || !authorized(&client.approver_id, row)
            || !awaits_a_human(row)
        {
            return;
        }
        let mut presented = client.presented.lock().await;
        if client.shown.lock().contains(&row.decision_id) {
            return;
        }
        let Ok(at_ms) = SystemDecisionClock.now_ms() else {
            tracing::warn!("no clock reading for a decision presentation");
            return;
        };
        let n = self.next_notice.fetch_add(1, Ordering::Relaxed);
        let notice_id = format!("{INGRESS}:{}:notice-{n}", std::process::id());
        let (Ok(message_ref), Ok(channel_ref)) = (
            DecisionReference::new(notice_id.clone()),
            DecisionReference::new(client.scope.as_str().to_string()),
        ) else {
            return;
        };
        let notice = notice_of(row, notice_id);
        let line = format!(
            "{}\n",
            json!({"jsonrpc": "2.0", "method": NOTICE, "params": notice})
        );
        let (written, acknowledged) = oneshot::channel();
        if client
            .out
            .send(Outgoing {
                line,
                written: Some(written),
            })
            .is_err()
        {
            return;
        }
        if !matches!(
            tokio::time::timeout(NOTICE_WRITE, acknowledged).await,
            Ok(Ok(()))
        ) {
            return;
        }
        client.shown.lock().insert(row.decision_id.clone());
        let record = PresentationRecord::complete(
            PresentationAudience::Approver {
                approver_id: row.descriptor.approver_id.clone(),
            },
            &row.descriptor.rendering,
            None,
            channel_ref,
            message_ref,
            at_ms,
        );
        let service = self.service.clone();
        let id = row.decision_id.clone();
        match blocking(move || service.present(&id, &record)).await {
            Ok(_) => {
                presented.insert(row.decision_id.clone());
            }
            Err(err) => {
                tracing::debug!(error = %err, decision = %row.decision_id, "a decision shown to a client was not recorded");
            }
        }
    }

    /// The hold of `prompt`'s escalation, open for `timeout` (and
    /// [`DEADLINE_SLACK`]).
    ///
    /// # Errors
    ///
    /// [`HoldError`]: why it cannot be held.
    pub fn hold_request(
        &self,
        prompt: &EscalationPrompt,
        timeout: Duration,
    ) -> Result<DecisionHoldRequest, HoldError> {
        let shown = DialogRendering {
            session_id: prompt.session_id.clone(),
            prompt: prompt.shown.clone(),
        };
        let rendering = serde_json::to_string(&shown)?;
        let subject = format!(
            "escalation:{}:{}:{}",
            prompt.session_id,
            std::process::id(),
            prompt.id
        );
        let now = SystemDecisionClock.now_ms()?;
        let window = u64::try_from((timeout + DEADLINE_SLACK).as_millis()).unwrap_or(u64::MAX);
        Ok(DecisionHoldRequest::new(
            DecisionKind::new(KIND)?,
            DecisionSubjectKey::new(subject.clone())?,
            DecisionOperationId::new(subject)?,
            Sha256Digest::of(rendering.as_bytes()),
            DecisionRequestBody::from_value(&serde_json::to_value(&shown)?)?,
            self.requester.clone(),
            self.principal.approver_id.clone(),
            now.saturating_add(window),
            DecisionRendering::new(rendering)?,
            DecisionRenderer::new(RENDERER)?,
        ))
    }

    /// Hold `request`; `settled` hears how a client decided it. The waiter
    /// is registered before the hold, so a decide that comes at once finds
    /// it.
    ///
    /// # Errors
    ///
    /// The owner's refusal; nothing waits then.
    pub async fn hold(
        &self,
        request: DecisionHoldRequest,
        settled: oneshot::Sender<Settled>,
    ) -> Result<DecisionId, DecisionError> {
        let id = DecisionId::derive(&request.kind, &request.subject_key);
        self.waiters.lock().insert(id.clone(), settled);
        let service = self.service.clone();
        match blocking(move || service.hold(request.clone(), false)).await {
            Ok(row) => Ok(row.decision_id),
            Err(err) => {
                self.waiters.lock().remove(&id);
                Err(err)
            }
        }
    }

    /// A client decided `id` (the owner recorded it): its waiter hears how.
    pub fn settled(&self, id: &DecisionId, settled: Settled) {
        if let Some(waiter) = self.waiters.lock().remove(id) {
            let _ = waiter.send(settled);
        }
    }

    /// End `id` without a client's decision (the engine gave up on it, the
    /// human interrupted): withdrawn as the host. Answers the phase the
    /// decision ended in.
    pub async fn withdraw(&self, id: &DecisionId) -> Option<DecisionPhase> {
        self.waiters.lock().remove(id);
        let service = self.service.clone();
        let decision = id.clone();
        match blocking(move || service.withdraw(&decision, &DecisionWithdrawer::Host)).await {
            Ok(row) => Some(row.state.phase),
            Err(err) => {
                tracing::debug!(error = %err, decision = %id, "a decision was not withdrawn");
                let service = self.service.clone();
                let decision = id.clone();
                blocking(move || service.get(&decision))
                    .await
                    .ok()
                    .map(|row| row.state.phase)
            }
        }
    }

    /// Push every file change the agents' file tools report (each tagged
    /// with its agent) to the attached clients as an `rkat/file_diff`
    /// notification ([`protocol::FileDiffNote`]; a client shows its own
    /// agent's), until the tools are gone. A client that is not attached
    /// when the change is made does not get it (the diff is a view; the
    /// tool result in the conversation stays).
    pub fn forward_diffs(
        hub: &Arc<Self>,
        mut diffs: mpsc::UnboundedReceiver<(String, crate::tools::FileDiff)>,
    ) -> tokio::task::JoinHandle<()> {
        let hub = Arc::clone(hub);
        tokio::spawn(async move {
            while let Some((agent, diff)) = diffs.recv().await {
                let params = protocol::FileDiffNote {
                    agent,
                    tool_use_id: diff.tool_use_id,
                    path: diff.path.display().to_string(),
                    unified: diff.unified,
                    added: diff.added,
                    removed: diff.removed,
                };
                let line = format!(
                    "{}\n",
                    json!({"jsonrpc": "2.0", "method": protocol::FILE_DIFF, "params": params})
                );
                let clients: Vec<Arc<Client>> = hub.clients.lock().values().cloned().collect();
                for client in clients {
                    let _ = client.out.send(Outgoing {
                        line: line.clone(),
                        written: None,
                    });
                }
            }
        })
    }

    /// Tell the clients that were shown `id` that it ended in `phase`.
    pub async fn ended(&self, id: &DecisionId, phase: Option<DecisionPhase>) {
        let clients: Vec<Arc<Client>> = self.clients.lock().values().cloned().collect();
        let params = DecisionEnded {
            decision_id: id.to_string(),
            phase: phase.map(wire_decision_phase),
        };
        let line = format!(
            "{}\n",
            json!({"jsonrpc": "2.0", "method": ENDED, "params": params})
        );
        for client in clients {
            // After a presentation pass in flight on the client: its notice
            // goes out before this one.
            let _pass = client.presented.lock().await;
            if client.shown.lock().contains(id) {
                let _ = client.out.send(Outgoing {
                    line: line.clone(),
                    written: None,
                });
            }
        }
    }

    /// Withdraw the gate's decisions a previous daemon left open: the
    /// dispatch each one held ended with that process, so no answer to it
    /// would run anything. Returns how many.
    pub async fn withdraw_leftovers(&self) -> usize {
        let mut withdrawn = 0;
        for row in self.open_rows().await {
            if row.descriptor.requester != self.requester {
                continue;
            }
            if self.withdraw(&row.decision_id).await == Some(DecisionPhase::Withdrawn) {
                withdrawn += 1;
            }
        }
        withdrawn
    }

    /// A client's `decision/decide` (its request `id` and `params`): the
    /// answer line (newline included). See the module docs.
    pub async fn decide(&self, client: u64, id: Option<RpcId>, params: Value) -> String {
        match self.try_decide(client, id.as_ref(), params).await {
            Ok(line) => line,
            Err((code, message)) => protocol::error_line(id, code, &message),
        }
    }

    async fn try_decide(
        &self,
        client_id: u64,
        id: Option<&RpcId>,
        params: Value,
    ) -> Result<String, (i32, String)> {
        let request: DecideParams = serde_json::from_value(params).map_err(|err| {
            (
                INVALID_PARAMS,
                format!(
                    "invalid decision/decide params: {err} (a decide names the decision, the answer, the presented digest and a reason; who decided comes from the connection, never from the request)"
                ),
            )
        })?;
        if let Some(answer) = request.answer {
            let approves = matches!(
                answer,
                RemoteAnswer::AllowOnce
                    | RemoteAnswer::AllowForSession
                    | RemoteAnswer::AllowAndAddRule
            );
            if approves != request.approve {
                return Err((
                    INVALID_PARAMS,
                    "the answer does not match `approve`".to_string(),
                ));
            }
        }
        let Some(router) = &self.router else {
            return Err((INTERNAL_ERROR, "the daemon decides nothing".to_string()));
        };
        let client = self
            .clients
            .lock()
            .get(&client_id)
            .cloned()
            .ok_or((INTERNAL_ERROR, "the connection has ended".to_string()))?;
        let invalid = |err: DecisionContractError| (INVALID_PARAMS, err.to_string());
        let decision_id = DecisionId::new(request.decision_id.clone()).map_err(invalid)?;
        let digest = Sha256Digest::new(request.presented_digest.clone()).map_err(invalid)?;
        // A decide waits for a presentation pass in flight on this
        // connection.
        let eligible = client.presented.lock().await.contains(&decision_id);
        let observed_at_ms = SystemDecisionClock
            .now_ms()
            .map_err(|err| (INTERNAL_ERROR, err.to_string()))?;
        let approver = self.principal.approver(client.scope.clone()).ok_or((
            INTERNAL_ERROR,
            "the daemon's principal cannot approve".to_string(),
        ))?;
        let n = self.next_notice.fetch_add(1, Ordering::Relaxed);
        let event_ref = DecisionReference::new(format!("{}:decide-{n}", client.scope.as_str()))
            .map_err(invalid)?;
        let mut decision = AttestedDecision::new(
            approver,
            DecisionIngress::new(INGRESS).map_err(invalid)?,
            event_ref,
            observed_at_ms,
            self.principal.approver_id.clone(),
            digest,
            request.approve,
        )
        .map_err(invalid)?;
        if let Some(reason) = &request.reason {
            decision = decision.with_reason(reason.clone()).map_err(invalid)?;
        }
        let event = AuthenticatedDecisionEvent::new(
            decision_id.clone(),
            decision,
            Arc::new(ConnectionEligibility {
                snapshot: eligible,
                open: Arc::clone(&client.open),
            }),
        );
        let mut engine_params = json!({
            "decision_id": request.decision_id,
            "approve": request.approve,
            "presented_digest": request.presented_digest,
        });
        if let Some(reason) = &request.reason {
            engine_params["reason"] = json!(reason);
        }
        let rpc: RpcRequest = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": DECIDE,
            "params": engine_params,
        }))
        .map_err(|err| (INVALID_PARAMS, err.to_string()))?;
        let busy = meerkat_contracts::ErrorCode::ApprovalStoreUnavailable.jsonrpc_code();
        let mut attempt = 0;
        let response = loop {
            let response = router
                .dispatch_with_decision_event(rpc.clone(), event.clone())
                .await;
            let code = response.error.as_ref().map(|error| error.code);
            if code == Some(busy) && attempt < STORE_RETRIES {
                attempt += 1;
                tokio::time::sleep(STORE_BACKOFF * attempt).await;
                continue;
            }
            break response;
        };
        if response.error.is_none() {
            self.settled(
                &decision_id,
                Settled {
                    approve: request.approve,
                    answer: request.answer,
                    reason: request.reason,
                    by: Some(format!("human:{}", client.scope.as_str())),
                },
            );
        }
        Ok(protocol::response_line(&response))
    }
}

/// The eligibility a decide on one connection carries: the connection was
/// shown the decision when the decide arrived, and is still open when the
/// decision commits.
struct ConnectionEligibility {
    snapshot: bool,
    open: Arc<AtomicBool>,
}

impl ConsentEligibility for ConnectionEligibility {
    fn check_current(&self) -> Result<(), ConsentIneligible> {
        if self.snapshot && self.open.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(ConsentIneligible)
        }
    }
}

/// The `decision/notice` of `row`: the whole rendering, as one canonical
/// part.
fn notice_of(row: &StoredDecision, notice_id: String) -> WireDecisionNotice {
    let descriptor = &row.descriptor;
    let rendering = descriptor.rendering.as_str();
    WireDecisionNotice {
        notice_id,
        decision_id: row.decision_id.to_string(),
        kind: descriptor.kind.to_string(),
        event: WireDecisionNoticeEvent::Presented,
        phase: wire_decision_phase(row.state.phase),
        approver_id: descriptor.approver_id.to_string(),
        requester: descriptor.requester.to_string(),
        presentation: Some(WireDecisionNoticePresentation {
            rendering: rendering.to_owned(),
            renderer: descriptor.renderer.to_string(),
            subject_digest: descriptor.subject_digest.to_string(),
            parts: canonical_parts(rendering, None)
                .iter()
                .map(wire_canonical_part)
                .collect(),
        }),
    }
}

/// Run a decision-owner call off the executor (a SQLite store blocks), and
/// again while the store is busy with another call.
async fn blocking<T, F>(call: F) -> Result<T, DecisionError>
where
    T: Send + 'static,
    F: Fn() -> Result<T, DecisionError> + Send + Sync + 'static,
{
    let call = Arc::new(call);
    let mut attempt = 0;
    loop {
        let once = Arc::clone(&call);
        match tokio::task::spawn_blocking(move || once()).await {
            Ok(Err(DecisionError::Unavailable(_))) if attempt < STORE_RETRIES => {
                attempt += 1;
                tokio::time::sleep(STORE_BACKOFF * attempt).await;
            }
            Ok(result) => return result,
            Err(err) => return Err(DecisionError::Unavailable(err.to_string())),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use meerkat_core::approval::decision::InMemoryDecisionStore;

    fn hub() -> Arc<Decisions> {
        let service = DurableDecisionService::new(Arc::new(InMemoryDecisionStore::new()));
        Decisions::new(service, Principal::new("alice").unwrap(), None).unwrap()
    }

    fn prompt(id: u64) -> EscalationPrompt {
        EscalationPrompt {
            id,
            session_id: SessionId::new(),
            shown: RecordedPrompt {
                tool: "shell".into(),
                args_preview: "ls".into(),
                subjects: vec!["ls".into()],
                policy_reason: Some("manual mode asks".into()),
                ..RecordedPrompt::default()
            },
        }
    }

    /// A client whose notices are acknowledged as written: returns the
    /// lines it was sent.
    fn client(hub: &Arc<Decisions>) -> (ClientGuard, mpsc::UnboundedReceiver<String>) {
        let (out, mut out_rx) = super::super::serve::ClientOut::channel(64);
        let (lines, lines_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(outgoing) = out_rx.recv().await {
                if let Some(written) = outgoing.written {
                    let _ = written.send(());
                }
                let _ = lines.send(outgoing.line);
            }
        });
        (hub.connect(out), lines_rx)
    }

    async fn next(lines: &mut mpsc::UnboundedReceiver<String>) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(5), lines.recv())
            .await
            .expect("a line")
            .expect("open");
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn the_principal_is_configured_or_the_login_name() {
        let alice = Principal::new("alice").unwrap();
        assert_eq!(alice.approver_id().as_str(), "person:alice");
        assert_eq!(
            Principal::from_config(Some("bob"))
                .unwrap()
                .approver_id()
                .as_str(),
            "person:bob"
        );
        assert!(Principal::new("two words").is_err());
        assert!(Principal::from_config(None).is_ok());
    }

    #[tokio::test]
    async fn a_held_decision_is_offered_to_each_client_once_and_presented() {
        let hub = hub();
        let _following = hub.follow();
        let (_a, mut a) = client(&hub);
        let shown = prompt(1);
        let request = hub.hold_request(&shown, Duration::from_mins(1)).unwrap();
        let (settled, _settled_rx) = oneshot::channel();
        let id = hub.hold(request, settled).await.unwrap();
        let notice = next(&mut a).await;
        assert_eq!(notice["method"], NOTICE);
        assert_eq!(notice["params"]["decision_id"], id.to_string());
        let rendering = notice["params"]["presentation"]["rendering"]
            .as_str()
            .unwrap();
        let dialog = DialogRendering::parse(rendering).unwrap();
        assert_eq!(dialog.session_id, shown.session_id);
        assert_eq!(dialog.prompt, shown.shown);
        // A client attaching later is offered the open decision (the
        // catch-up); a Missed reading offers it again only to clients that
        // were not shown it.
        let (_b, mut b) = client(&hub);
        assert_eq!(next(&mut b).await["params"]["decision_id"], id.to_string());
        hub.on_opening(DecisionOpening::Missed { missed: 3 }).await;
        let (_c, mut c) = client(&hub);
        assert_eq!(next(&mut c).await["params"]["decision_id"], id.to_string());
        hub.on_opening(DecisionOpening::Missed { missed: 1 }).await;
        for lines in [&mut a, &mut b, &mut c] {
            assert!(
                tokio::time::timeout(Duration::from_millis(100), lines.recv())
                    .await
                    .is_err(),
                "offered twice"
            );
        }
        // The presentation was recorded (to the approver).
        let row = hub.service().get(&id).unwrap();
        assert!(row.state.first_presented_to_approver_at_ms.is_some());
        // Withdrawn: the clients shown it are told.
        assert_eq!(hub.withdraw(&id).await, Some(DecisionPhase::Withdrawn));
        hub.ended(&id, Some(DecisionPhase::Withdrawn)).await;
        let ended = next(&mut a).await;
        assert_eq!(ended["method"], ENDED);
        assert_eq!(ended["params"]["phase"], "withdrawn");
    }

    #[tokio::test]
    async fn a_missed_reading_offers_what_opened_unseen() {
        let hub = hub();
        // Nobody follows the subscription: openings are only caught up.
        let (_a, mut a) = client(&hub);
        let request = hub
            .hold_request(&prompt(2), Duration::from_mins(1))
            .unwrap();
        let (settled, _settled_rx) = oneshot::channel();
        let id = hub.hold(request, settled).await.unwrap();
        hub.on_opening(DecisionOpening::Missed { missed: 300 })
            .await;
        assert_eq!(next(&mut a).await["params"]["decision_id"], id.to_string());
    }

    #[tokio::test]
    async fn leftovers_of_an_earlier_daemon_are_withdrawn() {
        let hub = hub();
        let request = hub
            .hold_request(&prompt(3), Duration::from_mins(1))
            .unwrap();
        let (settled, _settled_rx) = oneshot::channel();
        let id = hub.hold(request, settled).await.unwrap();
        // The same store, a new daemon.
        let again = Decisions::new(
            hub.service().clone(),
            Principal::new("alice").unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(again.withdraw_leftovers().await, 1);
        assert_eq!(
            again.service().get(&id).unwrap().state.phase,
            DecisionPhase::Withdrawn
        );
        let (_a, mut a) = client(&again);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), a.recv())
                .await
                .is_err(),
            "nothing to show"
        );
    }

    #[test]
    fn a_decide_naming_who_decided_is_refused() {
        for field in ["actor", "approver", "approver_id", "decider"] {
            let mut params = json!({
                "decision_id": "decision:0",
                "approve": true,
                "presented_digest": "sha256:0",
            });
            params[field] = json!("someone");
            assert!(
                serde_json::from_value::<DecideParams>(params).is_err(),
                "{field}"
            );
        }
        let params = json!({
            "decision_id": "decision:0",
            "approve": false,
            "presented_digest": "sha256:0",
            "reason": "no",
            "answer": "stop",
        });
        let request: DecideParams = serde_json::from_value(params).unwrap();
        assert_eq!(request.answer, Some(RemoteAnswer::Stop));
    }
}
