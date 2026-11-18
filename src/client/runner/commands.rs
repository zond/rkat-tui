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
                // The bar leaves these out: they are here, each on its line.
                lines.push(format!(
                    "daemon: {} (pid {}), mob `{}`",
                    link.endpoint.address,
                    link.endpoint.pid,
                    crate::daemon::MOB_ID
                ));
                lines.push(format!("agent: {}", link.connection.agent()));
                let session = link.session.to_string();
                lines.push(format!(
                    "conversation: {session} (short {})",
                    crate::text::short_id(&session)
                ));
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

    /// `/auto model`: the model picker for the classifier, its model now
    /// (`rkat/report`) marked; the lists are the ones `/model` shows.
    pub(super) fn classifier_picker(&self) {
        let Some(connection) = self.link_or_say("/auto model needs it") else {
            return;
        };
        let reports = self.reporter();
        let say = self.say();
        tokio::spawn(async move {
            match bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::Report>(&connection, &protocol::Empty {}),
            )
            .await
            {
                Ok(report) => reports.send(Report::ClassifierModel(report.classifier_model)),
                Err(err) => say.notice(
                    NoticeLevel::Warn,
                    format!("the classifier's model is unknown: {err}"),
                ),
            }
        });
    }

    /// `/auto model <ref> [--user]` or the classifier picker's choice:
    /// resolve the reference as `/model` does and set
    /// `auto.classifier_model` to `endpoint:model` (the daemon reviews with
    /// it at once); its answer is a notice.
    pub(super) fn set_classifier_model(&self, reference: &str, user: bool) {
        let switch = match super::turn::model_switch(
            reference,
            &self.config.endpoints,
            self.bench.models(),
            self.model.as_ref().map(|shown| shown.endpoint.as_str()),
        ) {
            Ok(switch) => switch,
            Err(err) => {
                self.notice(
                    NoticeLevel::Warn,
                    format!("the classifier model was not changed: {err} (/auto model picks from the lists)"),
                );
                return;
            }
        };
        self.set_settings(
            vec![(
                "auto.classifier_model".to_string(),
                Some(format!("{}:{}", switch.endpoint, switch.model)),
            )],
            user,
        );
    }

    /// `/set`, `/unset` and their shorthands: each change in order
    /// (`rkat/set_setting`), each answer a notice (a refusal stops the
    /// rest).
    pub(super) fn set_settings(&self, changes: Vec<(String, Option<String>)>, user: bool) {
        let Some(connection) = self.link_or_say("the setting was not changed") else {
            return;
        };
        let say = self.say();
        tokio::spawn(async move {
            for (key, value) in changes {
                let ok = set_setting(&connection, &say, key, value, user).await;
                if !ok {
                    break;
                }
            }
        });
    }

    /// `/settings [filter]`: the picker over every setting; with a change
    /// (the picker's), make it first and show the list again.
    pub(super) fn settings_picker(
        &self,
        filter: Option<String>,
        change: Option<(String, Option<String>, bool)>,
    ) {
        let Some(connection) = self.link_or_say("/settings needs it") else {
            return;
        };
        let say = self.say();
        tokio::spawn(async move {
            let refresh = change.is_some();
            let mut done = None;
            if let Some((key, value, user)) = change
                && set_setting(&connection, &say, key.clone(), value, user).await
            {
                done = Some(format!(
                    "{key} changed: the confirmation is in the transcript"
                ));
            }
            match bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::Settings>(&connection, &protocol::Empty {}),
            )
            .await
            {
                Ok(list) => {
                    let picker = settings_picker(&list, filter.as_deref());
                    let _ = say.ui_tx.send(Msg::ShowPicker { picker, refresh });
                    say.picker_done(crate::ui::PickerKind::Settings, done);
                }
                Err(err) => say.notice(
                    NoticeLevel::Warn,
                    format!("the settings were not listed: {err}"),
                ),
            }
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
            let mut changed = None;
            if let Some(action) = action {
                let done = bounded(
                    CALL_TIMEOUT,
                    remote::call::<protocol::method::Schedule>(&connection, &action),
                )
                .await;
                let (level, text) = schedule_notice(done);
                if level == NoticeLevel::Info {
                    changed = Some(text.clone());
                }
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
                    say.picker_done(crate::ui::PickerKind::Schedules, changed);
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
            let mut removed = None;
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
                if level == NoticeLevel::Info {
                    removed = Some(text.clone());
                }
                say.notice(level, text);
            }
            match bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::RuleRows>(&connection, &protocol::Empty {}),
            )
            .await
            {
                Ok(rows) => {
                    let picker = rule_picker(&rows.rules, &rows.grants, &rows.statements);
                    let _ = say.ui_tx.send(Msg::ShowPicker { picker, refresh });
                    say.picker_done(crate::ui::PickerKind::Permissions, removed);
                }
                Err(err) => say.notice(NoticeLevel::Warn, format!("cannot list the rules: {err}")),
            }
        });
    }

    /// The `/permissions` picker's confirmed removal of session grant
    /// `id` (`statement`: of statement `id`): its notice, then the picker
    /// again.
    pub(super) fn grant_removal(&self, id: u64, statement: bool) {
        let Some(connection) = self.link_or_say("/permissions needs it") else {
            return;
        };
        let say = self.say();
        tokio::spawn(async move {
            let params = protocol::GrantParams { id };
            let done = if statement {
                bounded(
                    CALL_TIMEOUT,
                    remote::call::<protocol::method::RemoveStatement>(&connection, &params),
                )
                .await
            } else {
                bounded(
                    CALL_TIMEOUT,
                    remote::call::<protocol::method::RemoveGrant>(&connection, &params),
                )
                .await
            };
            let (level, text) = outcome_notice(done, "/permissions");
            let removed = (level == NoticeLevel::Info).then(|| text.clone());
            say.notice(level, text);
            match bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::RuleRows>(&connection, &protocol::Empty {}),
            )
            .await
            {
                Ok(rows) => {
                    let picker = rule_picker(&rows.rules, &rows.grants, &rows.statements);
                    let _ = say.ui_tx.send(Msg::ShowPicker {
                        picker,
                        refresh: true,
                    });
                    say.picker_done(crate::ui::PickerKind::Permissions, removed);
                }
                Err(err) => say.notice(NoticeLevel::Warn, format!("cannot list the rules: {err}")),
            }
        });
    }

    /// `/approve`: the picker over the calls the gate recently blocked and
    /// the approvals once that wait; after the picker's act `picked` (its
    /// notice shown), a refresh of the one open.
    pub(super) fn approve_picker(&self, picked: Option<Picked>) {
        let Some(connection) = self.link_or_say("/approve needs it") else {
            return;
        };
        let say = self.say();
        tokio::spawn(async move {
            let refresh = picked.is_some();
            let mut refused = None;
            let mut granted = None;
            if let Some(picked) = picked {
                let done = match picked {
                    Picked::Approve(id, scope, effect) => {
                        bounded(
                            CALL_TIMEOUT,
                            remote::call::<protocol::method::Approve>(
                                &connection,
                                &protocol::ApproveParams { id, scope, effect },
                            ),
                        )
                        .await
                    }
                    Picked::Withdraw(id) => {
                        bounded(
                            CALL_TIMEOUT,
                            remote::call::<protocol::method::WithdrawApproval>(
                                &connection,
                                &protocol::GrantParams { id },
                            ),
                        )
                        .await
                    }
                };
                let ok = matches!(&done, Ok(outcome) if outcome.ok);
                let (level, text) = outcome_notice(done, "/approve");
                if ok {
                    granted = Some(text.clone());
                } else {
                    refused = Some(text.clone());
                }
                say.notice(level, text);
            }
            match bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::Blocked>(&connection, &protocol::Empty {}),
            )
            .await
            {
                Ok(list) => {
                    let picker = crate::ui::approve_picker(&list.calls, crate::pricing::unix_now());
                    let _ = say.ui_tx.send(Msg::ShowPicker { picker, refresh });
                    // A refusal (a statement that says only "anything")
                    // shows on the refreshed picker too.
                    if let Some(why) = refused {
                        let _ = say.ui_tx.send(Msg::PickerRefused {
                            kind: crate::ui::PickerKind::Approve,
                            why,
                        });
                    }
                    say.picker_done(crate::ui::PickerKind::Approve, granted);
                }
                Err(err) => say.notice(
                    NoticeLevel::Warn,
                    format!("cannot list the blocked calls: {err}"),
                ),
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
    /// Stop the helper `path` of this client's agent (`rkat/stop_helper`):
    /// a refusal is a notice; the ending comes as the daemon's
    /// `rkat/helper_ended`.
    pub(super) fn stop_helper(&self, path: Vec<String>) {
        let say = self.say();
        let Some(connection) = self.link_or_say("the helper was not stopped") else {
            return;
        };
        tokio::spawn(async move {
            let named = path.join(" › ");
            let params = protocol::StopHelperParams { path };
            let stopped = bounded(
                CALL_TIMEOUT,
                remote::call::<protocol::method::StopHelper>(&connection, &params),
            )
            .await;
            match stopped {
                Ok(_) => say.notice(NoticeLevel::Info, format!("stopping helper {named}…")),
                Err(err) => say.notice(
                    NoticeLevel::Warn,
                    format!("helper {named} was not stopped: {err}"),
                ),
            }
        });
    }

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
            let words = match retired {
                Ok(retired) => {
                    let words = retired_words(&retired);
                    say.notice(NoticeLevel::Info, words.clone());
                    words
                }
                Err(err) => {
                    let why = format!("{name} was not retired: {err}");
                    say.notice(NoticeLevel::Warn, why.clone());
                    let _ = say.ui_tx.send(Msg::AgentPickerRefused(why));
                    return;
                }
            };
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
            say.picker_done(crate::ui::PickerKind::Agents, Some(words));
        });
    }

    /// Shift+Tab, `/mode`, `/auto resume`: switch the daemon's mode.
    pub(super) fn set_mode(&mut self, mode: crate::approval::mode::PermissionMode) {
        let params = SetModeParams { mode, user: false };
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

    /// `/compact prepare` alone: what is set and its state, as `/compact
    /// status` has it, then the forms that change it.
    pub(super) fn compact_prepare_show(&self) {
        self.ask::<protocol::method::CompactStatus>(
            protocol::Empty {},
            "/compact prepare needs it",
            |answer| match answer {
                Ok(outcome) => vec![
                    (NoticeLevel::Info, outcome.text),
                    (
                        NoticeLevel::Info,
                        crate::ui::commands::PREPARE_USAGE.to_string(),
                    ),
                ],
                Err(err) => vec![(
                    NoticeLevel::Error,
                    format!("/compact prepare failed: {err}"),
                )],
            },
        );
    }

    /// `/auto check`: say how many cases go to the classifier and that
    /// they cost, ask the daemon to send them, and show what came out
    /// ([`crate::client::check`]). Its progress comes as pushes.
    pub(super) fn classifier_check(&self, selection: crate::approval::corpus::Selection) {
        let cases = match crate::approval::corpus::select(&selection) {
            Ok(cases) => cases.len(),
            Err(err) => {
                self.send(Msg::Notice {
                    level: NoticeLevel::Error,
                    text: format!("/auto check: {err}"),
                });
                return;
            }
        };
        let Some(connection) = self.link_or_say("/auto check needs it") else {
            return;
        };
        self.send(Msg::Notice {
            level: NoticeLevel::Info,
            text: crate::client::check::intro(cases),
        });
        let say = self.say();
        tokio::spawn(async move {
            let answer = tokio::time::timeout(
                crate::client::check::CALL_BOUND,
                remote::call::<protocol::method::ClassifierCheck>(&connection, &selection),
            )
            .await
            .unwrap_or(Err(ClientError::Timeout(
                "waiting for the classifier check",
            )));
            let (reading, text) = crate::client::check::outcome(&answer);
            let level = match reading {
                Some(crate::approval::corpus::Reading::Within) => NoticeLevel::Info,
                Some(_) => NoticeLevel::Warn,
                None => NoticeLevel::Error,
            };
            say.notice(level, text);
        });
    }

    /// The UI answered an approval dialog: decide its decision.
    pub(super) fn respond_escalation(&mut self, id: EscalationId, answer: &EscalationAnswer) {
        if let Some((decision, digest, helper)) = self.approvals.answer_decision(id) {
            self.decide(id, &decision, &digest, answer, helper);
        }
    }

    /// The UI answered the dialog `id` of `decision`: decide it. A decision
    /// another client (or the daemon, when the engine gave up on it)
    /// settled first is refused by the engine (`not_open`) and the answer
    /// is dropped; "deny and stop" of the agent's own call also interrupts
    /// the turn, of a `helper`'s call it stops that helper only (the
    /// daemon does that, from the answer).
    pub(super) fn decide(
        &mut self,
        id: EscalationId,
        decision: &str,
        digest: &str,
        answer: &EscalationAnswer,
        helper: bool,
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
        if matches!(answer, EscalationAnswer::Stop) && !helper {
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

    /// Ctrl+T: the daemon records the view as the workspace's `[ui]
    /// checklist` (`rkat/set_setting`); only what went wrong is shown.
    pub(super) fn set_checklist_view(&self, view: ChecklistView) {
        const UNSAVED: &str = "the checklist view applies to this run but was not saved";
        let value = match view {
            ChecklistView::Shown => "shown",
            ChecklistView::Hidden => "hidden",
        };
        self.ask::<protocol::method::SetSetting>(
            protocol::SetSettingParams {
                key: "ui.checklist".to_string(),
                value: Some(value.to_string()),
                user: false,
            },
            UNSAVED,
            |saved| match saved {
                Ok(outcome) if outcome.ok => Vec::new(),
                Ok(outcome) => vec![(NoticeLevel::Warn, format!("{UNSAVED}: {}", outcome.text))],
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
    lines.push(match &report.classifier_model {
        Some(model) => format!("classifier: {model} (/auto stats shows its numbers)"),
        None => "classifier: none configured (reviewed actions ask you; [auto] classifier_model sets one)".to_string(),
    });
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
pub(super) fn rule_picker(
    rules: &[crate::approval::desk::RuleRow],
    grants: &[crate::approval::grants::GrantRow],
    statements: &[crate::approval::statements::StatementRow],
) -> crate::ui::ListPicker {
    use crate::approval::grants::GrantOrigin;
    use crate::approval::rules::RuleOrigin;
    use crate::approval::statements::StatementOrigin;
    use crate::ui::{ListPicker, PickAct, PickKey, PickRow, PickerKind};
    use ratatui::crossterm::event::KeyCode;
    let grant_rows = grants.iter().map(|grant| {
        let what = grant.target.describe();
        let key = if grant.removable {
            PickKey::confirmed(
                KeyCode::Char('d'),
                PickAct::RemoveGrant(grant.id),
                format!("Remove the session grant of {what}?"),
            )
        } else {
            PickKey::refused(
                KeyCode::Char('d'),
                match grant.origin {
                    GrantOrigin::Default => "the sandbox grants it by default",
                    GrantOrigin::Flag => "it comes from the daemon's command line",
                    GrantOrigin::Always => "it is in the realm's tui.toml ([confinement] extra_write): edit that file to drop it",
                    GrantOrigin::Config | GrantOrigin::Session => "it is in a tui.toml ([confinement]): edit that file to drop it",
                },
            )
        };
        PickRow::new(
            format!("grant {what} · {}", grant.origin.word()),
            vec![key],
        )
        .keyed(format!("grant {}", grant.id))
    });
    let statement_rows = statements.iter().map(|statement| {
        let key = match statement.origin {
            StatementOrigin::Config => PickKey::refused(
                KeyCode::Char('d'),
                "it is in your own tui.toml ([auto] allow): edit that file to drop it",
            ),
            StatementOrigin::Session => PickKey::confirmed(
                KeyCode::Char('d'),
                PickAct::RemoveStatement(statement.id),
                format!("Stop allowing \"{}\" for this session?", statement.text),
            ),
            StatementOrigin::Always => PickKey::confirmed(
                KeyCode::Char('d'),
                PickAct::RemoveStatement(statement.id),
                format!(
                    "Stop allowing \"{}\"? It leaves the realm's tui.toml.",
                    statement.text
                ),
            ),
        };
        PickRow::new(
            format!(
                "allow kind \"{}\" · {}",
                statement.text,
                statement.origin.word()
            ),
            vec![key],
        )
        .noted("a statement: judged by the classifier, read narrowly")
        .keyed(format!("statement {}", statement.id))
    });
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
                    "It leaves the workspace's rules for good".to_string()
                }
                RuleOrigin::Global => "It is written in your own tui.toml: it is removed from that file, for every workspace".to_string(),
                RuleOrigin::Project => "It is written in the project's .rkat/tui.toml, so it is removed until the daemon restarts (edit that file to drop it for good)".to_string(),
                RuleOrigin::Cli => "It comes from the daemon's command line, so it is removed until the daemon restarts".to_string(),
            };
            let n = usize::try_from(rule.n).unwrap_or(usize::MAX);
            PickRow::new(
                format!("{}. {what} · {}", rule.n, rule.origin.as_str()),
                vec![PickKey::confirmed(
                    KeyCode::Char('d'),
                    PickAct::RemoveRule(n),
                    format!("Remove rule {}? {lasts}.", rule.n),
                )],
            )
            // Rules are numbered again after a removal: what the rule
            // says is what it is.
            .keyed(format!("rule {what} · {}", rule.origin.as_str()))
        })
        .chain(grant_rows)
        .chain(statement_rows)
        .collect();
    ListPicker::new(
        PickerKind::Permissions,
        rows,
        "no approval rules, grants or statements yet",
        "↑/↓ move · d remove · /permissions add allow|deny <tool> [pattern] adds a rule · /approve grants · Esc close",
    )
}

/// One `rkat/set_setting`: its confirmation (or refusal) and its warning
/// as notices; whether it was written.
async fn set_setting(
    connection: &Connection,
    say: &Say,
    key: String,
    value: Option<String>,
    user: bool,
) -> bool {
    let answer = bounded(
        CALL_TIMEOUT,
        remote::call::<protocol::method::SetSetting>(
            connection,
            &protocol::SetSettingParams { key, value, user },
        ),
    )
    .await;
    match answer {
        Ok(outcome) => {
            say.notice(
                if outcome.ok {
                    NoticeLevel::Info
                } else {
                    NoticeLevel::Error
                },
                outcome.text,
            );
            if let Some(warning) = outcome.warning {
                say.notice(NoticeLevel::Warn, warning);
            }
            outcome.ok
        }
        Err(err) => {
            say.notice(
                NoticeLevel::Error,
                format!("the setting was not changed: {err}"),
            );
            false
        }
    }
}

/// The `/settings` picker: one row per setting whose key holds `filter`
/// (the key, the value in force and its layer), its note what each layer
/// holds, when a change applies and what it does; then the structured
/// sections, each with the commands that change it. Enter edits the
/// workspace's value, `u` the user's, `d` (then `y`) removes the value at
/// the layer it comes from when that is the user's or the workspace's.
pub(super) fn settings_picker(
    list: &protocol::SettingsList,
    filter: Option<&str>,
) -> crate::ui::ListPicker {
    use crate::config::registry::Level;
    use crate::ui::{Field, FieldFor, ListPicker, PickAct, PickKey, PickRow, PickerKind};
    use ratatui::crossterm::event::KeyCode;
    let wanted = |key: &str| filter.is_none_or(|filter| key.contains(filter));
    let held = |row: &protocol::SettingRow, level: Level| {
        row.levels
            .iter()
            .find(|(at, _)| *at == level)
            .map(|(_, value)| value.clone())
    };
    let mut rows: Vec<PickRow> = list
        .rows
        .iter()
        .filter(|row| wanted(&row.key))
        .map(|row| {
            let levels = [
                Level::User,
                Level::Workspace,
                Level::CommandLine,
                Level::Project,
            ]
            .into_iter()
            .map(|level| {
                format!(
                    "{} {}",
                    level.label(),
                    held(row, level).unwrap_or_else(|| "—".to_string())
                )
            })
            .collect::<Vec<_>>()
            .join(" · ");
            let note = format!(
                "{levels} · default {}; a change applies {}; {} (takes {}; a project {})",
                row.default,
                row.applies.describe(),
                row.about,
                row.takes,
                row.project
            );
            let remove = match row.source {
                level @ (Level::User | Level::Workspace) => PickKey::confirmed(
                    KeyCode::Char('d'),
                    PickAct::SetSetting {
                        key: row.key.clone(),
                        value: None,
                        user: level == Level::User,
                        filter: filter.map(str::to_string),
                    },
                    format!(
                        "Remove {} from {}? The next layer's value applies.",
                        row.key,
                        if level == Level::User {
                            "your tui.toml"
                        } else {
                            "this workspace's tui.toml"
                        }
                    ),
                ),
                Level::Default => PickKey::refused(
                    KeyCode::Char('d'),
                    "no layer sets it: the default is in force",
                ),
                level => PickKey::refused(
                    KeyCode::Char('d'),
                    format!("{} sets it, which rkat-tui does not write", level.name()),
                ),
            };
            let edit = |code, user: bool| {
                PickKey::field(
                    code,
                    Field {
                        what: FieldFor::Setting {
                            key: row.key.clone(),
                            user,
                            filter: filter.map(str::to_string),
                        },
                        text: held(row, if user { Level::User } else { Level::Workspace })
                            .unwrap_or_default(),
                    },
                )
            };
            PickRow::new(
                format!("{}  {}  ({})", row.key, row.value, row.source.label()),
                vec![
                    edit(KeyCode::Enter, false),
                    edit(KeyCode::Char('u'), true),
                    remove,
                ],
            )
            .noted(note)
            .keyed(row.key.clone())
        })
        .collect();
    rows.extend(
        list.sections
            .iter()
            .filter(|section| wanted(&section.key))
            .map(|section| {
                let why = format!("a section: change it with {}", section.commands);
                PickRow::new(
                    format!("{}  (section)", section.key),
                    vec![
                        PickKey::refused(KeyCode::Enter, why.clone()),
                        PickKey::refused(KeyCode::Char('u'), why.clone()),
                        PickKey::refused(KeyCode::Char('d'), why),
                    ],
                )
                .noted(format!(
                    "{} (a change applies {}; a project file {}; flags: {})",
                    section.about, section.applies, section.project, section.flags
                ))
                .keyed(format!("[{}]", section.key))
            }),
    );
    ListPicker::new(
        PickerKind::Settings,
        rows,
        "no setting's key holds that",
        format!(
            "↑/↓ move · Enter edit for this workspace · u edit in your tui.toml · d remove · Esc close · user: {} · workspace: {}",
            list.user_file.display(),
            list.workspace_file.display()
        ),
    )
}

/// What the `/approve` picker's act asks of the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Picked {
    /// Approve blocked call `id` for the scope (with a statement: allow
    /// that kind of action instead).
    Approve(u64, crate::approval::grants::GrantScope, Option<String>),
    /// Withdraw the approval once of call `id`.
    Withdraw(u64),
}

/// What a daemon command's answer says, as a notice.
fn outcome_notice(done: Result<RuleOutcome, CallFailure>, what: &str) -> (NoticeLevel, String) {
    match done {
        Ok(outcome) if outcome.ok => (NoticeLevel::Info, outcome.text),
        Ok(outcome) => (NoticeLevel::Warn, outcome.text),
        Err(err) => (NoticeLevel::Error, format!("{what} failed: {err}")),
    }
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
                        format!("Delete schedule {}? It never fires again.", row.name),
                    ),
                ],
            )
            .keyed(row.id.clone())
        })
        .collect();
    ListPicker::new(
        PickerKind::Schedules,
        rows,
        "this agent has no schedules (the agent sets them with meerkat_schedule_create)",
        "↑/↓ move · Enter or p pause/resume · d delete · Esc close",
    )
}
