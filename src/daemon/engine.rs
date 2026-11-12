//! Composing Meerkat's JSON-RPC host over rkat-tui's bootstrap.
//!
//! Mirrors `rkat-rpc`'s assembly (`SessionRuntime::new_with_config_store`,
//! `set_realm_context`, `set_config_runtime`, `require_hosting_mode`, the
//! mob state set before any `MethodRouter`), with these differences: the
//! factory, config store and confinement come from
//! [`crate::bootstrap::assemble`], so the realm is configured as the
//! client reads it; the mob state is this crate's own (not
//! `compose_rpc_mob_state`), carrying rkat-tui's spawn customizer with the
//! agent as its [`RootMember`]; and the connection callback-tool
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
use super::{AGENT_PROFILE, DaemonError, MOB_ID};
use crate::approval::ApprovalEngine;
use crate::bootstrap::{Assembled, CODING_PERSONA, RuntimeScope};
use crate::error::TuiError;
use crate::memory::{NoteStore, NoteToolDispatcher, Scope, prompt_index};
use crate::mobs::customizer::{HelperCustomizer, HelperTools, RootMember};
use crate::mobs::{HelperKey, HelperRegistry};
use crate::thinking::ThinkingTable;
use crate::tools::FileToolDispatcher;

/// What the agents are seated with ([`super::mob::ensure_mob`] creates their mob from it).
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
    /// The helper bookkeeping (the agent registers as a root owner).
    pub registry: Arc<HelperRegistry>,
    /// The `[thinking]` of the start-up model (the profile's provider
    /// params on a fresh spawn).
    pub thinking: Option<meerkat_core::lifecycle::run_primitive::AnthropicThinkingConfig>,
    /// The agent profile's auto-compaction threshold, from the
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
    /// The task that pushes the agent's file changes to the clients.
    pub diffs_task: tokio::task::JoinHandle<()>,
    /// The task that pushes the approval engine's notes to the clients.
    pub notes_task: tokio::task::JoinHandle<()>,
    /// The helper follower's task.
    pub follower_task: tokio::task::JoinHandle<()>,
    /// The task that tells the clients of destroyed mobs.
    pub destroyed_task: tokio::task::JoinHandle<()>,
    /// What every agent's and helper's model calls used.
    pub ledger: Arc<super::usage::Ledger>,
    /// The members' tool catalogs.
    pub origins: Arc<DaemonToolOrigins>,
    /// The tools the agents' requests leave out until searched for, and
    /// each conversation's reveals.
    pub deferral: Arc<super::deferral::Deferral>,
    /// The agents' file watches.
    pub watches: Arc<super::watches::Watches>,
    /// Their batches of changes, to deliver to the agents.
    pub watch_batches: tokio::sync::mpsc::UnboundedReceiver<super::watches::Batch>,
    /// The `[thinking]` table (model switches over the wire).
    pub thinking: ThinkingTable,
    /// The daemon's endpoints (model switches over the wire).
    pub endpoints: crate::endpoints::EndpointRegistry,
    /// The workspace's project inputs and the trust record they are judged
    /// by (`rkat/trust`).
    pub project_inputs: crate::bootstrap::project_trust::ProjectInputs,
    /// The engine's task store (`rkat/tasks`).
    pub tasks_db: std::path::PathBuf,
    /// The realm's `tui.toml` as rkat-tui writes it.
    pub realm_file: Option<crate::config::RealmFile>,
    /// What the start-up had to say (`rkat/notices`).
    pub startup_notices: Vec<StartupNotice>,
    /// `[helpers] max_depth` (`rkat/report`).
    pub helper_max_depth: u8,
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
/// agent's tools cannot be composed.
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
        search_limits,
        llm_override,
        daemon_principal,
        tasks_db,
        notices,
        compaction,
        context_window,
        ..
    } = parts;
    // The `[auto]` compaction policy as the agent's threshold, for
    // the start-up model's window (a configured endpoint's own listing
    // first; unknown: the policy's fallback).
    let compact_threshold = compaction.trigger(context_window).threshold_override();
    let startup_notices =
        startup_notices(&confinement.notices, approval.notices.drain(..), notices);
    // The engine's decision events go to every attached client
    // (`rkat/approval_note`, forwarded once the consent surface exists).
    let approval_events = approval.events.take();
    let config_store = Arc::clone(&realm_config) as Arc<dyn ConfigStore>;
    let runtime = session_runtime(RuntimeParts {
        factory,
        effective: &effective,
        persistence,
        realm_config: &realm_config,
        realm_id: &scope.realm_id,
        backend: backend.as_str(),
        llm_override,
    })?;

    let realm_root = scope.realm_root();
    let registry = helper_registry(&realm_root, &approval.engine);
    let (diff_tx, diff_rx) = tokio::sync::mpsc::unbounded_channel();
    let notes = Arc::new(note_tools(&scope));
    // The agents' file watches: the daemon's process watches the
    // workspace (`watch_paths`), never the agent's shell.
    let (watches, watch_batches) =
        super::watches::Watches::new(scope.project_root.as_path().to_path_buf());
    let engine = Arc::clone(&approval.engine);
    let (origins, deferrable, reveals) = tool_origins(&runtime, &engine, &scope, &notes)?;
    let Some(escalations) = approval.escalations.take() else {
        return Err(DaemonError::Approval(
            "the approval engine has no escalation channel".to_string(),
        ));
    };
    let (principal, decision_service) = consent(&runtime, daemon_principal.as_deref())?;
    let helper_endpoints = endpoints.as_slice().to_vec();
    let daemon_endpoints = endpoints.clone();
    let host = Arc::new(AgentHost {
        scope: scope.clone(),
        notes: Arc::clone(&notes),
        watches: Arc::clone(&watches),
        diff_tx,
        engine: Arc::clone(&engine),
        registry: Arc::clone(&registry),
        thinking: thinking.clone(),
        endpoints,
        instructions,
        search_limits,
    });
    let root_plan = RootPlan {
        model: choice.engine_model.clone(),
        provider: choice.engine_provider,
        self_hosted_server: choice.self_hosted_server.clone(),
        auth_binding: choice.auth_binding.clone(),
        mcp_servers,
        memory,
        registry: Arc::clone(&registry),
        thinking: choice.thinking.clone(),
        compact_threshold,
    };
    let root = RootMember {
        mob: MOB_ID.into(),
        is_root: super::agents::is_agent,
        profile: AGENT_PROFILE.to_string(),
        current_profile: super::mob::main_profile(&root_plan),
        host,
        model: choice.engine_model.clone(),
        auth_binding: choice.auth_binding,
    };
    let project_root = scope.project_root.clone();
    let helper_tools: HelperTools = Arc::new(move || {
        Arc::new(
            FileToolDispatcher::new(project_root.clone(), None).with_search_limits(search_limits),
        ) as Arc<dyn AgentToolDispatcher>
    });
    // The daemon follows its helpers: the
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
    .with_root(root)
    .with_endpoints(helper_endpoints)
    .with_gated_fork_off();
    // The members' tool policy from the engine's deny rules (tests may
    // leave it out).
    let registry_for_policy = (!policy.without_registry)
        .then(|| super::policy::registry(&engine))
        .transpose()
        .map_err(|err| DaemonError::Approval(err.to_string()))?;
    let customizer = match registry_for_policy {
        Some(_) => customizer.with_application_tool_policy(super::policy::binding()),
        None => customizer,
    };

    let state = mob_state(
        &runtime,
        customizer,
        registry_for_policy,
        &realm_root,
        &registry,
    )?;
    // The agent-facing mob tools (`delegate`, `mob_*`, …) reach a session
    // build through the builder's mob-tools slot. `MethodRouter::new` fills
    // it on the first connection, which is after the agent's spawn:
    // without this its build mounts only the operator family
    // (`spawn_member`, …) and no `delegate`.
    //
    // The daemon's factory: an agent's build also gets its schedule tools
    // there, and both families are hidden until its conversation reveals
    // them ([`super::deferral`]). Every router installs the engine's
    // default factory, so the host's own router is made first and the
    // daemon's installed after it (and after every connection's, see
    // `Deferral::with_router`).
    //
    // The host's own router: the one entry a client's `decision/decide`
    // reaches the decision owner through (`dispatch_with_decision_event`,
    // which plain dispatch has no route for). Built after the mob state, so
    // it reuses it, as every connection's router does.
    let router = meerkat_rpc::router::MethodRouter::new(
        Arc::clone(&runtime),
        Arc::clone(&config_store),
        NotificationSink::noop(),
    )?;
    let deferral = Arc::new(super::deferral::Deferral::new(
        Arc::new(meerkat_mob_mcp::AgentMobToolSurfaceFactory::new(
            Arc::clone(&state),
        )),
        runtime.schedule_service(),
        deferrable,
        reveals,
    ));
    deferral.install(&runtime);
    let decisions = Decisions::new(decision_service, principal, Some(Arc::new(router)))
        .map_err(DaemonError::Approval)?;
    let decisions_task = decisions.follow();
    let diffs_task = Decisions::forward_diffs(&decisions, diff_rx);
    let ledger = ledger(&decisions, &realm_root);
    let (follower, destroyed_task) = follow_helpers(
        &state,
        &registry,
        &origins,
        &ledger,
        &decisions,
        spawning_rx,
    );
    let notes_task = forward_approval_notes(&decisions, &ledger, &engine, approval_events);
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
        root: root_plan,
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
        destroyed_task,
        ledger,
        origins,
        deferral,
        watches,
        watch_batches,
        thinking,
        endpoints: daemon_endpoints,
        project_inputs,
        tasks_db,
        realm_file: approval.realm_file.take(),
        startup_notices,
        helper_max_depth,
    })
}

/// What a client shows when it first attaches (`rkat/notices`): the
/// confinement's observations, then the rules' and the bootstrap's
/// warnings.
fn startup_notices(
    confinement: &[String],
    rules: impl Iterator<Item = String>,
    bootstrap: Vec<String>,
) -> Vec<StartupNotice> {
    confinement
        .iter()
        .map(|text| StartupNotice::info(text.clone()))
        .chain(rules.chain(bootstrap).map(StartupNotice::warn))
        .collect()
}

/// What [`session_runtime`] is made of.
struct RuntimeParts<'a> {
    factory: meerkat::AgentFactory,
    effective: &'a meerkat_core::Config,
    persistence: meerkat::PersistenceBundle,
    realm_config: &'a Arc<crate::bootstrap::RealmConfigStore>,
    realm_id: &'a str,
    backend: &'a str,
    llm_override: Option<Arc<dyn meerkat::LlmClient>>,
}

/// The session runtime over the realm, as `rkat-rpc` composes it.
fn session_runtime(parts: RuntimeParts<'_>) -> Result<Arc<SessionRuntime>, DaemonError> {
    let RuntimeParts {
        factory,
        effective,
        persistence,
        realm_config,
        realm_id,
        backend,
        llm_override,
    } = parts;
    persistence.require_hosting_mode(effective.storage.hosting_mode())?;
    let config_store = Arc::clone(realm_config) as Arc<dyn ConfigStore>;
    let runtime = SessionRuntime::new_with_config_store(
        factory,
        effective.clone(),
        Arc::clone(&config_store),
        effective.max_sessions(),
        persistence,
        NotificationSink::noop(),
    )?;
    runtime.set_realm_context(
        meerkat_core::connection::RealmId::parse(realm_id.to_string()).ok(),
        None,
        Some(backend.to_string()),
    );
    runtime.set_config_runtime(Arc::new(ConfigRuntime::new(
        config_store,
        realm_config.state_path().to_path_buf(),
    )));
    runtime.set_default_llm_client(llm_override);
    Ok(Arc::new(runtime))
}

/// The helpers the agents start, kept in the realm; a forgotten
/// session's helper bookkeeping goes with it.
fn helper_registry(
    realm_root: &std::path::Path,
    engine: &Arc<ApprovalEngine>,
) -> Arc<HelperRegistry> {
    let registry = Arc::new(HelperRegistry::persistent(realm_root));
    let forget = Arc::downgrade(&registry);
    engine.add_forget_hook(Arc::new(move |session: &meerkat_core::SessionId| {
        if let Some(registry) = forget.upgrade() {
            registry.forget_session(session);
        }
    }));
    registry
}

/// The principal the daemon decides as (`[daemon] principal`) and the
/// runtime's durable decisions, which the realm must keep: approval
/// requests reach the clients as decisions.
fn consent(
    runtime: &SessionRuntime,
    principal: Option<&str>,
) -> Result<(Principal, meerkat_runtime::decision::DurableDecisionService), DaemonError> {
    let principal = Principal::from_config(principal)
        .map_err(|err| DaemonError::Approval(format!("[daemon] principal: {err}")))?;
    let service = runtime.decision_service();
    if !service.is_available() {
        return Err(DaemonError::Approval(
            "the realm keeps no durable decisions (an ephemeral persistence); approval requests could not reach a client".to_string(),
        ));
    }
    Ok((principal, service))
}

/// The members' tool catalogs the gate judges calls by, with the tools an
/// agent's requests leave out until it searches for them and each
/// conversation's reveals. The tools' names come from one agent's set
/// (every agent's has the same ones); a forgotten session's catalog goes
/// with it.
fn tool_origins(
    runtime: &Arc<SessionRuntime>,
    engine: &Arc<ApprovalEngine>,
    scope: &RuntimeScope,
    notes: &Arc<NoteToolDispatcher>,
) -> Result<
    (
        Arc<DaemonToolOrigins>,
        Arc<super::deferral::Deferrable>,
        Arc<super::deferral::reveal::Reveals>,
    ),
    DaemonError,
> {
    let catalog = agent_tools(
        FileToolDispatcher::new(scope.project_root.clone(), None),
        notes,
        None,
    )?;
    let deferrable = Arc::new(super::deferral::Deferrable::new());
    let reveals = Arc::new(super::deferral::reveal::Reveals::new(&scope.realm_root()));
    let search = Arc::new(super::deferral::reveal::SearchTool::new(
        meerkat_core::SessionId::new(),
        Arc::clone(&reveals),
        Arc::clone(&deferrable),
    ));
    let origins = Arc::new(DaemonToolOrigins::new(
        runtime.session_service(),
        &[catalog, search],
        Arc::clone(&deferrable),
    ));
    engine.set_tool_origins(Arc::clone(&origins) as Arc<dyn crate::approval::ToolOrigins>);
    let forget = Arc::downgrade(&origins);
    engine.add_forget_hook(Arc::new(move |session: &meerkat_core::SessionId| {
        if let Some(origins) = forget.upgrade() {
            origins.forget(session);
        }
    }));
    Ok((origins, deferrable, reveals))
}

/// The mob host: the runtime's sessions, the members' tool policy (unless
/// the policy registry is left out), the realm's storage, the work graph,
/// the default LLM client, detached owners and the helpers' spawn
/// customizer; set as the runtime's mob state before any
/// `MethodRouter::new`, which would compose a default one.
fn mob_state(
    runtime: &Arc<SessionRuntime>,
    customizer: HelperCustomizer,
    registry_for_policy: Option<Arc<meerkat_core::ToolConsequencePolicyRegistry>>,
    realm_root: &std::path::Path,
    registry: &Arc<HelperRegistry>,
) -> Result<Arc<MobMcpState>, DaemonError> {
    let llm_provider: Arc<dyn Fn() -> Option<Arc<dyn meerkat::LlmClient>> + Send + Sync> =
        Arc::new({
            let runtime = Arc::downgrade(runtime);
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
        .with_persistent_storage_root(Some(realm_root.to_path_buf()))
        .with_workgraph_service(workgraph)
        .with_default_llm_client_provider(Some(llm_provider))
        .with_detached_owner_host(Arc::new(
            meerkat_rpc::detached_owner::RpcDetachedOwnerHost::new(runtime),
        ))
        .with_child_spawn_member_customizer(Arc::new(customizer))
        .into_shared();
    crate::mobs::install_mob_hooks(&state, registry);
    if let Err(error) = state.bind_continuations(
        runtime.runtime_delivery_inbox(),
        runtime.continuation_bindings(),
    ) {
        tracing::warn!(%error, "fork_off and council run in the turn: continuation owner not bound");
    }
    state.start_workgraph_flow_reconciler();
    runtime.set_mob_state(Arc::clone(&state));
    Ok(state)
}

/// Every model call of every agent, helper and classifier is counted
/// here; a conversation's new record (its totals and the gauge) goes to
/// every client.
fn ledger(decisions: &Arc<Decisions>, realm_root: &std::path::Path) -> Arc<super::usage::Ledger> {
    let decisions = Arc::downgrade(decisions);
    super::usage::Ledger::new(
        realm_root,
        move |session: &meerkat_core::SessionId, record: &super::usage::Record| {
            if let Some(decisions) = decisions.upgrade() {
                decisions.broadcast(
                    super::protocol::COST,
                    &super::protocol::CostNote {
                        session_id: session.clone(),
                        cost: record.cost.clone(),
                        context: record.context,
                    },
                );
            }
        },
    )
}

/// The daemon follows its helpers once each: their usage, their run ends
/// (the tool catalog is read then) and their sessions (for nesting).
/// Clients see the helpers' events on the mobs' own streams, and open a
/// mob's stream once the daemon follows a helper in it (`rkat/mob`)
/// instead of listing the mobs again and again. Returns the follower's
/// task and the one that tells the clients of destroyed mobs.
fn follow_helpers(
    state: &Arc<MobMcpState>,
    registry: &Arc<HelperRegistry>,
    origins: &Arc<DaemonToolOrigins>,
    ledger: &Arc<super::usage::Ledger>,
    decisions: &Arc<Decisions>,
    spawning: mpsc::UnboundedReceiver<crate::mobs::customizer::Spawning>,
) -> (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
    let helper_events = Arc::new(super::approvals::HelperEvents::new(
        super::approvals::CatalogReads::of(origins),
        Arc::clone(ledger),
        {
            let decisions = Arc::downgrade(decisions);
            move |mob: &meerkat_mob::MobId, change| {
                if let Some(decisions) = decisions.upgrade() {
                    decisions.broadcast(
                        super::protocol::MOB,
                        &super::protocol::MobNote {
                            mob_id: mob.clone(),
                            change,
                        },
                    );
                }
            }
        },
    ));
    let mut destroyed = registry.destroyed_mobs();
    let destroyed_task = tokio::spawn({
        let helper_events = Arc::clone(&helper_events);
        async move {
            loop {
                match destroyed.recv().await {
                    Ok(mob) => helper_events.mob_destroyed(&mob),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });
    let follower = tokio::spawn(crate::mobs::follow::watch(
        Arc::downgrade(state),
        Arc::clone(registry),
        helper_events,
        spawning,
    ));
    (follower, destroyed_task)
}

/// The note tools every agent shares (one store per scope).
fn note_tools(scope: &RuntimeScope) -> NoteToolDispatcher {
    NoteToolDispatcher::new(
        Some(NoteStore::open(scope.global.notes_dir(), Scope::User)),
        NoteStore::open(
            crate::memory::workspace_notes_dir(&scope.realm_root()),
            Scope::Workspace,
        ),
    )
}

/// What the daemon supplies for its agents on every spawn and rebuild
/// ([`crate::mobs::customizer::RootHost`]).
struct AgentHost {
    scope: RuntimeScope,
    notes: Arc<NoteToolDispatcher>,
    watches: Arc<super::watches::Watches>,
    diff_tx: tokio::sync::mpsc::UnboundedSender<(String, crate::tools::FileDiff)>,
    engine: Arc<ApprovalEngine>,
    registry: Arc<HelperRegistry>,
    thinking: ThinkingTable,
    endpoints: crate::endpoints::EndpointRegistry,
    instructions: crate::bootstrap::InstructionSources,
    /// `[search]`: how far the file tools' searches go.
    search_limits: crate::tools::SearchLimits,
}

impl crate::mobs::customizer::RootHost for AgentHost {
    /// The agent's file, note and watch tools.
    fn tools(&self, identity: &meerkat_mob::AgentIdentity) -> Arc<dyn AgentToolDispatcher> {
        let diffs = tagged_diffs(
            identity.to_string(),
            self.diff_tx.clone(),
            Arc::downgrade(&self.watches),
        );
        let watch = Arc::new(crate::tools::WatchToolDispatcher::new(
            identity.to_string(),
            Arc::clone(&self.watches) as Arc<dyn crate::tools::WatchHost>,
        ));
        let files = FileToolDispatcher::new(self.scope.project_root.clone(), Some(diffs))
            .with_search_limits(self.search_limits);
        match agent_tools(files, &self.notes, Some(watch)) {
            Ok(tools) => tools,
            Err(err) => {
                // The same composition succeeded at start for the catalog;
                // an agent without its file tools still has the engine's.
                tracing::error!(error = %err, agent = %identity, "the agent's file tools could not be composed");
                Arc::new(meerkat_tools::EmptyToolDispatcher) as Arc<dyn AgentToolDispatcher>
            }
        }
    }

    /// The persona, the agents' and the deferral's notes, the instruction
    /// files and the notes index.
    fn instructions(&self, identity: &meerkat_mob::AgentIdentity) -> Vec<String> {
        let mut sections = vec![
            CODING_PERSONA.to_string(),
            crate::bootstrap::persona::agents_persona(identity.as_str()),
            super::deferral::PERSONA.to_string(),
        ];
        sections.extend(self.instructions.render());
        sections.extend(prompt_index(self.notes.user(), self.notes.workspace()));
        sections
    }

    /// The daemon's gate (`RootGate`: the tiers, rules, classifier and
    /// modes, consent through the approval store); a council the agent
    /// convenes works for it.
    fn admission(&self, key: &HelperKey) -> Option<Arc<dyn meerkat_core::ToolDispatchAdmission>> {
        let gate = Arc::new(RootGate::new(
            Arc::clone(&self.engine),
            Arc::clone(&self.registry),
            key.clone(),
        ));
        Some(Arc::new(crate::mobs::council::Convening::new(
            gate,
            &self.registry,
            None,
        )))
    }

    /// The `[thinking]` of the identity's model on its endpoint.
    fn thinking(
        &self,
        identity: &meerkat_core::SessionLlmIdentity,
    ) -> crate::mobs::customizer::RootThinking {
        let endpoint = crate::endpoints::endpoint_of(
            identity.provider,
            identity.auth_binding.as_ref(),
            &self.endpoints,
        );
        crate::mobs::customizer::RootThinking::Set(self.thinking.resolve(
            endpoint.as_deref(),
            identity.provider,
            &identity.model,
        ))
    }
}

/// One agent's external tools: its own file tools (`files`, reporting
/// every change they make as a diff the clients show), the shared note
/// tools and its watch tools, behind one gateway. Without `watch` (the
/// catalog read at start) the watch tools are named over a host that
/// watches nothing.
fn agent_tools(
    files: FileToolDispatcher,
    notes: &Arc<NoteToolDispatcher>,
    watch: Option<Arc<crate::tools::WatchToolDispatcher>>,
) -> Result<Arc<dyn AgentToolDispatcher>, DaemonError> {
    let files = Arc::new(files);
    let watch = watch.unwrap_or_else(|| {
        Arc::new(crate::tools::WatchToolDispatcher::new(
            "",
            Arc::new(NoWatches) as Arc<dyn crate::tools::WatchHost>,
        ))
    });
    let gateway = ToolGatewayBuilder::new()
        .add_dispatcher(files as Arc<dyn AgentToolDispatcher>)
        .add_dispatcher(Arc::clone(notes) as Arc<dyn AgentToolDispatcher>)
        .add_dispatcher(watch as Arc<dyn AgentToolDispatcher>)
        .build()
        .map_err(TuiError::ToolGateway)?;
    Ok(Arc::new(gateway))
}

/// The watch host of the tool catalog read at start: it watches nothing.
struct NoWatches;

impl crate::tools::WatchHost for NoWatches {
    fn watch(&self, _agent: &str, _patterns: &[String], _once: bool) -> Result<u64, String> {
        Err("no watches here".to_string())
    }

    fn unwatch(&self, _agent: &str, _id: Option<u64>) -> usize {
        0
    }
}

/// A sender of file changes that forwards each to `all` tagged with
/// `agent` (until either end is gone).
fn tagged_diffs(
    agent: String,
    all: tokio::sync::mpsc::UnboundedSender<(String, crate::tools::FileDiff)>,
    watches: std::sync::Weak<super::watches::Watches>,
) -> tokio::sync::mpsc::UnboundedSender<crate::tools::FileDiff> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::tools::FileDiff>();
    tokio::spawn(async move {
        while let Some(diff) = rx.recv().await {
            // The agent's own write: its watches do not report it back.
            if let Some(watches) = watches.upgrade() {
                watches.own_write(&agent, &diff.path);
            }
            if all.send((agent.clone(), diff)).is_err() {
                break;
            }
        }
    });
    tx
}

/// Push every event of the approval engine's `events` to the clients as
/// an [`ApprovalNote`](super::protocol::ApprovalNote) (classifier verdicts, pauses, the classifier's work),
/// and count each classifier call in `ledger` for the conversation whose
/// call it judged, until the engine is gone.
fn forward_approval_notes(
    decisions: &Arc<Decisions>,
    ledger: &Arc<super::usage::Ledger>,
    engine: &Arc<ApprovalEngine>,
    events: Option<tokio::sync::mpsc::UnboundedReceiver<crate::approval::ApprovalEvent>>,
) -> tokio::task::JoinHandle<()> {
    let decisions = Arc::clone(decisions);
    let ledger = Arc::clone(ledger);
    let engine = Arc::clone(engine);
    tokio::spawn(async move {
        let Some(mut events) = events else {
            return;
        };
        while let Some(event) = events.recv().await {
            if let crate::approval::ApprovalEvent::ClassifierUsage {
                session_id,
                model,
                usage,
            } = &event
            {
                ledger.classifier(session_id, model, usage).await;
                continue;
            }
            if let Some(note) = super::protocol::ApprovalNote::of(event) {
                decisions.broadcast(super::protocol::APPROVAL_NOTE, &note);
                // A decision moves the denial counters; a pause or a
                // resume the mode.
                if matches!(
                    note,
                    super::protocol::ApprovalNote::Decision { .. }
                        | super::protocol::ApprovalNote::Paused { .. }
                        | super::protocol::ApprovalNote::Resumed
                ) {
                    decisions.broadcast(
                        super::protocol::MODE,
                        &crate::approval::desk::ModeStatus::of(&engine),
                    );
                }
            }
        }
    })
}
