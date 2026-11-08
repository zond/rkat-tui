//! Durable refusals: the engine's application tool policy binding.
//!
//! The approval gate ([`super::approvals::RootGate`]) is process-local: the
//! engine does not record it with the member. What it records, and refuses
//! to rebuild a member without, is the member's application tool policy
//! binding (`ApplicationToolPolicyBinding::Provider`), resolved against the
//! host's `ToolConsequencePolicyRegistry` at every build. The daemon
//! installs one provider ([`PROVIDER`]) with one policy ([`POLICY`]) and
//! binds the orchestrator, its same-mob helpers and the members of child
//! mobs (`delegate`) to it.
//!
//! The policy only narrows ("narrow-only"): it denies a call a user deny
//! rule matches or a file write into a sealed path (`.git/`, `.rkat/`), and
//! allows everything else, for the admission to decide. It never asks and
//! never allows what the admission would refuse. It reads the approval
//! engine's rules as they are when a snapshot is taken, so a `/rules add
//! deny …` applies to the next call; each change of the deny rules is a new
//! revision.

use std::sync::Arc;

use meerkat_core::{
    ApplicationToolPolicyBinding, PolicyDigest, PolicyEvaluationProvenance,
    PolicyEvaluationSupervisorConfig, PolicyId, PolicyProviderGeneration, PolicyProviderId,
    PolicyRevision, ToolConsequenceDenial, ToolConsequenceFailure, ToolConsequenceNarrowingPolicy,
    ToolConsequencePolicyRegistry, ToolConsequencePolicySnapshot, ToolConsequenceRequest,
    ToolConsequenceVerdict, ToolSourceKind,
};
use parking_lot::Mutex;
use serde_json::value::RawValue;

use crate::approval::catalog::ToolOrigin;
use crate::approval::policy::{Action, ActionKind};
use crate::approval::remote::describe_rule;
use crate::approval::rules::RuleSet;
use crate::approval::{ApprovalEngine, DENIAL_CODE, DenyReason, deny_message};
use crate::memory::NOTES_SOURCE;
use crate::paths::ProjectRoot;
use crate::tools::FILE_TOOLS_SOURCE;

/// The policy provider the daemon installs.
pub const PROVIDER: &str = "rkat-tui";

/// The one policy it serves.
pub const POLICY: &str = "deny-rules";

/// How long one evaluation may take. The engine's default (50 ms) marks the
/// provider unhealthy for good when a worker thread is merely scheduled
/// late on a loaded machine; the evaluation itself takes microseconds.
pub const EVALUATION_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// The binding every member of the daemon is built with.
///
/// # Panics
///
/// Never: the ids are constant and canonical.
#[must_use]
pub fn binding() -> ApplicationToolPolicyBinding {
    match (PolicyProviderId::new(PROVIDER), PolicyId::new(POLICY)) {
        (Ok(provider_id), Ok(policy_id)) => ApplicationToolPolicyBinding::Provider {
            provider_id,
            policy_id,
        },
        // Unreachable with the constants above; never widen to unmanaged.
        _ => ApplicationToolPolicyBinding::Inherit,
    }
}

/// The registry holding [`DenyRulesPolicy`] over `engine`.
///
/// # Errors
///
/// The engine's refusal to start the evaluator.
pub fn registry(
    engine: &Arc<ApprovalEngine>,
) -> Result<Arc<ToolConsequencePolicyRegistry>, ToolConsequenceFailure> {
    let policy = DenyRulesPolicy::new(Arc::clone(engine))?;
    let config = PolicyEvaluationSupervisorConfig {
        evaluation_deadline: EVALUATION_DEADLINE,
        ..PolicyEvaluationSupervisorConfig::default()
    };
    Ok(Arc::new(ToolConsequencePolicyRegistry::new(
        vec![Arc::new(policy) as Arc<dyn ToolConsequenceNarrowingPolicy>],
        config,
        None,
    )?))
}

/// The provider: snapshots of the engine's deny rules.
pub struct DenyRulesPolicy {
    engine: Arc<ApprovalEngine>,
    provider: PolicyProviderId,
    policy: PolicyId,
    /// The last digest served and its revision (revisions only grow).
    served: Mutex<(PolicyDigest, u64)>,
}

impl std::fmt::Debug for DenyRulesPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DenyRulesPolicy")
            .field("served", &*self.served.lock())
            .finish_non_exhaustive()
    }
}

impl DenyRulesPolicy {
    /// The provider over `engine`'s rules.
    ///
    /// # Errors
    ///
    /// Never with the constant ids.
    pub fn new(engine: Arc<ApprovalEngine>) -> Result<Self, ToolConsequenceFailure> {
        let invalid =
            |err: meerkat_core::PolicyIdentityError| ToolConsequenceFailure::InvalidProvenance {
                reason: err.to_string(),
            };
        let digest = digest_of(&engine.rules());
        Ok(Self {
            engine,
            provider: PolicyProviderId::new(PROVIDER).map_err(invalid)?,
            policy: PolicyId::new(POLICY).map_err(invalid)?,
            served: Mutex::new((digest, 1)),
        })
    }
}

/// The digest of what the policy decides on: the deny rules, in order.
fn digest_of(rules: &RuleSet) -> PolicyDigest {
    let text: Vec<String> = rules.deny().map(|(_, rule)| describe_rule(rule)).collect();
    PolicyDigest::from_canonical_bytes(text.join("\n").as_bytes())
}

impl ToolConsequenceNarrowingPolicy for DenyRulesPolicy {
    fn provider_id(&self) -> &PolicyProviderId {
        &self.provider
    }

    fn generation(&self) -> PolicyProviderGeneration {
        PolicyProviderGeneration(1)
    }

    fn snapshot(
        &self,
        policy_id: &PolicyId,
    ) -> Result<Arc<dyn ToolConsequencePolicySnapshot>, ToolConsequenceFailure> {
        if policy_id != &self.policy {
            return Err(ToolConsequenceFailure::PolicyMissing {
                provider_id: self.provider.clone(),
                policy_id: policy_id.clone(),
            });
        }
        let rules = self.engine.rules();
        let digest = digest_of(&rules);
        let revision = {
            let mut served = self.served.lock();
            if served.0 != digest {
                *served = (digest.clone(), served.1 + 1);
            }
            served.1
        };
        Ok(Arc::new(DenySnapshot {
            rules,
            root: self.engine.root().clone(),
            provenance: PolicyEvaluationProvenance {
                revision: PolicyRevision(revision),
                digest,
            },
        }))
    }
}

/// One revision of the deny rules.
pub struct DenySnapshot {
    rules: RuleSet,
    root: ProjectRoot,
    provenance: PolicyEvaluationProvenance,
}

impl DenySnapshot {
    /// Why `name(args)` is refused, if it is: a deny rule matching the call
    /// under any family its name may belong to (a refusal never depends on
    /// where the tool comes from), or a file write into a sealed path.
    #[must_use]
    pub fn refusal(&self, name: &str, args: &RawValue) -> Option<DenyReason> {
        let origins = [
            None,
            Some(ToolOrigin::native(ToolSourceKind::Shell)),
            Some(ToolOrigin::tui(FILE_TOOLS_SOURCE, name)),
            Some(ToolOrigin::tui(NOTES_SOURCE, name)),
        ];
        for origin in &origins {
            let action = Action::parse(name, args, origin.as_ref());
            let subjects = action.subjects(&self.root);
            if let Some(id) = self.rules.matches_deny(name, &subjects) {
                let rule = self.rules.get(id).map(describe_rule).unwrap_or_default();
                return Some(DenyReason::UserRule { id, rule });
            }
            if let ActionKind::FileWrite { paths } = &action.kind
                && let Some(path) = paths.iter().find(|path| {
                    self.root
                        .resolve(path)
                        .is_ok_and(|resolved| resolved.sealed)
                })
            {
                return Some(DenyReason::Sealed {
                    detail: format!(
                        "`{path}` is sealed: neither the agent nor a rule may write it"
                    ),
                });
            }
        }
        None
    }
}

impl ToolConsequencePolicySnapshot for DenySnapshot {
    fn provenance(&self) -> PolicyEvaluationProvenance {
        self.provenance.clone()
    }

    fn evaluate(&self, request: &ToolConsequenceRequest) -> ToolConsequenceVerdict {
        let Ok(args) = RawValue::from_string(request.arguments_json.clone()) else {
            // Malformed arguments are the admission's to judge.
            return ToolConsequenceVerdict::Allow;
        };
        match self.refusal(request.tool_name.as_str(), &args) {
            Some(reason) => ToolConsequenceVerdict::Deny(ToolConsequenceDenial::new(
                DENIAL_CODE,
                deny_message(&reason),
            )),
            None => ToolConsequenceVerdict::Allow,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::approval::rules::{Rule, RuleKind, RuleOrigin};

    fn snapshot(rules: RuleSet, root: &std::path::Path) -> DenySnapshot {
        DenySnapshot {
            rules,
            root: ProjectRoot::new(root).unwrap(),
            provenance: PolicyEvaluationProvenance {
                revision: PolicyRevision(1),
                digest: PolicyDigest::from_canonical_bytes(b""),
            },
        }
    }

    fn raw(value: &serde_json::Value) -> Box<RawValue> {
        serde_json::value::to_raw_value(value).unwrap()
    }

    #[test]
    fn deny_rules_and_sealed_writes_are_refused_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let mut rules = RuleSet::default();
        rules.push(Rule::new(RuleKind::Deny, "write_file", None, RuleOrigin::Realm).unwrap());
        rules.push(
            Rule::new(
                RuleKind::Deny,
                "shell",
                Some("git push*"),
                RuleOrigin::Realm,
            )
            .unwrap(),
        );
        rules.push(Rule::new(RuleKind::Allow, "shell", Some("ls*"), RuleOrigin::Realm).unwrap());
        let snap = snapshot(rules, dir.path());
        let write = raw(&serde_json::json!({"path": "a.txt", "content": "x"}));
        assert!(matches!(
            snap.refusal("write_file", &write),
            Some(DenyReason::UserRule { .. })
        ));
        let push = raw(&serde_json::json!({"command": "git push origin main"}));
        assert!(snap.refusal("shell", &push).is_some());
        let ls = raw(&serde_json::json!({"command": "ls -la"}));
        assert_eq!(
            snap.refusal("shell", &ls),
            None,
            "allow rules are the admission's"
        );
        let edit =
            raw(&serde_json::json!({"path": ".git/config", "old_string": "a", "new_string": "b"}));
        assert!(matches!(
            snap.refusal("edit_file", &edit),
            Some(DenyReason::Sealed { .. })
        ));
        let read = raw(&serde_json::json!({"path": "a.txt"}));
        assert_eq!(snap.refusal("read_file", &read), None);
    }

    #[test]
    fn the_binding_names_the_provider() {
        let ApplicationToolPolicyBinding::Provider {
            provider_id,
            policy_id,
        } = binding()
        else {
            unreachable!("a provider binding");
        };
        assert_eq!(
            (provider_id.as_str(), policy_id.as_str()),
            (PROVIDER, POLICY)
        );
    }
}
