//! What the daemon reads out for the clients: `/status`'s daemon side
//! (`rkat/report`), `/context`, `/audit`, `/trust`, the conversations,
//! their transcripts, usage and checklist, the schedules, the rules and
//! the unattended denials kept for the next client.

use super::*;

impl Local {
    /// `rkat/holds`: the conversations the runtime still holds inputs for.
    pub(super) async fn holds(&self) -> protocol::Holds {
        protocol::Holds {
            held: self.conversations.holding().await,
        }
    }

    /// `rkat/schedules`: the caller's agent's schedules (records of ones
    /// deleted meanwhile go first).
    pub(super) async fn schedules(
        &self,
        caller: &Caller,
    ) -> Result<protocol::ScheduleList, Refusal> {
        let agent = self.agent_of(caller)?;
        self.agents.schedules().reconcile().await;
        Ok(protocol::ScheduleList {
            schedules: self
                .agents
                .schedules()
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
    ) -> Result<RuleOutcome, Refusal> {
        let agent = self.agent_of(caller)?;
        Ok(
            match self.agents.schedules().act(&agent, op, schedule).await {
                Ok(text) => {
                    self.audit(
                        crate::daemon::audit::Line::event("schedule_changed", text.clone())
                            .of_agent(&agent),
                    );
                    RuleOutcome { ok: true, text }
                }
                Err(err) => RuleOutcome {
                    ok: false,
                    text: err.to_string(),
                },
            },
        )
    }

    /// `rkat/rules`: a rule command; a rule written to the realm file is
    /// blocking I/O, off the connection's loop.
    pub(super) async fn rules(
        &self,
        command: crate::approval::desk::RuleCommand,
    ) -> Result<RuleOutcome, Refusal> {
        let engine = Arc::clone(&self.engine);
        tokio::task::spawn_blocking(move || run_rule_command(&engine, &command))
            .await
            .map_err(Refusal::internal)
    }

    /// `rkat/conversations`: the conversations, the caller's agent's own
    /// marked.
    pub(super) async fn conversation_list(&self, caller: &Caller) -> ConversationList {
        let agent = self.agent_of(caller).ok();
        ConversationList {
            conversations: self
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
            .conversations
            .messages(session_id)
            .await
            .map_err(Refusal::switch_failed)?;
        Ok(protocol::Transcript { messages })
    }

    /// `rkat/context`: what the caller's agent's conversation holds (its
    /// messages, tools, hidden tools and skills).
    pub(super) async fn context(
        &self,
        caller: &Caller,
    ) -> Result<protocol::ContextReport, Refusal> {
        let agent = self.agent_of(caller)?;
        let session_id = self
            .conversations
            .current(&agent)
            .ok_or_else(|| Refusal::switch_failed(format!("agent {agent} runs no conversation")))?;
        let messages = self
            .conversations
            .messages(&session_id)
            .await
            .map_err(Refusal::switch_failed)?;
        Ok(protocol::ContextReport {
            messages: crate::daemon::context::MessageCounts::of(&messages),
            tools: self.origins.sizes(&session_id),
            hidden: self.deferral.hidden(&session_id),
            skills: self.skills().await?,
            session_id,
        })
    }

    /// `rkat/endpoint`: the endpoint a conversation runs on.
    pub(super) async fn endpoint_of(
        &self,
        session_id: &meerkat_core::SessionId,
    ) -> protocol::SessionEndpoint {
        protocol::SessionEndpoint {
            endpoint: self.conversations.endpoint_of(session_id).await,
        }
    }

    /// `rkat/usage`: a conversation's token totals, with its cost and last
    /// context as the daemon counted them.
    pub(super) async fn usage(
        &self,
        session_id: meerkat_core::SessionId,
    ) -> Result<Usage, Refusal> {
        let usage = self
            .conversations
            .usage(&session_id)
            .await
            .map_err(Refusal::switch_failed)?;
        let counted = self
            .agents
            .ledger()
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
        let tasks = crate::daemon::tasks::read_tasks(&self.tasks_db, session_id)
            .await
            .map_err(Refusal::internal)?;
        Ok(Tasks { tasks })
    }

    /// `rkat/missed`: what the caller's agent was denied while no client
    /// was attached, emptied.
    pub(super) fn missed(&self, caller: &Caller) -> Result<Notices, Refusal> {
        let agent = self.agent_of(caller)?;
        Ok(Notices {
            notices: self
                .agents
                .take_missed(&agent)
                .into_iter()
                .map(StartupNotice::warn)
                .collect(),
        })
    }

    /// `rkat/checklist_view`: the checklist view, written to the realm's
    /// `tui.toml` (when there is one).
    pub(super) async fn checklist_view(
        &self,
        view: crate::config::ChecklistView,
    ) -> Result<Empty, Refusal> {
        if let Some(file) = self.realm_file.clone() {
            tokio::task::spawn_blocking(move || file.write_checklist(view))
                .await
                .map_err(Refusal::internal)?
                .map_err(Refusal::internal)?;
        }
        Ok(Empty {})
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
        let Some(log) = &self.audit else {
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
        let config = self.skills.config().await.map_err(Refusal::switch_failed)?;
        let threshold = config.skills.inventory_threshold;
        let global = self.realm_config.global().cloned();
        let workspace = self.workspace.clone();
        let mut report = tokio::task::spawn_blocking(move || {
            crate::bootstrap::skills::report(&config, global.as_ref(), &workspace)
        })
        .await
        .map_err(Refusal::switch_failed)?;
        if let SkillsReport::On { inventory, .. } = &mut report {
            let host = Arc::clone(&self.skills) as Arc<dyn crate::tools::SkillHost>;
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

    /// `rkat/trust {"op": "project"}`: `/trust`, every present project
    /// input recorded as it is now. The record is the one the realm config
    /// store's guard and the factory's hook-source check read on every
    /// session build, so the hooks run from the agent's next build
    /// (`/new`, `/resume`, a daemon restart). The agent's MCP servers
    /// are part of its mob's definition, made when the mob was first
    /// created; trusting `.rkat/mcp.toml` here does not add them to it.
    pub(super) fn trust_project(&self) -> RuleOutcome {
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
                self.audit(crate::daemon::audit::Line::event("project_trusted", files.clone()));
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
}
