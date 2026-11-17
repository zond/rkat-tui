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
//! prompt or steer for the agent (`mob/member_send`) is the human's words, recorded in the classifier's
//! transcript (the connection proved it holds the daemon's token), and the
//! run that takes it is one a user started (attended, see
//! [`crate::approval::attendance`]); an
//! interrupt
//! (`mob/stop_member_run`, `mob/force_cancel`) stops the review of the
//! agent's own calls and answers the open escalations of its own calls with
//! Stop; its helpers keep running, and the endings of its helpers are held
//! until the user's next message to it ([`super::agents::endings`]). A
//! helper is stopped on its own ([`method::StopHelper`]).
//!
//! [`Local::call`] is the dispatch table, one line per method; the
//! handlers live in files by concern: an agent's conversations and its
//! leaving ([`lifecycle`]), the inputs a client sent ([`inputs`]), the
//! user's interrupt, bound to one run ([`interrupt`]), the
//! model and the permission mode ([`model`]), the wires to other
//! workspaces ([`wires`]), and the reads behind `/status`, `/context`,
//! `/audit` and `/trust` ([`report`]).

/// The longest a prompt waits for a mode change the same client sent
/// before it ([`Local::observe`]).
const MODE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
mod inputs;
mod interrupt;
mod lifecycle;
mod model;
mod report;
mod settings;
mod wires;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use meerkat_core::lifecycle::run_primitive::TurnMetadataOverride;
use meerkat_mob::{AgentIdentity, MobId};
use serde_json::Value;

use super::MOB_ID;
use super::agents::Agents;
use super::approvals::Desk;
use super::conversations::Target;
use crate::approval::InputKey;
use crate::approval::desk::{ModeStatus, RuleOutcome, mode_notice, run_rule_command};
use crate::approval::mode::PermissionMode;
use crate::bootstrap::project_trust::ProjectInputs;
use crate::endpoints::{WireDialect, is_endpoint_realm};
use crate::mobs::HelperRegistry;
use crate::thinking::{self, ThinkingTable};

use super::core::Core;
use super::lifecycle::{Caller, Lifecycle};
use super::protocol::{
    self, AUDIT_LINES, AuditTail, ConversationList, Empty, METHOD_NOT_FOUND, Method, ModeSet,
    ModelSet, Notices, Refusal, Report, Retired, SWITCH_BUSY, SetModelParams, StartupNotice,
    Status, Switched, Tasks, TrustedFile, Usage, engine, method,
};
use super::serve::Observed;

/// The source of the daemon's plan-mode and grant notes.
pub(crate) const SYSTEM_CONTEXT_SOURCE: &str = "rkat-tui";

/// What the local methods and the observer act on.
pub struct Local {
    /// What the local methods share with the agents.
    core: Arc<Core>,
    /// The live agents.
    agents: Arc<Agents>,
    /// What only the local methods read.
    parts: LocalParts,
    /// Which agents were told plan mode is on (an agent missing was told
    /// nothing: it is told when plan mode is on). A daemon started in plan
    /// mode tells every agent ([`Local::announce_plan_mode`]), so leaving
    /// plan mode after a restart says so. An async lock held across the
    /// notes' appends, so two overlapping mode changes (on any connections)
    /// cannot interleave their check and their note.
    plan_noted: tokio::sync::Mutex<std::collections::HashMap<String, bool>>,
    inputs: AtomicU64,
    /// The calls that may switch the mode ([`may_switch_mode`]) routed so
    /// far ([`Local::routing_local`]), counted when the proxy reads them,
    /// before their task runs.
    modes_routed: AtomicU64,
    /// Those calls that ended (their note in the agents' context): a
    /// prompt to an idle agent waits until every one routed before it
    /// ended ([`Local::observe`]).
    modes_done: tokio::sync::watch::Sender<u64>,
    /// Tests: how many times each engine method was forwarded.
    #[cfg(any(test, feature = "test-support"))]
    forwarded: parking_lot::Mutex<std::collections::HashMap<String, usize>>,
    /// Tests: a mode change waits on it before its note.
    #[cfg(any(test, feature = "test-support"))]
    mode_hold: Arc<tokio::sync::RwLock<()>>,
    /// Tests: every `rkat/interrupt`'s answer, in order.
    #[cfg(any(test, feature = "test-support"))]
    interrupts: parking_lot::Mutex<Vec<protocol::Interrupted>>,
    /// Tests: how many prompts waited for a mode change.
    #[cfg(any(test, feature = "test-support"))]
    mode_waits: Arc<AtomicU64>,
}

/// What only [`Local`] reads, beside the [`Core`] it shares.
pub struct LocalParts {
    /// The daemon's approval desk.
    pub desk: Arc<Desk>,
    /// The `[thinking]` settings a model switch carries.
    pub thinking: ThinkingTable,
    /// The workspace's project inputs and their trust (`rkat/trust`).
    pub project: ProjectInputs,
    /// The engine's task store (`rkat/tasks`).
    pub tasks_db: std::path::PathBuf,
    /// The runtime the agents' sessions live in (`rkat/set_model`).
    pub machine: Arc<meerkat_runtime::MeerkatMachine>,
    /// The realm config (a switched-to model's profile is written there).
    pub realm_config: Arc<crate::bootstrap::RealmConfigStore>,
    /// The workspace (`/context` names its `.rkat/skills`).
    pub workspace: std::path::PathBuf,
    /// The skills a session build lists (`/context`'s inventory line).
    pub skills: Arc<crate::bootstrap::skills::SkillCatalog>,
    /// The daemon's endpoints (a switch names one by its binding).
    pub endpoints: crate::endpoints::EndpointRegistry,
    /// What the start-up had to say (`rkat/notices`).
    pub startup_notices: Vec<StartupNotice>,
    /// The fixed part of `rkat/report`.
    pub report: ReportParts,
}

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
}

impl std::fmt::Debug for Local {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Local")
            .field("conversations", &self.core.conversations)
            .finish_non_exhaustive()
    }
}

/// Whether the local call `method` with `params` may switch the
/// permission mode: `rkat/set_mode`, or `rkat/set_setting` of a setting
/// whose hook is the mode's (`/set auto.mode …`). The proxy counts such a
/// call when it routes it and the call counts itself ended, by this one
/// test, so both always agree.
fn may_switch_mode(method: &str, params: &Value) -> bool {
    method == <method::SetMode as Method>::NAME
        || (method == <method::SetSetting as Method>::NAME
            && params
                .get("key")
                .and_then(Value::as_str)
                .and_then(|key| crate::config::registry::Setting::find(key.trim()))
                .is_some_and(|setting| setting.hook == crate::config::registry::Hook::Mode))
}

impl Local {
    /// The local surface over `agents` (and the core they share) and
    /// `parts`.
    #[must_use]
    pub fn new(agents: Arc<Agents>, parts: LocalParts) -> Self {
        // In plan mode every restored agent heard it (its conversation
        // has the note): leaving plan mode tells it.
        let core = Arc::clone(agents.core());
        let in_plan = core.engine.mode() == PermissionMode::Plan;
        let noted = agents
            .names()
            .into_iter()
            .map(|name| (name, in_plan))
            .collect();
        Self {
            core,
            agents,
            parts,
            plan_noted: tokio::sync::Mutex::new(noted),
            inputs: AtomicU64::new(0),
            modes_routed: AtomicU64::new(0),
            modes_done: tokio::sync::watch::Sender::new(0),
            #[cfg(any(test, feature = "test-support"))]
            forwarded: parking_lot::Mutex::default(),
            #[cfg(any(test, feature = "test-support"))]
            mode_hold: Arc::default(),
            #[cfg(any(test, feature = "test-support"))]
            interrupts: parking_lot::Mutex::default(),
            #[cfg(any(test, feature = "test-support"))]
            mode_waits: Arc::default(),
        }
    }

    /// Tests: hold every mode change before its note until the guard is
    /// dropped.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn hold_mode_notes(&self) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        Arc::clone(&self.mode_hold).write_owned().await
    }

    /// Tests: how many prompts waited for a mode change to end.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn mode_waits(&self) -> u64 {
        self.mode_waits.load(Ordering::SeqCst)
    }

    /// The proxy read `caller`'s local call `method` and is about to run
    /// it on its own task: a call that may switch the mode is counted now,
    /// so a prompt the same client sends right after it waits for it
    /// ([`Self::observe`]).
    pub fn routing_local(&self, method: &str, params: &Value) {
        if may_switch_mode(method, params) {
            self.modes_routed.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// What a prompt to an idle agent waits for: every mode change routed
    /// before it to end, so the change's note is in the agent's context
    /// before the run it starts (bounded: a change held up longer lets the
    /// prompt go).
    fn after_mode_changes(&self) -> Option<super::serve::Wait> {
        let target = self.modes_routed.load(Ordering::SeqCst);
        if *self.modes_done.borrow() >= target {
            return None;
        }
        let mut done = self.modes_done.subscribe();
        #[cfg(any(test, feature = "test-support"))]
        self.mode_waits.fetch_add(1, Ordering::SeqCst);
        Some(super::serve::Wait(Box::pin(async move {
            if tokio::time::timeout(MODE_WAIT, done.wait_for(|ended| *ended >= target))
                .await
                .is_err()
            {
                tracing::warn!(
                    "a prompt waited {MODE_WAIT:?} for a mode change to end; sent anyway"
                );
            }
        })))
    }

    /// What the local methods share with the agents.
    #[must_use]
    pub fn core(&self) -> &Arc<Core> {
        &self.core
    }

    /// How many requests of the engine method `method` the clients sent
    /// through the proxy (tests: what a client polls).
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn forwarded(&self, method: &str) -> usize {
        self.forwarded.lock().get(method).copied().unwrap_or(0)
    }

    /// Append `line` to the audit log (nothing when there is none).
    pub fn audit(&self, line: super::audit::Line) {
        if let Some(log) = &self.core.audit {
            log.write(&line);
        }
    }

    /// The daemon's lifecycle state.
    #[must_use]
    pub fn lifecycle(&self) -> &Arc<Lifecycle> {
        &self.core.lifecycle
    }

    /// The helpers the agents started.
    #[must_use]
    pub fn registry(&self) -> &Arc<HelperRegistry> {
        &self.core.registry
    }

    /// The agents' hidden tools and their reveals.
    #[must_use]
    pub fn deferral(&self) -> &Arc<super::deferral::Deferral> {
        &self.core.deferral
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
            workspace: self.core.lifecycle.workspace().to_path_buf(),
            host: crate::paths::local_host(),
            pid: std::process::id(),
            started_at: self.core.lifecycle.started_at().to_string(),
            clients: self.core.lifecycle.clients(),
            detached: self.agents.any_detached(),
            owner: caller.owner,
            session_id: row.and_then(|row| row.session_id.clone()),
            running: row.is_some_and(|row| row.running != Some(false)),
            agent: caller.agent.clone(),
            agents,
        }
    }

    /// The consent surface.
    #[must_use]
    pub fn decisions(&self) -> &Arc<super::decisions::Decisions> {
        &self.core.decisions
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
        /// Counts a call that may switch the mode as ended however it ends
        /// (answered, refused, or dropped unfinished), so no prompt waits
        /// for one that never will.
        struct Ended<'a>(Option<&'a tokio::sync::watch::Sender<u64>>);
        impl Drop for Ended<'_> {
            fn drop(&mut self) {
                if let Some(done) = self.0 {
                    done.send_modify(|ended| *ended += 1);
                }
            }
        }
        let _ended = Ended(may_switch_mode(method, &params).then_some(&self.modes_done));
        self.dispatch(caller, method, params).await
    }

    async fn dispatch(
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
            method::Holds, Empty {} => Ok(self.holds().await);
            method::Detach, Empty {} => self.detach(&caller);
            method::Shutdown, protocol::ShutdownParams { retire } => Ok(self.shutdown(&caller, retire));
            method::Quit, Empty {} => self.quit(&caller).await;
            method::RetireAgent, protocol::AgentParams { name } => self.retire_other(&caller, &name).await;
            method::StopHelper, protocol::StopHelperParams { path } => self.stop_helper(&caller, &path);
            method::Recall, protocol::RecallParams { input_ids } => self.recall(&caller, &input_ids).await;
            method::Inputs, protocol::RecallParams { input_ids } => self.input_fates(&caller, &input_ids).await;
            method::Interrupt, params => self.interrupt(&caller, params).await;
            method::Schedules, Empty {} => self.schedules(&caller).await;
            method::Schedule, protocol::ScheduleAction { op, schedule } => self.schedule(&caller, op, &schedule).await;
            method::Report, Empty {} => Ok(self.report(self.agent_of(&caller).ok().as_deref()));
            method::SetMode, protocol::SetModeParams { mode, user } => Ok(self.set_mode(mode, user).await);
            method::Rules, command => self.rules(command).await;
            method::RuleRows, Empty {} => Ok(self.rule_and_grant_rows(&caller));
            method::Blocked, Empty {} => self.blocked(&caller);
            method::Approve, protocol::ApproveParams { id, scope, effect } => self.approve(&caller, id, scope, effect.as_deref());
            method::RemoveGrant, protocol::GrantParams { id } => self.remove_grant(&caller, id);
            method::WithdrawApproval, protocol::GrantParams { id } => self.withdraw_approval(&caller, id);
            method::RemoveStatement, protocol::GrantParams { id } => self.remove_statement(&caller, id);
            method::Conversations, Empty {} => Ok(self.conversation_list(&caller).await);
            method::New, Empty {} => self.switch(&self.agent_of(&caller)?, Target::Fresh).await;
            method::Resume, protocol::SessionParams { session_id } => self.switch(&self.agent_of(&caller)?, Target::Resume(session_id)).await;
            method::Transcript, protocol::SessionParams { session_id } => self.transcript(&session_id).await;
            method::Context, Empty {} => self.context(&caller).await;
            method::Endpoint, protocol::SessionParams { session_id } => Ok(self.endpoint_of(&session_id).await);
            method::Usage, protocol::SessionParams { session_id } => self.usage(session_id).await;
            method::Tasks, protocol::SessionParams { session_id } => self.tasks(&session_id).await;
            method::Notices, Empty {} => Ok(self.notices());
            method::AutoStats, Empty {} => Ok(self.auto_stats(&caller));
            method::CompactStatus, Empty {} => Ok(self.compact_status(&caller));
            method::SetSetting, params => Ok(self.set_setting(params).await);
            method::Settings, Empty {} => Ok(self.settings_rows());
            method::ClassifierCheck, selection => self.classifier_check(&selection).await;
            method::Audit, protocol::AuditParams { lines } => self.audit_tail(&self.agent_of(&caller)?, lines.unwrap_or(AUDIT_LINES));
            method::Missed, Empty {} => self.missed(&caller);
            method::SetModel, params => self.set_model(&self.agent_of(&caller)?, params).await;
            method::Trust, Empty {} => Ok(self.trust_project());
            method::Card, protocol::WireAgentParams { agent } => self.card(&caller, agent).await;
            method::Wire, protocol::WireParams { agent, card } => self.wire(&caller, agent, card).await;
            method::Unwire, protocol::UnwireParams { agent, workspace, peer } => self.unwire(&caller, agent, &workspace, &peer).await;
        }
        Err(Refusal::new(
            METHOD_NOT_FOUND,
            format!("unknown method `{method}`"),
        ))
    }

    /// Write the conversation log as it is now (the daemon's stop).
    pub fn flush_conversations(&self) {
        self.core.conversations.flush();
    }

    /// Learn from `caller`'s engine request on its way to the router (see
    /// the module docs); returns the request as it is forwarded.
    ///
    /// # Errors
    ///
    /// [`protocol::NOT_YOURS`] for a request [`Self::admit`] refuses: it is
    /// answered to the client and never forwarded.
    pub fn observe(&self, caller: &Caller, mut request: Value) -> Result<Observed, Refusal> {
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        self.admit(caller, &method, request.get("params"))?;
        #[cfg(any(test, feature = "test-support"))]
        {
            *self.forwarded.lock().entry(method.clone()).or_insert(0) += 1;
        }
        let Some(params) = request.get_mut("params") else {
            return Ok(Observed::plain(request));
        };
        let agent = match (
            params.get("mob_id").and_then(Value::as_str),
            params.get("agent_identity").and_then(Value::as_str),
        ) {
            (Some(MOB_ID), Some(agent)) if self.agents.is_live(agent) => agent.to_string(),
            _ => return Ok(Observed::plain(request)),
        };
        let mut input = None;
        let mut wait = None;
        match method.as_str() {
            engine::MEMBER_SEND => {
                // A prompt to an idle agent starts a run: a mode change
                // the client sent before it is in the agent's context
                // first. A steer into a running turn does not wait (that
                // turn's note comes after it anyway).
                if !self.core.activity.running(&agent) {
                    wait = self.after_mode_changes();
                }
                self.agents.touch(&agent);
                // The user takes over: a checkpoint under way is cancelled,
                // and its automatic continues count from none again.
                self.agents.user_acted(&agent, true);
                // On its way until the engine answers: the agent is not
                // idle meanwhile.
                input = Some(self.core.activity.sending(&agent));
                // The user's input: the run that takes it is attended, and
                // its conversation is the one used last (`-c` after a
                // restart continues it).
                if let Some(session) = self.core.conversations.current(&agent) {
                    self.core.engine.human_input(&session);
                    self.core.conversations.touch(&session, &agent);
                    if let Some(text) = params.get("content").and_then(content_text)
                        && !text.trim().is_empty()
                    {
                        let n = self.inputs.fetch_add(1, Ordering::Relaxed);
                        let key = InputKey::new(format!("rkat-attach:{}:{n}", std::process::id()));
                        self.core.engine.record_user_input(&session, key, &text);
                    }
                    // The endings of helpers held since the user interrupted
                    // the agent go with this message, before its words (the
                    // classifier's transcript has the user's words only).
                    if let Some(held) = self.agents.take_held(&agent, &session)
                        && let Some(content) = params.get_mut("content")
                    {
                        prepend_note(content, &held);
                    }
                }
            }
            engine::STOP_MEMBER_RUN | engine::FORCE_CANCEL | engine::HARD_CANCEL_MEMBER => {
                tracing::info!(%method, %agent, "the client interrupts the agent");
                // The agent's own run only: its helpers keep running (the
                // owner's decision of 2026-11-15), their calls are not
                // denied by its Stop, their dialogs stay open, and their
                // endings are held for the user's next message.
                self.agents.interrupted(&agent);
                // A checkpoint under way is cancelled: nothing continues.
                self.agents.user_acted(&agent, false);
                // A stop names its run: one that ended stops no newer run.
                let run = (method == engine::STOP_MEMBER_RUN)
                    .then(|| params.get("run_id").cloned())
                    .flatten()
                    .and_then(|run| {
                        serde_json::from_value::<meerkat_core::lifecycle::RunId>(run).ok()
                    });
                if let Some(session) = self.core.conversations.current(&agent)
                    && self.core.engine.interrupted(&session, run.as_ref())
                {
                    self.parts.desk.stop(&session);
                }
            }
            _ => {}
        }
        Ok(Observed {
            request,
            input,
            wait,
        })
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
            let current = own.and_then(|own| self.core.conversations.current(own));
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
            if Some(member) == own {
                return Ok(());
            }
            if self.agents.is_live(member) || method == engine::COMPACT_MEMBER {
                return Err(refuse(format!("member {member}")));
            }
            // A helper seated in the workspace's mob (`assign`,
            // `spawn_member`): judged by whom it works for, as below.
        } else if method == engine::COMPACT_MEMBER {
            return Err(refuse(format!("helper {member} of mob {mob}")));
        }
        let key = crate::mobs::HelperKey::new(MobId::from(mob), AgentIdentity::from(member));
        let Some(owner) = self.core.registry.owner_of(&key) else {
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
}

/// A method's answer as JSON.
fn answer(result: impl serde::Serialize) -> Result<Value, Refusal> {
    serde_json::to_value(result).map_err(Refusal::internal)
}

/// Put `note` (what the daemon held for the agent) before the user's
/// words in the wire content input `content`: a string gets it in front,
/// a block list a text block first.
fn prepend_note(content: &mut Value, note: &str) {
    let lead = format!("{note}\n\n{HELD_NOTE_SEPARATOR}\n\n");
    match content {
        Value::String(text) => *text = format!("{lead}{text}"),
        Value::Array(blocks) => blocks.insert(
            0,
            serde_json::json!({"type": "text", "text": lead.trim_end()}),
        ),
        _ => {}
    }
}

/// What separates held notes from the user's message they travel with.
const HELD_NOTE_SEPARATOR: &str =
    "(rkat-tui noted the above while you were stopped; the user's message follows)";

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
    use serde_json::json;

    #[test]
    fn a_held_note_goes_before_the_user_s_words() {
        let mut text = json!("fix it");
        prepend_note(&mut text, "Your helper `a` finished.");
        assert_eq!(
            text,
            json!(format!(
                "Your helper `a` finished.\n\n{HELD_NOTE_SEPARATOR}\n\nfix it"
            ))
        );
        let mut blocks = json!([{"type": "text", "text": "fix it"}]);
        prepend_note(&mut blocks, "note");
        assert_eq!(
            content_text(&blocks).as_deref(),
            Some(format!("note\n\n{HELD_NOTE_SEPARATOR}\nfix it").as_str())
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
}
