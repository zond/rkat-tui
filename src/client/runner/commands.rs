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

    /// `/schedules`: the schedule picker over this agent's schedules
    /// (`rkat/schedules`), each with what its runs cost at the readout's
    /// prices; or the user's own pause, resume or delete of one.
    pub(super) fn schedules(&self, action: Option<(protocol::ScheduleOp, String)>) {
        match action {
            Some((op, schedule)) => self.ask::<protocol::method::Schedule>(
                protocol::ScheduleAction { op, schedule },
                "the schedule was not changed",
                |done| vec![schedule_notice(done)],
            ),
            None => self.schedule_picker(None),
        }
    }

    /// The schedule picker's act on the schedule `id`: as the typed
    /// command, then the picker's list again (it replaces the picker when
    /// that is still open).
    pub(super) fn picked_schedule(&self, op: protocol::ScheduleOp, id: String) {
        self.schedule_picker(Some(protocol::ScheduleAction { op, schedule: id }));
    }

    /// Do `action` first when there is one (its notice shown), then list
    /// the schedules into the picker: a fresh one, or (after an action) a
    /// refresh of the one open.
    fn schedule_picker(&self, action: Option<protocol::ScheduleAction>) {
        let Some(connection) = self.link_or_say(if action.is_some() {
            "the schedule was not changed"
        } else {
            "the schedules were not read"
        }) else {
            return;
        };
        let say = self.say();
        let prices = self.bench.prices().clone();
        tokio::spawn(async move {
            let refresh = action.is_some();
            if let Some(action) = action {
                let done = bounded(
                    CALL_TIMEOUT,
                    remote::call::<protocol::method::Schedule>(&connection, &action),
                )
                .await;
                let (level, text) = schedule_notice(done);
                say.notice(level, text);
            }
            let listed = bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::Schedules>(&connection, &protocol::Empty {}),
            )
            .await;
            match listed {
                Ok(list) => {
                    let picker = schedule_picker(&list.schedules, &prices);
                    let _ = say.ui_tx.send(Msg::ShowPicker { picker, refresh });
                }
                Err(err) => say.notice(
                    NoticeLevel::Warn,
                    format!("cannot list the schedules: {err}"),
                ),
            }
        });
    }

    /// The `/permissions` picker over the daemon's rules
    /// (`rkat/rule_rows`); after the picker's removal of rule `remove`
    /// (as `/permissions rm`, its notice shown), a refresh of the one open.
    pub(super) fn rule_picker(&self, remove: Option<usize>) {
        let Some(connection) = self.link_or_say(if remove.is_some() {
            "/permissions needs it"
        } else {
            "the rules were not read"
        }) else {
            return;
        };
        let say = self.say();
        tokio::spawn(async move {
            let refresh = remove.is_some();
            if let Some(n) = remove {
                let done = bounded(
                    CALL_TIMEOUT,
                    remote::call::<protocol::method::Rules>(
                        &connection,
                        &RuleCommand::Remove { n },
                    ),
                )
                .await;
                let (level, text) = match done {
                    Ok(outcome) if outcome.ok => (NoticeLevel::Info, outcome.text),
                    Ok(outcome) => (NoticeLevel::Error, outcome.text),
                    Err(err) => (NoticeLevel::Error, format!("/permissions failed: {err}")),
                };
                say.notice(level, text);
            }
            match bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::RuleRows>(&connection, &protocol::Empty {}),
            )
            .await
            {
                Ok(rows) => {
                    let picker = rule_picker(&rows.rules);
                    let _ = say.ui_tx.send(Msg::ShowPicker { picker, refresh });
                }
                Err(err) => say.notice(NoticeLevel::Warn, format!("cannot list the rules: {err}")),
            }
        });
    }

    /// `/agents`: open the picker over the workspace's agents, each with
    /// its conversation's title when the conversation list has it (a list
    /// that cannot be read leaves the titles out).
    pub(super) fn show_agent_picker(&self) {
        let say = self.say();
        let Some(connection) = self.link_or_say("the agents were not listed") else {
            return;
        };
        let bound = self.agent.clone();
        tokio::spawn(async move {
            let Some(status) = daemon_status(&connection).await else {
                say.notice(NoticeLevel::Warn, "the daemon did not list its agents");
                return;
            };
            let listed = bounded(CALL_TIMEOUT, remote::conversations(&connection))
                .await
                .map_err(|err| tracing::info!(error = %err, "no conversation titles for /agents"))
                .unwrap_or_default();
            let mine = status.agent.as_deref().or(bound.as_deref());
            let _ = say.ui_tx.send(Msg::ShowAgentPicker(agent_entries(
                &status.agents,
                &listed,
                mine,
            )));
        });
    }

    /// The picker's confirmed retire: `name` leaves the workspace through
    /// the daemon (`rkat/retire_agent`), which refuses an agent in use.
    /// The picker stays open: once the daemon confirms, it lists the agents
    /// again without the retired one ([`Msg::RefreshAgentPicker`]); a
    /// refusal is shown on the picker ([`Msg::AgentPickerRefused`]) as well
    /// as in the transcript.
    pub(super) fn retire_agent(&self, name: String) {
        let say = self.say();
        let Some(connection) = self.link_or_say("the agent was not retired") else {
            return;
        };
        let bound = self.agent.clone();
        tokio::spawn(async move {
            let params = protocol::AgentParams { name: name.clone() };
            let retired = bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::RetireAgent>(&connection, &params),
            )
            .await;
            match retired {
                Ok(retired) => say.notice(NoticeLevel::Info, retired_words(&retired)),
                Err(err) => {
                    let why = format!("{name} was not retired: {err}");
                    say.notice(NoticeLevel::Warn, why.clone());
                    let _ = say.ui_tx.send(Msg::AgentPickerRefused(why));
                    return;
                }
            }
            let Some(status) = daemon_status(&connection).await else {
                return;
            };
            let listed = bounded(CALL_TIMEOUT, remote::conversations(&connection))
                .await
                .unwrap_or_default();
            let mine = status.agent.as_deref().or(bound.as_deref());
            let _ = say.ui_tx.send(Msg::RefreshAgentPicker(agent_entries(
                &status.agents,
                &listed,
                mine,
            )));
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

/// The start-up notice about the agents other than `mine` working here;
/// `None` when there are none. A client that asked for a `new` agent
/// (a start with neither `-c` nor `--resume`) is told so, with how many
/// others there are, how many of them are idle without a client, and how
/// to reach them; one that joined an agent is told the others' state.
#[must_use]
pub fn others_notice(agents: &[protocol::AgentRow], mine: &str, new: bool) -> Option<String> {
    let others: Vec<&protocol::AgentRow> = agents.iter().filter(|row| row.name != mine).collect();
    if others.is_empty() {
        return None;
    }
    if new {
        let idle = others
            .iter()
            .filter(|row| row.clients == 0 && row.running == Some(false))
            .count();
        let idle = if idle == 0 {
            String::new()
        } else {
            format!(", {idle} idle without a client")
        };
        let count = others.len();
        let verb = if count == 1 { "works" } else { "work" };
        let noun = if count == 1 { "other" } else { "others" };
        return Some(format!(
            "started a new agent ({mine}); {count} {noun} {verb} here{idle}: /agents picks one (rkat-tui -c joins the most recently active instead of starting a new one)"
        ));
    }
    let states: Vec<String> = others
        .iter()
        .map(|row| format!("{} is {}", row.name, row.run_label()))
        .collect();
    Some(format!(
        "other agents work in this workspace: {}; you are agent {mine}, wired to them (/agents picks one)",
        states.join(", ")
    ))
}

/// The `/agents` picker's rows: each agent of `agents` with its
/// conversation's title from `listed`, marked when it is `mine`, with why
/// the retire key refuses it.
#[must_use]
pub fn agent_entries(
    agents: &[protocol::AgentRow],
    listed: &[crate::daemon::conversations::Listed],
    mine: Option<&str>,
) -> Vec<crate::ui::AgentEntry> {
    agents
        .iter()
        .map(|row| {
            let title = row.session_id.as_ref().and_then(|session| {
                listed
                    .iter()
                    .find(|listed| &listed.session_id == session)
                    .and_then(|listed| listed.title.clone())
            });
            crate::ui::AgentEntry {
                name: row.name.clone(),
                run: row.run_label(),
                conversation: row
                    .session_id
                    .as_ref()
                    .map(|session| crate::text::short_id(&session.to_string()).to_string()),
                title,
                clients: row.clients,
                detached: row.detached,
                mine: mine == Some(row.name.as_str()),
                unretirable: row.retire_refusal(mine),
                wires: row.wires.iter().map(protocol::WireRow::label).collect(),
            }
        })
        .collect()
}

/// What a retire from `/agents` did, in words.
pub(super) fn retired_words(retired: &protocol::Retired) -> String {
    let mut text = format!("retired agent {}", retired.agent);
    if let Some(session) = &retired.session_id {
        text.push_str(&format!(
            "; its conversation {} stays in /resume",
            crate::text::short_id(&session.to_string())
        ));
    }
    if !retired.schedules_deleted.is_empty() {
        text.push_str(&format!(
            "; its schedules were deleted: {}",
            retired.schedules_deleted.join(", ")
        ));
    }
    text
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

/// The `/permissions` picker: a row per rule (number, allow or deny, tool,
/// pattern or literal, layer); `d` removes it after a confirmation that
/// says how long the removal lasts: for good for the realm's own rules,
/// until the daemon restarts for those written in another layer (as
/// `/permissions rm` does). Adding stays typed; the hint says how.
pub(super) fn rule_picker(rules: &[crate::approval::desk::RuleRow]) -> crate::ui::ListPicker {
    use crate::approval::rules::RuleOrigin;
    use crate::ui::{ListPicker, PickAct, PickKey, PickRow, PickerKind};
    use ratatui::crossterm::event::KeyCode;
    let rows = rules
        .iter()
        .map(|rule| {
            let kind = if rule.deny { "deny" } else { "allow" };
            let what = match (&rule.arg, rule.literal) {
                (None, _) => format!("{kind} {} (every call)", rule.tool),
                (Some(arg), true) => format!("{kind} {} literal `{arg}`", rule.tool),
                (Some(arg), false) => format!("{kind} {} pattern `{arg}`", rule.tool),
            };
            let lasts = match rule.origin {
                RuleOrigin::Realm | RuleOrigin::Session => {
                    "it leaves the workspace's rules for good".to_string()
                }
                RuleOrigin::Global => "it is written in your global tui.toml, so it is removed until the daemon restarts (edit that file to drop it for good)".to_string(),
                RuleOrigin::Project => "it is written in the project's .rkat/tui.toml, so it is removed until the daemon restarts (edit that file to drop it for good)".to_string(),
                RuleOrigin::Cli => "it comes from the daemon's command line, so it is removed until the daemon restarts".to_string(),
            };
            let n = usize::try_from(rule.n).unwrap_or(usize::MAX);
            PickRow::new(
                format!("{}. {what} · {}", rule.n, rule.origin.as_str()),
                vec![PickKey::confirmed(
                    KeyCode::Char('d'),
                    PickAct::RemoveRule(n),
                    format!("remove rule {}? {lasts}", rule.n),
                )],
            )
        })
        .collect();
    ListPicker::new(
        PickerKind::Permissions,
        rows,
        "no approval rules yet",
        "↑/↓ move · d remove · /permissions add allow|deny <tool> [pattern] adds one · Esc close",
    )
}

/// What a pause, resume or delete of a schedule did, as a notice.
fn schedule_notice(done: Result<RuleOutcome, CallFailure>) -> (NoticeLevel, String) {
    match done {
        Ok(outcome) if outcome.ok => (NoticeLevel::Info, outcome.text),
        Ok(outcome) => (NoticeLevel::Warn, outcome.text),
        Err(err) => (NoticeLevel::Warn, format!("schedule not changed: {err}")),
    }
}

/// The `/schedules` picker: a row per schedule (name, when, next
/// occurrence, state, times fired, what its runs cost); Enter or `p`
/// pauses an active one and resumes a paused one, `d` deletes after a
/// confirmation (each by its id, as the typed command does).
pub(super) fn schedule_picker(
    rows: &[protocol::ScheduleRow],
    prices: &crate::pricing::PriceTable,
) -> crate::ui::ListPicker {
    use crate::ui::{ListPicker, PickAct, PickKey, PickRow, PickerKind};
    use protocol::ScheduleOp;
    use ratatui::crossterm::event::KeyCode;
    let rows = rows
        .iter()
        .map(|row| {
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
            let text = format!(
                "{}  {}  next {}  {}  fired {}x  {}",
                row.name,
                row.expression,
                row.next.as_deref().unwrap_or("none"),
                row.state,
                row.fired,
                cost,
            );
            let toggle = if row.state == "paused" {
                ScheduleOp::Resume
            } else {
                ScheduleOp::Pause
            };
            let toggle = PickAct::Schedule(toggle, row.id.clone());
            PickRow::new(
                text,
                vec![
                    PickKey::now(KeyCode::Enter, toggle.clone()),
                    PickKey::now(KeyCode::Char('p'), toggle),
                    PickKey::confirmed(
                        KeyCode::Char('d'),
                        PickAct::Schedule(ScheduleOp::Delete, row.id.clone()),
                        format!("delete schedule {}? it never fires again", row.name),
                    ),
                ],
            )
        })
        .collect();
    ListPicker::new(
        PickerKind::Schedules,
        rows,
        "this agent has no schedules (the agent sets them with meerkat_schedule_create)",
        "↑/↓ move · Enter or p pause/resume · d delete · Esc close",
    )
}
