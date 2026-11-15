//! What the gate tells the audit log (recommendation 11 of the security
//! review): every decision with who made it, and the changes of authority
//! the engine makes itself (mode, rules). The daemon's sink writes them
//! (`crate::daemon::audit`); without one the engine records nothing.

use std::sync::{Arc, OnceLock};

use meerkat_core::{RunId, SessionId};
use tokio::sync::mpsc;

use super::ApprovalEvent;
use super::policy::Tier;

/// One decided call.
#[derive(Debug)]
pub struct DecisionRecord<'a> {
    /// The session the call was judged for.
    pub session: &'a SessionId,
    /// The helper that made it (`None`: the session's own agent).
    pub helper: Option<&'a str>,
    /// The run it belongs to, when known.
    pub run: Option<&'a RunId>,
    /// The tool.
    pub tool: &'a str,
    /// The decisive argument, whole: the command, the paths, else the raw
    /// arguments.
    pub argument: String,
    /// The tier it fell into.
    pub tier: Tier,
    /// Whether it may run.
    pub allowed: bool,
    /// Who decided: `rule:<id>`, `classifier`, `human:<connection>`,
    /// `human`, `mode`, `policy`, `unattended`, `timeout`.
    pub decider: String,
    /// Why it was denied.
    pub reason: Option<String>,
    /// The classifier's review of it, when one ran.
    pub review: Option<ReviewRecord>,
}

/// One classifier review, as the audit log records it beside the decision.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewRecord {
    /// The stage that decided: 1 (the quick check allowed it) or 2.
    pub stage: u8,
    /// `allow`, `block`, or `error` (the review could not be completed).
    pub verdict: String,
    /// The block rule named, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// How long the review took, in milliseconds.
    pub latency_ms: u64,
    /// The classifier's input tokens (cache reads and writes included).
    pub input_tokens: u64,
    /// Its output tokens.
    pub output_tokens: u64,
}

/// Where the gate and its parts report: the clients' events, and the audit
/// sink once the daemon set one ([`Outlets::set_audit`]). Cloned into the
/// parts that report on their own (the classifier slot); a clone reports
/// to the same places.
#[derive(Clone)]
pub(super) struct Outlets {
    events: mpsc::UnboundedSender<ApprovalEvent>,
    audit: Arc<OnceLock<Arc<dyn AuditSink>>>,
}

impl Outlets {
    pub(super) fn new(events: mpsc::UnboundedSender<ApprovalEvent>) -> Self {
        Self {
            events,
            audit: Arc::new(OnceLock::new()),
        }
    }

    /// Tell the clients.
    pub(super) fn emit(&self, event: ApprovalEvent) {
        let _ = self.events.send(event);
    }

    /// Record from now on in `audit`. Set once; `false` when it was set.
    pub(super) fn set_audit(&self, audit: Arc<dyn AuditSink>) -> bool {
        self.audit.set(audit).is_ok()
    }

    /// The audit sink, when one is set.
    pub(super) fn audit(&self) -> Option<&Arc<dyn AuditSink>> {
        self.audit.get()
    }

    /// An audit line of a change of authority, when a sink is set.
    pub(super) fn audit_event(&self, event: &str, detail: String) {
        if let Some(audit) = self.audit.get() {
            audit.event(event, detail);
        }
    }
}

/// Where the gate's audit lines go.
pub trait AuditSink: Send + Sync {
    /// A call was decided.
    fn decision(&self, record: DecisionRecord<'_>);

    /// The engine changed what is allowed: `event` (`mode`, `rule_added`,
    /// `rule_removed`, `rules_trusted`) with `detail`.
    fn event(&self, event: &str, detail: String);
}
