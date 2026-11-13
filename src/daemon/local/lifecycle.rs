//! An agent's conversations and its leaving: `/new` and `/resume` (`rkat/new`, `rkat/resume`), `rkat/quit`, `rkat/retire_agent` and the retire at a terminating stop.

use super::*;

/// `reason` for asking at `/quit`, with what detaching or terminating does
/// to the caller's agent's schedules, when it has any.
pub(super) fn with_schedules(reason: String, status: &Status) -> String {
    let count = status
        .agent
        .as_ref()
        .and_then(|agent| status.agents.iter().find(|row| &row.name == agent))
        .map_or(0, |row| row.schedules);
    if count == 0 {
        return reason;
    }
    let them = if count == 1 { "it" } else { "them" };
    format!(
        "{reason}\nThis agent has {count} schedule{}: detaching keeps {them} firing; terminating deletes {them}.",
        crate::text::plural(count),
    )
}

impl Local {
    /// `/new` or `/resume`: switch `agent`'s conversation, forget the
    /// gate's state of the one left (its classifier transcript, allow
    /// cache and helpers; a later `/resume` seeds the transcript again),
    /// then tell the new one about plan mode when it is on, and read its
    /// tools. Another agent's conversation is refused with where it is.
    pub(super) async fn switch(&self, agent: &str, target: Target) -> Result<Switched, Refusal> {
        use crate::daemon::conversations::SwitchError;
        let resumed = matches!(target, Target::Resume(_));
        let previous = self.conversations.current(agent);
        let session = self
            .agents
            .switch(agent, target)
            .await
            .map_err(|err| match &err {
                SwitchError::OtherAgent { agent: owner, .. } if self.agents.is_live(owner) => {
                    Refusal::switch_failed(format!(
                        "{err}, which works on it now: /agents {owner} switches to it"
                    ))
                }
                SwitchError::OtherAgent { .. } => Refusal::switch_failed(format!(
                    "{err}, which has quit: `rkat-tui --resume <id>` seats it again on it"
                )),
                _ => err.into(),
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

    /// `agent` leaves the workspace (`rkat/quit`'s retire). A running turn is
    /// interrupted first (its review stops, its open escalations are
    /// answered with Stop), then the agent retires
    /// ([`Agents::retire`]): its helpers stop, the engine archives its
    /// session, which stays in the conversation list and can be seated
    /// again.
    pub(super) async fn retire(
        &self,
        agent: &str,
        why: &crate::daemon::agents::Leave,
        seated: Option<&crate::daemon::agents::Seated<'_>>,
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
            Some(seated) => self.agents.retire_seated(agent, why, seated).await,
            None => self.agents.retire(agent, why).await,
        }
        .map_err(Refusal::switch_failed)?;
        if !schedules_deleted.is_empty() {
            self.audit(
                crate::daemon::audit::Line::event(
                    "schedules_deleted",
                    schedules_deleted.join(", "),
                )
                .of_agent(agent),
            );
        }
        self.plan_noted.lock().await.remove(agent);
        self.audit(
            crate::daemon::audit::Line::event(
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

    /// `rkat/quit` for `caller` ([`crate::daemon::lifecycle::quit_decision`]),
    /// decided and done under the seating lock: no client binds, and no
    /// other `/quit` is decided, between the two. A client that leaves
    /// stops counting at once ([`Lifecycle::leave`]), so a second `/quit`
    /// decided right after sees it gone.
    pub(super) async fn quit(&self, caller: &Caller) -> Result<protocol::QuitOutcome, Refusal> {
        use crate::daemon::lifecycle::QuitPlan;
        use protocol::QuitOutcome;
        let Some(agent) = caller.agent.clone() else {
            return Err(Refusal::new(
                protocol::NO_AGENT,
                "this connection works with no agent: there is nothing to quit",
            ));
        };
        let seated = self.agents.seating().await;
        let status = self.status(caller).await;
        Ok(match crate::daemon::lifecycle::quit_decision(&status) {
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
                let retired = self
                    .retire(&agent, &crate::daemon::agents::Leave::Quit, Some(&seated))
                    .await?;
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

    /// `rkat/retire_agent` for `caller`: retire `name` when it is another
    /// agent than the caller's, has no client and is idle, decided and done
    /// under the seating lock so no client binds to it in between.
    pub(super) async fn retire_other(
        &self,
        caller: &Caller,
        name: &str,
    ) -> Result<Retired, Refusal> {
        let seated = self.agents.seating().await;
        let rows = self.agents.rows().await;
        let Some(row) = rows.iter().find(|row| row.name == name) else {
            return Err(Refusal::new(
                protocol::NO_AGENT,
                format!("no agent {name} works here"),
            ));
        };
        if let Some(why) = row.retire_refusal(caller.agent.as_deref()) {
            return Err(Refusal::new(protocol::AGENT_IN_USE, why));
        }
        tracing::info!(agent = %name, by = ?caller.agent, "an idle agent with no client is retired");
        let why = crate::daemon::agents::Leave::Retired {
            by: caller.agent.clone(),
        };
        self.retire(name, &why, Some(&seated)).await
    }

    /// Retire every agent ([`Self::retire`]; the daemon is shutting down
    /// with `rkat/shutdown {"retire": true}`). A refusal is logged.
    pub async fn retire_all(&self) {
        for agent in self.agents.names() {
            if let Err(refusal) = self
                .retire(&agent, &crate::daemon::agents::Leave::Closed, None)
                .await
            {
                tracing::warn!(%agent, error = %refusal.message, "the agent did not retire");
            }
        }
    }

    /// `rkat/detach`: the caller's agent keeps running without it.
    pub(super) fn detach(&self, caller: &Caller) -> Result<Empty, Refusal> {
        let agent = self.agent_of(caller)?;
        tracing::info!(%agent, "a client detached: the agent keeps running without it");
        self.agents.detach(&agent);
        Ok(Empty {})
    }

    /// `rkat/shutdown`: the daemon stops (`retire`: its agents retire
    /// with it).
    pub(super) fn shutdown(&self, caller: &Caller, retire: bool) -> Empty {
        tracing::info!(owner = caller.owner, retire, "rkat/shutdown: shutting down");
        if retire {
            self.lifecycle.shutdown_retiring();
        } else {
            self.lifecycle.shutdown();
        }
        Empty {}
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
                wires: Vec::new(),
            }],
        };
        assert_eq!(with_schedules("why".into(), &status(0)), "why");
        assert_eq!(
            with_schedules("why".into(), &status(2)),
            "why\nThis agent has 2 schedules: detaching keeps them firing; terminating deletes them."
        );
    }
}
