//! Composing Meerkat's JSON-RPC host over rkat-tui's bootstrap.
//!
//! Mirrors `rkat-rpc`'s assembly (`SessionRuntime::new_with_config_store`,
//! `set_realm_context`, `set_config_runtime`, `require_hosting_mode`, the
//! mob state set before any `MethodRouter`), with these differences: the
//! factory, config store and confinement come from
//! [`crate::bootstrap::assemble`], so the realm is configured exactly as
//! for the in-process TUI; the mob state is this crate's own (not
//! `compose_rpc_mob_state`), carrying the TUI's spawn customizer with the
//! orchestrator as its [`RootMember`]; and the connection callback-tool
//! routes `compose_rpc_mob_state` installs are left out (their builders are
//! crate-private, and the daemon's members get their tools from the
//! customizer); and the approval engine the bootstrap built gates every
//! member ([`super::approvals`]), its escalations answered through the
//! runtime's approval service, its deny rules installed as the members'
//! application tool policy ([`super::policy`]).

use std::sync::Arc;

use meerkat::ToolGatewayBuilder;
use meerkat_core::{AgentToolDispatcher, ConfigRuntime, ConfigStore, Provider};
use meerkat_mob_mcp::MobMcpState;
use meerkat_rpc::router::NotificationSink;
use meerkat_rpc::session_runtime::SessionRuntime;
use meerkat_store::RealmLeaseGuard;
use tokio::sync::mpsc;

use super::approvals::{DaemonToolOrigins, Desk, RootGate};
use super::{DaemonError, MOB_ID, ORCHESTRATOR};
use crate::approval::ApprovalEngine;
use crate::bootstrap::{Assembled, CODING_PERSONA, RuntimeScope};
use crate::error::TuiError;
use crate::memory::{NoteStore, NoteToolDispatcher, Scope, prompt_index};
use crate::mobs::customizer::{HelperCustomizer, HelperTools, RootMember};
use crate::mobs::{HelperKey, HelperRegistry};
use crate::thinking::ThinkingTable;
use crate::tools::FileToolDispatcher;

/// What [`super::orchestrator::ensure`] spawns the orchestrator with.
#[derive(Debug, Clone)]
pub struct RootPlan {
    /// The start-up model.
    pub model: String,
    /// Its provider.
    pub provider: Provider,
    /// Its endpoint's auth binding.
    pub auth_binding: Option<meerkat_core::AuthBindingRef>,
    /// The declarative MCP servers (the profile carries them durably).
    pub mcp_servers: Vec<meerkat_core::McpServerConfig>,
    /// Whether the realm's semantic memory is on.
    pub memory: bool,
    /// The helper bookkeeping (the orchestrator registers as a root owner).
    pub registry: Arc<HelperRegistry>,
    /// The `[thinking]` of the start-up model (the profile's provider
    /// params on a fresh spawn).
    pub thinking: Option<meerkat_core::lifecycle::run_primitive::AnthropicThinkingConfig>,
}

/// The composed host.
pub struct Composed {
    pub runtime: Arc<SessionRuntime>,
    pub config_store: Arc<dyn ConfigStore>,
    pub state: Arc<MobMcpState>,
    pub root: RootPlan,
    pub lease: RealmLeaseGuard,
    pub scope: RuntimeScope,
    /// The approval engine every member's gate shares.
    pub engine: Arc<ApprovalEngine>,
    /// Where escalations wait for their decision.
    pub desk: Arc<Desk>,
    /// The desk's task.
    pub desk_task: tokio::task::JoinHandle<()>,
    /// The helper follower's task.
    pub follower_task: tokio::task::JoinHandle<()>,
    /// The task reading helpers' tool catalogs after their runs.
    pub helper_catalogs_task: tokio::task::JoinHandle<()>,
    /// The members' tool catalogs.
    pub origins: Arc<DaemonToolOrigins>,
    /// The `[thinking]` table (model switches over the wire).
    pub thinking: ThinkingTable,
    /// The workspace's project inputs and the trust record they are judged
    /// by (`rkat/trust`).
    pub project_inputs: crate::bootstrap::project_trust::ProjectInputs,
}

/// How the daemon composes its approval side (tests vary it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PolicyOptions {
    /// Leave the consequence-policy registry out (a daemon of an older
    /// version, as far as the members' durable binding is concerned).
    pub without_registry: bool,
}

/// Compose the runtime and the mob host over `parts`.
///
/// # Errors
///
/// [`DaemonError::Hosting`], [`DaemonError::Composition`],
/// [`DaemonError::Driver`], or [`DaemonError::Bootstrap`] when the
/// orchestrator's tools cannot be composed.
pub fn compose(parts: Assembled, policy: PolicyOptions) -> Result<Composed, DaemonError> {
    let instructions = parts.instructions();
    let Assembled {
        scope,
        realm_config,
        effective,
        choice,
        thinking,
        endpoints,
        confinement,
        persistence,
        backend,
        realm_lease,
        factory,
        mut approval,
        mcp_servers,
        project_inputs,
        memory,
        helper_max_depth,
        llm_override,
        ..
    } = parts;
    // Nobody renders the engine's decision events in the daemon (the
    // client reads its status with `rkat/get_mode`); dropped, they are not
    // queued.
    drop(approval.events.take());
    persistence.require_hosting_mode(effective.storage.hosting_mode())?;
    let config_store = Arc::clone(&realm_config) as Arc<dyn ConfigStore>;
    let runtime = SessionRuntime::new_with_config_store(
        factory,
        effective.clone(),
        Arc::clone(&config_store),
        effective.max_sessions(),
        persistence,
        NotificationSink::noop(),
    )?;
    runtime.set_realm_context(
        meerkat_core::connection::RealmId::parse(scope.realm_id.clone()).ok(),
        None,
        Some(backend.as_str().to_string()),
    );
    runtime.set_config_runtime(Arc::new(ConfigRuntime::new(
        Arc::clone(&config_store),
        realm_config.state_path().to_path_buf(),
    )));
    runtime.set_default_llm_client(llm_override);
    let runtime = Arc::new(runtime);

    let realm_root = scope.realm_root();
    // Nobody renders the registry's helper updates in the daemon (clients
    // follow the helpers on the mobs' event streams); they tell the daemon
    // when a helper's run ended, so its tool catalog can be read.
    let (registry, updates) = HelperRegistry::persistent(&realm_root);
    let registry = Arc::new(registry);
    let root_tools = orchestrator_tools(&scope)?;
    let tools_for_root = Arc::clone(&root_tools.0);
    let notes = root_tools.1;
    let engine = Arc::clone(&approval.engine);
    let origins = Arc::new(DaemonToolOrigins::new(
        runtime.session_service(),
        &[Arc::clone(&root_tools.0)],
    ));
    engine.set_tool_origins(Arc::clone(&origins) as Arc<dyn crate::approval::ToolOrigins>);
    let helper_catalogs = super::approvals::watch_helpers(updates, Arc::clone(&origins));
    let Some(escalations) = approval.escalations.take() else {
        return Err(DaemonError::Approval(
            "the approval engine has no escalation channel".to_string(),
        ));
    };
    let (desk, desk_task) = Desk::spawn(runtime.approval_service(), escalations);
    let key = HelperKey::new(MOB_ID.into(), ORCHESTRATOR.into());
    let root_thinking: crate::mobs::customizer::RootThinking = Arc::new({
        let thinking = thinking.clone();
        move |identity: &meerkat_core::SessionLlmIdentity| {
            let endpoint = crate::endpoints::endpoint_of(
                identity.provider,
                identity.auth_binding.as_ref(),
                &endpoints,
            );
            thinking.resolve(
                endpoint
                    .as_ref()
                    .map(crate::endpoints::EndpointName::as_str),
                identity.provider,
                &identity.model,
            )
        }
    });
    let root = RootMember {
        key: key.clone(),
        tools: Arc::new(move || Arc::clone(&tools_for_root)),
        instructions: Arc::new(move || {
            let mut sections = vec![CODING_PERSONA.to_string()];
            sections.extend(instructions.render());
            sections.extend(prompt_index(notes.user(), notes.workspace()));
            sections
        }),
        model: choice.model.clone(),
        auth_binding: choice.auth_binding.clone(),
        admission: Some(Arc::new(RootGate::new(
            Arc::clone(&engine),
            Arc::clone(&registry),
            key,
        ))),
        thinking: Some(root_thinking),
    };
    let project_root = scope.project_root.clone();
    let helper_tools: HelperTools = Arc::new(move || {
        Arc::new(FileToolDispatcher::new(project_root.clone(), None))
            as Arc<dyn AgentToolDispatcher>
    });
    // The daemon follows its helpers as the in-process host does: the
    // registry's running set (an interrupt cancels it), and each helper's
    // session bound for the helpers it delegates to in turn. Clients see
    // the helpers' events on the mobs' own streams.
    let (spawning, spawning_rx) = mpsc::unbounded_channel();
    let customizer = HelperCustomizer::new(
        Arc::clone(&registry),
        Arc::clone(&approval.engine),
        helper_tools,
        confinement.shell_env,
        spawning,
        helper_max_depth,
    )
    .with_root(root);
    let registry_for_policy = if policy.without_registry {
        None
    } else {
        Some(
            super::policy::registry(&engine)
                .map_err(|err| DaemonError::Approval(err.to_string()))?,
        )
    };
    let customizer = match registry_for_policy {
        Some(_) => customizer.with_application_tool_policy(super::policy::binding()),
        None => customizer,
    };

    let llm_provider: Arc<dyn Fn() -> Option<Arc<dyn meerkat::LlmClient>> + Send + Sync> =
        Arc::new({
            let runtime = Arc::downgrade(&runtime);
            move || runtime.upgrade()?.default_llm_client()
        });
    let workgraph = (runtime.workgraph_store().kind() != meerkat::WorkGraphStoreKind::Disabled)
        .then(|| {
            runtime
                .workgraph_service()
                .unwrap_or_else(|_| meerkat::WorkGraphService::new(runtime.workgraph_store()))
        });
    let state = MobMcpState::new_with_runtime_adapter(
        runtime.session_service(),
        Some(runtime.runtime_adapter()),
        meerkat_mob::MobControlPrincipal::Owner,
    )?;
    let state = match registry_for_policy {
        Some(registry) => state
            .with_tool_consequence_policy_registry(registry)
            .with_child_application_tool_policy(super::policy::binding()),
        None => state,
    };
    let state = state
        .with_persistent_storage_root(Some(realm_root))
        .with_workgraph_service(workgraph)
        .with_default_llm_client_provider(Some(llm_provider))
        .with_detached_owner_host(Arc::new(
            meerkat_rpc::detached_owner::RpcDetachedOwnerHost::new(&runtime),
        ))
        .with_child_spawn_member_customizer(Arc::new(customizer))
        .into_shared();
    crate::mobs::install_mob_hooks(&state, &registry);
    let follower = tokio::spawn(crate::mobs::follow::watch(
        Arc::downgrade(&state),
        Arc::clone(&registry),
        spawning_rx,
    ));
    if let Err(error) = state.bind_continuations(
        runtime.runtime_delivery_inbox(),
        runtime.continuation_bindings(),
    ) {
        tracing::warn!(%error, "fork_off and council run in the turn: continuation owner not bound");
    }
    state.start_workgraph_flow_reconciler();
    // Before any `MethodRouter::new`, which would compose a default one.
    runtime.set_mob_state(Arc::clone(&state));
    // The agent-facing mob tools (`delegate`, `mob_*`, …) reach a session
    // build through the builder's mob-tools slot. `MethodRouter::new` fills
    // it on the first connection, which is after the orchestrator's spawn:
    // without this its build mounts only the operator family
    // (`spawn_member`, …) and no `delegate`.
    runtime.set_mob_tools(Arc::new(meerkat_mob_mcp::AgentMobToolSurfaceFactory::new(
        Arc::clone(&state),
    )));

    Ok(Composed {
        runtime,
        config_store,
        state,
        root: RootPlan {
            model: choice.model,
            provider: choice.provider,
            auth_binding: choice.auth_binding,
            mcp_servers,
            memory,
            registry,
            thinking: choice.thinking,
        },
        lease: realm_lease,
        scope,
        engine,
        desk,
        desk_task,
        follower_task: follower,
        helper_catalogs_task: helper_catalogs,
        origins,
        thinking,
        project_inputs,
    })
}

/// The orchestrator's external tools (the TUI session's file and note
/// tools behind one gateway; no `fork`, which needs the TUI's service) and
/// the note stores its instructions index.
fn orchestrator_tools(
    scope: &RuntimeScope,
) -> Result<(Arc<dyn AgentToolDispatcher>, Arc<NoteToolDispatcher>), DaemonError> {
    let files = Arc::new(FileToolDispatcher::new(scope.project_root.clone(), None));
    let notes = Arc::new(NoteToolDispatcher::new(
        scope
            .global
            .as_ref()
            .map(|global| NoteStore::open(global.notes_dir(), Scope::User)),
        NoteStore::open(
            crate::runner::workspace_notes_dir(&scope.context_root),
            Scope::Workspace,
        ),
    ));
    let gateway = ToolGatewayBuilder::new()
        .add_dispatcher(files as Arc<dyn AgentToolDispatcher>)
        .add_dispatcher(Arc::clone(&notes) as Arc<dyn AgentToolDispatcher>)
        .build()
        .map_err(TuiError::ToolGateway)?;
    Ok((Arc::new(gateway), notes))
}
