//! What the daemon builds before it composes its engine host: the realm,
//! the config, the agent factory, the approval engine and the client's
//! view of the workspace.
//!
//! 1. [`read_layers`] resolves the [`RuntimeScope`] and reads the layered
//!    `tui.toml` once ([`Layers`]; for `--isolated` this mints a fresh
//!    realm id, so call it once); [`prepare`] adds the realm [`Config`].
//! 2. [`assemble`] picks the start-up model ([`plan_model`]) on the
//!    discovered endpoints ([`crate::endpoints`]), lowers `--confinement`
//!    into the shell's OS confinement ([`confinement::plan`], installed
//!    with `AgentFactory::with_shell_confinement`), builds the approval
//!    engine ([`approval::build`]), opens the realm persistence, takes the
//!    realm lease and builds the factory; the daemon composes its
//!    JSON-RPC runtime and mob host over the result (`crate::daemon`),
//!    and every member and helper it seats carries the gate as its
//!    `ToolDispatchAdmission`.
//!
//! The factory reads the realm's own `config.toml` through its owner
//! ([`realm_config`]) on every build. The configured endpoints are
//! projected into that document as realm connections
//! ([`crate::endpoints::project_endpoints`]), and models only an endpoint
//! knows get a `[models.<id>]` profile there
//! ([`crate::endpoints::ensure_model_profile`]), so a session's auth binding
//! and model resolve for `rkat`, REST and RPC on the same realm too.
//!
//! Context ([`instructions`], [`memory_probe`], [`mcp`]): a user-global
//! `SYSTEM.md` becomes the config's `system_prompt_file` when the realm sets
//! no prompt; the factory runs with native semantic memory (`memory_search`,
//! compaction-time indexing) when the realm's index opens; the effective
//! MCP set (realm `[[tools.mcp_servers]]`, project and user `mcp.toml`) is
//! loaded once and given to every session build as its declarative servers.

pub mod approval;
pub mod confinement;
pub mod credentials;
pub mod dirs;
pub mod instructions;
pub mod mcp;
pub mod persona;
pub mod project_trust;
pub mod realm_config;
pub mod realm_home;
pub mod scope;
pub mod trust;

use std::path::PathBuf;
use std::sync::Arc;

use meerkat::{AgentFactory, LlmClient};
use meerkat_core::connection::RealmId;
use meerkat_core::{AuthBindingRef, Config, ConfigStore, McpServerConfig, Provider};
use meerkat_store::{
    RealmBackend, RealmLeaseGuard, realm_lease_dir, realm_paths_in, start_realm_lease_in,
};
use meerkat_tools::builtin::shell::ShellConfinement;

use crate::cli::Args;
use crate::endpoints::{
    EndpointError, EndpointName, EndpointProjection, EndpointRef, EndpointRegistry, ModelRefError,
    StartupPlan, StartupRequest, ensure_aliases, ensure_model_profile, needs_lookup, plan_startup,
    project_endpoints,
};
use crate::error::TuiError;
use crate::models::{ModelCache, ModelLister, live_lister};
use crate::thinking::ThinkingTable;
use meerkat_core::lifecycle::run_primitive::AnthropicThinkingConfig;

pub use approval::{ApprovalOverrides, ApprovalParts};
pub use confinement::{ConfinementPlan, HostPaths};
pub use dirs::GlobalDirs;
pub use instructions::InstructionSources;
pub use persona::{CODING_PERSONA, HELPER_PERSONA};
pub use realm_config::RealmConfigStore;
pub use scope::{RuntimeScope, load_config, resolve_scope};

/// The model a session runs on, and where it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    /// Endpoint name (`anthropic`, `litellm`, …).
    pub endpoint: EndpointName,
    /// Model id as the endpoint knows it.
    pub model: String,
    /// The provider whose client speaks the endpoint's dialect.
    pub provider: Provider,
    /// The explicit auth binding of a configured endpoint (`None`: default
    /// resolution, the built-in endpoints).
    pub auth_binding: Option<AuthBindingRef>,
    /// The Anthropic thinking `[thinking]` configures for the model on
    /// this endpoint (`None`: no override; see [`crate::thinking`]).
    pub thinking: Option<AnthropicThinkingConfig>,
    /// The engine's model id: the alias `<endpoint>:<model>` on a
    /// self-hosted endpoint ([`crate::endpoints::Endpoint::engine_model`]),
    /// else the model.
    pub engine_model: String,
    /// The engine's provider ([`Provider::SelfHosted`] on a self-hosted
    /// endpoint, else [`Self::provider`]).
    pub engine_provider: Provider,
    /// The engine's self-hosted server (the endpoint's name), on a
    /// self-hosted endpoint.
    pub self_hosted_server: Option<String>,
}

impl ModelChoice {
    /// The choice with the thinking `table` configures for it.
    #[must_use]
    pub fn with_thinking(mut self, table: &ThinkingTable) -> Self {
        self.thinking = table.resolve(Some(self.endpoint.as_str()), self.provider, &self.model);
        self
    }

    /// The choice as an `endpoint:model` reference.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn reference(&self) -> EndpointRef {
        EndpointRef::new(self.endpoint.as_str(), self.model.clone())
    }
}

/// The choice for `reference` on `endpoints`.
fn choice_for(
    endpoints: &EndpointRegistry,
    reference: &EndpointRef,
) -> Result<ModelChoice, TuiError> {
    let endpoint = endpoints.get(&reference.endpoint).ok_or_else(|| {
        EndpointError::from(ModelRefError::UnknownEndpoint {
            name: reference.endpoint.clone(),
            known: endpoints.names(),
        })
    })?;
    Ok(ModelChoice {
        endpoint: EndpointName::new(endpoint.name.clone()),
        model: reference.model.clone(),
        provider: endpoint.provider(),
        auth_binding: endpoint.binding(),
        thinking: None,
        engine_model: endpoint.engine_model(&reference.model),
        engine_provider: endpoint.engine_provider(),
        self_hosted_server: endpoint.self_hosted_server(),
    })
}

/// Engine-level overrides for [`assemble`].
#[derive(Default, Clone)]
pub struct BootstrapOptions {
    /// Serve every session with this client instead of resolving provider
    /// credentials (tests, or a host that already built a client). With an
    /// override the user's provider token store is not opened, the
    /// classifier runs on the same client, and every built-in endpoint
    /// exists (keyless, so its list is not fetched).
    pub llm_override: Option<Arc<dyn LlmClient>>,
    /// Approval-engine overrides (classifier, escalation timeout).
    pub approval: ApprovalOverrides,
    /// Fetch model lists with this instead of the live endpoints.
    pub lister: Option<ModelLister>,
    /// The directories outside the workspace a confined shell may use
    /// (default: [`HostPaths::detect`]).
    pub host_paths: Option<HostPaths>,
    /// Tests that run the engine's own clients against a fake endpoint:
    /// never open the user's provider token store.
    #[cfg(any(test, feature = "test-support"))]
    pub without_token_store: bool,
}

/// The shell's extra grants: the command line's `--confinement-read`/
/// `--confinement-write`, then the `[confinement]` grants of the user's own
/// `tui.toml` layers (which yield to the flags, see
/// [`confinement::grants_with`]).
fn extra_grants(args: &Args, layered: &crate::config::Layered) -> confinement::ExtraGrants {
    let mut extra = confinement::ExtraGrants::from_args(args);
    extra.extend_from(&layered.confinement);
    extra
}

/// The realm `config` of `scope` as a process may read it: a workspace
/// realm config sets no confinement, and an untrusted one names no
/// endpoint, key source or skill source and starts no MCP server or hook
/// ([`project_trust::ProjectInputs::guard`]).
pub async fn guarded_config(scope: &RuntimeScope, args: &Args, mut config: Config) -> Config {
    let trust = Arc::new(parking_lot::Mutex::new(approval::trust_record(scope)));
    project_trust::ProjectInputs::new(scope, trust, args.trust_project)
        .guard(&mut config)
        .await;
    config
}

/// The user's `!` shell of `scope`, built from the agent shell's
/// confinement plan, curated environment and the `[shell]` timeout, for
/// the client, which runs no engine of its own.
///
/// # Errors
///
/// [`TuiError`] when the layered `tui.toml` cannot be read or the
/// confinement cannot be planned.
#[cfg(any(test, feature = "test-support"))]
pub fn user_shell(
    layers: &Layers,
    config: &Config,
    args: &Args,
    host_paths: &HostPaths,
) -> Result<crate::user_shell::UserShell, TuiError> {
    Ok(user_shell_with(
        layers,
        &shell_plan(layers, config, args, host_paths)?,
    ))
}

/// The agent shell's confinement plan `args` and the `tui.toml` layers
/// give over `config` on this host: what a daemon started with them
/// installs, and what the client compares a running daemon's with.
///
/// # Errors
///
/// [`TuiError`] when the confinement cannot be planned.
pub fn shell_plan(
    layers: &Layers,
    config: &Config,
    args: &Args,
    host_paths: &HostPaths,
) -> Result<ConfinementPlan, TuiError> {
    let Layers { scope, layered, .. } = layers;
    Ok(confinement::plan(
        args.effective_confinement(),
        scope.project_root.as_path(),
        config,
        host_paths,
        &extra_grants(args, layered),
    )?)
}

/// Refuse a shell confinement `plan` that reaches the realm directory
/// `realm` ([`realm_home::exposing_grant`]): the confined shell must not
/// read or rewrite the sessions, approvals, rules or the audit log. An
/// unconfined shell is the trusted host and passes.
///
/// # Errors
///
/// [`TuiError::RealmExposed`] naming the grant.
pub fn check_realm_private(
    plan: &ConfinementPlan,
    realm: &std::path::Path,
) -> Result<(), TuiError> {
    match realm_home::exposing_grant(plan, realm) {
        Some(grant) => Err(TuiError::RealmExposed {
            realm: realm.to_path_buf(),
            grant,
        }),
        None => Ok(()),
    }
}

/// [`user_shell`] from a [`shell_plan`].
#[must_use]
pub fn user_shell_with(layers: &Layers, plan: &ConfinementPlan) -> crate::user_shell::UserShell {
    let Layers { scope, layered, .. } = layers;
    crate::user_shell::UserShell::new(
        approval::resolved_shell_program(),
        scope.project_root.as_path(),
        &plan.shell,
        plan.shell_env.as_ref(),
        std::time::Duration::from_secs(layered.shell.user_timeout_secs),
    )
}

/// The workspace and its layered `tui.toml`, read once at start and handed
/// to the process that runs (the client or the daemon).
#[derive(Debug)]
pub struct Layers {
    /// The workspace and realm.
    pub scope: RuntimeScope,
    /// The `tui.toml` layers (global, realm, project, command line).
    pub layered: crate::config::Layered,
    /// What reading them had to say.
    pub notices: Vec<String>,
}

/// Resolve the runtime scope and read its `tui.toml` layers.
///
/// # Errors
///
/// See [`resolve_scope`]; [`TuiError::Overlay`] / [`TuiError::Approval`]
/// when the layers cannot be read.
pub fn read_layers(args: &Args) -> Result<Layers, TuiError> {
    let scope = resolve_scope(args)?;
    let (layered, layer_notices) = approval::load_layers(&scope, args)?;
    let notices = scope
        .offer
        .clone()
        .into_iter()
        .chain(layer_notices)
        .collect();
    Ok(Layers {
        scope,
        layered,
        notices,
    })
}

/// [`read_layers`], then the realm config.
///
/// # Errors
///
/// See [`read_layers`] and [`load_config`].
pub async fn prepare(args: &Args) -> Result<(Layers, Config), TuiError> {
    let layers = read_layers(args)?;
    let config = load_config(&layers.scope).await?;
    Ok((layers, config))
}

/// What the daemon builds before it composes its engine host
/// (`crate::daemon`: the JSON-RPC `SessionRuntime` and its mob state).
/// The persistence is still unopened by any service; the realm lease is
/// held.
pub struct Assembled {
    /// Project, conventions roots and realm.
    pub scope: RuntimeScope,
    /// The realm document the factory reads on every build.
    pub realm_config: Arc<RealmConfigStore>,
    /// The effective config at start-up.
    pub effective: Config,
    /// Model of new sessions.
    pub choice: ModelChoice,
    /// The `[thinking]` settings.
    pub thinking: ThinkingTable,
    /// The LLM endpoints of this process.
    pub endpoints: EndpointRegistry,
    /// The shell's OS confinement.
    pub confinement: ConfinementPlan,
    /// The realm's stores, for the session substrate to take.
    pub persistence: meerkat::PersistenceBundle,
    /// Backend pinned in the realm manifest.
    pub backend: RealmBackend,
    /// This process's realm lease.
    pub realm_lease: RealmLeaseGuard,
    /// The agent factory every session and member is built with.
    pub factory: AgentFactory,
    /// The approval engine and the channels a UI serves.
    pub approval: ApprovalParts,
    /// The declarative MCP servers every session build carries.
    pub mcp_servers: Vec<McpServerConfig>,
    /// The workspace files that make the host run something, and their trust.
    pub project_inputs: project_trust::ProjectInputs,
    /// Whether sessions get native semantic memory.
    pub memory: bool,
    /// The engine's task store.
    pub tasks_db: PathBuf,
    /// The context-compaction policy of the `tui.toml` `[auto]` settings.
    pub compaction: crate::ui::compaction::CompactionPolicy,
    /// The start-up model's context window ([`model_window`]): what the
    /// compaction threshold is computed from.
    pub context_window: Option<u64>,
    /// How deep helpers may nest.
    pub helper_max_depth: u8,
    /// `[daemon] principal` of the user's `tui.toml` layers.
    pub daemon_principal: Option<String>,
    /// Start-up notices (warnings).
    pub notices: Vec<String>,
    /// The client every session uses instead of resolving credentials
    /// ([`BootstrapOptions::llm_override`]).
    pub llm_override: Option<Arc<dyn LlmClient>>,
}

impl Assembled {
    /// Where the instruction files of every session build come from.
    #[must_use]
    pub fn instructions(&self) -> InstructionSources {
        InstructionSources::of(&self.scope)
    }
}

/// Resolve endpoints, model, confinement, trust, approval engine and MCP
/// set, open the realm persistence, take the realm lease under
/// `lease_surface` and build the factory: everything the daemon composes
/// its engine host over.
///
/// # Errors
///
/// [`TuiError::Endpoint`] when no start-up model can be chosen,
/// [`TuiError::Confinement`] when `--confinement` cannot be enforced,
/// [`TuiError::PersistenceOpen`] / [`TuiError::RealmLease`] when the realm
/// cannot be opened, and [`TuiError::Overlay`] / [`TuiError::Approval`]
/// when the approval rules cannot be loaded.
pub async fn assemble(
    layers: Layers,
    config: Config,
    args: &Args,
    options: BootstrapOptions,
    lease_surface: &str,
) -> Result<Assembled, TuiError> {
    let Layers {
        scope,
        mut layered,
        notices: mut approval_notices,
    } = layers;
    let realm = RealmId::parse(scope.realm_id.clone()).ok();
    let trust = Arc::new(parking_lot::Mutex::new(approval::trust_record(&scope)));
    let project_inputs =
        project_trust::ProjectInputs::new(&scope, Arc::clone(&trust), args.trust_project);
    // Everything below reads the realm config as the guard leaves it: a
    // workspace realm config sets no confinement, and an untrusted one
    // names no endpoint, key source or skill source. `raw` is the document
    // as read, for the notices.
    let raw = config;
    let mut config = raw.clone();
    project_inputs.guard(&mut config).await;
    let (mut endpoints, endpoint_notices) =
        EndpointRegistry::discover(&config, realm.as_ref(), &layered.endpoints)?;
    if options.llm_override.is_some() {
        endpoints.add_keyless_builtins();
    }
    let lister = options.lister.clone().unwrap_or_else(live_lister);
    let (thinking, thinking_notices) =
        ThinkingTable::validate(std::mem::take(&mut layered.thinking), endpoints.as_slice());
    let (choice, mut models) = plan_model(args, &config, &endpoints, &lister).await?;
    // A configured endpoint is the authority for its models' facts: its
    // list is read at start even when the model was named explicitly.
    if models.is_none()
        && let Some(endpoint) = endpoints
            .get(choice.endpoint.as_str())
            .filter(|e| !e.builtin && e.has_key())
    {
        models = Some(ModelCache::from_results(lister(vec![endpoint.clone()]).await).0);
    }
    let choice = choice.with_thinking(&thinking);

    project_inputs.set_endpoint_projection(EndpointProjection::of(endpoints.as_slice())?);
    let realm_config = Arc::new(
        RealmConfigStore::new(
            &scope.config_path(),
            scope.config_state_path(),
            scope.global.clone(),
        )
        .with_inputs(project_inputs.clone()),
    );
    // Before any write: a profile rkat-tui wrote for a model the catalog
    // has since added would make the engine refuse the document.
    let owned_profiles = crate::endpoints::OwnedProfiles::in_realm(&scope.realm_root());
    let pruned =
        crate::endpoints::prune_catalogued_profiles(realm_config.runtime(), &owned_profiles)
            .await?;
    project_endpoints(realm_config.runtime(), endpoints.as_slice()).await?;
    let listed = models
        .as_ref()
        .and_then(|cache| cache.entry(choice.endpoint.as_str(), &choice.model));
    let endpoint = endpoints.get(choice.endpoint.as_str());
    match endpoint {
        Some(endpoint) if endpoint.is_self_hosted() => {
            // Every fact of a self-hosted endpoint's models is its own:
            // the start-up model, the classifier's and every model it
            // lists get an alias, so a switch or a helper finds its model
            // in the registry the session was built with.
            let classifier = approval::classifier_model(&layered.settings, choice.provider);
            let mut wanted: Vec<(&str, Option<&crate::models::ModelEntry>)> =
                vec![(choice.model.as_str(), listed)];
            if let Some(classifier) = classifier.as_deref()
                && classifier != choice.model
            {
                let entry = models
                    .as_ref()
                    .and_then(|cache| cache.entry(&endpoint.name, classifier));
                wanted.push((classifier, entry));
            }
            if let Some(Ok(list)) = models.as_ref().and_then(|cache| cache.get(&endpoint.name)) {
                for entry in list {
                    if !wanted.iter().any(|(id, _)| *id == entry.id) {
                        wanted.push((entry.id.as_str(), Some(entry)));
                    }
                }
            }
            ensure_aliases(realm_config.runtime(), endpoint, &wanted).await?;
        }
        _ => {
            ensure_model_profile(
                realm_config.runtime(),
                &owned_profiles,
                choice.provider,
                &choice.model,
                listed,
            )
            .await?;
        }
    }
    let effective = realm_config
        .get()
        .await
        .map_err(EndpointError::ConfigRead)?;
    let context_window = model_window(&effective, &endpoints, &choice, listed);

    let host_paths = options.host_paths.clone().unwrap_or_else(HostPaths::detect);
    let confinement = confinement::plan(
        args.effective_confinement(),
        scope.project_root.as_path(),
        &config,
        &host_paths,
        &extra_grants(args, &layered),
    )?;

    let realm_root = realm_paths_in(&scope.state_root, &scope.realm_id).root;
    check_realm_private(&confinement, &realm_root)?;
    let (manifest, persistence) = meerkat::open_realm_persistence_in(
        &scope.state_root,
        &scope.realm_id,
        scope.backend_hint,
        Some(scope.origin),
    )
    .await
    .map_err(|source| TuiError::PersistenceOpen {
        realm_root: realm_root.clone(),
        state_root: scope.state_root.clone(),
        realm_id: scope.realm_id.clone(),
        source: Box::new(source),
    })?;
    let realm_lease = start_realm_lease_in(&scope.state_root, &scope.realm_id, None, lease_surface)
        .await
        .map_err(|source| TuiError::RealmLease {
            realm_id: scope.realm_id.clone(),
            lease_dir: realm_lease_dir(&realm_paths_in(&scope.state_root, &scope.realm_id)),
            source: Box::new(source),
        })?;
    let store_path = persistence
        .store_path()
        .map_or_else(|| realm_root.clone(), PathBuf::from);

    let mut notices: Vec<String> = Vec::new();
    notices.extend(crate::endpoints::pruned_notice(&pruned));
    let memory = memory_probe(&store_path, manifest.backend, &mut notices);
    let tasks_db = tasks_db(&store_path);
    let factory = build_factory(
        &scope,
        store_path,
        FactoryToggles {
            token_store: options.llm_override.is_none() && !without_token_store(&options),
            memory,
        },
        &confinement.shell,
        Arc::new(project_inputs.clone()),
    );
    let compaction = layered.settings.compaction_policy(None);
    approval_notices.extend(endpoint_notices.iter().map(ToString::to_string));
    approval_notices.extend(thinking_notices);
    let helper_depth = layered.helpers.max_depth;
    let daemon_principal = layered.daemon_principal.clone();
    let approval = approval::build(
        &scope,
        layered,
        approval_notices,
        approval::ClassifierSource {
            factory: &factory,
            config: &effective,
            provider: choice.provider,
            auth_binding: choice.auth_binding.clone(),
            self_hosted: endpoints
                .get(choice.endpoint.as_str())
                .filter(|endpoint| endpoint.is_self_hosted())
                .cloned(),
            llm_override: options.llm_override.clone(),
        },
        &options.approval,
        trust,
    )
    .await?;
    let mcp = mcp::load(&scope, &raw, &project_inputs).await?;
    let mut refused = mcp.refused;
    for item in project_inputs.guard(&mut raw.clone()).await {
        if !refused.contains(&item) {
            refused.push(item);
        }
    }
    notices.extend(project_inputs.notice(&refused));
    notices.extend(project_inputs.ignored_confinement(&raw));
    notices.extend(mcp.notices);
    Ok(Assembled {
        scope,
        realm_config,
        effective,
        choice,
        thinking,
        endpoints,
        confinement,
        persistence,
        backend: manifest.backend,
        realm_lease,
        factory,
        approval,
        mcp_servers: mcp.servers,
        project_inputs,
        memory,
        tasks_db,
        compaction,
        context_window,
        helper_max_depth: helper_depth,
        daemon_principal,
        notices,
        llm_override: options.llm_override,
    })
}

/// The context window of `choice`'s model: a configured endpoint's own
/// listing (`listed`), else the realm's model registry; for a built-in
/// endpoint the registry (the catalog, or a profile written from a
/// listing), else the listing.
#[must_use]
pub fn model_window(
    config: &Config,
    endpoints: &EndpointRegistry,
    choice: &ModelChoice,
    listed: Option<&crate::models::ModelEntry>,
) -> Option<u64> {
    let registry = || {
        crate::endpoints::registry_context_window(
            config,
            choice.engine_provider,
            &choice.engine_model,
        )
    };
    let listed = || listed.and_then(|entry| entry.context_window);
    if endpoints
        .get(choice.endpoint.as_str())
        .is_some_and(|e| !e.builtin)
    {
        listed().or_else(registry)
    } else {
        registry().or_else(listed)
    }
}

/// Whether `options` keep the user's token store closed (tests only).
#[cfg(any(test, feature = "test-support"))]
fn without_token_store(options: &BootstrapOptions) -> bool {
    options.without_token_store
}

#[cfg(not(any(test, feature = "test-support")))]
fn without_token_store(_: &BootstrapOptions) -> bool {
    false
}

/// The canonical home directory, if any.
fn dirs_home() -> Option<PathBuf> {
    ::dirs::home_dir().map(|home| std::fs::canonicalize(&home).unwrap_or(home))
}

/// The SQLite file the factory keeps every session's tasks in (the task
/// tools' [`meerkat_tools::SqliteTaskStore`], scoped by session id).
/// Upstream ask: the facade names this path only inside its dispatcher
/// build (`store_path.join("tasks.db")`), so it is mirrored here.
#[must_use]
pub fn tasks_db(store_path: &std::path::Path) -> PathBuf {
    store_path.join("tasks.db")
}

/// Directory of the realm's semantic-memory index (where the factory opens
/// its `HnswMemoryStore`).
#[must_use]
pub fn memory_dir(store_path: &std::path::Path) -> PathBuf {
    store_path.join("memory")
}

/// Whether sessions get native semantic memory: the realm keeps files (not
/// an in-memory backend) and its index opens. The index is opened once
/// here so a failure (an unwritable or damaged index) degrades
/// to a start-up notice instead of failing every session build.
fn memory_probe(
    store_path: &std::path::Path,
    backend: RealmBackend,
    notices: &mut Vec<String>,
) -> bool {
    if backend == RealmBackend::Memory {
        return false;
    }
    let dir = memory_dir(store_path);
    match meerkat_memory::HnswMemoryStore::open(&dir) {
        Ok(_) => true,
        Err(err) => {
            notices.push(format!(
                "semantic memory is off for this run (memory_search unavailable, compaction does not index): cannot open {}: {err}",
                dir.display()
            ));
            false
        }
    }
}

/// The agent factory every session is built with.
///
/// `project_root` and `context_root` are the workspace, so the factory's
/// native `AGENTS.md`, skills and hooks discovery see the project. Builtin
/// tools and the shell are on: every call passes the approval gate each
/// member carries, and the shell runs under the OS confinement the caller
/// installs ([`confinement::plan`]). Mob tools are on (helpers, see
/// [`crate::mobs`]); semantic memory when `toggles.memory`
/// (the index lives under `store_path`, see [`memory_dir`]). The file
/// and note tools come in per member as external tools (see
/// `crate::daemon::engine`). The factory's `runtime_root` is the realm, so every
/// shell call runs in durable process custody (`<realm>/process_custody`),
/// which the runtime-backed composition settles on every attachment.
/// `with_token_store` attaches the user's provider token store, so OAuth and
/// stored keys resolve like they do for `rkat`.
/// Optional factory capabilities.
#[derive(Debug, Clone, Copy)]
struct FactoryToggles {
    /// Attach the user's provider token store.
    token_store: bool,
    /// Native semantic memory (`memory_search`, compaction indexing).
    memory: bool,
}

fn build_factory(
    scope: &RuntimeScope,
    store_path: PathBuf,
    toggles: FactoryToggles,
    shell_confinement: &ShellConfinement,
    hook_source_trust: Arc<dyn meerkat::HookSourceTrust>,
) -> AgentFactory {
    let mut factory = AgentFactory::new(store_path)
        .with_shell_confinement(shell_confinement.clone())
        // The factory judges the project hook file over the bytes it reads
        // on each build (see `project_trust`).
        .with_hook_source_trust(hook_source_trust)
        .runtime_root(scope.realm_root())
        .project_root(scope.project_root.as_path())
        .context_root(scope.context_root.clone())
        .builtins(true)
        .shell(true)
        .memory(toggles.memory)
        .mob(true);
    if let Some(user_root) = &scope.user_config_root {
        factory = factory.user_config_root(user_root.clone());
    }
    if toggles.token_store {
        factory = match factory.clone().with_default_token_store() {
            Ok(with_store) => with_store,
            Err(err) => {
                tracing::warn!(error = %err, "provider token store unavailable; using environment keys only");
                factory
            }
        };
    }
    factory
}

/// The model of new sessions, and the lists fetched to choose it.
///
/// `--model` (`endpoint:id` or a bare id), `--endpoint` and the
/// realm config's `agent.model` are planned with [`plan_startup`]; the
/// endpoints' lists are fetched first when a bare `--model` id could belong
/// to several endpoints, or when the plan asks for an endpoint's first listed
/// model.
///
/// # Errors
///
/// [`TuiError::Endpoint`] when no endpoint is configured, the reference does
/// not resolve, or the endpoint to take the first model of lists none.
pub async fn plan_model(
    args: &Args,
    config: &Config,
    endpoints: &EndpointRegistry,
    lister: &ModelLister,
) -> Result<(ModelChoice, Option<ModelCache>), TuiError> {
    let request = StartupRequest {
        model: args.model.as_deref(),
        endpoint: args.endpoint.as_deref(),
        config_model: &config.agent.model,
    };
    let fetch = || async {
        let listable = endpoints.iter().filter(|e| e.has_key()).cloned().collect();
        ModelCache::from_results(lister(listable).await).0
    };
    let mut cache = None;
    if request.endpoint.is_none()
        && request
            .model
            .is_some_and(|model| needs_lookup(model, endpoints.as_slice()))
    {
        cache = Some(fetch().await);
    }
    let index = cache.as_ref().map(ModelCache::index).unwrap_or_default();
    let reference =
        match plan_startup(request, endpoints.as_slice(), &index).map_err(EndpointError::from)? {
            StartupPlan::Resolved(reference) => reference,
            StartupPlan::FirstListed(name) => {
                if cache.is_none() {
                    cache = Some(fetch().await);
                }
                let first = cache
                    .as_ref()
                    .and_then(|cache| cache.first(&name))
                    .ok_or_else(|| {
                        let why = cache
                            .as_ref()
                            .and_then(|cache| cache.get(&name))
                            .and_then(|list| list.as_ref().err().map(ToString::to_string))
                            .unwrap_or_else(|| "it lists no models".to_string());
                        TuiError::from(EndpointError::NothingListed {
                            endpoint: name.clone(),
                            why,
                        })
                    })?;
                EndpointRef::new(name, first.id.clone())
            }
        };
    Ok((choice_for(endpoints, &reference)?, cache))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::endpoints::{Endpoint, SecretString, WireDialect};
    use crate::models::{EndpointListing, ModelEntry, ModelSource};
    use clap::Parser;

    fn args(extra: &[&str]) -> Args {
        let mut argv = vec!["rkat-tui"];
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv).unwrap()
    }

    fn ep(name: &str, dialect: WireDialect, builtin: bool) -> Endpoint {
        Endpoint {
            name: name.to_string(),
            dialect,
            base_url: (!builtin).then(|| "http://127.0.0.1:4000".to_string()),
            api_key: SecretString::new("k"),
            key_env: None,
            builtin,
            priority: 0,
            api_style: crate::endpoints::ApiStyle::default(),
        }
    }

    fn entry(id: &str) -> ModelEntry {
        ModelEntry {
            id: id.to_string(),
            display_name: None,
            source: ModelSource::Provider,
            context_window: Some(64_000),
            max_output_tokens: None,
            capabilities: Default::default(),
        }
    }

    /// Lists `litellm: [llama3:8b, gpt-5.5]`, `openai: [gpt-5.5]`.
    fn lister() -> ModelLister {
        Arc::new(|endpoints: Vec<Endpoint>| {
            Box::pin(async move {
                endpoints
                    .into_iter()
                    .map(|e| {
                        let models = match e.name.as_str() {
                            "litellm" => vec![entry("llama3:8b"), entry("gpt-5.5")],
                            "openai" => vec![entry("gpt-5.5")],
                            _ => Vec::new(),
                        };
                        (
                            e.name,
                            Ok(EndpointListing {
                                models,
                                prices: Vec::new(),
                            }),
                        )
                    })
                    .collect()
            })
        })
    }

    fn registry() -> EndpointRegistry {
        EndpointRegistry::from_endpoints(vec![
            ep("anthropic", WireDialect::Anthropic, true),
            ep("openai", WireDialect::OpenAi, true),
            ep("litellm", WireDialect::OpenAi, false),
        ])
    }

    async fn plan(extra: &[&str], config_model: &str) -> Result<ModelChoice, TuiError> {
        let mut config = Config::default();
        config.agent.model = config_model.to_string();
        plan_model(&args(extra), &config, &registry(), &lister())
            .await
            .map(|(choice, _)| choice)
    }

    #[tokio::test]
    async fn startup_model_planning() {
        let choice = plan(&["--model", "claude-sonnet-4-6"], "").await.unwrap();
        assert_eq!(choice.endpoint.as_str(), "anthropic");
        assert_eq!(choice.provider, Provider::Anthropic);
        assert!(choice.auth_binding.is_none());
        // A bare id only one endpoint lists resolves by lookup.
        let choice = plan(&["--model", "llama3:8b"], "").await.unwrap();
        assert_eq!(
            (choice.endpoint.as_str(), choice.model.as_str()),
            ("litellm", "llama3:8b")
        );
        let binding = choice.auth_binding.unwrap();
        assert_eq!(binding.realm.as_str(), crate::endpoints::SELF_HOSTED_REALM);
        assert_eq!(binding.binding.as_str(), "litellm");
        // An OpenAI-dialect proxy runs the model as its alias.
        assert_eq!(choice.engine_model, "litellm:llama3:8b");
        assert_eq!(choice.engine_provider, Provider::SelfHosted);
        assert_eq!(choice.self_hosted_server.as_deref(), Some("litellm"));
        // Listed by both: the built-in of its dialect wins.
        let choice = plan(&["--model", "gpt-5.5"], "").await.unwrap();
        assert_eq!(choice.endpoint.as_str(), "openai");
        // Explicit endpoint.
        let choice = plan(&["--model", "litellm:gpt-5.5"], "").await.unwrap();
        assert_eq!(choice.endpoint.as_str(), "litellm");
        assert_eq!(choice.provider, Provider::OpenAI);
        // `--endpoint` with a bare id.
        let choice = plan(&["--endpoint", "litellm", "--model", "x"], "")
            .await
            .unwrap();
        assert_eq!(choice.reference().to_string(), "litellm:x");
        // `--endpoint` alone on a configured endpoint: its first listed model.
        let choice = plan(&["--endpoint", "litellm"], "").await.unwrap();
        assert_eq!(choice.reference().to_string(), "litellm:llama3:8b");
        // The config model.
        let choice = plan(&[], "gpt-5.5").await.unwrap();
        assert_eq!(choice.reference().to_string(), "openai:gpt-5.5");
        let err = plan(&["--endpoint", "nope"], "").await.unwrap_err();
        assert!(err.to_string().contains("unknown endpoint `nope`"), "{err}");
    }

    #[test]
    fn tui_toml_confinement_grants_reach_the_plan() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ws = root.join("ws");
        let cfg = root.join("cfg");
        let (global_dir, overlay_dir, flag_dir) =
            (root.join("global"), root.join("overlay"), root.join("flag"));
        for d in [&ws, &cfg, &global_dir, &overlay_dir, &flag_dir] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(
            cfg.join("tui.toml"),
            format!(
                "[confinement]\nextra_read = [{:?}]\nextra_write = [{:?}]\n",
                global_dir.to_str().unwrap(),
                flag_dir.to_str().unwrap()
            ),
        )
        .unwrap();
        let set = format!(
            "confinement.extra_read=[{:?}]",
            overlay_dir.to_str().unwrap()
        );
        let args = args(&[
            "-w",
            ws.to_str().unwrap(),
            "--config-dir",
            cfg.to_str().unwrap(),
            "--set",
            &set,
            "--confinement-read",
            flag_dir.to_str().unwrap(),
        ]);
        let scope = resolve_scope(&args).unwrap();
        let (layered, _) = approval::load_layers(&scope, &args).unwrap();
        let extra = extra_grants(&args, &layered);
        assert_eq!(extra.read, vec![flag_dir.clone()]);
        assert_eq!(
            extra.file.extra_read,
            vec![global_dir.clone(), overlay_dir.clone()]
        );
        assert_eq!(extra.file.extra_write, vec![flag_dir.clone()]);
        // The flag wins over the file's write grant of the same directory.
        let (write, read, _) =
            confinement::grants_with(&ws, &HostPaths::default(), &extra).unwrap();
        assert!(
            read.contains(&flag_dir) && !write.contains(&flag_dir),
            "{write:?}"
        );
        assert!(
            read.contains(&global_dir) && read.contains(&overlay_dir),
            "{read:?}"
        );
        let requirement = confinement::lower_with(
            args.effective_confinement(),
            scope.project_root.as_path(),
            &HostPaths::default(),
            &extra,
        )
        .unwrap()
        .unwrap();
        let shown = format!("{requirement:?}");
        assert!(shown.contains(global_dir.to_str().unwrap()), "{shown}");
    }

    #[test]
    fn each_confinement_mode_reaches_the_factory_shell() {
        let dir = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(dir.path()).unwrap();
        let ws_arg = ws.to_str().unwrap();
        let cfg = tempfile::tempdir().unwrap();
        let cfg_arg = cfg.path().to_str().unwrap();
        for mode in ["workspace", "strict", "off"] {
            let args = args(&["-w", ws_arg, "--config-dir", cfg_arg, "--confinement", mode]);
            let scope = resolve_scope(&args).unwrap();
            let shell = match confinement::lower(
                args.effective_confinement(),
                scope.project_root.as_path(),
                &HostPaths::default(),
            )
            .unwrap()
            {
                Some(requirement) => ShellConfinement::Required { requirement },
                None => ShellConfinement::TrustedHost,
            };
            let factory = build_factory(
                &scope,
                ws.join("store"),
                FactoryToggles {
                    token_store: false,
                    memory: false,
                },
                &shell,
                Arc::new(project_trust::ProjectInputs::new(
                    &scope,
                    Arc::new(parking_lot::Mutex::new(approval::trust_record(&scope))),
                    false,
                )),
            );
            assert_eq!(factory.shell_confinement(), &shell, "{mode}");
            assert_eq!(
                mode == "off",
                shell == ShellConfinement::TrustedHost,
                "{mode}"
            );
        }
    }
}
