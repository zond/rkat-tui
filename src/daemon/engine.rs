//! Composing Meerkat's JSON-RPC host over rkat-tui's bootstrap.
//!
//! Mirrors `rkat-rpc`'s assembly (`SessionRuntime::new_with_config_store`,
//! `set_realm_context`, `set_config_runtime`, `require_hosting_mode`, the
//! mob state set before any `MethodRouter`), with these differences: the
//! factory, config store and confinement come from
//! [`crate::bootstrap::assemble`], so the realm is configured as the
//! client reads it; the mob state is this crate's own (not
//! `compose_rpc_mob_state`), carrying the TUI's spawn customizer with the
//! orchestrator as its [`RootMember`]; and the connection callback-tool
//! routes `compose_rpc_mob_state` installs are left out (their builders are
//! crate-private, and the daemon's members get their tools from the
//! customizer); and the approval engine the bootstrap built gates every
//! member ([`super::approvals`]), its escalations answered through the
//! runtime's approval service, its deny rules installed as the members'
//! application tool policy ([`super::policy`]); and its escalations are
//! held as the runtime's durable decisions, which every attached client is
//! offered and decides through the daemon ([`super::decisions`]).

use super::protocol::StartupNotice;
use std::sync::Arc;

use meerkat::ToolGatewayBuilder;
use meerkat_core::{AgentToolDispatcher, ConfigRuntime, ConfigStore, Provider};
use meerkat_mob_mcp::MobMcpState;
use meerkat_rpc::router::NotificationSink;
use meerkat_rpc::session_runtime::SessionRuntime;
use meerkat_store::RealmLeaseGuard;
use tokio::sync::mpsc;

use super::approvals::{DaemonToolOrigins, Desk, RootGate};
use super::decisions::{Decisions, Principal};
use super::{DaemonError, MOB_ID, ORCHESTRATOR};
use crate::approval::ApprovalEngine;
use crate::bootstrap::{Assembled, CODING_PERSONA, RuntimeScope};
use crate::error::TuiError;
use crate::memory::{NoteStore, NoteToolDispatcher, Scope, prompt_index};
use crate::mobs::customizer::{HelperCustomizer, HelperTools, RootMember};
use crate::mobs::{HelperKey, HelperRegistry};
use crate::thinking::ThinkingTable;
use crate::tools::FileToolDispatcher;

/// What the agents are seated with ([`super::orchestrator::ensure_mob`] creates their mob from it).
#[derive(Debug, Clone)]
pub struct RootPlan {
    /// The start-up model, as the engine names it (a self-hosted
    /// endpoint's alias).
    pub model: String,
    /// Its engine provider.
    pub provider: Provider,
    /// Its self-hosted server, on a self-hosted endpoint.
    pub self_hosted_server: Option<String>,
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
    /// The orchestrator profile's auto-compaction threshold, from the
    /// `[auto]` compaction policy (`CompactionTrigger::threshold_override`:
    /// none in native mode, the policy's tokens in auto mode, the largest
    /// threshold in warn mode).
    pub compact_threshold: Option<std::num::NonZeroU64>,
}

/// The composed host.
pub struct Composed {
    pub runtime: Arc<SessionRuntime>,
    pub config_store: Arc<dyn ConfigStore>,
    /// The same store, typed (the endpoints' documents are written there).
    pub realm_config: Arc<crate::bootstrap::RealmConfigStore>,
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
    /// The consent surface every client is offered decisions through.
    pub decisions: Arc<Decisions>,
    /// The task that offers the decisions that open.
    pub decisions_task: tokio::task::JoinHandle<()>,
    /// The task that pushes the orchestrator's file changes to the clients.
    pub diffs_task: tokio::task::JoinHandle<()>,
    /// The task that pushes the approval engine's notes to the clients.
    pub notes_task: tokio::task::JoinHandle<()>,
    /// The helper follower's task.
    pub follower_task: tokio::task::JoinHandle<()>,
    /// The task reading helpers' tool catalogs after their runs.
    pub helper_catalogs_task: tokio::task::JoinHandle<()>,
    /// The members' tool catalogs.
    pub origins: Arc<DaemonToolOrigins>,
    /// The `[thinking]` table (model switches over the wire).
    pub thinking: ThinkingTable,
    /// The daemon's endpoints (model switches over the wire).
    pub endpoints: crate::endpoints::EndpointRegistry,
    /// The workspace's project inputs and the trust record they are judged
    /// by (`rkat/trust`).
    pub project_inputs: crate::bootstrap::project_trust::ProjectInputs,
    /// The engine's task store (`rkat/tasks`).
    pub tasks_db: std::path::PathBuf,
    /// What the start-up had to say (`rkat/notices`).
    pub startup_notices: Vec<StartupNotice>,
    /// `[helpers] max_depth` (`rkat/report`).
    pub helper_max_depth: u8,
    /// The agent shell's confinement (`rkat/status` reports it).
    pub shell: meerkat_tools::builtin::shell::ShellConfinement,
}

/// How the daemon composes its approval side (tests vary it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PolicyOptions {
    /// Leave the consequence-policy registry out (tests: what the engine
    /// does with a member whose durable binding names it).
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
        daemon_principal,
        tasks_db,
        notices,
        compaction,
        context_window,
        ..
    } = parts;
    // The `[auto]` compaction policy as the orchestrator's threshold, for
    // the start-up model's window (a configured endpoint's own listing
    // first; unknown: the policy's fallback).
    let compact_threshold = compaction.trigger(context_window).threshold_override();
    // What a client shows when it first attaches (`rkat/notices`): the confinement's observations, then the rules'
    // and the bootstrap's warnings.
    let startup_notices: Vec<StartupNotice> = confinement
        .notices
        .iter()
        .map(|text| StartupNotice::info(text.clone()))
        .chain(
            approval
                .notices
                .drain(..)
                .chain(notices)
                .map(StartupNotice::warn),
        )
        .collect();
    // The engine's decision events go to every attached client
    // (`rkat/approval_note`, forwarded once the consent surface exists).
    let approval_events = approval.events.take();
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
    // A forgotten session's helper bookkeeping goes with it.
    let forget = Arc::downgrade(&registry);
    approval
        .engine
        .add_forget_hook(Arc::new(move |session: &meerkat_core::SessionId| {
            if let Some(registry) = forget.upgrade() {
                registry.forget_session(session);
            }
        }));
    let (diff_tx, diff_rx) = tokio::sync::mpsc::unbounded_channel();
    let notes = Arc::new(note_tools(&scope));
    // The tools' names, for the gate's provenance (every agent's set has
    // the same ones).
    let catalog = agent_tools(&scope, &notes, None)?;
    let engine = Arc::clone(&approval.engine);
    let origins = Arc::new(DaemonToolOrigins::new(
        runtime.session_service(),
        &[catalog],
    ));
    engine.set_tool_origins(Arc::clone(&origins) as Arc<dyn crate::approval::ToolOrigins>);
    let helper_catalogs = super::approvals::watch_helpers(updates, Arc::clone(&origins));
    let Some(escalations) = approval.escalations.take() else {
        return Err(DaemonError::Approval(
            "the approval engine has no escalation channel".to_string(),
        ));
    };
    let principal = Principal::from_config(daemon_principal.as_deref())
        .map_err(|err| DaemonError::Approval(format!("[daemon] principal: {err}")))?;
    let decision_service = runtime.decision_service();
    if !decision_service.is_available() {
        return Err(DaemonError::Approval(
            "the realm keeps no durable decisions (an ephemeral persistence); approval requests could not reach a client".to_string(),
        ));
    }
    let helper_endpoints = endpoints.as_slice().to_vec();
    let daemon_endpoints = endpoints.clone();
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
    let notes_for_index = Arc::clone(&notes);
    let root_tools: crate::mobs::customizer::RootTools = {
        let scope = scope.clone();
        let notes = Arc::clone(&notes);
        Arc::new(move |identity: &meerkat_mob::AgentIdentity| {
            let diffs = tagged_diffs(identity.to_string(), diff_tx.clone());
            match agent_tools(&scope, &notes, Some(diffs)) {
                Ok(tools) => tools,
                Err(err) => {
                    // The same composition succeeded at start for the
                    // catalog; an agent without its file tools still has
                    // the engine's.
                    tracing::error!(error = %err, agent = %identity, "the agent's file tools could not be composed");
                    Arc::new(meerkat_tools::EmptyToolDispatcher) as Arc<dyn AgentToolDispatcher>
                }
            }
        })
    };
    let admission: crate::mobs::customizer::RootAdmission = {
        let engine = Arc::clone(&engine);
        let registry = Arc::clone(&registry);
        Arc::new(move |key: &HelperKey| {
            Arc::new(RootGate::new(
                Arc::clone(&engine),
                Arc::clone(&registry),
                key.clone(),
            )) as Arc<dyn meerkat_core::ToolDispatchAdmission>
        })
    };
    let root = RootMember {
        mob: MOB_ID.into(),
        is_root: super::agents::is_agent,
        profile: ORCHESTRATOR.to_string(),
        tools: root_tools,
        instructions: Arc::new(move |identity: &meerkat_mob::AgentIdentity| {
            let mut sections = vec![
                CODING_PERSONA.to_string(),
                crate::bootstrap::persona::agents_persona(identity.as_str()),
            ];
            sections.extend(instructions.render());
            sections.extend(prompt_index(
                notes_for_index.user(),
                notes_for_index.workspace(),
            ));
            sections
        }),
        model: choice.engine_model.clone(),
        auth_binding: choice.auth_binding.clone(),
        admission: Some(admission),
        thinking: Some(root_thinking),
    };
    let project_root = scope.project_root.clone();
    let helper_tools: HelperTools = Arc::new(move || {
        Arc::new(FileToolDispatcher::new(project_root.clone(), None))
            as Arc<dyn AgentToolDispatcher>
    });
    // The daemon follows its helpers: the
    // registry's running set (an interrupt cancels it), and each helper's
    // session bound for the helpers it delegates to in turn. Clients see
    // the helpers' events on the mobs' own streams.
    let (spawning, spawning_rx) = mpsc::unbounded_channel();
    let shell = confinement.shell.clone();
    let customizer = HelperCustomizer::new(
        Arc::clone(&registry),
        Arc::clone(&approval.engine),
        helper_tools,
        confinement.shell_env,
        spawning,
        helper_max_depth,
    )
    .with_root(root)
    .with_endpoints(helper_endpoints)
    .with_gated_fork_off();
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
    // The host's own router: the one entry a client's `decision/decide`
    // reaches the decision owner through (`dispatch_with_decision_event`,
    // which plain dispatch has no route for). Built after the mob state, so
    // it reuses it, as every connection's router does.
    let router = meerkat_rpc::router::MethodRouter::new(
        Arc::clone(&runtime),
        Arc::clone(&config_store),
        NotificationSink::noop(),
    )?;
    let decisions = Decisions::new(decision_service, principal, Some(Arc::new(router)))
        .map_err(DaemonError::Approval)?;
    let decisions_task = decisions.follow();
    let diffs_task = Decisions::forward_diffs(&decisions, diff_rx);
    let notes_task = forward_approval_notes(&decisions, approval_events);
    let (desk, desk_task) = Desk::spawn(
        Arc::clone(&decisions),
        engine.escalation_timeout(),
        escalations,
    );

    Ok(Composed {
        runtime,
        config_store,
        realm_config,
        state,
        root: RootPlan {
            model: choice.engine_model,
            provider: choice.engine_provider,
            self_hosted_server: choice.self_hosted_server,
            auth_binding: choice.auth_binding,
            mcp_servers,
            memory,
            registry,
            thinking: choice.thinking,
            compact_threshold,
        },
        lease: realm_lease,
        scope,
        engine,
        desk,
        desk_task,
        decisions,
        decisions_task,
        notes_task,
        diffs_task,
        follower_task: follower,
        helper_catalogs_task: helper_catalogs,
        origins,
        thinking,
        endpoints: daemon_endpoints,
        project_inputs,
        tasks_db,
        startup_notices,
        helper_max_depth,
        shell,
    })
}

/// The note tools every agent shares (one store per scope).
fn note_tools(scope: &RuntimeScope) -> NoteToolDispatcher {
    NoteToolDispatcher::new(
        scope
            .global
            .as_ref()
            .map(|global| NoteStore::open(global.notes_dir(), Scope::User)),
        NoteStore::open(
            crate::memory::workspace_notes_dir(&scope.realm_root()),
            Scope::Workspace,
        ),
    )
}

/// One agent's external tools: its own file tools (reporting every change
/// they make to `diffs`, which the clients show as a diff) and the shared
/// note tools, behind one gateway.
fn agent_tools(
    scope: &RuntimeScope,
    notes: &Arc<NoteToolDispatcher>,
    diffs: Option<tokio::sync::mpsc::UnboundedSender<crate::tools::FileDiff>>,
) -> Result<Arc<dyn AgentToolDispatcher>, DaemonError> {
    let files = Arc::new(FileToolDispatcher::new(scope.project_root.clone(), diffs));
    let gateway = ToolGatewayBuilder::new()
        .add_dispatcher(files as Arc<dyn AgentToolDispatcher>)
        .add_dispatcher(Arc::clone(notes) as Arc<dyn AgentToolDispatcher>)
        .build()
        .map_err(TuiError::ToolGateway)?;
    Ok(Arc::new(gateway))
}

/// A sender of file changes that forwards each to `all` tagged with
/// `agent` (until either end is gone).
fn tagged_diffs(
    agent: String,
    all: tokio::sync::mpsc::UnboundedSender<(String, crate::tools::FileDiff)>,
) -> tokio::sync::mpsc::UnboundedSender<crate::tools::FileDiff> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(diff) = rx.recv().await {
            if all.send((agent.clone(), diff)).is_err() {
                break;
            }
        }
    });
    tx
}

/// Push every event of the approval engine's `events` to the clients as
/// an [`ApprovalNote`](super::protocol::ApprovalNote) (classifier verdicts, pauses, the classifier's work and
/// usage), until the engine is gone.
fn forward_approval_notes(
    decisions: &Arc<Decisions>,
    events: Option<tokio::sync::mpsc::UnboundedReceiver<crate::approval::ApprovalEvent>>,
) -> tokio::task::JoinHandle<()> {
    let decisions = Arc::clone(decisions);
    tokio::spawn(async move {
        let Some(mut events) = events else {
            return;
        };
        while let Some(event) = events.recv().await {
            if let Some(note) = super::protocol::ApprovalNote::of(event) {
                decisions.broadcast(super::protocol::APPROVAL_NOTE, &note);
            }
        }
    })
}
