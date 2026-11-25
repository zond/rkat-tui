//! What the daemon reads out for the clients: `/status`'s daemon side
//! (`rkat/report`), `/context`, `/audit`, `/trust`, the conversations,
//! their transcripts, usage and checklist, the schedules, the rules, the
//! review numbers and the classifier check (`/auto stats`, `/auto check`)
//! and the unattended denials kept for the next client.

use super::*;

impl Local {
    /// `rkat/holds`: the conversations the runtime still holds inputs for.
    pub(super) async fn holds(&self) -> protocol::Holds {
        protocol::Holds {
            held: self.core.conversations.holding().await,
        }
    }

    /// `rkat/schedules`: the caller's agent's schedules (records of ones
    /// deleted meanwhile go first).
    pub(super) async fn schedules(
        &self,
        caller: &Caller,
    ) -> Result<protocol::ScheduleList, Refusal> {
        let agent = self.agent_of(caller)?;
        self.core.schedules.reconcile().await;
        Ok(protocol::ScheduleList {
            schedules: self
                .core
                .schedules
                .rows(&agent)
                .await
                .map_err(Refusal::internal)?,
        })
    }

    /// `rkat/schedule`: pause, resume or delete one of the caller's
    /// agent's schedules; a change is audited.
    pub(super) async fn schedule(
        &self,
        caller: &Caller,
        op: crate::daemon::protocol::ScheduleOp,
        schedule: &str,
    ) -> Result<Outcome, Refusal> {
        let agent = self.agent_of(caller)?;
        Ok(match self.core.schedules.act(&agent, op, schedule).await {
            Ok(text) => {
                self.audit(
                    crate::daemon::audit::Line::event("schedule_changed", text.clone())
                        .of_agent(&agent),
                );
                Outcome::ok(text)
            }
            Err(err) => Outcome::failed(err.to_string()),
        })
    }

    /// `rkat/rules`: a rule command; a rule written to the realm file is
    /// blocking I/O, off the connection's loop.
    pub(super) async fn rules(
        &self,
        command: crate::approval::desk::RuleCommand,
    ) -> Result<Outcome, Refusal> {
        let engine = Arc::clone(&self.core.engine);
        let outcome = tokio::task::spawn_blocking(move || run_rule_command(&engine, &command))
            .await
            .map_err(Refusal::internal)?;
        self.after_permission_command();
        Ok(outcome)
    }

    /// `rkat/conversations`: the conversations, the caller's agent's own
    /// marked.
    pub(super) async fn conversation_list(&self, caller: &Caller) -> ConversationList {
        let agent = self.agent_of(caller).ok();
        ConversationList {
            conversations: self
                .core
                .conversations
                .list(agent.as_deref(), &self.agents.live())
                .await,
        }
    }

    /// `rkat/transcript`: a conversation's committed messages.
    pub(super) async fn transcript(
        &self,
        session_id: &meerkat_core::SessionId,
    ) -> Result<protocol::Transcript, Refusal> {
        let messages = self
            .core
            .conversations
            .messages(session_id)
            .await
            .map_err(Refusal::switch_failed)?;
        Ok(protocol::Transcript { messages })
    }

    /// `rkat/last_message`: a conversation's last message that is no
    /// system row or notice (none, or that one).
    pub(super) async fn last_message(
        &self,
        session_id: &meerkat_core::SessionId,
    ) -> Result<protocol::Transcript, Refusal> {
        let messages = self
            .core
            .conversations
            .messages(session_id)
            .await
            .map_err(Refusal::switch_failed)?;
        let last = messages.into_iter().rev().find(|message| {
            !matches!(
                message,
                meerkat_core::Message::System(_) | meerkat_core::Message::SystemNotice(_)
            )
        });
        Ok(protocol::Transcript {
            messages: last.into_iter().collect(),
        })
    }

    /// `rkat/context`: what the caller's agent's conversation holds (its
    /// messages, tools, hidden tools and skills).
    pub(super) async fn context(
        &self,
        caller: &Caller,
    ) -> Result<protocol::ContextReport, Refusal> {
        let agent = self.agent_of(caller)?;
        let session_id =
            self.core.conversations.current(&agent).ok_or_else(|| {
                Refusal::switch_failed(format!("agent {agent} runs no conversation"))
            })?;
        let messages = self
            .core
            .conversations
            .messages(&session_id)
            .await
            .map_err(Refusal::switch_failed)?;
        Ok(protocol::ContextReport {
            messages: crate::daemon::context::MessageCounts::of(&messages),
            tools: self.core.origins.sizes(&session_id),
            hidden: self.core.deferral.hidden(&session_id),
            skills: self.skills().await?,
            instruction_files: self.core.instruction_files.read().clone(),
            session_id,
        })
    }

    /// `rkat/endpoint`: the endpoint a conversation runs on.
    pub(super) async fn endpoint_of(
        &self,
        session_id: &meerkat_core::SessionId,
    ) -> protocol::SessionEndpoint {
        protocol::SessionEndpoint {
            endpoint: self.core.conversations.endpoint_of(session_id).await,
        }
    }

    /// `rkat/usage`: a conversation's token totals, with its cost and last
    /// context as the daemon counted them.
    pub(super) async fn usage(
        &self,
        session_id: meerkat_core::SessionId,
    ) -> Result<Usage, Refusal> {
        let usage = self
            .core
            .conversations
            .usage(&session_id)
            .await
            .map_err(Refusal::switch_failed)?;
        let counted = self
            .core
            .ledger
            .get(&crate::daemon::usage::Account::Session(session_id))
            .await;
        Ok(Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cost: counted.cost,
            context: counted.context,
        })
    }

    /// `rkat/tasks`: a conversation's checklist.
    pub(super) async fn tasks(
        &self,
        session_id: &meerkat_core::SessionId,
    ) -> Result<Tasks, Refusal> {
        let tasks = crate::daemon::tasks::read_tasks(&self.parts.tasks_db, session_id)
            .await
            .map_err(Refusal::internal)?;
        Ok(Tasks { tasks })
    }

    /// `rkat/missed`: what the caller's agent was told or denied while no
    /// client was attached, emptied.
    pub(super) fn missed(&self, caller: &Caller) -> Result<Notices, Refusal> {
        let agent = self.agent_of(caller)?;
        Ok(Notices {
            notices: self.agents.take_missed(&agent),
        })
    }

    /// `/compact status`'s daemon side: the prepare instruction and its
    /// threshold for the caller's agent, and what is beneath the
    /// workspace's instruction.
    pub(super) fn compact_status(&self, caller: &Caller) -> Outcome {
        use crate::config::registry::{Level, PREPARE};
        use crate::config::settings::Sourced;
        let beneath = Sourced::instruction(
            &self
                .core
                .settings
                .resolve_without(&PREPARE, Level::Workspace),
        );
        Outcome::ok(
            self.core
                .prepare
                .status(self.agent_of(caller).ok().as_deref(), &beneath),
        )
    }
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

impl Local {
    /// `rkat/audit`: the last lines of the audit log for `agent` (its
    /// decisions and the workspace-wide changes of authority).
    pub(super) fn audit_tail(&self, agent: &str, lines: usize) -> Result<AuditTail, Refusal> {
        let Some(log) = &self.core.audit else {
            return Err(Refusal::new(
                protocol::INTERNAL_ERROR,
                "this daemon keeps no audit log (its start-up notice says why)",
            ));
        };
        let lines = crate::daemon::audit::tail(log.path(), lines, |line| {
            line.agent.as_deref().is_none_or(|of| of == agent)
        })
        .map_err(Refusal::internal)?;
        Ok(AuditTail {
            path: log.path().to_path_buf(),
            lines,
        })
    }

    /// The skills directories a session build of this workspace lists
    /// skills from and the size of its inventory (`/context`), as the
    /// realm config serves it now.
    ///
    /// # Errors
    ///
    /// [`protocol::SWITCH_FAILED`] when the realm config cannot be read.
    pub(super) async fn skills(&self) -> Result<crate::bootstrap::skills::SkillsReport, Refusal> {
        use crate::bootstrap::skills::{Inventory, SkillsReport};
        let config = self
            .parts
            .skills
            .config()
            .await
            .map_err(Refusal::switch_failed)?;
        let threshold = config.skills.inventory_threshold;
        let global = self.parts.realm_config.global().cloned();
        let workspace = self.parts.workspace.clone();
        let mut report = tokio::task::spawn_blocking(move || {
            crate::bootstrap::skills::report(&config, global.as_ref(), &workspace)
        })
        .await
        .map_err(Refusal::switch_failed)?;
        if let SkillsReport::On { inventory, .. } = &mut report {
            let host = Arc::clone(&self.parts.skills) as Arc<dyn crate::tools::SkillHost>;
            let listed = crate::tools::with_skill_runtime(&host, |runtime| async move {
                match runtime {
                    Some(runtime) => crate::tools::listed_skills(&runtime).await.map(|s| s.len()),
                    None => Ok(0),
                }
            })
            .await;
            match listed {
                Ok(skills) => *inventory = Some(Inventory { skills, threshold }),
                Err(err) => tracing::debug!(error = %err, "the skill inventory could not be read"),
            }
        }
        Ok(report)
    }

    /// `rkat/report`: the daemon's side of `/status` (`agent`: the caller's
    /// agent, whose watches it lists).
    pub(super) fn report(&self, agent: Option<&str>) -> Report {
        let ReportParts {
            realm,
            realm_id,
            confinement,
            helper_depth,
            ..
        } = self.parts.report.clone();
        let realm_rules = realm.join(crate::config::RULES_FILE_NAME);
        let project_inputs = self
            .parts
            .project
            .files()
            .map(|file| {
                let mut trust = trust_words(&self.parts.project.status(file));
                if self.parts.project.by_flag() && trust.starts_with("not trusted") {
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
            mode: ModeStatus::of(&self.core.engine),
            realm_rules,
            project_inputs,
            helper_depth,
            watches: agent.map_or_else(Vec::new, |agent| self.core.watches.describe(agent)),
            classifier_model: self.core.engine.reviewer().model(),
        }
    }

    /// The session of `caller`'s agent.
    fn session_of_caller(&self, caller: &Caller) -> Result<meerkat_core::SessionId, Refusal> {
        let agent = self.agent_of(caller)?;
        self.core.conversations.current(&agent).ok_or_else(|| {
            Refusal::new(
                protocol::NO_AGENT,
                format!("agent {agent} has no conversation open"),
            )
        })
    }

    /// `rkat/rule_rows`: the rules, and the grants in force for `caller`'s
    /// agent.
    pub(super) fn rule_and_grant_rows(&self, caller: &Caller) -> crate::approval::desk::RuleRows {
        let mut rows = crate::approval::desk::rule_rows(&self.core.engine);
        let session = self.session_of_caller(caller).ok();
        rows.grants = self.core.engine.grants().rows(session.as_ref());
        rows.statements = self.core.engine.statements().rows(session.as_ref());
        rows
    }

    /// `rkat/notices`: the start-up notices, and what the classifier's
    /// start-up check found, once it did.
    pub(super) fn notices(&self) -> Notices {
        let mut notices = self.parts.startup_notices.clone();
        if let Some(text) = self.core.engine.reviewer().probe_notice() {
            notices.push(StartupNotice::warn(text));
        }
        Notices { notices }
    }

    /// `rkat/auto_stats`: the review numbers since the daemon started and
    /// for `caller`'s agent's conversation.
    pub(super) fn auto_stats(&self, caller: &Caller) -> Outcome {
        let session = self.session_of_caller(caller).ok();
        let (total, mine) = self.core.engine.stats().of(session.as_ref());
        let model = self.core.engine.reviewer().model().map_or_else(
            || "no classifier model".to_string(),
            |m| format!("classifier {m}"),
        );
        Outcome::ok(format!(
            "{model}\nthis conversation: {}\nsince the daemon started: {}",
            mine.describe(),
            total.describe()
        ))
    }

    /// `rkat/classifier_check`: the corpus cases `selection` names through
    /// the daemon's classifier, outside every conversation.
    pub(super) async fn classifier_check(
        &self,
        selection: &crate::approval::corpus::Selection,
    ) -> Result<protocol::ClassifierChecked, Refusal> {
        let refused = |message: String| Refusal::new(protocol::CHECK_REFUSED, message);
        let cases =
            crate::approval::corpus::select(selection).map_err(|err| refused(err.to_string()))?;
        let (model, report) = self
            .core
            .engine
            .reviewer()
            .check(&cases, crate::approval::corpus::Bounds::default())
            .await
            .map_err(|err| refused(err.to_string()))?;
        Ok(protocol::ClassifierChecked { model, report })
    }

    /// `rkat/blocked`: the calls of `caller`'s agent the gate recently
    /// blocked.
    pub(super) fn blocked(&self, caller: &Caller) -> Result<protocol::BlockedList, Refusal> {
        let session = self.session_of_caller(caller)?;
        Ok(protocol::BlockedList {
            calls: self.core.engine.blocks().list(&session),
        })
    }

    /// `rkat/approve`: the user approves blocked call `id`, or allows its
    /// kind of action in their words `effect`. Whoever made the call is
    /// told at once, off this answer's task (an idle agent gets a run of
    /// its own, [`crate::daemon::agents::Agents::approved`]); the answer
    /// waits for no run.
    pub(super) fn approve(
        &self,
        caller: &Caller,
        id: u64,
        scope: crate::approval::grants::GrantScope,
        effect: Option<&str>,
    ) -> Result<Outcome, Refusal> {
        let session = self.session_of_caller(caller)?;
        let done = match effect {
            Some(text) => self.core.engine.approve_effect(&session, id, text, scope),
            None => self.core.engine.approve(&session, id, scope),
        };
        self.after_permission_command();
        Ok(match done {
            Ok(approved) => {
                let notice = approved.notice.clone();
                let agents = Arc::clone(&self.agents);
                tokio::spawn(async move { agents.approved(&session, approved).await });
                Outcome::ok(notice)
            }
            Err(why) => Outcome::failed(why),
        })
    }

    /// `rkat/allow_statement`: the user allows the kind of action `text`
    /// describes for `scope`, typed from scratch; the engine tells the
    /// agent in one system note ([`crate::approval::ApprovalEvent::Granted`]).
    pub(super) fn allow_statement(
        &self,
        caller: &Caller,
        text: &str,
        scope: crate::approval::grants::GrantScope,
    ) -> Result<Outcome, Refusal> {
        let session = self.session_of_caller(caller)?;
        let done = self.core.engine.allow_statement(&session, text, scope);
        self.after_permission_command();
        Ok(Outcome::from(done.map(|approved| approved.notice)))
    }

    /// `rkat/trust_host`: the user trusts a host they typed (the agent of
    /// `caller` is told in one system note), or the hosts of blocked call
    /// `id` (whoever made the call is told to run it again, off this
    /// answer's task, as [`Self::approve`] does).
    pub(super) fn trust_host(
        &self,
        caller: &Caller,
        params: protocol::TrustHostParams,
    ) -> Result<Outcome, Refusal> {
        use crate::config::registry::Level;
        let level = |user: bool| if user { Level::User } else { Level::Workspace };
        let done = match params {
            protocol::TrustHostParams::Host { host, user } => {
                let session = self.session_of_caller(caller).ok();
                self.core
                    .engine
                    .trust_host(session.as_ref(), &host, level(user))
                    .map(|approved| approved.notice)
            }
            protocol::TrustHostParams::Call { id, user } => {
                let session = self.session_of_caller(caller)?;
                self.core
                    .engine
                    .trust_call_hosts(&session, id, level(user))
                    .map(|approved| {
                        let notice = approved.notice.clone();
                        let agents = Arc::clone(&self.agents);
                        tokio::spawn(async move { agents.approved(&session, approved).await });
                        notice
                    })
            }
        };
        self.after_permission_command();
        Ok(Outcome::from(done))
    }

    /// `rkat/untrust_host`: the user no longer trusts `host`.
    pub(super) fn untrust_host(&self, host: &str) -> Outcome {
        let done = self.core.engine.untrust_host(host);
        self.after_permission_command();
        Outcome::from(done)
    }

    /// `rkat/withdraw_approval`: withdraw the approval once of call `id`
    /// of `caller`'s agent; it is a blocked call again.
    pub(super) fn withdraw_approval(&self, caller: &Caller, id: u64) -> Result<Outcome, Refusal> {
        let session = self.session_of_caller(caller)?;
        Ok(Outcome::from(
            self.core.engine.withdraw_approval(&session, id),
        ))
    }

    /// `rkat/remove_statement`: remove statement `id` of `caller`'s agent's
    /// session, or one added always.
    pub(super) fn remove_statement(&self, caller: &Caller, id: u64) -> Result<Outcome, Refusal> {
        let session = self.session_of_caller(caller)?;
        let removed = self.core.engine.remove_statement(&session, id);
        self.after_permission_command();
        Ok(Outcome::from(removed))
    }

    /// `rkat/remove_grant`: remove session grant `id` of `caller`'s agent.
    pub(super) fn remove_grant(&self, caller: &Caller, id: u64) -> Result<Outcome, Refusal> {
        let session = self.session_of_caller(caller)?;
        Ok(Outcome::from(self.core.engine.remove_grant(&session, id)))
    }

    /// `rkat/trust_preview`: `/trust`'s question, each project input read
    /// once with its digest and what of it would run as the host.
    pub(super) fn trust_preview(&self) -> protocol::TrustPreviewList {
        protocol::TrustPreviewList {
            files: self.parts.project.preview(),
        }
    }

    /// `rkat/trust`: `/trust` answered yes, the project inputs recorded as
    /// they were `shown` (their digests, never a new read). The record is
    /// the one the realm config store's guard and the factory's
    /// hook-source check read on every session build, so the hooks run
    /// from the agent's next build (`/new`, `/resume`, a daemon restart).
    /// The agent's MCP servers are part of its mob's definition, made when
    /// the mob was first created; trusting `.rkat/mcp.toml` here does not
    /// add them to it.
    pub(super) fn trust_project(
        &self,
        shown: &[crate::bootstrap::project_trust::TrustPreview],
    ) -> Outcome {
        use std::fmt::Write as _;
        match self.parts.project.trust_shown(shown) {
            Ok(recorded) if recorded.is_empty() => {
                Outcome::ok("the project has no .rkat/mcp.toml or .rkat/config.toml to trust")
            }
            Ok(recorded) => {
                let files = recorded
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                self.audit(crate::daemon::audit::Line::event(
                    "project_trusted",
                    files.clone(),
                ));
                let changed = self.parts.project.changed_since(&recorded);
                let mut text = format!(
                    "trusted {files} as you were shown them; their hooks run from the agent's next session (/new, /resume or a daemon restart), their MCP servers from the daemon's next start"
                );
                if !changed.is_empty() {
                    let names: Vec<String> =
                        changed.iter().map(|p| p.display().to_string()).collect();
                    let _ = write!(
                        text,
                        "; {} changed since you were shown it, so it is not trusted: run /trust again to see it",
                        names.join(", ")
                    );
                }
                Outcome::ok(text)
            }
            Err(err) => Outcome::failed(format!("not trusted: {err}")),
        }
    }
}
