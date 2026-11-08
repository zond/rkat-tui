//! Composing Meerkat's JSON-RPC host over rkat-tui's bootstrap.
//!
//! Mirrors `rkat-rpc`'s assembly (`SessionRuntime::new_with_config_store`,
//! `set_realm_context`, `set_config_runtime`, `require_hosting_mode`, the
//! mob state set before any `MethodRouter`), with three differences: the
//! factory, config store and confinement come from
//! [`crate::bootstrap::assemble`], so the realm is configured exactly as
//! for the in-process TUI; the mob state is this crate's own (not
//! `compose_rpc_mob_state`), carrying the TUI's spawn customizer with the
//! orchestrator as its [`RootMember`]; and the connection callback-tool
//! routes `compose_rpc_mob_state` installs are left out (their builders are
//! crate-private, and the daemon's members get their tools from the
//! customizer).

use std::sync::Arc;

use meerkat::ToolGatewayBuilder;
use meerkat_core::{AgentToolDispatcher, ConfigRuntime, ConfigStore, Provider};
use meerkat_mob_mcp::MobMcpState;
use meerkat_rpc::router::NotificationSink;
use meerkat_rpc::session_runtime::SessionRuntime;
use meerkat_store::RealmLeaseGuard;
use tokio::sync::mpsc;

use super::{DaemonError, MOB_ID, ORCHESTRATOR};
use crate::bootstrap::{Assembled, CODING_PERSONA, RuntimeScope};
use crate::error::TuiError;
use crate::memory::{NoteStore, NoteToolDispatcher, Scope, prompt_index};
use crate::mobs::customizer::{HelperCustomizer, HelperTools, RootMember};
use crate::mobs::{HelperKey, HelperRegistry};
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
}

/// The composed host.
pub struct Composed {
    pub runtime: Arc<SessionRuntime>,
    pub config_store: Arc<dyn ConfigStore>,
    pub state: Arc<MobMcpState>,
    pub root: RootPlan,
    pub lease: RealmLeaseGuard,
    pub scope: RuntimeScope,
}

/// Compose the runtime and the mob host over `parts`.
///
/// # Errors
///
/// [`DaemonError::Hosting`], [`DaemonError::Composition`],
/// [`DaemonError::Driver`], or [`DaemonError::Bootstrap`] when the
/// orchestrator's tools cannot be composed.
pub fn compose(parts: Assembled) -> Result<Composed, DaemonError> {
    let instructions = parts.instructions();
    let Assembled {
        scope,
        realm_config,
        effective,
        choice,
        confinement,
        persistence,
        backend,
        realm_lease,
        factory,
        approval,
        mcp_servers,
        memory,
        helper_max_depth,
        llm_override,
        ..
    } = parts;
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
    let (registry, updates) = HelperRegistry::persistent(&realm_root);
    // Nobody renders helper updates in the daemon (P4 streams them to the
    // client).
    drop(updates);
    let registry = Arc::new(registry);
    let root_tools = orchestrator_tools(&scope)?;
    let tools_for_root = Arc::clone(&root_tools.0);
    let notes = root_tools.1;
    let root = RootMember {
        key: HelperKey::new(MOB_ID.into(), ORCHESTRATOR.into()),
        tools: Arc::new(move || Arc::clone(&tools_for_root)),
        instructions: Arc::new(move || {
            let mut sections = vec![CODING_PERSONA.to_string()];
            sections.extend(instructions.render());
            sections.extend(prompt_index(notes.user(), notes.workspace()));
            sections
        }),
        model: choice.model.clone(),
        auth_binding: choice.auth_binding.clone(),
        // P3: the daemon-side approval gate for the orchestrator.
        admission: None,
    };
    let project_root = scope.project_root.clone();
    let helper_tools: HelperTools = Arc::new(move || {
        Arc::new(FileToolDispatcher::new(project_root.clone(), None))
            as Arc<dyn AgentToolDispatcher>
    });
    // No follower in the daemon: the spawn notices go nowhere.
    let (spawning, _) = mpsc::unbounded_channel();
    let customizer = HelperCustomizer::new(
        Arc::clone(&registry),
        Arc::clone(&approval.engine),
        helper_tools,
        confinement.shell_env,
        spawning,
        helper_max_depth,
    )
    .with_root(root);

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
    )?
    .with_persistent_storage_root(Some(realm_root))
    .with_workgraph_service(workgraph)
    .with_default_llm_client_provider(Some(llm_provider))
    .with_detached_owner_host(Arc::new(
        meerkat_rpc::detached_owner::RpcDetachedOwnerHost::new(&runtime),
    ))
    .with_child_spawn_member_customizer(Arc::new(customizer))
    .into_shared();
    crate::mobs::install_mob_hooks(&state, &registry);
    if let Err(error) = state.bind_continuations(
        runtime.runtime_delivery_inbox(),
        runtime.continuation_bindings(),
    ) {
        tracing::warn!(%error, "fork_off and council run in the turn: continuation owner not bound");
    }
    state.start_workgraph_flow_reconciler();
    // Before any `MethodRouter::new`, which would compose a default one.
    runtime.set_mob_state(Arc::clone(&state));

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
        },
        lease: realm_lease,
        scope,
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
