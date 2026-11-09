//! The daemon's own methods, served before the engine's router sees a
//! line, and what the daemon learns from the engine methods it forwards.
//!
//! The wire is Meerkat's JSON-RPC; the only non-engine methods are these
//! `rkat/*` ones (their names, parameters and results are typed in
//! [`super::protocol`]) (and [`super::handshake`]'s `rkat/attach`). They exist
//! because the approval policy lives in the daemon and the engine's wire
//! has nothing for an application's permission mode or rule list:
//!
//! - [`GET_MODE`] `{}` → [`ModeStatus`] (`{"mode", "paused", "denials"}`);
//! - [`SET_MODE`] `{"mode": "default"|"accept-edits"|"plan"|"auto"}` →
//!   `{"status": ModeStatus, "notice": "…", "warning"?: "…"}`: switches the
//!   daemon's permission mode (one per daemon: it governs the orchestrator
//!   and its helpers, like the in-process mode governs a session and its
//!   helpers), records it in the realm rules file and tells the
//!   orchestrator when plan mode starts or ends;
//! - [`RULES`] a [`RuleCommand`] (`{"op": "add"|"remove"|"list", …}`) →
//!   [`crate::approval::desk::RuleOutcome`]: `/rules` run on the daemon's
//!   engine, which persists to the realm rules file as the in-process one
//!   does.
//!
//! - [`CONVERSATIONS`] `{}` → `{"conversations": [Listed]}`: the
//!   orchestrator's conversations, last used first
//!   ([`super::conversations`]);
//! - [`NEW`] `{}` → `{"session_id"}`: `/new`, the orchestrator on a fresh
//!   session;
//! - [`RESUME`] `{"session_id"}` → `{"session_id"}`: `/resume`, the
//!   orchestrator back on an earlier session of its own; both are refused
//!   with [`SWITCH_BUSY`] while the orchestrator's run is open (another
//!   client's turn);
//! - [`TRANSCRIPT`] `{"session_id"}` → `{"messages": [Message]}`: a
//!   conversation's committed messages (the client renders a resumed
//!   conversation from them, as the in-process `/resume` does);
//! - [`ENDPOINT`] `{"session_id"}` → `{"endpoint"?}`: the configured
//!   endpoint a conversation's durable auth binding names (absent: a
//!   built-in one), which the status bar shows;
//! - [`USAGE`] `{"session_id"}` → `{"input_tokens", "output_tokens"}`: a
//!   conversation's total usage as the engine keeps it with the session (the
//!   client seeds its token counter from it on attach and after a switch,
//!   as the in-process runner does from `Session::total_usage`; the engine's
//!   wire reports usage only per turn);
//! - [`TRUST`] `{"op": "project"|"rules"}` → `{"ok", "text"}`: the
//!   project trust record the daemon owns (a missing `op` is refused).
//!   `project` is `/trust`
//!   (every present input recorded as it is now, in the host trust record
//!   the daemon's realm config store and factory read on every session
//!   build); `rules` is `/rules trust` (the realm rules file's summary, then
//!   its digest recorded and its allow rules applied).
//!
//! - [`NOTICES`] `{}` → `{"notices": [{"warn", "text"}]}`: what the
//!   daemon's start-up had to say (the confinement, the rules, the
//!   configuration); a client shows them when it first attaches to this
//!   daemon, as the in-process TUI shows its own at start;
//! - [`TASKS`] `{"session_id"}` → `{"tasks": [Task]}`: a conversation's
//!   tasks in the engine's task store (the Ctrl+T checklist; the client
//!   reads them on attach, after a switch and after each of the agent's
//!   `task_create` and `task_update` calls);
//! - [`CHECKLIST_VIEW`] `{"view": "hidden"|"shown"}` → `{}`: Ctrl+T's
//!   choice saved as the realm's `[ui] checklist`, through the engine that
//!   owns the realm file and its trust record.
//!
//! - [`STATUS`] `{}` → [`Status`]: the workspace, pid, start time, the
//!   attached clients, whether anyone detached, whether the caller started
//!   the daemon, the current conversation and whether its turn runs
//!   ([`super::lifecycle`]; `/quit` decides from it, `--list` prints it);
//! - [`DETACH`] `{}` → `{}`: `/detach`, recorded so that a later `/quit`
//!   asks before stopping the daemon;
//! - [`SHUTDOWN`] `{}` → `{}`: stop the daemon (its endpoint file goes
//!   first, as on SIGTERM).
//!
//! A future engine extension (an application-defined method namespace, or
//! the governed realm's own review policy) could replace them.
//!
//! What the forwarded methods tell the daemon ([`Local::observe`]): a
//! prompt or steer for the orchestrator (`mob/member_send`,
//! `mob/turn_start`) is the human's words, recorded in the classifier's
//! transcript as the in-process session records them at admission (the
//! connection proved it holds the daemon's token); an interrupt
//! (`mob/stop_member_run`, `mob/force_cancel`) stops the session's review
//! and answers its open escalations with Stop, and stops the session's
//! running helpers, as the in-process runner does; and a `mob/turn_start` that switches the model carries the
//! `[thinking]` provider params for the new model unless it names its own.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use meerkat_core::lifecycle::run_primitive::TurnMetadataOverride;
use meerkat_core::{AppendSystemContextRequest, Provider, SessionId};
use meerkat_mob::{AgentIdentity, MobId};
use meerkat_mob_mcp::MobMcpState;
use serde_json::{Value, json};

use super::approvals::{DaemonToolOrigins, Desk};
use super::conversations::{Conversations, Target};
use super::{MOB_ID, ORCHESTRATOR};
use crate::approval::desk::{ModeStatus, RuleCommand, RuleOutcome, mode_notice, run_rule_command};
use crate::approval::mode::PermissionMode;
use crate::approval::{ApprovalEngine, InputKey};
use crate::bootstrap::project_trust::ProjectInputs;
use crate::endpoints::{ENDPOINT_REALM, WireDialect};
use crate::mobs::HelperRegistry;
use crate::thinking::{self, ThinkingTable};

use super::lifecycle::{Caller, Lifecycle};
use super::protocol::{
    self, ConversationList, METHOD_NOT_FOUND, ModeSet, SWITCH_BUSY, SWITCH_FAILED, SessionParams,
    SetModeParams, Switched, Transcript, TrustOp, TrustParams, Usage, engine,
};
use super::protocol::{
    CHECKLIST_VIEW, ChecklistViewParams, ModelSet, SET_MODEL, SetModelParams, TASKS, Tasks,
};
use super::protocol::{
    CONVERSATIONS, ENDPOINT, GET_MODE, NEW, RESUME, RULES, Refusal, SET_MODE, SessionEndpoint,
    TRANSCRIPT, TRUST, USAGE,
};
use super::protocol::{DETACH, SHUTDOWN, STATUS, Status};
use super::protocol::{NOTICES, Notices, StartupNotice};

/// The idempotency source of the daemon's system-context notes.
const SYSTEM_CONTEXT_SOURCE: &str = "rkat-tui";

/// What the local methods and the observer act on.
pub struct Local {
    engine: Arc<ApprovalEngine>,
    state: Arc<MobMcpState>,
    conversations: Arc<Conversations>,
    registry: Arc<HelperRegistry>,
    origins: Arc<DaemonToolOrigins>,
    desk: Arc<Desk>,
    thinking: ThinkingTable,
    /// The orchestrator was told plan mode is on. Starts as the persisted
    /// mode says (a daemon started in plan mode tells the orchestrator,
    /// [`Local::announce_plan_mode`]), so leaving plan mode after a restart
    /// says so. An async lock held across the note's append, so two
    /// overlapping mode changes (on any connections) cannot interleave
    /// their check and their note.
    plan_noted: tokio::sync::Mutex<bool>,
    inputs: AtomicU64,
    /// The workspace's project inputs and their trust (`rkat/trust`).
    project: ProjectInputs,
    /// The consent surface: the decisions offered to the clients.
    decisions: Arc<super::decisions::Decisions>,
    /// The engine's task store (`rkat/tasks`).
    tasks_db: std::path::PathBuf,
    /// The runtime the orchestrator's session lives in (`rkat/set_model`).
    machine: Arc<meerkat_runtime::MeerkatMachine>,
    /// The realm config (a switched-to model's profile is written there).
    realm_config: Arc<crate::bootstrap::RealmConfigStore>,
    /// What the start-up had to say (`rkat/notices`).
    startup_notices: Vec<StartupNotice>,
    /// The clients, the detach record and the shutdown request
    /// (`rkat/status`, `rkat/detach`, `rkat/shutdown`).
    lifecycle: Arc<Lifecycle>,
}

/// What [`Local`] is made of.
pub struct LocalParts {
    /// The daemon's approval engine.
    pub engine: Arc<ApprovalEngine>,
    /// The mob host.
    pub state: Arc<MobMcpState>,
    /// The orchestrator's conversations.
    pub conversations: Arc<Conversations>,
    /// The helpers the orchestrator started.
    pub registry: Arc<HelperRegistry>,
    /// The members' tool catalogs the gate reads.
    pub origins: Arc<DaemonToolOrigins>,
    /// The daemon's approval desk.
    pub desk: Arc<Desk>,
    /// The `[thinking]` settings a model switch carries.
    pub thinking: ThinkingTable,
    /// The workspace's project inputs and their trust.
    pub project: ProjectInputs,
    /// The consent surface.
    pub decisions: Arc<super::decisions::Decisions>,
    /// The engine's task store.
    pub tasks_db: std::path::PathBuf,
    /// The runtime the members' sessions live in.
    pub machine: Arc<meerkat_runtime::MeerkatMachine>,
    /// The realm config store.
    pub realm_config: Arc<crate::bootstrap::RealmConfigStore>,
    /// What the start-up had to say.
    pub startup_notices: Vec<StartupNotice>,
    /// The daemon's lifecycle state.
    pub lifecycle: Arc<Lifecycle>,
}

impl std::fmt::Debug for Local {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Local")
            .field("conversations", &self.conversations)
            .finish_non_exhaustive()
    }
}

impl Local {
    /// The local surface over `parts`.
    #[must_use]
    pub fn new(parts: LocalParts) -> Self {
        let LocalParts {
            engine,
            state,
            conversations,
            registry,
            origins,
            desk,
            thinking,
            project,
            decisions,
            tasks_db,
            machine,
            realm_config,
            startup_notices,
            lifecycle,
        } = parts;
        let in_plan = engine.mode() == PermissionMode::Plan;
        Self {
            engine,
            state,
            conversations,
            registry,
            origins,
            desk,
            thinking,
            plan_noted: tokio::sync::Mutex::new(in_plan),
            inputs: AtomicU64::new(0),
            project,
            decisions,
            tasks_db,
            machine,
            realm_config,
            startup_notices,
            lifecycle,
        }
    }

    /// The daemon's lifecycle state.
    #[must_use]
    pub fn lifecycle(&self) -> &Arc<Lifecycle> {
        &self.lifecycle
    }

    /// `rkat/status` for `caller`.
    async fn status(&self, caller: Caller) -> Status {
        Status {
            workspace: self.lifecycle.workspace().to_path_buf(),
            host: crate::client::address::local_host(),
            pid: std::process::id(),
            started_at: self.lifecycle.started_at().to_string(),
            clients: self.lifecycle.clients(),
            detached: self.lifecycle.detached(),
            owner: caller.owner,
            session_id: self.session(),
            running: self.conversations.running().await.unwrap_or(false),
        }
    }

    /// The consent surface.
    #[must_use]
    pub fn decisions(&self) -> &Arc<super::decisions::Decisions> {
        &self.decisions
    }

    /// Answer the local method `method` for `caller`.
    ///
    /// # Errors
    ///
    /// [`protocol::METHOD_NOT_FOUND`] for an unknown `rkat/*` method,
    /// [`protocol::INVALID_PARAMS`] for bad parameters.
    pub async fn call(
        &self,
        caller: Caller,
        method: &str,
        params: Value,
    ) -> Result<Value, Refusal> {
        match method {
            STATUS => answer(self.status(caller).await),
            DETACH => {
                self.lifecycle.detach();
                answer(json!({}))
            }
            SHUTDOWN => {
                tracing::info!(owner = caller.owner, "rkat/shutdown: shutting down");
                self.lifecycle.shutdown();
                answer(json!({}))
            }
            GET_MODE => answer(ModeStatus::of(&self.engine)),
            SET_MODE => {
                let SetModeParams { mode } = protocol::params(params)?;
                let mode = PermissionMode::parse(&mode).ok_or_else(|| {
                    Refusal::new(
                        protocol::INVALID_PARAMS,
                        "`mode` must be one of default, accept-edits, plan, auto",
                    )
                })?;
                answer(self.set_mode(mode).await)
            }
            RULES => {
                let command: RuleCommand = protocol::params(params)?;
                answer(run_rule_command(&self.engine, &command))
            }
            CONVERSATIONS => answer(ConversationList {
                conversations: self.conversations.list().await,
            }),
            NEW => answer(self.switch(Target::Fresh).await?),
            RESUME => {
                let SessionParams { session_id } = protocol::params(params)?;
                answer(self.switch(Target::Resume(session_id)).await?)
            }
            TRANSCRIPT => {
                let SessionParams { session_id } = protocol::params(params)?;
                let messages = self
                    .conversations
                    .messages(&session_id)
                    .await
                    .map_err(|err| Refusal::new(SWITCH_FAILED, err.to_string()))?;
                answer(Transcript { messages })
            }
            ENDPOINT => {
                let SessionParams { session_id } = protocol::params(params)?;
                answer(SessionEndpoint {
                    endpoint: self.conversations.endpoint_of(&session_id).await,
                })
            }
            USAGE => {
                let SessionParams { session_id } = protocol::params(params)?;
                let usage = self
                    .conversations
                    .usage(&session_id)
                    .await
                    .map_err(|err| Refusal::new(SWITCH_FAILED, err.to_string()))?;
                answer(Usage {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                })
            }
            TASKS => {
                let SessionParams { session_id } = protocol::params(params)?;
                let tasks = crate::workbench::tasks::read_tasks(&self.tasks_db, &session_id)
                    .await
                    .map_err(|err| Refusal::new(protocol::INTERNAL_ERROR, err.to_string()))?;
                answer(Tasks { tasks })
            }
            NOTICES => answer(Notices {
                notices: self.startup_notices.clone(),
            }),
            SET_MODEL => {
                let params: SetModelParams = protocol::params(params)?;
                answer(self.set_model(params).await?)
            }
            CHECKLIST_VIEW => {
                let ChecklistViewParams { view } = protocol::params(params)?;
                self.engine
                    .persist_checklist_view(view)
                    .map_err(|err| Refusal::new(protocol::INTERNAL_ERROR, err.to_string()))?;
                answer(json!({}))
            }
            TRUST => {
                let TrustParams { op } = protocol::params(params)?;
                match op {
                    TrustOp::Project => answer(self.trust_project()),
                    TrustOp::Rules => answer(match self.engine.trust_realm_rules() {
                        Ok(summary) => RuleOutcome {
                            ok: true,
                            text: summary,
                        },
                        Err(err) => RuleOutcome {
                            ok: false,
                            text: format!("not trusted: {err}"),
                        },
                    }),
                }
            }
            other => Err(Refusal::new(
                METHOD_NOT_FOUND,
                format!("unknown method `{other}`"),
            )),
        }
    }

    /// `/new` or `/resume`: switch the orchestrator's conversation, forget
    /// the gate's state of the one left (its classifier transcript, allow
    /// cache and helpers; a later `/resume` seeds the transcript again),
    /// then tell the new one about plan mode when it is on, and read its
    /// tools.
    async fn switch(&self, target: Target) -> Result<Switched, Refusal> {
        let resumed = matches!(target, Target::Resume(_));
        let previous = self.conversations.current();
        let session = self.conversations.switch(target).await.map_err(|err| {
            let code = match err {
                super::conversations::SwitchError::Running => SWITCH_BUSY,
                _ => SWITCH_FAILED,
            };
            Refusal::new(code, err.to_string())
        })?;
        if let Some(previous) = previous.filter(|previous| *previous != session) {
            self.engine.forget_session(&previous);
        }
        if resumed {
            self.conversations.seed(&self.engine, &session).await;
        }
        // The new conversation was never told anything.
        *self.plan_noted.lock().await = false;
        let warning = self.note_plan_mode().await;
        let origins = Arc::clone(&self.origins);
        let idle = session.clone();
        tokio::spawn(async move {
            origins.refresh(&idle).await;
        });
        Ok(Switched {
            session_id: session,
            warning,
        })
    }

    /// `rkat/trust {"op": "project"}`: `/trust`, every present project
    /// input recorded as it is now. The record is the one the realm config
    /// store's guard and the factory's hook-source check read on every
    /// session build, so the hooks run from the orchestrator's next build
    /// (`/new`, `/resume`, a daemon restart). The orchestrator's MCP servers
    /// are part of its mob's definition, made when the mob was first
    /// created; trusting `.rkat/mcp.toml` here does not add them to it.
    fn trust_project(&self) -> RuleOutcome {
        match self.project.trust_all() {
            Ok(recorded) if recorded.is_empty() => RuleOutcome {
                ok: true,
                text: "the project has no .rkat/mcp.toml, .rkat/config.toml or workspace realm config.toml to trust".to_string(),
            },
            Ok(recorded) => {
                let files = recorded
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                RuleOutcome {
                    ok: true,
                    text: format!(
                        "trusted {files} as they are now; their hooks run from the agent's next session (/new, /resume or a daemon restart); in attach mode the agent's MCP servers are the ones its mob was created with"
                    ),
                }
            }
            Err(err) => RuleOutcome {
                ok: false,
                text: format!("not trusted: {err}"),
            },
        }
    }

    /// `rkat/set_model`: switch the orchestrator's session to another model
    /// in place, as the in-process `/model` does (`SessionHandle::
    /// switch_model`): the engine's LLM reconfigure on the live session,
    /// with the endpoint's auth binding and the `[thinking]` setting the
    /// new model is configured with. Refused while a turn runs (as
    /// in-process); the session keeps the model, and the next prompt is an
    /// ordinary `mob/member_send`, tied to no connection.
    async fn set_model(&self, params: SetModelParams) -> Result<ModelSet, Refusal> {
        use meerkat_runtime::SessionServiceRuntimeExt as _;

        let SetModelParams {
            model,
            provider,
            auth_binding,
            listed,
        } = params;
        // A model only an endpoint knows needs its profile in the realm
        // config before the runtime accepts it, as the in-process switch
        // registered one.
        let mut reseated = false;
        if let Some(dialect) = provider.as_deref().and_then(WireDialect::parse) {
            let listed = listed.map(|listed| crate::models::ModelEntry {
                id: model.clone(),
                display_name: None,
                source: crate::models::ModelSource::Provider,
                context_window: listed.context_window,
                max_output_tokens: listed.max_output_tokens,
            });
            let written = crate::endpoints::ensure_model_profile(
                self.realm_config.runtime(),
                dialect.into(),
                &model,
                listed.as_ref(),
            )
            .await
            .map_err(|err| Refusal::new(SWITCH_FAILED, err.to_string()))?;
            // The live agent's model registry predates the profile: seat
            // the orchestrator again on its session, as the in-process
            // switch rebuilt its agent.
            if written {
                reseated = true;
                self.conversations.reseat().await.map_err(|err| {
                    let code = match err {
                        super::conversations::SwitchError::Running => SWITCH_BUSY,
                        _ => SWITCH_FAILED,
                    };
                    Refusal::new(code, err.to_string())
                })?;
            }
        }
        let provider_params = provider.as_deref().and_then(|provider| {
            let dialect = WireDialect::parse(provider)?;
            // A configured endpoint is named by its auth binding.
            let endpoint = auth_binding
                .as_ref()
                .filter(|binding| binding.realm.as_str() == ENDPOINT_REALM)
                .map_or(provider, |binding| binding.binding.as_str());
            let configured = self
                .thinking
                .resolve(Some(endpoint), dialect.into(), &model);
            Some(match thinking::params_for(configured.as_ref()) {
                Some(params) => TurnMetadataOverride::Set(params),
                None => TurnMetadataOverride::Clear,
            })
        });
        let request = meerkat_runtime::SessionLlmReconfigureRequest {
            model: Some(model),
            provider,
            self_hosted_server_id: None,
            provider_params,
            auth_binding: Some(match auth_binding {
                Some(binding) => TurnMetadataOverride::Set(binding),
                None => TurnMetadataOverride::Clear,
            }),
        };
        let machine = Arc::clone(&self.machine);
        let switched = self
            .conversations
            .while_idle(|session| async move {
                let report = machine
                    .reconfigure_session_llm_identity(&session, request)
                    .await?;
                Ok::<_, meerkat_runtime::RuntimeDriverError>((session, report))
            })
            .await
            .map_err(|err| {
                let code = match err {
                    super::conversations::SwitchError::Running => SWITCH_BUSY,
                    _ => SWITCH_FAILED,
                };
                let message = match err {
                    super::conversations::SwitchError::Running => {
                        "a turn is running; switch models once it ends (or interrupt it)"
                            .to_string()
                    }
                    other => other.to_string(),
                };
                Refusal::new(code, message)
            })?;
        let (session, report) =
            switched.map_err(|err| Refusal::new(SWITCH_FAILED, err.to_string()))?;
        tracing::debug!(?report, "switched the orchestrator's model");
        Ok(ModelSet {
            session_id: session,
            model: report.new_identity.model,
            provider: report.new_identity.provider.as_str().to_string(),
            reseated,
        })
    }

    /// The session the orchestrator runs as now.
    fn session(&self) -> Option<SessionId> {
        self.conversations.current()
    }

    async fn set_mode(&self, mode: PermissionMode) -> ModeSet {
        self.engine.set_mode(mode);
        let mut warnings = Vec::new();
        if let Err(err) = self.engine.persist_mode() {
            warnings.push(format!(
                "the mode applies to this run but was not saved: {err}"
            ));
        }
        if let Some(warning) = self.note_plan_mode().await {
            warnings.push(warning);
        }
        ModeSet {
            status: ModeStatus::of(&self.engine),
            notice: mode_notice(mode).to_string(),
            warning: (!warnings.is_empty()).then(|| warnings.join("\n")),
        }
    }

    /// At start: when the persisted mode is plan, tell the orchestrator
    /// (a fresh one never heard it; a restored one may have been switched
    /// to plan mode by another process). Returns a warning when it could
    /// not be told.
    pub async fn announce_plan_mode(&self) -> Option<String> {
        let mut noted = self.plan_noted.lock().await;
        if self.engine.mode() != PermissionMode::Plan {
            return None;
        }
        let warning = self.append_plan_note(true).await;
        if warning.is_none() {
            *noted = true;
        }
        warning
    }

    /// Tell the orchestrator plan mode started or ended, once per change.
    /// The mode is read under the lock, not taken from the caller: of two
    /// overlapping changes, the later note always matches the mode that
    /// holds, and the notes alternate.
    async fn note_plan_mode(&self) -> Option<String> {
        let mut noted = self.plan_noted.lock().await;
        let plan = self.engine.mode() == PermissionMode::Plan;
        if *noted == plan {
            return None;
        }
        let warning = self.append_plan_note(plan).await;
        if warning.is_none() {
            *noted = plan;
        }
        warning
    }

    async fn append_plan_note(&self, plan: bool) -> Option<String> {
        let note = if plan {
            crate::approval::mode::PLAN_ON_NOTE
        } else {
            crate::approval::mode::PLAN_OFF_NOTE
        };
        let mut request = AppendSystemContextRequest::from_text(note.to_string());
        request.source = Some(SYSTEM_CONTEXT_SOURCE.to_string());
        request.idempotency_key = Some(crate::approval::mode::plan_note_key(plan));
        match self
            .state
            .mob_append_system_context(
                &MobId::from(MOB_ID),
                &AgentIdentity::from(ORCHESTRATOR),
                request,
            )
            .await
        {
            Ok(_) => None,
            Err(err) => Some(format!(
                "the agent could not be told about plan mode now (the tool errors say it): {err}"
            )),
        }
    }

    /// Learn from an engine request on its way to the router (see the
    /// module docs); returns the request as it is forwarded.
    #[must_use]
    pub fn observe(&self, mut request: Value) -> Value {
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let Some(params) = request.get_mut("params") else {
            return request;
        };
        let ours = params.get("mob_id").and_then(Value::as_str) == Some(MOB_ID)
            && params.get("agent_identity").and_then(Value::as_str) == Some(ORCHESTRATOR);
        if !ours {
            return request;
        }
        match method.as_str() {
            engine::MEMBER_SEND | engine::TURN_START => {
                let field = if method == engine::MEMBER_SEND {
                    "content"
                } else {
                    "prompt"
                };
                if let Some(text) = params.get(field).and_then(content_text)
                    && !text.trim().is_empty()
                    && let Some(session) = self.session()
                {
                    let n = self.inputs.fetch_add(1, Ordering::Relaxed);
                    let key = InputKey::new(format!("rkat-attach:{}:{n}", std::process::id()));
                    self.engine.record_user_input(&session, key, &text);
                }
                if method == engine::TURN_START {
                    self.with_thinking(params);
                }
            }
            engine::STOP_MEMBER_RUN | engine::FORCE_CANCEL => {
                if let Some(session) = self.session() {
                    self.engine.stop(&session);
                    self.desk.stop(&session);
                    // The `delegate` call that would retire a helper is
                    // abandoned with the run: stop the helpers too.
                    let state = Arc::clone(&self.state);
                    let registry = Arc::clone(&self.registry);
                    tokio::spawn(async move {
                        crate::mobs::cancel_helpers(&state, &registry, &session).await;
                    });
                }
            }
            _ => {}
        }
        request
    }

    /// A model switch carries the `[thinking]` the new model is configured
    /// with (or clears the old model's), unless the caller names params.
    fn with_thinking(&self, params: &mut Value) {
        if params.get("provider_params").is_some() {
            return;
        }
        let (Some(model), Some(provider)) = (
            params.get("model").and_then(Value::as_str),
            params.get("provider").and_then(Value::as_str),
        ) else {
            return;
        };
        let Some(dialect) = WireDialect::parse(provider) else {
            return;
        };
        let provider_kind: Provider = dialect.into();
        // A configured endpoint is named by its auth binding.
        let endpoint = params
            .pointer("/auth_binding/value")
            .filter(|binding| binding.get("realm").and_then(Value::as_str) == Some(ENDPOINT_REALM))
            .and_then(|binding| binding.get("binding").and_then(Value::as_str))
            .unwrap_or(provider);
        let configured = self.thinking.resolve(Some(endpoint), provider_kind, model);
        params["provider_params"] = thinking_override(configured.as_ref());
    }
}

/// The `provider_params` override of a model switch: set the thinking the
/// new model is configured with, or clear the old model's.
#[must_use]
pub fn thinking_override(
    thinking: Option<&meerkat_core::lifecycle::run_primitive::AnthropicThinkingConfig>,
) -> Value {
    match thinking::params_for(thinking) {
        Some(params) => json!({
            "action": "set",
            "value": meerkat_contracts::wire::runtime::WireProviderParamsOverride::from(params),
        }),
        None => json!({"action": "clear"}),
    }
}

/// A method's answer as JSON.
fn answer(result: impl serde::Serialize) -> Result<Value, Refusal> {
    serde_json::to_value(result)
        .map_err(|err| Refusal::new(protocol::INTERNAL_ERROR, err.to_string()))
}

/// The text of a wire content input: a string, or the text blocks of a
/// block list.
fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn content_text_reads_strings_and_text_blocks() {
        assert_eq!(content_text(&json!("hi")).as_deref(), Some("hi"));
        assert_eq!(
            content_text(&json!([{"type": "text", "text": "a"}, {"type": "image"}, {"type": "text", "text": "b"}]))
                .as_deref(),
            Some("a\nb")
        );
        assert_eq!(content_text(&json!(3)), None);
    }

    #[test]
    fn a_model_switch_sets_or_clears_thinking() {
        let clear = thinking_override(None);
        assert_eq!(clear, json!({"action": "clear"}));
        let set = thinking_override(Some(
            &meerkat_core::lifecycle::run_primitive::AnthropicThinkingConfig::Adaptive,
        ));
        assert_eq!(set["action"], "set");
        assert!(set["value"]["provider_tag"].is_object(), "{set}");
    }
}
