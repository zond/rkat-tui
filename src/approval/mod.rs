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
//! daemon puts a gate on every member and helper it seats (its root
//! member's admission and the spawn customizer, see [`crate::mobs`]).
//! [`engine::ApprovalEngine`] joins the parts below: tier policy
//! ([`policy`]), rules and settings ([`rules`]), shell analysis ([`shell`]),
//! the classifier and its prompts ([`classifier`], [`prompts`]), denial
//! counters ([`counters`]), the escalation broker ([`escalation`]) and the
//! reasoning-blind classifier transcript ([`transcript`]). [`desk`] turns
//! the human's answer into the engine's decision for the daemon.

pub mod admission;
pub mod attendance;
pub mod audit;
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
pub mod scratch;
pub mod shell;
pub(crate) mod shell_subject;
pub mod transcript;

use counters::PauseCause;
use meerkat_core::{SessionId, Usage};
use policy::{Decider, Tier, VerdictSummary};
use rules::{Rule, RuleId, RulesError};
use std::fmt;

pub use admission::ApprovalGate;
pub use attendance::{AgentPeers, Attendance, PeerKind};
pub use catalog::ToolOrigins;
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

/// Why an unattended call was denied: the tool error the model reads.
pub const UNATTENDED_DENIAL: &str = "this action needs a user attached to this agent: no user \
started this turn (another agent's message, a schedule or a file watch did), no user is \
attached now, and the action needs the user's answer (a protected path, a question the \
permission mode asks, a paused auto mode, or a review that could not be completed). The user is \
told what you wanted to do when they attach; say in your answer that it waits for them";

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
    /// The call asks for what the engine offers but cannot run here
    /// ([`policy::Tier::Unavailable`]); `detail` says what to do instead.
    Unavailable {
        /// What is unavailable, why, and what to do instead.
        detail: String,
    },
    /// The classifier blocked the action.
    Classifier {
        /// Block-rule id, if named (the user's text only).
        rule: Option<String>,
        /// Explanation.
        explanation: String,
        /// How the classifier checked (the user's text only).
        annotation: Option<String>,
        /// What would allow it, when a path outside the allowed scope is
        /// why ([`scope_hint`]): for the agent and the user alike.
        hint: Option<String>,
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
    /// No classifier or human could review the call (none is configured,
    /// or the call is too large to review and no human can be asked).
    ClassifierUnavailable(String),
    /// The classifier's review failed (it timed out, the provider failed)
    /// and no human could be asked: not a refusal of the action.
    ReviewIncomplete(String),
    /// Nobody answered the escalation in time.
    EscalationTimedOut,
    /// The call needs review, no human started the turn (another agent's
    /// message did) and no client is attached to the agent
    /// ([`attendance`]).
    Unattended,
    /// Plan mode is on, the call is a message to an agent of another
    /// workspace (`label`), which plan mode puts to the user, and no
    /// client is attached to ask.
    PlanWired {
        /// The other agent, `<workspace>/<agent>`.
        label: String,
    },
}

impl DenyReason {
    /// Who made the call.
    #[must_use]
    pub fn decider(&self) -> Decider {
        match self {
            Self::UserRule { .. }
            | Self::Sealed { .. }
            | Self::Unavailable { .. }
            | Self::PlanMode
            | Self::PlanWired { .. }
            | Self::Unattended
            | Self::ClassifierUnavailable(_)
            | Self::ReviewIncomplete(_) => Decider::Policy,
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
            Self::UserRule { id, rule } => write!(
                f,
                "denied by the user's deny rule {id} (`{rule}`); only the user can lift it \
                 (`/permissions` lists and removes rules)"
            ),
            Self::Sealed { detail } => write!(
                f,
                "denied by rkat-tui's fixed policy, which no rule or answer opens: {detail}"
            ),
            Self::Unavailable { detail } => f.write_str(detail),
            Self::Classifier {
                rule,
                explanation,
                annotation,
                hint,
            } => {
                write!(f, "{CLASSIFIER_SOURCE}{explanation}")?;
                if let Some(rule) = rule {
                    write!(f, " [{rule}]")?;
                }
                if let Some(annotation) = annotation {
                    write!(f, " {annotation}")?;
                }
                if let Some(hint) = hint {
                    write!(f, ". {hint}")?;
                }
                Ok(())
            }
            Self::Human { comment: None } => {
                f.write_str("the user denied this action in the approval dialog")
            }
            Self::Human {
                comment: Some(comment),
            } => write!(f, "denied by the user: {comment}"),
            Self::Stopped => f.write_str("the user stopped this turn from the approval dialog"),
            Self::PlanMode => f.write_str(PLAN_MODE_DENIAL),
            Self::ClassifierUnavailable(err) => write!(
                f,
                "the safety classifier is unavailable ({err}) and no user could be asked; \
                 the user can answer such calls once attached, or switch to manual mode"
            ),
            Self::ReviewIncomplete(err) => write!(
                f,
                "{REVIEW_INCOMPLETE} ({err}); the call was not run, and this is not a refusal of \
                 the action: try it again in a moment, and if the review keeps failing say so \
                 in your answer, so the user can check the classifier or answer such calls \
                 themselves"
            ),
            Self::EscalationTimedOut => f.write_str(
                "the user did not answer the approval request in time (or one queued ahead of \
                 it went unanswered); say in your answer what you wanted to do, so the user can \
                 ask for it again",
            ),
            Self::Unattended => f.write_str(UNATTENDED_DENIAL),
            Self::PlanWired { label } => write!(
                f,
                "plan mode is on, and plan mode puts a message to {label}, an agent of another workspace, to the user: it covers what this agent sets in motion there too. No client is attached to ask, so it was not sent; the user can leave plan mode, or send it themselves"
            ),
        }
    }
}

/// How a classifier denial starts: its source.
pub const CLASSIFIER_SOURCE: &str = "the safety classifier blocked it: ";

/// How a failed review starts, for the agent and the user alike.
pub const REVIEW_INCOMPLETE: &str = "the safety review could not be completed";

/// What the agent is told after a classifier block (Codex's wording for
/// its reviewer's denials, adapted): no workaround, a materially safer way
/// or the user.
pub const BLOCK_GUIDANCE: &str = "Do not work around this block: do not try to reach the same \
effect another way (a different tool, a rewritten or encoded command, a script). Choose a \
materially safer alternative within the user's request, or stop and ask the user";

impl DenyReason {
    /// The reason as the agent reads it: what was denied, by whom and why,
    /// with the block rule's id but without how the classifier checked,
    /// which the notice and the dialog show.
    #[must_use]
    pub fn for_model(&self) -> String {
        match self {
            Self::Classifier {
                explanation,
                hint,
                rule,
                ..
            } => {
                let rule = rule
                    .as_ref()
                    .map_or_else(String::new, |rule| format!(" (rule {rule})"));
                match hint {
                    Some(hint) => format!("{CLASSIFIER_SOURCE}{explanation}{rule}. {hint}"),
                    None => format!("{CLASSIFIER_SOURCE}{explanation}{rule}"),
                }
            }
            other => other.to_string(),
        }
    }
}

/// Model-facing denial message.
#[must_use]
pub fn deny_message(reason: &DenyReason) -> String {
    let user_reason = reason;
    let reason = user_reason.for_model();
    if let DenyReason::Unavailable { .. } = user_reason {
        // Not a judgement of the action: nothing here can run it.
        return format!("Not available: {reason}.");
    }
    if let DenyReason::ReviewIncomplete(_) = user_reason {
        // Not a judgement of the action either: the review failed.
        return format!("Not run: {reason}.");
    }
    if let DenyReason::Human {
        comment: Some(_), ..
    } = user_reason
    {
        // The user said what to do instead: that is the way forward, not a
        // detour around the denial.
        return format!(
            "Blocked by auto-mode safety policy: {reason}. Do what the user says instead; do not \
             try to achieve the denied action's effect another way."
        );
    }
    if let DenyReason::Classifier { hint: Some(_), .. } = user_reason {
        // The way forward is named: the user's grant, or a place inside
        // the allowed scope.
        return format!(
            "Blocked by auto-mode safety policy: {reason}. Tell the user what would allow it, or \
             take the alternative named; do not try to reach the same place another way \
             (different tool, rewritten or encoded command, or a script)."
        );
    }
    if let DenyReason::Classifier { .. } = user_reason {
        return format!("Blocked by auto-mode safety policy: {reason}. {BLOCK_GUIDANCE}.");
    }
    format!(
        "Blocked by auto-mode safety policy: {reason}. Do not try to achieve the same effect \
         another way (different tool, rewritten or encoded command, or a script). Choose a safer \
         approach within the user's request, or stop and ask the user."
    )
}

/// What would allow a call the classifier blocked because `path` lies
/// outside the project and the directories the user allows: the user's
/// grant of its nearest existing directory (`--confinement-write`, or
/// `--confinement-read` for a read, or `[confinement]` in the user's own
/// `tui.toml`), `rule` (an allow rule's exact `/permissions add` line),
/// and the places inside the allowed scope to use instead (`temp`: the
/// temp directory the user allows).
#[must_use]
pub fn scope_hint(
    path: &std::path::Path,
    write: bool,
    rule: Option<&str>,
    temp: Option<&std::path::Path>,
) -> String {
    let dir = path
        .ancestors()
        .find(|dir| dir.is_dir())
        .unwrap_or(path)
        .display();
    let (flag, key) = if write {
        ("--confinement-write", "extra_write")
    } else {
        ("--confinement-read", "extra_read")
    };
    let rule = rule.map_or_else(String::new, |rule| {
        format!(", or allow this one call's kind with `{rule}`")
    });
    let temp = temp.map_or_else(String::new, |temp| {
        format!(" or the temp directory `{}`", temp.display())
    });
    format!(
        "`{}` is outside the project and the directories the user allows. The user can allow \
         `{dir}` by starting rkat-tui with `{flag} {dir}`, or with `{key} = [\"{dir}\"]` under \
         `[confinement]` in their own tui.toml (`~/.rkat/tui/tui.toml`) and a daemon restart \
         (`/daemon stop`){rule}; or use a directory inside the project{temp} instead",
        path.display()
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
        /// The session whose call it judged (the agent's, for its
        /// helpers' calls too).
        session_id: SessionId,
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
        let reason = DenyReason::Classifier {
            rule: Some("T3".into()),
            explanation: "force push rewrites shared history".into(),
            annotation: Some("[classifier checked 0 files]".into()),
            hint: None,
        };
        let msg = deny_message(&reason);
        assert!(
            msg.starts_with(
                "Blocked by auto-mode safety policy: the safety classifier blocked it: force push \
                 rewrites shared history (rule T3). Do not work around this block"
            ),
            "{msg}"
        );
        assert!(msg.contains("materially safer alternative"), "{msg}");
        assert!(msg.ends_with("or stop and ask the user."));
        // How the classifier checked is the user's: in the notice, never
        // in the model's text.
        assert!(
            !msg.contains("[T3]") && !msg.contains("classifier checked"),
            "{msg}"
        );
        // A failed review is not a refusal.
        let failed = deny_message(&DenyReason::ReviewIncomplete("classifier timed out".into()));
        assert!(
            failed.starts_with("Not run: the safety review could not be completed (classifier timed out); the call was not run, and this is not a refusal"),
            "{failed}"
        );
        assert!(!failed.contains("Blocked"), "{failed}");
        assert_eq!(
            reason.to_string(),
            "the safety classifier blocked it: force push rewrites shared history [T3] \
             [classifier checked 0 files]"
        );
    }

    /// Every denial says who denied it, for the agent and the user alike,
    /// and what the agent can do next.
    #[test]
    fn every_denial_names_its_source() {
        let cases = [
            (
                DenyReason::UserRule {
                    id: RuleId {
                        kind: rules::RuleKind::Deny,
                        serial: 3,
                    },
                    rule: "shell git push*".into(),
                },
                "the user's deny rule deny#3 (`shell git push*`)",
            ),
            (DenyReason::PlanMode, "plan mode is on"),
            (
                DenyReason::Human { comment: None },
                "the user denied this action in the approval dialog",
            ),
            (
                DenyReason::Human {
                    comment: Some("use b.txt".into()),
                },
                "denied by the user: use b.txt",
            ),
            (DenyReason::Stopped, "the user stopped this turn"),
            (
                DenyReason::EscalationTimedOut,
                "the user did not answer the approval request in time",
            ),
            (DenyReason::Unattended, "needs a user attached"),
            (
                DenyReason::ClassifierUnavailable("timed out".into()),
                "the safety classifier is unavailable (timed out) and no user could be asked",
            ),
            (
                DenyReason::ReviewIncomplete("classifier timed out".into()),
                REVIEW_INCOMPLETE,
            ),
            (
                DenyReason::Sealed {
                    detail: "`.git/x` is sealed".into(),
                },
                "rkat-tui's fixed policy",
            ),
            (
                DenyReason::Classifier {
                    rule: None,
                    explanation: "x".into(),
                    annotation: None,
                    hint: None,
                },
                CLASSIFIER_SOURCE,
            ),
        ];
        for (reason, source) in cases {
            let model = deny_message(&reason);
            assert!(model.contains(source), "{model}");
            assert!(reason.to_string().contains(source), "{reason}");
        }
    }

    /// A denial whose reason is a path outside the allowed scope names the
    /// path, the user's exact ways to allow it, and the places to use
    /// instead; the agent is told to relay it or take the alternative.
    #[test]
    fn a_scope_denial_says_how_to_allow_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("pnpm/store");
        std::fs::create_dir_all(&store).unwrap();
        let target = store.join("v3/files/ab");
        let hint = scope_hint(
            &target,
            true,
            Some("/permissions add allow shell pnpm install"),
            Some(std::path::Path::new("/tmp")),
        );
        let store = store.display();
        for part in [
            format!("`{}` is outside the project", target.display()),
            format!("`--confinement-write {store}`"),
            format!("`extra_write = [\"{store}\"]` under `[confinement]`"),
            "`/permissions add allow shell pnpm install`".to_string(),
            "a directory inside the project or the temp directory `/tmp`".to_string(),
        ] {
            assert!(hint.contains(&part), "{part}: {hint}");
        }
        let read = scope_hint(&target, false, None, None);
        assert!(
            read.contains("--confinement-read") && read.contains("extra_read"),
            "{read}"
        );
        let reason = DenyReason::Classifier {
            rule: Some("T3".into()),
            explanation: "pnpm writes its store outside the project".into(),
            annotation: None,
            hint: Some(hint.clone()),
        };
        let model = deny_message(&reason);
        assert!(model.contains(&hint), "{model}");
        assert!(
            model.contains("Tell the user what would allow it"),
            "{model}"
        );
        assert!(reason.to_string().contains(&hint));
    }

    #[test]
    fn deny_reasons_map_to_deciders() {
        let classifier = DenyReason::Classifier {
            rule: Some("D1".into()),
            explanation: "x".into(),
            annotation: None,
            hint: None,
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
