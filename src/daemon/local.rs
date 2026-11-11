//! The daemon's own methods, served before the engine's router sees a
//! line, and what the daemon learns from the engine methods it forwards.
//!
//! The wire is Meerkat's JSON-RPC; the only non-engine methods are these
//! `rkat/*` ones (and [`super::handshake`]'s `rkat/attach`). They exist
//! because the approval policy and the agents' lifecycle live in the
//! daemon and the engine's wire has nothing for an application's
//! permission mode, rules, conversations or agents. Each is declared once,
//! with its parameters and result, in [`protocol::method`]
//! ([`protocol::Method`]); [`Local::call`] dispatches on those
//! declarations, so a handler answers its method's result type or does not
//! compile.
//!
//! A future engine extension (an application-defined method namespace, or
//! the governed realm's own review policy) could replace them.
//!
//! What a connection may send the engine ([`Local::observe`]): a member
//! action ([`engine::MEMBER_ACTIONS`]) on its own agent or that agent's
//! helpers, a `session/compact` of its agent's current conversation, and
//! any read; anything else is answered with [`protocol::NOT_YOURS`] and
//! never forwarded.
//!
//! What the forwarded methods tell the daemon ([`Local::observe`]): a
//! prompt or steer for the orchestrator (`mob/member_send`,
//! `mob/turn_start`) is the human's words, recorded in the classifier's
//! transcript (the connection proved it holds the daemon's token), and the
//! run that takes it is one a user started (attended, see
//! [`crate::approval::attendance`]); an
//! interrupt
//! (`mob/stop_member_run`, `mob/force_cancel`) stops the session's review
//! and answers its open escalations with Stop, and stops the session's
//! running helpers; and a `mob/turn_start` that switches the model carries the
//! `[thinking]` provider params for the new model unless it names its own.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use meerkat_core::lifecycle::run_primitive::TurnMetadataOverride;
use meerkat_core::{AppendSystemContextRequest, Provider};
use meerkat_mob::{AgentIdentity, MobId};
use meerkat_mob_mcp::MobMcpState;
use serde_json::{Value, json};

use super::MOB_ID;
use super::agents::Agents;
use super::approvals::{DaemonToolOrigins, Desk};
use super::conversations::{Conversations, Target};
use crate::approval::desk::{ModeStatus, RuleOutcome, mode_notice, run_rule_command};
use crate::approval::mode::PermissionMode;
use crate::approval::{ApprovalEngine, InputKey};
use crate::bootstrap::project_trust::ProjectInputs;
use crate::endpoints::{WireDialect, is_endpoint_realm};
use crate::mobs::HelperRegistry;
use crate::thinking::{self, ThinkingTable};

use super::lifecycle::{Caller, Lifecycle};
use super::protocol::{
    self, AUDIT_LINES, AuditTail, ConversationList, Empty, METHOD_NOT_FOUND, Method, ModeSet,
    ModelSet, Notices, Refusal, Report, Retired, SWITCH_BUSY, SWITCH_FAILED, SetModelParams,
    StartupNotice, Status, Switched, Tasks, TrustedFile, Usage, engine, method,
};

/// `reason` for asking at `/quit`, with what detaching or terminating does
/// to the caller's agent's schedules, when it has any.
fn with_schedules(reason: String, status: &Status) -> String {
    let count = status
        .agent
        .as_ref()
        .and_then(|agent| status.agents.iter().find(|row| &row.name == agent))
        .map_or(0, |row| row.schedules);
    if count == 0 {
        return reason;
    }
    format!(
        "{reason}\nThis agent has {count} schedule{}: detaching keeps {} firing; terminating deletes {}.",
        if count == 1 { "" } else { "s" },
        if count == 1 { "it" } else { "them" },
        if count == 1 { "it" } else { "them" },
    )
}

/// The idempotency source of the daemon's system-context notes.
const SYSTEM_CONTEXT_SOURCE: &str = "rkat-tui";

/// What the local methods and the observer act on.
pub struct Local {
    engine: Arc<ApprovalEngine>,
    state: Arc<MobMcpState>,
    conversations: Arc<Conversations>,
    agents: Arc<Agents>,
    registry: Arc<HelperRegistry>,
    origins: Arc<DaemonToolOrigins>,
    desk: Arc<Desk>,
    thinking: ThinkingTable,
    /// Which agents were told plan mode is on (an agent missing was told
    /// nothing: it is told when plan mode is on). A daemon started in plan
    /// mode tells every agent ([`Local::announce_plan_mode`]), so leaving
    /// plan mode after a restart says so. An async lock held across the
    /// notes' appends, so two overlapping mode changes (on any connections)
    /// cannot interleave their check and their note.
    plan_noted: tokio::sync::Mutex<std::collections::HashMap<String, bool>>,
    inputs: AtomicU64,
    /// The workspace's project inputs and their trust (`rkat/trust`).
    project: ProjectInputs,
    /// The consent surface: the decisions offered to the clients.
    decisions: Arc<super::decisions::Decisions>,
    /// The engine's task store (`rkat/tasks`).
    tasks_db: std::path::PathBuf,
    realm_file: Option<crate::config::RealmFile>,
    /// The runtime the orchestrator's session lives in (`rkat/set_model`).
    machine: Arc<meerkat_runtime::MeerkatMachine>,
    /// The realm config (a switched-to model's profile is written there).
    realm_config: Arc<crate::bootstrap::RealmConfigStore>,
    /// The daemon's endpoints (a switch names one by its binding).
    endpoints: crate::endpoints::EndpointRegistry,
    /// What the start-up had to say (`rkat/notices`).
    startup_notices: Vec<StartupNotice>,
    /// The clients, the detach record and the shutdown request
    /// (`rkat/status`, `rkat/detach`, `rkat/shutdown`).
    lifecycle: Arc<Lifecycle>,
    /// The fixed part of `rkat/report`.
    report: ReportParts,
    /// The audit log (`rkat/audit`), when it could be opened.
    audit: Option<Arc<super::audit::AuditLog>>,
}

/// What [`Local`] is made of.
pub struct LocalParts {
    /// The daemon's approval engine.
    pub engine: Arc<ApprovalEngine>,
    /// The mob host.
    pub state: Arc<MobMcpState>,
    /// The agents' conversations.
    pub conversations: Arc<Conversations>,
    /// The live agents.
    pub agents: Arc<Agents>,
    /// The helpers the agents started.
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
    /// The realm's `tui.toml` as rkat-tui writes it (the checklist view).
    pub realm_file: Option<crate::config::RealmFile>,
    /// The runtime the members' sessions live in.
    pub machine: Arc<meerkat_runtime::MeerkatMachine>,
    /// The realm config store.
    pub realm_config: Arc<crate::bootstrap::RealmConfigStore>,
    /// The daemon's endpoints.
    pub endpoints: crate::endpoints::EndpointRegistry,
    /// What the start-up had to say.
    pub startup_notices: Vec<StartupNotice>,
    /// The daemon's lifecycle state.
    pub lifecycle: Arc<Lifecycle>,
    /// What `rkat/report` names besides the engine's state.
    pub report: ReportParts,
    /// The audit log, when it could be opened.
    pub audit: Option<Arc<super::audit::AuditLog>>,
}

/// The reason recorded on an input `rkat/recall` abandons.
const RECALL_REASON: &str = "rkat-tui: recalled by the user";

/// The fixed part of [`protocol::method::Report`]'s answer.
#[derive(Debug, Clone)]
pub struct ReportParts {
    /// The realm directory.
    pub realm: std::path::PathBuf,
    /// The realm id.
    pub realm_id: String,
    /// The shell's confinement mode.
    pub confinement: String,
    /// `[helpers] max_depth`.
    pub helper_depth: u8,
    /// The agent shell's confinement (`rkat/status`).
    pub shell: meerkat_tools::builtin::shell::ShellConfinement,
}

/// A trust state in words.
fn trust_words(status: &crate::bootstrap::trust::TrustStatus) -> String {
    use crate::bootstrap::trust::TrustStatus;
    match status {
        TrustStatus::Missing => "absent".to_string(),
        TrustStatus::Trusted => "trusted".to_string(),
        TrustStatus::Untrusted(why) => format!("not trusted: {why}"),
    }
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
            agents,
            registry,
            origins,
            desk,
            thinking,
            project,
            decisions,
            tasks_db,
            realm_file,
            machine,
            realm_config,
            endpoints,
            startup_notices,
            lifecycle,
            report,
            audit,
        } = parts;
        // In plan mode every restored agent heard it (its conversation
        // has the note): leaving plan mode tells it.
        let in_plan = engine.mode() == PermissionMode::Plan;
        let noted = agents
            .names()
            .into_iter()
            .map(|name| (name, in_plan))
            .collect();
        Self {
            engine,
            state,
            conversations,
            agents,
            registry,
            origins,
            desk,
            thinking,
            plan_noted: tokio::sync::Mutex::new(noted),
            inputs: AtomicU64::new(0),
            project,
            decisions,
            tasks_db,
            realm_file,
            machine,
            realm_config,
            endpoints,
            startup_notices,
            lifecycle,
            report,
            audit,
        }
    }

    /// Append `line` to the audit log (nothing when there is none).
    pub fn audit(&self, line: super::audit::Line) {
        if let Some(log) = &self.audit {
            log.write(&line);
        }
    }

    /// `rkat/audit`: the last lines of the audit log for `agent` (its
    /// decisions and the workspace-wide changes of authority).
    fn audit_tail(&self, agent: &str, lines: usize) -> Result<AuditTail, Refusal> {
        let Some(log) = &self.audit else {
            return Err(Refusal::new(
                protocol::INTERNAL_ERROR,
                "this daemon keeps no audit log (its start-up notice says why)",
            ));
        };
        let lines = super::audit::tail(log.path(), lines, |line| {
            line.agent.as_deref().is_none_or(|of| of == agent)
        })
        .map_err(|err| Refusal::new(protocol::INTERNAL_ERROR, err.to_string()))?;
        Ok(AuditTail {
            path: log.path().to_path_buf(),
            lines,
        })
    }

    /// The daemon's lifecycle state.
    #[must_use]
    pub fn lifecycle(&self) -> &Arc<Lifecycle> {
        &self.lifecycle
    }

    /// The helpers the agents started.
    #[must_use]
    pub fn registry(&self) -> &Arc<HelperRegistry> {
        &self.registry
    }

    /// The live agents.
    #[must_use]
    pub fn agents(&self) -> &Arc<Agents> {
        &self.agents
    }

    /// The agent `caller` works with: its binding, else (a connection that
    /// asked for none) the most recently active one.
    ///
    /// # Errors
    ///
    /// [`protocol::NO_AGENT`] when no agent is live.
    fn agent_of(&self, caller: &Caller) -> Result<String, Refusal> {
        caller
            .agent
            .clone()
            .or_else(|| self.agents.recent())
            .ok_or_else(|| Refusal::new(protocol::NO_AGENT, "no agent works in this workspace"))
    }

    /// `rkat/status` for `caller`.
    async fn status(&self, caller: &Caller) -> Status {
        let agents = self.agents.rows().await;
        let agent = self.agent_of(caller).ok();
        let row = agent
            .as_ref()
            .and_then(|name| agents.iter().find(|row| &row.name == name));
        Status {
            workspace: self.lifecycle.workspace().to_path_buf(),
            host: crate::paths::local_host(),
            pid: std::process::id(),
            started_at: self.lifecycle.started_at().to_string(),
            clients: self.lifecycle.clients(),
            detached: self.agents.any_detached(),
            owner: caller.owner,
            session_id: row.and_then(|row| row.session_id.clone()),
            running: row.is_some_and(|row| row.running != Some(false)),
            agent: caller.agent.clone(),
            agents,
            shell: self.report.shell.clone(),
        }
    }

    /// The consent surface.
    #[must_use]
    pub fn decisions(&self) -> &Arc<super::decisions::Decisions> {
        &self.decisions
    }

    /// Answer the local method `method` for `caller`: its parameters read
    /// as the method's ([`protocol::Method`]), its handler's answer
    /// written as the method's result.
    ///
    /// # Errors
    ///
    /// [`protocol::METHOD_NOT_FOUND`] for an unknown `rkat/*` method,
    /// [`protocol::INVALID_PARAMS`] for bad parameters, and the handler's
    /// refusal.
    pub async fn call(
        &self,
        caller: Caller,
        method: &str,
        params: Value,
    ) -> Result<Value, Refusal> {
        /// One arm per method: `$p` is its parameters, `$body` (which may
        /// await) its `Result<M::Result, Refusal>`.
        macro_rules! dispatch {
            ($($m:ty, $p:pat => $body:expr;)*) => {
                $(
                    if method == <$m as Method>::NAME {
                        let $p: <$m as Method>::Params = protocol::params(params)?;
                        let result: Result<<$m as Method>::Result, Refusal> = $body;
                        return answer(result?);
                    }
                )*
            };
        }
        dispatch! {
            method::Status, Empty {} => Ok(self.status(&caller).await);
            method::Holds, Empty {} => Ok(protocol::Holds {
                held: self.conversations.holding().await,
            });
            method::Detach, Empty {} => {
                let agent = self.agent_of(&caller)?;
                tracing::info!(%agent, "a client detached: the agent keeps running without it");
                self.agents.detach(&agent);
                Ok(Empty {})
            };
            method::Shutdown, protocol::ShutdownParams { retire } => {
                tracing::info!(owner = caller.owner, retire, "rkat/shutdown: shutting down");
                if retire {
                    self.lifecycle.shutdown_retiring();
                } else {
                    self.lifecycle.shutdown();
                }
                Ok(Empty {})
            };
            method::Retire, Empty {} => self.retire(&self.agent_of(&caller)?, None).await;
            method::Quit, Empty {} => self.quit(&caller).await;
            method::Recall, protocol::RecallParams { input_ids } => {
                self.recall(&caller, &input_ids).await
            };
            method::ActiveInputs, Empty {} => self.active_inputs(&caller).await;
            method::Schedules, Empty {} => {
                let agent = self.agent_of(&caller)?;
                Ok(protocol::ScheduleList {
                    schedules: self.agents.schedules().rows(&agent).await.map_err(|err| {
                        Refusal::new(protocol::INTERNAL_ERROR, err.to_string())
                    })?,
                })
            };
            method::Schedule, protocol::ScheduleAction { op, schedule } => {
                let agent = self.agent_of(&caller)?;
                Ok(match self.agents.schedules().act(&agent, op, &schedule).await {
                    Ok(text) => {
                        self.audit(
                            super::audit::Line::event("schedule_changed", text.clone())
                                .of_agent(&agent),
                        );
                        RuleOutcome { ok: true, text }
                    }
                    Err(err) => RuleOutcome {
                        ok: false,
                        text: err.to_string(),
                    },
                })
            };
            method::GetMode, Empty {} => Ok(ModeStatus::of(&self.engine));
            method::Report, Empty {} => Ok(self.report(self.agent_of(&caller).ok().as_deref()));
            method::SetMode, protocol::SetModeParams { mode } => {
                let mode = PermissionMode::parse(&mode).ok_or_else(|| {
                    Refusal::new(
                        protocol::INVALID_PARAMS,
                        "`mode` must be one of default, accept-edits, plan, auto",
                    )
                })?;
                Ok(self.set_mode(mode).await)
            };
            method::Rules, command => {
                // A rule written to the realm file: blocking I/O, off the
                // connection's loop.
                let engine = Arc::clone(&self.engine);
                tokio::task::spawn_blocking(move || run_rule_command(&engine, &command))
                    .await
                    .map_err(|err| Refusal::new(protocol::INTERNAL_ERROR, err.to_string()))
            };
            method::Conversations, Empty {} => {
                let agent = self.agent_of(&caller).ok();
                Ok(ConversationList {
                    conversations: self
                        .conversations
                        .list(agent.as_deref(), &self.agents.live())
                        .await,
                })
            };
            method::New, Empty {} => self.switch(&self.agent_of(&caller)?, Target::Fresh).await;
            method::Resume, protocol::SessionParams { session_id } => {
                self.switch(&self.agent_of(&caller)?, Target::Resume(session_id))
                    .await
            };
            method::Transcript, protocol::SessionParams { session_id } => {
                let messages = self
                    .conversations
                    .messages(&session_id)
                    .await
                    .map_err(|err| Refusal::new(SWITCH_FAILED, err.to_string()))?;
                Ok(protocol::Transcript { messages })
            };
            method::Endpoint, protocol::SessionParams { session_id } => {
                Ok(protocol::SessionEndpoint {
                    endpoint: self.conversations.endpoint_of(&session_id).await,
                })
            };
            method::Usage, protocol::SessionParams { session_id } => {
                let usage = self
                    .conversations
                    .usage(&session_id)
                    .await
                    .map_err(|err| Refusal::new(SWITCH_FAILED, err.to_string()))?;
                Ok(Usage {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                })
            };
            method::Tasks, protocol::SessionParams { session_id } => {
                let tasks = super::tasks::read_tasks(&self.tasks_db, &session_id)
                    .await
                    .map_err(|err| Refusal::new(protocol::INTERNAL_ERROR, err.to_string()))?;
                Ok(Tasks { tasks })
            };
            method::Notices, Empty {} => Ok(Notices {
                notices: self.startup_notices.clone(),
            });
            method::Audit, protocol::AuditParams { lines } => {
                let agent = self.agent_of(&caller)?;
                self.audit_tail(&agent, lines.unwrap_or(AUDIT_LINES))
            };
            method::Missed, Empty {} => {
                let agent = self.agent_of(&caller)?;
                Ok(Notices {
                    notices: self
                        .agents
                        .take_missed(&agent)
                        .into_iter()
                        .map(StartupNotice::warn)
                        .collect(),
                })
            };
            method::SetModel, params => self.set_model(&self.agent_of(&caller)?, params).await;
            method::ChecklistView, protocol::ChecklistViewParams { view } => {
                if let Some(file) = self.realm_file.clone() {
                    tokio::task::spawn_blocking(move || file.write_checklist(view))
                        .await
                        .map_err(|err| Refusal::new(protocol::INTERNAL_ERROR, err.to_string()))?
                        .map_err(|err| Refusal::new(protocol::INTERNAL_ERROR, err.to_string()))?;
                }
                Ok(Empty {})
            };
            method::Trust, Empty {} => Ok(self.trust_project());
        }
        Err(Refusal::new(
            METHOD_NOT_FOUND,
            format!("unknown method `{method}`"),
        ))
    }

    /// `rkat/report`: the daemon's side of `/status` (`agent`: the caller's
    /// agent, whose watches it lists).
    fn report(&self, agent: Option<&str>) -> Report {
        let ReportParts {
            realm,
            realm_id,
            confinement,
            helper_depth,
            ..
        } = self.report.clone();
        let realm_rules = realm.join(crate::config::RULES_FILE_NAME);
        let project_inputs = self
            .project
            .files()
            .map(|file| {
                let mut trust = trust_words(&self.project.status(file));
                if self.project.by_flag() && trust.starts_with("not trusted") {
                    trust.push_str(" (trusted for this run by --trust-project)");
                }
                TrustedFile {
                    path: file.to_path_buf(),
                    trust,
                }
            })
            .collect();
        Report {
            realm,
            realm_id,
            confinement,
            mode: ModeStatus::of(&self.engine),
            realm_rules,
            project_inputs,
            helper_depth,
            watches: agent.map_or_else(Vec::new, |agent| self.agents.watches().describe(agent)),
        }
    }

    /// `/new` or `/resume`: switch `agent`'s conversation, forget the
    /// gate's state of the one left (its classifier transcript, allow
    /// cache and helpers; a later `/resume` seeds the transcript again),
    /// then tell the new one about plan mode when it is on, and read its
    /// tools. Another agent's conversation is refused with where it is.
    async fn switch(&self, agent: &str, target: Target) -> Result<Switched, Refusal> {
        use super::conversations::SwitchError;
        let resumed = matches!(target, Target::Resume(_));
        let previous = self.conversations.current(agent);
        let session = self.agents.switch(agent, target).await.map_err(|err| {
            let code = match err {
                SwitchError::Running => SWITCH_BUSY,
                _ => SWITCH_FAILED,
            };
            let message = match &err {
                SwitchError::OtherAgent { agent: owner, .. } if self.agents.is_live(owner) => {
                    format!("{err}, which works on it now: /agents {owner} switches to it")
                }
                SwitchError::OtherAgent { .. } => {
                    format!("{err}, which has quit: `rkat-tui --resume <id>` seats it again on it")
                }
                _ => err.to_string(),
            };
            Refusal::new(code, message)
        })?;
        if let Some(previous) = previous.filter(|previous| *previous != session) {
            self.engine.forget_session(&previous);
        }
        if resumed {
            self.conversations.seed(&self.engine, &session).await;
        }
        // The new conversation was never told anything.
        self.plan_noted.lock().await.remove(agent);
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

    /// `rkat/retire`: `agent` leaves the workspace. A running turn is
    /// interrupted first (its review stops, its open escalations are
    /// answered with Stop), then the agent retires
    /// ([`Agents::retire`]): its helpers stop, the engine archives its
    /// session, which stays in the conversation list and can be seated
    /// again.
    async fn retire(
        &self,
        agent: &str,
        seated: Option<&super::agents::Seated<'_>>,
    ) -> Result<Retired, Refusal> {
        let session = self.conversations.current(agent);
        if let Some(session) = &session {
            self.engine.stop(session);
            self.desk.stop(session);
        }
        // Nothing wakes a retired agent: its schedules go first, so none
        // fires between the retire and their deletion.
        let schedules_deleted = self.agents.schedules().delete_all(agent).await;
        let retired = match seated {
            Some(seated) => self.agents.retire_seated(agent, seated).await,
            None => self.agents.retire(agent).await,
        }
        .map_err(|err| Refusal::new(SWITCH_FAILED, err.to_string()))?;
        if !schedules_deleted.is_empty() {
            self.audit(
                super::audit::Line::event("schedules_deleted", schedules_deleted.join(", "))
                    .of_agent(agent),
            );
        }
        self.plan_noted.lock().await.remove(agent);
        self.audit(
            super::audit::Line::event(
                "agent_retired",
                session
                    .as_ref()
                    .map_or_else(String::new, |s| format!("session {s}")),
            )
            .of_agent(agent),
        );
        Ok(Retired {
            agent: agent.to_string(),
            session_id: retired.or(session),
            schedules_deleted,
        })
    }

    /// `rkat/quit` for `caller` ([`super::lifecycle::quit_decision`]),
    /// decided and done under the seating lock: no client binds, and no
    /// other `/quit` is decided, between the two. A client that leaves
    /// stops counting at once ([`Lifecycle::leave`]), so a second `/quit`
    /// decided right after sees it gone.
    async fn quit(&self, caller: &Caller) -> Result<protocol::QuitOutcome, Refusal> {
        use super::lifecycle::QuitPlan;
        use protocol::QuitOutcome;
        let Some(agent) = caller.agent.clone() else {
            return Err(Refusal::new(
                protocol::NO_AGENT,
                "this connection works with no agent: there is nothing to quit",
            ));
        };
        let seated = self.agents.seating().await;
        let status = self.status(caller).await;
        Ok(match super::lifecycle::quit_decision(&status) {
            QuitPlan::Leave => {
                let others = status
                    .agents
                    .iter()
                    .find(|row| row.name == agent)
                    .map_or(status.clients, |row| row.clients)
                    .saturating_sub(1);
                self.lifecycle.leave(caller);
                self.agents.left(&agent);
                tracing::info!(%agent, others, "a client quit: the agent stays for its other clients");
                QuitOutcome::Left { others }
            }
            QuitPlan::Retire => {
                let retired = self.retire(&agent, Some(&seated)).await?;
                self.lifecycle.leave(caller);
                QuitOutcome::Retired { retired }
            }
            QuitPlan::Terminate => {
                self.lifecycle.leave(caller);
                tracing::info!(%agent, "the owner quit as the last client: shutting down");
                self.lifecycle.shutdown_retiring();
                QuitOutcome::Terminated
            }
            QuitPlan::Ask { reason, running } => QuitOutcome::Ask {
                reason: with_schedules(reason, &status),
                running,
            },
        })
    }

    /// Retire every agent ([`Self::retire`]; the daemon is shutting down
    /// with `rkat/shutdown {"retire": true}`). A refusal is logged.
    pub async fn retire_all(&self) {
        for agent in self.agents.names() {
            if let Err(refusal) = self.retire(&agent, None).await {
                tracing::warn!(%agent, error = %refusal.message, "the agent did not retire");
            }
        }
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
                self.audit(super::audit::Line::event(
                    "project_trusted",
                    recorded
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                ));
                let files = recorded
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                RuleOutcome {
                    ok: true,
                    text: format!(
                        "trusted {files} as they are now; their hooks run from the agent's next session (/new, /resume or a daemon restart), their MCP servers from the daemon's next start"
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
    /// in place: the engine's LLM reconfigure on the live session,
    /// with the endpoint's auth binding and the `[thinking]` setting the
    /// new model is configured with. Refused while a turn runs;
    /// the session keeps the model, and the next prompt is an
    /// ordinary `mob/member_send`, tied to no connection.
    /// `rkat/recall` for `caller`: only its own agent's current
    /// conversation is searched, so another agent's input is never
    /// touched. A queued input is abandoned (`cancel_input_if_present`);
    /// any other state means the agent has it, and nothing is cancelled
    /// (that call would interrupt the input's run). A steer staged between
    /// the read and the cancel is the remaining window (an engine ask: an
    /// atomic "abandon if still queued").
    ///
    /// # Errors
    ///
    /// [`protocol::NO_AGENT`] when the connection is bound to no agent or
    /// its agent has no conversation.
    async fn recall(
        &self,
        caller: &Caller,
        inputs: &[meerkat_core::lifecycle::InputId],
    ) -> Result<protocol::Recalled, Refusal> {
        use meerkat_runtime::SessionServiceRuntimeExt as _;
        use meerkat_runtime::input_state::InputLifecycleState;

        let session = caller
            .agent
            .as_deref()
            .and_then(|agent| self.conversations.current(agent))
            .ok_or_else(|| {
                Refusal::new(
                    protocol::NO_AGENT,
                    "rkat/recall takes back only the messages of the agent this connection works with",
                )
            })?;
        let mut answer = protocol::Recalled::default();
        for input in inputs {
            match self.machine.input_state(&session, input).await {
                Ok(Some(stored)) if stored.seed.phase == InputLifecycleState::Queued => {
                    match self
                        .machine
                        .cancel_input_if_present(&session, input, RECALL_REASON)
                        .await
                    {
                        Ok(true) => answer.recalled.push(input.clone()),
                        Ok(false) => answer.reached.push(input.clone()),
                        Err(err) => answer.errors.push(format!("input {input}: {err}")),
                    }
                }
                Ok(Some(_)) => answer.reached.push(input.clone()),
                Ok(None) => answer.foreign.push(input.clone()),
                Err(err) => answer.errors.push(format!("input {input}: {err}")),
            }
        }
        if !answer.recalled.is_empty() {
            tracing::info!(recalled = answer.recalled.len(), %session, "rkat/recall");
        }
        Ok(answer)
    }

    /// Write the conversation log as it is now (the daemon's stop).
    pub fn flush_conversations(&self) {
        self.conversations.flush();
    }

    /// [`protocol::method::ActiveInputs`] for the caller's agent.
    ///
    /// # Errors
    ///
    /// [`protocol::NO_AGENT`] when the connection is bound to no agent or
    /// its agent has no conversation; [`protocol::INTERNAL_ERROR`] when the
    /// runtime cannot list them.
    async fn active_inputs(&self, caller: &Caller) -> Result<protocol::UnfinishedInputs, Refusal> {
        use meerkat_runtime::SessionServiceRuntimeExt as _;
        let refused = || {
            Refusal::new(
                protocol::NO_AGENT,
                "rkat/active_inputs answers for the agent this connection works with",
            )
        };
        let agent = caller.agent.as_deref().ok_or_else(refused)?;
        let session = self.conversations.current(agent).ok_or_else(refused)?;
        let active = self
            .machine
            .list_active_inputs(&session)
            .await
            .map_err(|err| Refusal::new(protocol::INTERNAL_ERROR, err.to_string()))?;
        // A run still closing is unfinished too (or a state that cannot be
        // read): an append made before it closed lands only after its turn.
        let running =
            self.conversations.running(agent).await != super::conversations::RunState::Idle;
        Ok(protocol::UnfinishedInputs {
            count: active.len() + usize::from(running),
        })
    }

    async fn set_model(&self, agent: &str, params: SetModelParams) -> Result<ModelSet, Refusal> {
        use meerkat_runtime::SessionServiceRuntimeExt as _;

        let SetModelParams {
            model,
            provider,
            auth_binding,
            listed,
        } = params;
        // The configured endpoint the switch's binding names (`None`: a
        // built-in endpoint).
        let endpoint = auth_binding
            .as_ref()
            .filter(|binding| is_endpoint_realm(binding.realm.as_str()))
            .and_then(|binding| self.endpoints.get(binding.binding.as_str()))
            .cloned();
        // A model only an endpoint knows needs its profile (on a
        // self-hosted endpoint: its alias) in the realm config before the
        // runtime accepts it.
        let mut reseated = false;
        if let Some(dialect) = provider.as_deref().and_then(WireDialect::parse) {
            let listed = listed.map(|listed| crate::models::ModelEntry {
                id: model.clone(),
                display_name: None,
                source: crate::models::ModelSource::Provider,
                context_window: listed.context_window,
                max_output_tokens: listed.max_output_tokens,
                capabilities: crate::models::ModelCapabilities {
                    reasoning: listed.reasoning,
                    vision: listed.vision,
                    function_calling: None,
                },
            });
            let owned = crate::endpoints::OwnedProfiles::in_realm(&self.report.realm);
            // A profile rkat-tui wrote for a model the catalog has since
            // added goes first (the engine refuses the document with it).
            let pruned =
                crate::endpoints::prune_catalogued_profiles(self.realm_config.runtime(), &owned)
                    .await
                    .map_err(|err| Refusal::new(SWITCH_FAILED, err.to_string()))?;
            if !pruned.is_empty() {
                tracing::info!(?pruned, "dropped profiles the catalog now defines");
            }
            let written = match &endpoint {
                Some(endpoint) => {
                    crate::endpoints::ensure_endpoint_model(
                        self.realm_config.runtime(),
                        &owned,
                        endpoint,
                        &model,
                        listed.as_ref(),
                    )
                    .await
                }
                None => {
                    crate::endpoints::ensure_model_profile(
                        self.realm_config.runtime(),
                        &owned,
                        dialect.into(),
                        &model,
                        listed.as_ref(),
                    )
                    .await
                }
            }
            .map_err(|err| Refusal::new(SWITCH_FAILED, err.to_string()))?;
            // The live agent's model registry predates the profile: seat
            // the orchestrator again on its session, which rebuilds its
            // agent.
            if written {
                reseated = true;
                self.agents.reseat(agent).await.map_err(|err| {
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
                .filter(|binding| is_endpoint_realm(binding.realm.as_str()))
                .map_or(provider, |binding| binding.binding.as_str());
            let configured = self
                .thinking
                .resolve(Some(endpoint), dialect.into(), &model);
            Some(match thinking::params_for(configured.as_ref()) {
                Some(params) => TurnMetadataOverride::Set(params),
                None => TurnMetadataOverride::Clear,
            })
        });
        // On a self-hosted endpoint the engine runs the model's alias.
        let (engine_model, engine_provider, self_hosted_server_id) = match &endpoint {
            Some(endpoint) if endpoint.is_self_hosted() => (
                endpoint.engine_model(&model),
                Some(endpoint.engine_provider().as_str().to_string()),
                endpoint.self_hosted_server(),
            ),
            _ => (model, provider, None),
        };
        let request = meerkat_runtime::SessionLlmReconfigureRequest {
            model: Some(engine_model),
            provider: engine_provider,
            self_hosted_server_id,
            provider_params,
            auth_binding: Some(match auth_binding {
                Some(binding) => TurnMetadataOverride::Set(binding),
                None => TurnMetadataOverride::Clear,
            }),
        };
        let machine = Arc::clone(&self.machine);
        let switched = self
            .conversations
            .while_idle(agent, |session| async move {
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
        let (model, provider) = crate::endpoints::from_engine(
            &report.new_identity.model,
            report.new_identity.provider.as_str(),
        );
        Ok(ModelSet {
            session_id: session,
            model,
            provider,
            reseated,
        })
    }

    async fn set_mode(&self, mode: PermissionMode) -> ModeSet {
        self.engine.set_mode(mode);
        let mut warnings = Vec::new();
        let engine = Arc::clone(&self.engine);
        let persisted = tokio::task::spawn_blocking(move || engine.persist_mode())
            .await
            .map_err(|err| err.to_string())
            .and_then(|written| written.map_err(|err| err.to_string()));
        if let Err(err) = persisted {
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

    /// At start: when the persisted mode is plan, tell every agent (a
    /// fresh one never heard it; a restored one may have been switched to
    /// plan mode by another process). Returns a warning when one could not
    /// be told.
    pub async fn announce_plan_mode(&self) -> Option<String> {
        let mut noted = self.plan_noted.lock().await;
        if self.engine.mode() != PermissionMode::Plan {
            return None;
        }
        let mut warnings = Vec::new();
        for agent in self.agents.names() {
            match self.append_plan_note(&agent, true).await {
                None => {
                    noted.insert(agent, true);
                }
                Some(warning) => warnings.push(warning),
            }
        }
        (!warnings.is_empty()).then(|| warnings.join("\n"))
    }

    /// Tell every live agent plan mode started or ended, once per change
    /// (an agent never told is told only when plan mode is on). The mode is
    /// read under the lock, not taken from the caller: of two overlapping
    /// changes, the later note always matches the mode that holds, and the
    /// notes alternate.
    async fn note_plan_mode(&self) -> Option<String> {
        let mut noted = self.plan_noted.lock().await;
        let plan = self.engine.mode() == PermissionMode::Plan;
        let live = self.agents.names();
        noted.retain(|agent, _| live.contains(agent));
        let mut warnings = Vec::new();
        for agent in live {
            if noted.get(&agent).copied().unwrap_or(false) == plan {
                continue;
            }
            match self.append_plan_note(&agent, plan).await {
                None => {
                    noted.insert(agent, plan);
                }
                Some(warning) => warnings.push(warning),
            }
        }
        (!warnings.is_empty()).then(|| warnings.join("\n"))
    }

    async fn append_plan_note(&self, agent: &str, plan: bool) -> Option<String> {
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
            .mob_append_system_context(&MobId::from(MOB_ID), &AgentIdentity::from(agent), request)
            .await
        {
            Ok(_) => None,
            Err(err) => Some(format!(
                "agent {agent} could not be told about plan mode now (the tool errors say it): {err}"
            )),
        }
    }

    /// A connection was just bound to `agent`: when plan mode is on and the
    /// agent was never told (one seated for it), tell it.
    pub async fn bound(&self, agent: &str) {
        if self.engine.mode() == PermissionMode::Plan
            && let Some(warning) = self.note_plan_mode().await
        {
            tracing::warn!(%agent, %warning, "plan mode");
        }
    }

    /// Learn from `caller`'s engine request on its way to the router (see
    /// the module docs); returns the request as it is forwarded.
    ///
    /// # Errors
    ///
    /// [`protocol::NOT_YOURS`] for a request [`Self::admit`] refuses: it is
    /// answered to the client and never forwarded.
    pub fn observe(&self, caller: &Caller, mut request: Value) -> Result<Value, Refusal> {
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        self.admit(caller, &method, request.get("params"))?;
        let Some(params) = request.get_mut("params") else {
            return Ok(request);
        };
        let agent = match (
            params.get("mob_id").and_then(Value::as_str),
            params.get("agent_identity").and_then(Value::as_str),
        ) {
            (Some(MOB_ID), Some(agent)) if self.agents.is_live(agent) => agent.to_string(),
            _ => return Ok(request),
        };
        match method.as_str() {
            engine::MEMBER_SEND | engine::TURN_START => {
                let field = if method == engine::MEMBER_SEND {
                    "content"
                } else {
                    "prompt"
                };
                self.agents.touch(&agent);
                // The user's input: the run that takes it is attended, and
                // its conversation is the one used last (`-c` after a
                // restart continues it).
                if let Some(session) = self.conversations.current(&agent) {
                    self.engine.human_input(&session);
                    self.conversations.touch(&session, &agent);
                }
                if let Some(text) = params.get(field).and_then(content_text)
                    && !text.trim().is_empty()
                    && let Some(session) = self.conversations.current(&agent)
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
                if let Some(session) = self.conversations.current(&agent) {
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
        Ok(request)
    }

    /// Whether `caller` may send the engine request `method` with
    /// `params`. A connection drives the agent it is bound to: a member
    /// action ([`engine::MEMBER_ACTIONS`]) on another live agent, or on a
    /// helper that works for another live agent's session, is refused, and
    /// so is a compaction (`mob/compact_member`, `session/compact`) of
    /// anything but the caller's own agent and its current session (a
    /// compaction is a model call the user pays for). Reads pass: every
    /// client follows every mob.
    ///
    /// # Errors
    ///
    /// [`protocol::NOT_YOURS`], naming what was refused.
    fn admit(&self, caller: &Caller, method: &str, params: Option<&Value>) -> Result<(), Refusal> {
        let own = caller.agent.as_deref();
        let field = |name: &str| params.and_then(|params| params.get(name)?.as_str());
        let refuse = |what: String| {
            Refusal::new(
                protocol::NOT_YOURS,
                format!(
                    "{method} on {what} is refused: this connection works with {}",
                    own.map_or_else(|| "no agent".to_string(), |own| format!("agent {own}"))
                ),
            )
        };
        if method == engine::SESSION_COMPACT {
            let session = field("session_id").unwrap_or("");
            let current = own.and_then(|own| self.conversations.current(own));
            return if current.is_some_and(|current| current.to_string() == session) {
                Ok(())
            } else {
                Err(refuse(format!(
                    "session {session}, which is not its agent's current conversation"
                )))
            };
        }
        if !engine::MEMBER_ACTIONS.contains(&method) {
            return Ok(());
        }
        let (Some(mob), Some(member)) = (field("mob_id"), field("agent_identity")) else {
            return Ok(());
        };
        if mob == MOB_ID {
            let foreign = Some(member) != own
                && (self.agents.is_live(member) || method == engine::COMPACT_MEMBER);
            return if foreign {
                Err(refuse(format!("member {member}")))
            } else {
                Ok(())
            };
        }
        if method == engine::COMPACT_MEMBER {
            return Err(refuse(format!("helper {member} of mob {mob}")));
        }
        let key = crate::mobs::HelperKey::new(MobId::from(mob), AgentIdentity::from(member));
        let Some(owner) = self.registry.owner_of(&key) else {
            return Ok(());
        };
        match self
            .agents
            .live()
            .into_iter()
            .find(|(_, session)| *session == owner)
        {
            Some((agent, _)) if Some(agent.as_str()) != own => Err(refuse(format!(
                "helper {member} of mob {mob}, which works for agent {agent}"
            ))),
            _ => Ok(()),
        }
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
            .filter(|binding| {
                binding
                    .get("realm")
                    .and_then(Value::as_str)
                    .is_some_and(is_endpoint_realm)
            })
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
    fn quitting_names_the_agents_schedules() {
        let status = |schedules| Status {
            workspace: "/w".into(),
            host: None,
            pid: 1,
            started_at: String::new(),
            clients: 1,
            detached: false,
            owner: true,
            session_id: None,
            running: false,
            agent: Some("main-ab12".into()),
            agents: vec![protocol::AgentRow {
                name: "main-ab12".into(),
                session_id: None,
                running: Some(false),
                clients: 1,
                detached: false,
                schedules,
            }],
            shell: meerkat_tools::builtin::shell::ShellConfinement::TrustedHost,
        };
        assert_eq!(with_schedules("why".into(), &status(0)), "why");
        assert_eq!(
            with_schedules("why".into(), &status(2)),
            "why\nThis agent has 2 schedules: detaching keeps them firing; terminating deletes them."
        );
    }

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
