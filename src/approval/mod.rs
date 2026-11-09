//! Auto-mode approval: tiers, rules, shell analysis, classifier and escalation.
//!
//! Every tool call is decided in tiers ([`policy::Tier`]), first match wins:
//!
//! 0. a user deny rule matches ⇒ deny ([`policy::Tier::UserDeny`]); a file
//!    write into a sealed path (the gate's own state, a git directory) or a
//!    call to a builtin every session's tool policy withholds ⇒ deny, before
//!    any allow rule ([`policy::Tier::Sealed`]);
//! 1. read-only builtins, in-project reads/searches, allowlisted shell commands
//!    and narrow user allow rules ⇒ allow;
//! 2. writes whose every target is inside the project and unprotected ⇒ allow;
//!
//!    between 2 and 3, a write with a protected target inside the project
//!    (secrets, build scripts, agent configuration) ⇒ the human is asked,
//!    never the classifier ([`policy::Tier::AskUser`]);
//! 3. everything else ⇒ the reasoning-blind [`classifier::ActionClassifier`].
//!
//! Classifier blocks are returned to the model as denials with a reason.
//! After three consecutive (or twenty total) blocks, auto mode pauses and
//! Tier-3 actions are escalated to the human through the
//! [`escalation::EscalationBroker`].
//!
//! That is auto mode, the default [`mode::PermissionMode`]. Manual mode asks
//! the human for tiers 2 and 3 instead, accept-edits mode for Tier 3 only,
//! and plan mode denies writes and every action above Tier 1 except the
//! policy's own read-only shell commands.
//!
//! The gate is [`admission::ApprovalGate`], Meerkat's `ToolDispatchAdmission`
//! seam: the agent's dispatcher consults it immediately before every tool
//! call, and a denial comes back to the model as an ordinary tool error
//! (`policy_denied`) carrying [`deny_message`], so the run continues. The
//! gate is installed per session build (see [`admission::install`]).
//! [`engine::ApprovalEngine`] joins the parts below: tier policy
//! ([`policy`]), rules and settings ([`rules`]), shell analysis ([`shell`]),
//! the classifier and its prompts ([`classifier`], [`prompts`]), denial
//! counters ([`counters`]), the escalation broker ([`escalation`]) and the
//! reasoning-blind classifier transcript ([`transcript`]). [`desk`] turns
//! the human's answer into the engine's decision for both hosts (the
//! in-process runner and the daemon).

pub mod admission;
pub mod catalog;
pub mod classifier;
pub mod counters;
pub mod desk;
pub mod engine;
pub mod escalation;
pub mod mode;
pub mod policy;
pub mod prompts;
pub mod rules;
pub mod shell;
pub(crate) mod shell_subject;
pub mod transcript;

use counters::PauseCause;
use meerkat_core::{SessionId, Usage};
use policy::{Decider, Tier, VerdictSummary};
use rules::{Rule, RuleId, RulesError};
use std::fmt;

pub use admission::ApprovalGate;
pub use catalog::{ServiceToolOrigins, ToolOrigins};
pub use engine::{ApprovalConfig, ApprovalEngine};
pub use transcript::InputKey;

/// Typed code of the tool error a denial returns to the model
/// (`ToolError::PolicyDenied`, rendered as `Tool call denied (auto_mode): …`).
pub const DENIAL_CODE: &str = "auto_mode";

/// Why plan mode denied a call: the tool error the model reads.
pub const PLAN_MODE_DENIAL: &str = "plan mode is on: the user wants a plan before any change, \
so file writes, edits and commands that are not read-only are not run. Do not try another way \
to make the change. Keep researching with read-only tools, then present your plan and wait for \
the user to approve it";

/// Final decision for one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Run the tool.
    Allow {
        /// Tier the action fell into.
        tier: Tier,
        /// Who allowed it.
        by: Decider,
        /// Short explanation.
        why: String,
    },
    /// Do not run the tool; the model receives the reason.
    Deny {
        /// Tier the action fell into.
        tier: Tier,
        /// Why.
        reason: DenyReason,
    },
}

impl Verdict {
    /// Compact form for the UI and the transcript.
    #[must_use]
    pub fn summary(&self) -> VerdictSummary {
        match self {
            Self::Allow { tier, by, .. } => VerdictSummary::Allowed {
                tier: *tier,
                by: *by,
            },
            Self::Deny { tier, reason } => VerdictSummary::Denied {
                tier: *tier,
                by: reason.decider(),
            },
        }
    }

    /// Whether the tool may run.
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow { .. })
    }
}

/// Why a call was denied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    /// A user deny rule matched.
    UserRule {
        /// Rule id.
        id: RuleId,
        /// The rule as written (`shell git push*`).
        rule: String,
    },
    /// A file tool targeted a sealed path (the gate's own `.rkat/` state or
    /// `.git/`), or the call is to a builtin every session's tool policy
    /// withholds. Neither the classifier nor an allow rule can open it.
    Sealed {
        /// Which path, and why it is sealed.
        detail: String,
    },
    /// The classifier blocked the action.
    Classifier {
        /// Block-rule id, if named.
        rule: Option<String>,
        /// Explanation.
        explanation: String,
    },
    /// The human denied it, optionally telling the agent what to do instead.
    Human {
        /// The user's instruction to the agent, if they wrote one.
        comment: Option<String>,
    },
    /// The human stopped the turn.
    Stopped,
    /// Plan mode is on: the call would change something.
    PlanMode,
    /// The classifier failed and no human could be asked.
    ClassifierUnavailable(String),
    /// Nobody answered the escalation in time.
    EscalationTimedOut,
}

impl DenyReason {
    /// Who made the call.
    #[must_use]
    pub fn decider(&self) -> Decider {
        match self {
            Self::UserRule { .. }
            | Self::Sealed { .. }
            | Self::PlanMode
            | Self::ClassifierUnavailable(_) => Decider::Policy,
            Self::Classifier { .. } => Decider::Classifier,
            Self::Human { .. } | Self::Stopped | Self::EscalationTimedOut => Decider::Human,
        }
    }

    /// The rule that decided (user rule id or classifier block-rule id), if any.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn rule_label(&self) -> Option<String> {
        match self {
            Self::UserRule { id, .. } => Some(id.to_string()),
            Self::Classifier { rule, .. } => rule.clone(),
            _ => None,
        }
    }
}

impl fmt::Display for DenyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UserRule { id, rule } => write!(f, "the user's deny rule {id} ({rule}) matches"),
            Self::Sealed { detail } => f.write_str(detail),
            Self::Classifier {
                rule: Some(rule),
                explanation,
            } => write!(f, "{explanation} [{rule}]"),
            Self::Classifier {
                rule: None,
                explanation,
            } => f.write_str(explanation),
            Self::Human { comment: None } => f.write_str("the user denied this action"),
            Self::Human {
                comment: Some(comment),
            } => write!(f, "denied by the user: {comment}"),
            Self::Stopped => f.write_str("the user stopped this turn"),
            Self::PlanMode => f.write_str(PLAN_MODE_DENIAL),
            Self::ClassifierUnavailable(err) => {
                write!(f, "the safety classifier is unavailable ({err})")
            }
            Self::EscalationTimedOut => {
                f.write_str("the user did not answer the approval request in time")
            }
        }
    }
}

/// Model-facing denial message.
#[must_use]
pub fn deny_message(reason: &DenyReason) -> String {
    if let DenyReason::Human {
        comment: Some(_), ..
    } = reason
    {
        // The user said what to do instead: that is the way forward, not a
        // detour around the denial.
        return format!(
            "Blocked by auto-mode safety policy: {reason}. Do what the user says instead; do not \
             try to achieve the denied action's effect another way."
        );
    }
    format!(
        "Blocked by auto-mode safety policy: {reason}. Do not try to achieve the same effect \
         another way (different tool, rewritten or encoded command, or a script). Choose a safer \
         approach within the user's request, or stop and ask the user."
    )
}

/// UI-facing notifications.
#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalEvent {
    /// A tool call was decided.
    Decision {
        /// Session.
        session_id: SessionId,
        /// The helper that made the call (`None`: the session's own agent).
        agent: Option<String>,
        /// Tool-use id.
        tool_use_id: String,
        /// Tool name.
        tool: String,
        /// Tier.
        tier: Tier,
        /// Outcome.
        verdict: VerdictSummary,
        /// Why it was denied (the reason the model receives), or, for an
        /// action the classifier allowed, the classifier's stated reason.
        reason: Option<String>,
    },
    /// Auto mode paused.
    Paused(PauseCause),
    /// Auto mode resumed.
    Resumed,
    /// Broad allow rules were dropped on entering auto mode.
    RulesDropped(Vec<Rule>),
    /// The classifier started (`true`) or finished (`false`) reviewing a call.
    Classifying(bool),
    /// One classifier model call's token usage.
    ClassifierUsage {
        /// Classifier model.
        model: String,
        /// Its usage.
        usage: Usage,
    },
    /// An escalation ended without the human's answer (it timed out or its
    /// tool call was abandoned): the dialog for it should close.
    EscalationWithdrawn(u64),
    /// Something the human should know about the approval machinery (an
    /// allow rule could not be saved, a rules file was not trusted).
    Notice(String),
}

/// Errors from approval setup.
#[derive(Debug, thiserror::Error)]
pub enum ApprovalError {
    /// Rules could not be loaded or persisted.
    #[error(transparent)]
    Rules(#[from] RulesError),
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn deny_message_wording() {
        let msg = deny_message(&DenyReason::Classifier {
            rule: Some("D2".into()),
            explanation: "force push rewrites shared history".into(),
        });
        assert!(msg.starts_with(
            "Blocked by auto-mode safety policy: force push rewrites shared history [D2]. Do not try"
        ));
        assert!(msg.ends_with("or stop and ask the user."));
    }

    #[test]
    fn deny_reasons_map_to_deciders() {
        let classifier = DenyReason::Classifier {
            rule: Some("D1".into()),
            explanation: "x".into(),
        };
        assert_eq!(classifier.decider(), Decider::Classifier);
        assert_eq!(classifier.rule_label().as_deref(), Some("D1"));
        assert_eq!(
            DenyReason::Human { comment: None }.decider(),
            Decider::Human
        );
        assert_eq!(DenyReason::EscalationTimedOut.decider(), Decider::Human);
        assert_eq!(DenyReason::PlanMode.decider(), Decider::Policy);
        assert_eq!(DenyReason::Stopped.rule_label(), None);
    }
}
