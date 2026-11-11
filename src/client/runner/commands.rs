//! The commands answered by the daemon's own methods: `/status`,
//! `/audit`, `/schedules`, `/agents`, the permission mode, `/permissions`
//! and `/trust`, the approval dialogs' answers, the checklist and
//! `/context`.

use super::*;

/// The notices a daemon command's answer makes.
pub(super) type Notices = Vec<(NoticeLevel, String)>;

impl Runner {
    /// Ask the daemon `M` with `params` in a task of its own, bounded by
    /// [`CALL_TIMEOUT`], and show the notices `render` makes of the
    /// answer. Unattached, nothing is asked and the user is told that
    /// `what` did not happen and why.
    pub(super) fn ask<M: protocol::Method>(
        &self,
        params: M::Params,
        what: &str,
        render: impl FnOnce(Result<M::Result, CallFailure>) -> Notices + Send + 'static,
    ) where
        M::Params: 'static,
        M::Result: Send,
    {
        if let Some(connection) = self.link_or_say(what) {
            self.spawn_ask::<M>(connection, params, render);
        }
    }

    /// [`Self::ask`] when attached; unattached, nothing (a start-up read
    /// the next attach makes again).
    pub(super) fn ask_if_attached<M: protocol::Method>(
        &self,
        params: M::Params,
        render: impl FnOnce(Result<M::Result, CallFailure>) -> Notices + Send + 'static,
    ) where
        M::Params: 'static,
        M::Result: Send,
    {
        if let Some(connection) = self.connection() {
            self.spawn_ask::<M>(connection, params, render);
        }
    }

    fn spawn_ask<M: protocol::Method>(
        &self,
        connection: Arc<Connection>,
        params: M::Params,
        render: impl FnOnce(Result<M::Result, CallFailure>) -> Notices + Send + 'static,
    ) where
        M::Params: 'static,
        M::Result: Send,
    {
        let say = self.say();
        tokio::spawn(async move {
            let answer = bounded(CALL_TIMEOUT, remote::call::<M>(&connection, &params)).await;
            for (level, text) in render(answer) {
                say.notice(level, text);
            }
        });
    }

    pub(super) fn status_report(&self) {
        let mut lines = vec![format!(
            "rkat-tui {} (attached to the workspace daemon)",
            crate::BUILD
        )];
        match self.conn.link() {
            Some(link) => {
                lines.push(format!(
                    "daemon: {} (pid {}), mob `{}`, agent `{}`",
                    link.endpoint.address,
                    link.endpoint.pid,
                    crate::daemon::MOB_ID,
                    link.connection.agent()
                ));
                lines.push(format!("session: {}", link.session));
            }
            None => lines.push("daemon: not attached".to_string()),
        }
        if let Some(ShownModel { endpoint, model }) = &self.model {
            lines.push(format!("model: {endpoint}/{model}"));
        }
        match (&self.config.remote, &self.config.address) {
            (Some(why), _) => lines.push(format!(
                "workspace: the daemon's, not on this machine ({why}); local files: {}",
                self.config.scope.context_root.display()
            )),
            (None, address) => lines.push(format!(
                "workspace: {}{}",
                self.config.scope.context_root.display(),
                if address.is_some() {
                    " (daemon reached by address)"
                } else {
                    ""
                }
            )),
        }
        let files: Vec<String> = self
            .config
            .endpoint_files
            .iter()
            .map(|file| file.display().to_string())
            .collect();
        lines.push(format!("endpoint file: {}", files.join(", then ")));
        lines.push(
            "approval gate: in the daemon (requests show here; /permissions and Shift+Tab act on the daemon)"
                .to_string(),
        );
        let say = self.say();
        let Some(connection) = self.connection() else {
            say.notice(NoticeLevel::Info, lines.join("\n"));
            return;
        };
        tokio::spawn(async move {
            match bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::Report>(&connection, &protocol::Empty {}),
            )
            .await
            {
                Ok(report) => lines.extend(report_lines(&report)),
                Err(err) => lines.push(format!("the daemon's side is unknown: {err}")),
            }
            say.notice(NoticeLevel::Info, lines.join("\n"));
        });
    }

    /// `/audit [n]`: the audit log's last lines for this agent
    /// (`rkat/audit`).
    pub(super) fn show_audit(&self, lines: Option<usize>) {
        self.ask::<protocol::method::Audit>(
            protocol::AuditParams { lines },
            "the audit log was not read",
            |read| {
                vec![match read {
                    Ok(tail) if tail.lines.is_empty() => (
                        NoticeLevel::Info,
                        format!(
                            "the audit log {} has nothing for this agent yet",
                            tail.path.display()
                        ),
                    ),
                    Ok(tail) => (
                        NoticeLevel::Info,
                        format!(
                            "audit log {} (last {} lines for this agent):\n{}",
                            tail.path.display(),
                            tail.lines.len(),
                            tail.lines.join("\n")
                        ),
                    ),
                    Err(err) => (NoticeLevel::Warn, format!("no audit log: {err}")),
                }]
            },
        );
    }

    /// `/schedules`: this agent's schedules (`rkat/schedules`), each with
    /// what its runs cost at the readout's prices; or the user's own
    /// pause, resume or delete of one.
    pub(super) fn schedules(&self, action: Option<(protocol::ScheduleOp, String)>) {
        if let Some((op, schedule)) = action {
            self.ask::<protocol::method::Schedule>(
                protocol::ScheduleAction { op, schedule },
                "the schedule was not changed",
                |done| {
                    vec![match done {
                        Ok(outcome) if outcome.ok => (NoticeLevel::Info, outcome.text),
                        Ok(outcome) => (NoticeLevel::Warn, outcome.text),
                        Err(err) => (NoticeLevel::Warn, format!("schedule not changed: {err}")),
                    }]
                },
            );
            return;
        }
        let prices = self.bench.prices().clone();
        self.ask::<protocol::method::Schedules>(
            protocol::Empty {},
            "the schedules were not read",
            move |listed| {
                vec![match listed {
                    Ok(list) if list.schedules.is_empty() => (
                        NoticeLevel::Info,
                        "this agent has no schedules (the agent sets them with meerkat_schedule_create)"
                            .to_string(),
                    ),
                    Ok(list) => (NoticeLevel::Info, schedules_text(&list.schedules, &prices)),
                    Err(err) => (
                        NoticeLevel::Warn,
                        format!("cannot list the schedules: {err}"),
                    ),
                }]
            },
        );
    }

    /// `/agents`: the workspace's agents, from the daemon's `rkat/status`.
    pub(super) fn list_agents(&self) {
        let say = self.say();
        let Some(connection) = self.link_or_say("the agents were not listed") else {
            return;
        };
        let bound = self.agent.clone();
        tokio::spawn(async move {
            match daemon_status(&connection).await {
                Some(status) => {
                    let mine = status.agent.as_deref().or(bound.as_deref());
                    say.notice(
                        NoticeLevel::Info,
                        agent_lines(&status.agents, mine).join("\n"),
                    );
                }
                None => say.notice(NoticeLevel::Warn, "the daemon did not list its agents"),
            }
        });
    }

    /// Shift+Tab, `/mode`, `/auto resume`: switch the daemon's mode.
    pub(super) fn set_mode(&mut self, mode: crate::approval::mode::PermissionMode) {
        let params = SetModeParams { mode };
        self.ask::<protocol::method::SetMode>(params, "the mode was not changed", move |set| {
            match set {
                Ok(set) => {
                    // The mode itself comes as every client's push.
                    let mut notices: Notices = set
                        .warning
                        .map(|warning| (NoticeLevel::Warn, warning))
                        .into_iter()
                        .collect();
                    notices.push((NoticeLevel::Info, set.notice));
                    notices
                }
                Err(err) => vec![(NoticeLevel::Error, format!("mode not changed: {err}"))],
            }
        });
    }

    /// A command whose answer is a [`RuleOutcome`] (`/permissions …` on the
    /// daemon's engine, `/trust` on its trust record),
    /// named `name` in what goes wrong.
    pub(super) fn outcome_command<M: protocol::Method<Result = RuleOutcome>>(
        &self,
        params: M::Params,
        name: &str,
    ) where
        M::Params: 'static,
    {
        let failed = format!("{name} failed");
        self.ask::<M>(params, &format!("{name} needs it"), move |answer| {
            vec![match answer {
                Ok(outcome) if outcome.ok => (NoticeLevel::Info, outcome.text),
                Ok(outcome) => (NoticeLevel::Error, outcome.text),
                Err(err) => (NoticeLevel::Error, format!("{failed}: {err}")),
            }]
        });
    }

    /// The UI answered an approval dialog: decide its decision.
    pub(super) fn respond_escalation(&mut self, id: EscalationId, answer: &EscalationAnswer) {
        if let Some((decision, digest)) = self.approvals.answer_decision(id) {
            self.decide(id, &decision, &digest, answer);
        }
    }

    /// The UI answered the dialog `id` of `decision`: decide it. A decision
    /// another client (or the daemon, when the engine gave up on it)
    /// settled first is refused by the engine (`not_open`) and the answer
    /// is dropped; "deny and stop" also interrupts the turn.
    pub(super) fn decide(
        &mut self,
        id: EscalationId,
        decision: &str,
        digest: &str,
        answer: &EscalationAnswer,
    ) {
        self.send(Msg::EscalationResolved(id));
        let Some(connection) = self.link_or_say(
            "the answer was not sent (the request shows again after the reattach while it is open)",
        ) else {
            return;
        };
        let params = decision_params(decision, digest, answer);
        let ui_tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let decided = bounded(CALL_TIMEOUT, remote::decide(&connection, &params)).await;
            let text = match decided {
                Ok(_) => return,
                Err(CallFailure::Failed(ClientError::Rpc { message, .. })) => {
                    tracing::debug!(%message, "decision already settled");
                    (
                        NoticeLevel::Info,
                        format!(
                            "that approval request was already settled; your answer was not used ({message})"
                        ),
                    )
                }
                Err(err) => (NoticeLevel::Warn, format!("the answer was not sent: {err}")),
            };
            let _ = ui_tx.send(Msg::Notice {
                level: text.0,
                text: text.1,
            });
        });
        if matches!(answer, EscalationAnswer::Stop) {
            self.interrupt();
        }
    }

    /// `/context`: the UI's `header` plus what the context holds
    /// ([`crate::workbench::context`]): the conversation's messages and
    /// tool definitions as the daemon reports them (`rkat/context`), the
    /// instruction files the daemon's session builds read (this
    /// workspace's, found here).
    pub(super) fn context_report(&self, header: String) {
        let link = self.conn.link().map(|link| Arc::clone(&link.connection));
        let scope = self.config.scope.clone();
        let ui_tx = self.ui_tx.clone();
        let unattached = self.unattached();
        tokio::spawn(async move {
            let report = match link {
                Some(connection) => bounded(
                    CALL_TIMEOUT,
                    remote::call::<protocol::method::Context>(&connection, &protocol::Empty {}),
                )
                .await
                .map_err(|err| err.to_string()),
                None => Err(unattached),
            };
            let sources = crate::bootstrap::InstructionSources::of(&scope);
            let files = context::instruction_files(&scope, &sources);
            let text =
                context::context_breakdown(&header, report.as_ref().map_err(Clone::clone), &files);
            let _ = ui_tx.send(Msg::Notice {
                level: NoticeLevel::Info,
                text,
            });
        });
    }

    /// Read the conversation's tasks from the daemon (`rkat/tasks`) for the
    /// Ctrl+T checklist.
    pub(super) fn refresh_tasks(&self) {
        let Some(link) = self.conn.link() else {
            return;
        };
        let connection = Arc::clone(&link.connection);
        let session = link.session.clone();
        let reports = self.reporter();
        tokio::spawn(async move {
            let params = protocol::SessionParams {
                session_id: session.clone(),
            };
            let tasks = bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::Tasks>(&connection, &params),
            )
            .await
            .map(|read| read.tasks)
            .map_err(|err| err.to_string());
            reports.send(Report::Tasks { session, tasks });
        });
    }

    /// Ctrl+T: the daemon records the view as the realm's `[ui] checklist`
    /// (the realm file and its trust record are the daemon's).
    pub(super) fn set_checklist_view(&self, view: ChecklistView) {
        const UNSAVED: &str = "the checklist view applies to this run but was not saved";
        self.ask::<protocol::method::ChecklistView>(
            protocol::ChecklistViewParams { view },
            UNSAVED,
            |saved| match saved {
                Ok(_) => Vec::new(),
                Err(err) => vec![(NoticeLevel::Warn, format!("{UNSAVED}: {err}"))],
            },
        );
    }
}

/// The daemon's `rkat/status` (`None`: no answer).
pub(super) async fn daemon_status(connection: &Connection) -> Option<protocol::Status> {
    bounded(
        LEAVE_TIMEOUT,
        remote::call::<protocol::method::Status>(connection, &protocol::Empty {}),
    )
    .await
    .map_err(|err| tracing::info!(error = %err, "the daemon's status is unknown"))
    .ok()
}

/// The start-up notice naming the agents other than `mine` working here,
/// with their state ("main-abc is running, main-def is idle; `-c` joins
/// the most recent"); `None` when there are none.
#[must_use]
pub fn others_notice(agents: &[protocol::AgentRow], mine: &str) -> Option<String> {
    let others: Vec<String> = agents
        .iter()
        .filter(|row| row.name != mine)
        .map(|row| format!("{} is {}", row.name, row.run_label()))
        .collect();
    if others.is_empty() {
        return None;
    }
    Some(format!(
        "other agents work in this workspace: {}; you are agent {mine}, wired to them (rkat-tui -c joins the most recent one, /agents <name> switches)",
        others.join(", ")
    ))
}

/// `/agents`' lines: one per agent (name, state, conversation, clients),
/// `mine` marked.
#[must_use]
pub fn agent_lines(agents: &[protocol::AgentRow], mine: Option<&str>) -> Vec<String> {
    let mut lines = vec![format!(
        "{} in this workspace:",
        crate::text::count(agents.len(), "agent")
    )];
    for row in agents {
        let session = row.session_id.as_ref().map_or_else(
            || "no conversation".to_string(),
            |session| {
                let id = session.to_string();
                format!("conversation {}", id.get(..8).unwrap_or(&id))
            },
        );
        let mut line = format!(
            "  {} · {} · {session} · {} client{}",
            row.name,
            row.run_label(),
            row.clients,
            crate::text::plural(row.clients)
        );
        if row.detached && row.clients == 0 {
            line.push_str(" (detached)");
        }
        if mine == Some(row.name.as_str()) {
            line.push_str(" (this rkat-tui)");
        }
        lines.push(line);
    }
    lines.push("/agents <name> switches to one".to_string());
    lines
}

/// `/status`'s lines for the daemon's side ([`protocol::method::Report`]).
pub(super) fn report_lines(report: &protocol::Report) -> Vec<String> {
    use crate::approval::mode::PermissionMode;
    let mut lines = vec![
        format!("realm: {} (id {})", report.realm.display(), report.realm_id),
        format!("shell confinement: {}", report.confinement),
    ];
    let mode = &report.mode;
    lines.push(format!(
        "permission mode: {}",
        match mode.mode {
            PermissionMode::Auto => format!(
                "auto{} ({})",
                if mode.paused { ", PAUSED" } else { "" },
                mode.denials.describe()
            ),
            other => other.label().to_string(),
        }
    ));
    lines.push(format!("realm rules: {}", report.realm_rules.display()));
    for input in &report.project_inputs {
        lines.push(format!("project {}: {}", input.path.display(), input.trust));
    }
    lines.push(match report.helper_depth {
        0 => "helpers: depth limit 0, every helper is refused".to_string(),
        1 => "helpers: depth limit on (max_depth 1: the agent's helpers start none of their own)"
            .to_string(),
        n => format!("helpers: depth limit on (max_depth {n})"),
    });
    if !report.watches.is_empty() {
        lines.push(format!("file watches: {}", report.watches.join("; ")));
    }
    lines
}

/// `/schedules`' text: a line per schedule.
pub(super) fn schedules_text(
    rows: &[protocol::ScheduleRow],
    prices: &crate::pricing::PriceTable,
) -> String {
    let mut text = format!(
        "{} of this agent:",
        crate::text::count(rows.len(), "schedule")
    );
    for row in rows {
        let cost = match &row.cost {
            None => "$0".to_string(),
            Some(cost) => {
                let (usd, all) = cost.cost_usd(prices);
                if all {
                    format!("${usd:.4}")
                } else {
                    format!("${usd:.4}+ (some models unpriced)")
                }
            }
        };
        text.push_str(&format!(
            "\n  {}  {}  next {}  {}  fired {}x  {}",
            row.name,
            row.expression,
            row.next.as_deref().unwrap_or("none"),
            row.state,
            row.fired,
            cost,
        ));
    }
    text
}
