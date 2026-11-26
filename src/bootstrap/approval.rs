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
use meerkat_core::{Config, Provider, SessionLlmIdentity};
use tokio::sync::mpsc;

use crate::approval::classifier::{
    ActionClassifier, Environment, GrantedDirs, LlmClassifier, UnavailableClassifier,
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
    /// The model `classifier` stands for (`/status`, `/auto check`; the
    /// start-up check does not run on it). `None`: no model is named.
    pub classifier_model: Option<String>,
}

/// The approval engine plus what the runner serves.
pub struct ApprovalParts {
    /// The engine every session's gate uses.
    pub engine: Arc<ApprovalEngine>,
    /// Escalations for the human (taken by the runner).
    pub escalations: Option<mpsc::Receiver<EscalationRequest>>,
    /// Decisions, pauses and classifier usage (taken by the runner).
    pub events: Option<mpsc::UnboundedReceiver<ApprovalEvent>>,
    /// Observations from loading the rules, shown once as warnings.
    pub notices: Vec<String>,
    /// What the setup chose that the user may want to change (the model
    /// the classifier reviews with), shown once as information.
    pub info: Vec<String>,
    /// The user's and the realm's `tui.toml` as rkat-tui writes them (the
    /// daemon's settings, rules, grants and statements).
    pub files: Option<crate::config::SettingsFiles>,
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
    /// The start-up endpoint: the classifier's when `[auto]
    /// classifier_model` names a model without an endpoint, or none.
    pub endpoint: crate::endpoints::Endpoint,
    /// The start-up model: the classifier's when `[auto] classifier_model`
    /// names none (no model id of ours is a default).
    pub model: String,
    /// Client every session uses instead of resolving credentials, if any.
    pub llm_override: Option<Arc<dyn LlmClient>>,
    /// The process's endpoints: `[auto] classifier_model` may name one
    /// (`endpoint:model`, as `/auto model` writes it).
    pub endpoints: &'a crate::endpoints::EndpointRegistry,
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
    let overlay = ConfigOverlay::from_args(args)?;
    let cli_layer = overlay.rules_file()?;
    let realm_dir = scope.realm_root();
    let mut notices = Vec::new();
    // The realm lives under the config directory, out of the agent's
    // confined shell's reach (the start refuses a plan that reaches it) and
    // a clone's: its `tui.toml` is the user's.
    let layered = config::load_layered_replacing(
        &realm_dir,
        Some(&scope.global.rules_file()),
        &scope.context_root.join(".rkat"),
        cli_layer.as_ref(),
        overlay.replaced(),
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
    let files =
        config::SettingsFiles::new(scope.global.rules_file(), realm_dir.join(RULES_FILE_NAME));
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (broker, escalations) = EscalationBroker::new(ESCALATION_CAPACITY);
    let cwd = std::env::current_dir().map_err(|e| TuiError::io("read current directory", e))?;
    let environment = Environment::detect(scope.project_root.as_path(), &cwd)
        .await
        .with_granted(granted)
        .with_containment(containment)
        .with_own_state(own_state(scope));
    // The model the classifier runs on, and whether the start-up check
    // runs against it (not for a scripted client, nor when turned off).
    let mut model = None;
    let mut info = Vec::new();
    let mut check = None;
    let mut builder = None;
    let classifier = match &overrides.classifier {
        Some(classifier) => {
            model = overrides
                .classifier_model
                .clone()
                .map(|named| (named, false));
            Arc::clone(classifier)
        }
        None => {
            let probe = layered.settings.classifier_probe && source.llm_override.is_none();
            let named = classifier_model(&layered.settings, &source.model);
            // A classifier model its endpoint's list does not allow stops
            // the start, as the agent's own model does.
            let (endpoint, target) = classifier_target(&named, &source);
            endpoint.check_model(&target).map_err(|err| {
                TuiError::from(crate::endpoints::EndpointError::from(
                    crate::endpoints::ModelRefError::from(err),
                ))
            })?;
            let build = classifier_builder(
                &source,
                layered.settings.classifier_tools,
                scope.project_root.clone(),
                events_tx.clone(),
            );
            let (classifier, checker, notice) = make_classifier(&build, &named, &source).await;
            match notice {
                Some(notice) => notices.push(notice),
                None => {
                    if layered.settings.classifier_model.is_none() {
                        info.push(default_classifier_notice(&named));
                    }
                    model = Some((named, probe));
                }
            }
            builder = Some(build);
            check = checker;
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
        files: Some(files.clone()),
    });
    if let Some((model, probe)) = model {
        engine.reviewer().set_model(model, probe);
    }
    if let Some(check) = check {
        engine.reviewer().set_check_classifier(check);
    }
    if let Some(builder) = builder {
        engine.reviewer().set_builder(builder);
    }
    Ok(ApprovalParts {
        engine,
        files: Some(files),
        escalations: Some(escalations),
        events: Some(events_rx),
        notices,
        info,
    })
}

/// rkat-tui's own configuration and state and the engine's user-global
/// configuration, as the classifier's environment names them: the config
/// directory as resolved (`--config-dir` too), `<user config root>/.rkat/`,
/// and every secret root of the project (the realms, the trust records,
/// the endpoint directories, the comms keys).
fn own_state(scope: &RuntimeScope) -> Vec<std::path::PathBuf> {
    let mut dirs = vec![scope.global.config.clone()];
    dirs.extend(
        scope
            .user_config_root
            .as_ref()
            .map(|root| root.join(".rkat")),
    );
    dirs.extend(scope.project_root.secret_roots().iter().cloned());
    dirs
}

/// The classifier's model: `[auto] classifier_model`, else the start-up
/// model `start` (no model id of ours is a default: the user picks a
/// cheaper one with `/auto model`).
#[must_use]
pub(crate) fn classifier_model(settings: &AutoSettings, start: &str) -> String {
    settings
        .classifier_model
        .clone()
        .unwrap_or_else(|| start.to_string())
}

/// The start-up notice when no `[auto] classifier_model` is set: which
/// model reviews, and how to pick another.
fn default_classifier_notice(model: &str) -> String {
    format!(
        "the safety classifier reviews with the agent's model ({model}); /auto model picks another (a cheaper one is usually enough)"
    )
}

/// The endpoint and model `named` (`[auto] classifier_model` or the
/// start-up model) runs on: `endpoint:model` on one of the process's
/// endpoints, as `/auto model` writes it; else a model of the start-up
/// endpoint.
fn classifier_target(
    named: &str,
    source: &ClassifierSource<'_>,
) -> (crate::endpoints::Endpoint, String) {
    crate::endpoints::EndpointRef::parse(named)
        .and_then(|named| Some((source.endpoints.get(&named.endpoint)?.clone(), named.model)))
        .unwrap_or_else(|| (source.endpoint.clone(), named.to_string()))
}

/// The two-stage LLM classifier on `named`'s endpoint and model
/// ([`classifier_target`]), built by `build` (the one builder `/auto
/// model` uses too), and the same classifier for the checks (`/auto
/// check`, the start-up check). Without a client every Tier-3 call is
/// escalated to the human; the notice says why.
async fn make_classifier(
    build: &crate::approval::reviewer::ClassifierBuilder,
    named: &str,
    source: &ClassifierSource<'_>,
) -> (
    Arc<dyn ActionClassifier>,
    Option<Arc<dyn ActionClassifier>>,
    Option<String>,
) {
    let (endpoint, model) = classifier_target(named, source);
    match build(endpoint, model).await {
        Ok(built) => (built.review, Some(built.check), None),
        Err(err) => (
            Arc::new(UnavailableClassifier),
            None,
            Some(format!(
                "the safety classifier ({named}) is unavailable: {err}; every reviewed action asks you"
            )),
        ),
    }
}

/// The engine identity of `model` on `endpoint` (its alias on a
/// self-hosted one, its auth binding).
fn endpoint_identity(endpoint: &crate::endpoints::Endpoint, model: &str) -> SessionLlmIdentity {
    let self_hosted = endpoint.is_self_hosted();
    SessionLlmIdentity {
        model: if self_hosted {
            endpoint.engine_model(model)
        } else {
            model.to_string()
        },
        provider: if self_hosted {
            endpoint.engine_provider()
        } else {
            endpoint.provider()
        },
        self_hosted_server_id: endpoint.self_hosted_server(),
        provider_params: None,
        auth_binding: endpoint.binding(),
    }
}

/// The reviews' classifier on `client` (stage 2 reading the project at
/// `tools_root` when given, its usage on `events`) and the checks'.
fn classifiers(
    client: Arc<dyn LlmClient>,
    provider: Provider,
    model: String,
    tools_root: Option<ProjectRoot>,
    events: mpsc::UnboundedSender<ApprovalEvent>,
) -> crate::approval::reviewer::BuiltClassifier {
    let check: Arc<dyn ActionClassifier> = Arc::new(LlmClassifier::new(
        Arc::clone(&client),
        provider,
        model.clone(),
    ));
    let mut review = LlmClassifier::new(client, provider, model).with_usage_events(events);
    // Stage 2 may read project files (read-only, bounded) to judge what a
    // command will actually execute.
    if let Some(root) = tools_root {
        review = review.with_project_tools(Arc::new(ReadOnlyProjectTools::new(root)));
    }
    crate::approval::reviewer::BuiltClassifier {
        review: Arc::new(review),
        check,
    }
}

/// How the classifier is built for a model of an endpoint, at start-up and
/// by `/auto model`: the factory's own credential resolution (or the
/// override client), the endpoint's identity ([`endpoint_identity`]),
/// stage 2 reading the project when `tools`.
fn classifier_builder(
    source: &ClassifierSource<'_>,
    tools: bool,
    root: ProjectRoot,
    events: mpsc::UnboundedSender<ApprovalEvent>,
) -> crate::approval::reviewer::ClassifierBuilder {
    let factory = source.factory.clone();
    let config = source.config.clone();
    let llm_override = source.llm_override.clone();
    Arc::new(move |endpoint, model| {
        let factory = factory.clone();
        let config = config.clone();
        let llm_override = llm_override.clone();
        let root = root.clone();
        let events = events.clone();
        Box::pin(async move {
            // `endpoints.<name>.models` holds for the classifier too: at
            // start, `/auto model`, `/set auto.classifier_model`, a file's
            // change of it; the checks run on the classifier built here.
            endpoint
                .check_model(&model)
                .map_err(|err| format!("{err}; pick one of them with /auto model"))?;
            let client = match llm_override {
                Some(client) => client,
                None => factory
                    .build_llm_client_for_identity(&config, &endpoint_identity(&endpoint, &model))
                    .await
                    .map_err(|err| {
                        format!("no client for {model} on endpoint {}: {err}", endpoint.name)
                    })?,
            };
            Ok(classifiers(
                client,
                endpoint.provider(),
                model,
                tools.then_some(root),
                events,
            ))
        })
    })
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
