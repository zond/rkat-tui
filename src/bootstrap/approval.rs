//! The approval engine every session's gate shares.
//!
//! [`load_layers`] layers `tui.toml` (global → realm → command line →
//! project). [`build`] reads the environment facts,
//! builds the classifier and opens the escalation broker, and returns the
//! engine with the channels the runner serves.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use meerkat::{AgentFactory, LlmClient};
use meerkat_core::{AuthBindingRef, Config, Provider, SessionLlmIdentity};
use tokio::sync::mpsc;

use crate::approval::classifier::{
    ActionClassifier, Environment, GrantedDirs, LlmClassifier, UnavailableClassifier,
    classifier_default_model,
};
use crate::approval::engine::DEFAULT_ESCALATION_TIMEOUT;
use crate::approval::escalation::{EscalationBroker, EscalationRequest};
use crate::approval::{ApprovalConfig, ApprovalEngine, ApprovalEvent};
use crate::bootstrap::trust::HostTrustRecord;
use crate::cli::Args;
use crate::config::{self, AutoSettings, RULES_FILE_NAME};
use crate::error::TuiError;
use crate::overlay::ConfigOverlay;
use crate::paths::ProjectRoot;
use crate::tools::ReadOnlyProjectTools;

use super::scope::RuntimeScope;

/// Capacity of the escalation channel.
const ESCALATION_CAPACITY: usize = 16;

/// Engine-level overrides for the approval setup (tests and embedders).
#[derive(Clone, Default)]
pub struct ApprovalOverrides {
    /// Review Tier-3 calls with this classifier instead of the LLM one.
    pub classifier: Option<Arc<dyn ActionClassifier>>,
    /// How long the human has to answer an escalation
    /// (default [`DEFAULT_ESCALATION_TIMEOUT`]).
    pub escalation_timeout: Option<Duration>,
}

/// The approval engine plus what the runner serves.
pub struct ApprovalParts {
    /// The engine every session's gate uses.
    pub engine: Arc<ApprovalEngine>,
    /// Escalations for the human (taken by the runner).
    pub escalations: Option<mpsc::Receiver<EscalationRequest>>,
    /// Decisions, pauses and classifier usage (taken by the runner).
    pub events: Option<mpsc::UnboundedReceiver<ApprovalEvent>>,
    /// Observations from loading the rules, shown once.
    pub notices: Vec<String>,
    /// The realm's `tui.toml` as rkat-tui writes it (the daemon saves the
    /// checklist view through it).
    pub realm_file: Option<crate::config::RealmFile>,
}

impl std::fmt::Debug for ApprovalParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalParts")
            .field("engine", &self.engine)
            .field("notices", &self.notices)
            .finish_non_exhaustive()
    }
}

/// What the classifier runs on.
pub(crate) struct ClassifierSource<'a> {
    /// The factory the sessions are built with (its credential resolution
    /// builds the classifier's client).
    pub factory: &'a AgentFactory,
    /// Realm config.
    pub config: &'a Config,
    /// Provider of the start-up model.
    pub provider: Provider,
    /// The auth binding of the start-up endpoint (`None` for a built-in one).
    pub auth_binding: Option<AuthBindingRef>,
    /// The start-up endpoint, when the engine reaches it as a self-hosted
    /// server: the classifier runs on its alias of the classifier model.
    pub self_hosted: Option<crate::endpoints::Endpoint>,
    /// Client every session uses instead of resolving credentials, if any.
    pub llm_override: Option<Arc<dyn LlmClient>>,
}

/// Load the layered `tui.toml` (global, realm, project, command line) of
/// `scope`: rules, `[auto]` settings, endpoints and price overrides, plus
/// the notices loading produced.
///
/// # Errors
///
/// [`TuiError::Overlay`] for invalid command-line rule flags,
/// [`TuiError::Approval`] when a rules file cannot be read.
pub(crate) fn load_layers(
    scope: &RuntimeScope,
    args: &Args,
) -> Result<(config::Layered, Vec<String>), TuiError> {
    let cli_layer = ConfigOverlay::from_args(args)?.rules_file()?;
    let realm_dir = scope.realm_root();
    let mut notices = Vec::new();
    // The realm lives under the config directory, out of the agent's
    // confined shell's reach (the start refuses a plan that reaches it) and
    // a clone's: its `tui.toml` is the user's.
    let layered = config::load_layered_with(
        &realm_dir,
        Some(&scope.global.rules_file()),
        &scope.context_root.join(".rkat"),
        cli_layer.as_ref(),
    )
    .map_err(crate::approval::ApprovalError::from)?;
    notices.extend(layered.notices.iter().map(ToString::to_string));
    Ok((layered, notices))
}

/// The host trust record of `scope`'s workspace: keyed by its realm, so
/// the record follows the realm through a re-home.
#[must_use]
pub fn trust_record(scope: &RuntimeScope) -> HostTrustRecord {
    HostTrustRecord::open(&scope.global.config, &scope.context_root, &scope.home.id)
}

/// Build the approval engine for `scope` from the loaded `layered` rules.
///
/// # Errors
///
/// [`TuiError::Io`] when the current directory cannot be read.
pub(crate) async fn build(
    scope: &RuntimeScope,
    layered: config::Layered,
    mut notices: Vec<String>,
    (granted, containment): (GrantedDirs, crate::approval::classifier::Containment),
    source: ClassifierSource<'_>,
    overrides: &ApprovalOverrides,
) -> Result<ApprovalParts, TuiError> {
    let realm_dir = scope.realm_root();
    let realm_file = config::RealmFile::new(realm_dir.join(RULES_FILE_NAME));
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (broker, escalations) = EscalationBroker::new(ESCALATION_CAPACITY);
    let cwd = std::env::current_dir().map_err(|e| TuiError::io("read current directory", e))?;
    let environment = Environment::detect(scope.project_root.as_path(), &cwd)
        .await
        .with_granted(granted)
        .with_containment(containment);
    // The model the classifier runs on, and whether the start-up check
    // runs against it (not for a scripted client, nor when turned off).
    let mut model = None;
    let classifier = match &overrides.classifier {
        Some(classifier) => Arc::clone(classifier),
        None => {
            let probe = layered.settings.classifier_probe && source.llm_override.is_none();
            let named = classifier_model(&layered.settings, source.provider);
            let (classifier, notice) = make_classifier(
                &layered.settings,
                source,
                scope.project_root.clone(),
                events_tx.clone(),
            )
            .await;
            if notice.is_none() {
                model = named.map(|named| (named, probe));
            }
            notices.extend(notice);
            classifier
        }
    };
    let engine = ApprovalEngine::new(ApprovalConfig {
        root: scope.project_root.clone(),
        rules: layered.rules,
        settings: layered.settings,
        mode: layered.mode,
        classifier,
        broker: Some(broker),
        escalation_timeout: overrides
            .escalation_timeout
            .unwrap_or(DEFAULT_ESCALATION_TIMEOUT),
        environment,
        posix_shell: shell_is_posix(),
        events: events_tx,
        realm_file: Some(realm_file.clone()),
    });
    if let Some((model, probe)) = model {
        engine.set_classifier_model(model, probe);
    }
    Ok(ApprovalParts {
        engine,
        realm_file: Some(realm_file),
        escalations: Some(escalations),
        events: Some(events_rx),
        notices,
    })
}

/// The classifier's model: `[auto] classifier_model`, else the default
/// of the start-up model's `provider` ([`classifier_default_model`]).
#[must_use]
pub(crate) fn classifier_model(settings: &AutoSettings, provider: Provider) -> Option<String> {
    settings
        .classifier_model
        .clone()
        .or_else(|| classifier_default_model(provider).map(str::to_string))
}

/// The two-stage LLM classifier on the session's provider (model from
/// `[auto] classifier_model`, else [`classifier_default_model`]), built with
/// the factory's own credential resolution. Without a model or a client
/// every Tier-3 call is escalated to the human; the notice says why.
async fn make_classifier(
    settings: &AutoSettings,
    source: ClassifierSource<'_>,
    root: ProjectRoot,
    events: mpsc::UnboundedSender<ApprovalEvent>,
) -> (Arc<dyn ActionClassifier>, Option<String>) {
    let Some(model) = classifier_model(settings, source.provider) else {
        return (
            Arc::new(UnavailableClassifier),
            Some(format!(
                "no classifier model for provider {}: set [auto] classifier_model; until then every reviewed action asks you",
                source.provider.as_str()
            )),
        );
    };
    let client = match source.llm_override {
        Some(client) => client,
        None => {
            let identity = SessionLlmIdentity {
                model: source
                    .self_hosted
                    .as_ref()
                    .map_or_else(|| model.clone(), |endpoint| endpoint.engine_model(&model)),
                provider: source
                    .self_hosted
                    .as_ref()
                    .map_or(source.provider, crate::endpoints::Endpoint::engine_provider),
                self_hosted_server_id: source
                    .self_hosted
                    .as_ref()
                    .and_then(crate::endpoints::Endpoint::self_hosted_server),
                provider_params: None,
                auth_binding: source.auth_binding.clone(),
            };
            match source
                .factory
                .build_llm_client_for_identity(source.config, &identity)
                .await
            {
                Ok(client) => client,
                Err(err) => {
                    return (
                        Arc::new(UnavailableClassifier),
                        Some(format!(
                            "the safety classifier ({model}) is unavailable: {err}; every reviewed action asks you"
                        )),
                    );
                }
            }
        }
    };
    let mut classifier =
        LlmClassifier::new(client, source.provider, model).with_usage_events(events);
    // Stage 2 may read project files (read-only, bounded) to judge what a
    // command will actually execute.
    if settings.classifier_tools {
        classifier = classifier.with_project_tools(Arc::new(ReadOnlyProjectTools::new(root)));
    }
    (Arc::new(classifier), None)
}

/// Shells whose syntax the Tier-1 shell analysis understands.
const POSIX_SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "ash"];

/// The program the shell tool runs, as the tool resolves it (its default
/// shell, else, when that is `nu` and missing, `$SHELL`, bash, zsh or sh).
#[must_use]
pub fn resolved_shell_program() -> Option<std::path::PathBuf> {
    use meerkat_tools::builtin::shell::ShellConfig;

    let default = ShellConfig::default();
    if let Ok(path) = default.resolve_shell_path() {
        return Some(path);
    }
    if default.shell != "nu" {
        return None;
    }
    let shell_env = std::env::var("SHELL").ok();
    let mut candidates: Vec<String> = Vec::new();
    if let Some(env) = shell_env.as_deref().map(str::trim)
        && !env.is_empty()
        && env != default.shell
    {
        candidates.push(env.to_string());
    }
    for name in ["bash", "zsh", "sh"] {
        if !candidates.iter().any(|c| c == name) {
            candidates.push(name.to_string());
        }
    }
    candidates.into_iter().find_map(|shell| {
        ShellConfig {
            shell,
            ..ShellConfig::default()
        }
        .resolve_shell_path()
        .ok()
    })
}

/// Whether `program` is a POSIX shell.
#[must_use]
pub fn is_posix_shell(program: &Path) -> bool {
    program
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| POSIX_SHELLS.contains(&n))
}

/// Whether the shell tool will run a POSIX shell. Unknown or non-POSIX
/// programs (the engine default is `nu`) disable Tier-1 shell analysis.
#[must_use]
pub fn shell_is_posix() -> bool {
    cfg!(unix) && resolved_shell_program().is_some_and(|p| is_posix_shell(&p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_shell_names() {
        assert!(is_posix_shell(Path::new("/bin/bash")));
        assert!(is_posix_shell(Path::new("/usr/bin/sh")));
        assert!(!is_posix_shell(Path::new("/usr/bin/nu")));
        assert!(!is_posix_shell(Path::new("/usr/bin/fish")));
    }
}
