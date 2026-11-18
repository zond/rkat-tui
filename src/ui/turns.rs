//! Turns and the messages typed during them: submitting, steering,
//! holding, recalling, the turn's end and what goes next, `/rewind`, and
//! the context gauge.

use std::time::Instant;

use super::{
    App, Block, BlockId, COMPACT_REQUESTED, COMPACT_RUNNING, Command, CompactFailure, Effect,
    NoticeLevel, Pending, PendingId, PendingState, SteerFailure, TurnFailure, TurnSummary,
    commands, quote_short,
};

impl App {
    /// Whether a turn is running.
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Whether a message the UI holds can be recalled without the runner.
    pub fn has_recallable(&self) -> bool {
        self.pending.newest_recallable().is_some()
    }

    /// Whether ↑ on an empty editor takes queued messages back: one the UI
    /// holds, one the runtime holds and the agent has not read (recalled
    /// through the runner), or a recall already on its way.
    pub fn can_take_back(&self) -> bool {
        self.has_recallable() || self.recalling || self.has_admitted_recallable()
    }

    pub(super) fn submit(&mut self, now: Instant, fx: &mut Vec<Effect>) {
        if self.input.text().trim().is_empty() {
            return;
        }
        let raw = self.input.take();
        self.submitted.push(raw.clone());
        match commands::parse(&raw) {
            Some(Ok(cmd)) => self.run_command(cmd, now, fx),
            Some(Err(err)) => {
                self.transcript
                    .push_notice(NoticeLevel::Warn, err.to_string());
            }
            None => {
                if let Some(command) = commands::parse_shell(&raw) {
                    let command = command.to_string();
                    self.shell_input(command, raw, now, fx);
                    return;
                }
                if let Some(text) = commands::parse_remember(&raw) {
                    self.run_command(Command::Remember(text), now, fx);
                    return;
                }
                let prompt = match raw.trim_start().strip_prefix("//") {
                    Some(rest) => format!("/{rest}"),
                    None => commands::unescape_hash(&raw)
                        .or_else(|| commands::unescape_bang(&raw))
                        .unwrap_or(raw),
                };
                if self.has_held_command() {
                    // A command waits for the turn's end: the message goes
                    // after it.
                    self.hold(prompt);
                } else if self.running {
                    self.enqueue(prompt, fx);
                } else if self.shell_busy() || self.compaction_run {
                    self.hold(prompt);
                } else {
                    self.start_turn(prompt, now, fx);
                }
            }
        }
    }

    /// [`Msg::Mentions`](crate::ui::Msg::Mentions): a dim `+ N file(s)` line naming what a prompt's
    /// `@` mentions attached as the runner sent it, and a warning per
    /// sealed path it mentioned, which was never read.
    pub(super) fn note_mentions(&mut self, attached: &[String], sealed: &[String]) {
        if !attached.is_empty() {
            let plural = crate::text::plural(attached.len());
            self.transcript.push_notice(
                NoticeLevel::Info,
                format!(
                    "+ {} file{plural} ({})",
                    attached.len(),
                    attached.join(", ")
                ),
            );
        }
        for path in sealed {
            self.transcript.push_notice(
                NoticeLevel::Warn,
                format!("@{path} is sealed: not attached"),
            );
        }
    }

    /// Start a turn with user content, shown as a user block.
    pub(super) fn start_turn(&mut self, prompt: String, now: Instant, fx: &mut Vec<Effect>) {
        self.begin_turn(prompt, true, now, fx);
    }

    pub(super) fn begin_turn(
        &mut self,
        prompt: String,
        user_block: bool,
        now: Instant,
        fx: &mut Vec<Effect>,
    ) {
        // Helpers that finished drop off the helper panel.
        self.transcript.retire_finished_helpers();
        self.run_first_block = self.transcript.next_id();
        self.failures = super::failures::RunFailures::start(None, false);
        self.woken_by = None;
        self.streamed_text = false;
        if user_block {
            self.transcript.push(Block::User {
                text: prompt.clone(),
            });
        }
        self.running = true;
        self.status.running_since = Some(now);
        self.status.heard_at = Some(now);
        self.scroll.bottom();
        fx.push(Effect::StartTurn(prompt));
    }

    /// [`Msg::RemoteTurn`](crate::ui::Msg::RemoteTurn): a turn the daemon runs without this UI having
    /// started it; the UI is busy until its [`Msg::TurnFinished`](crate::ui::Msg::TurnFinished).
    /// While a turn runs it is another client's steer that joined it as a
    /// follow-up run: its text is shown.
    pub(super) fn remote_turn(&mut self, prompt: Option<String>, now: Instant) {
        // A detached job's outcome (a fork's report) starts a run of its
        // own: shown compact, not as the raw notice.
        let (prompt, outcome) = match prompt
            .as_deref()
            .and_then(crate::ui::transcript::outcome::JobOutcome::from_text)
        {
            Some(outcome) => (None, Some(outcome)),
            None => (prompt, None),
        };
        if let Some(outcome) = &outcome {
            self.transcript.push_outcome(outcome);
            self.scroll.bottom();
        }
        if self.running {
            if let Some(text) = prompt {
                // A follow-up run of the turn: a helper's ending note is
                // its folded line, shown once with its announcement.
                self.transcript.remote_input(text);
                self.scroll.bottom();
            }
            return;
        }
        self.transcript.retire_finished_helpers();
        self.run_first_block = self.transcript.next_id();
        self.failures = super::failures::RunFailures::start(self.woken_by.take(), true);
        self.streamed_text = false;
        if let Some(text) = prompt {
            // A run the daemon announced before it started is shown
            // labelled already; a helper's ending note is its folded line.
            self.transcript.remote_input(text);
        }
        self.running = true;
        self.status.running_since = Some(now);
        self.status.heard_at = Some(now);
        self.scroll.bottom();
    }

    /// Hold a message until the running `!` commands end.
    pub(super) fn hold(&mut self, text: String) {
        self.pending
            .add(&mut self.transcript, text, PendingState::Deferred);
        self.scroll.bottom();
    }

    /// Enter while a turn runs: queue the message and steer it into the turn.
    pub(super) fn enqueue(&mut self, text: String, fx: &mut Vec<Effect>) {
        let id = self
            .pending
            .add(&mut self.transcript, text.clone(), PendingState::Queued);
        self.scroll.bottom();
        fx.push(Effect::Steer { id, text });
    }

    /// `/compact now` answered: report the request (or why it failed).
    pub(super) fn compact_requested(
        &mut self,
        result: Result<(), CompactFailure>,
        now: Instant,
        fx: &mut Vec<Effect>,
    ) {
        // Session commands keep waiting while the compaction's run goes.
        self.compacting = self.compaction_run;
        match result {
            Ok(()) if self.compaction_run => {
                self.transcript
                    .push_notice(NoticeLevel::Info, COMPACT_RUNNING);
            }
            Ok(()) => {
                self.transcript
                    .push_notice(NoticeLevel::Info, COMPACT_REQUESTED);
            }
            Err(err) => {
                self.transcript
                    .push_notice(NoticeLevel::Error, format!("/compact failed: {err}"));
            }
        }
        // Commands held behind the request go once nothing runs.
        if !self.compacting && !self.running {
            self.run_held(now, fx);
        }
    }

    /// [`Msg::CompactionRun`](crate::ui::Msg::CompactionRun): the run that lets the idle agent
    /// compact began or ended; at its end what the user sent meanwhile goes.
    pub(super) fn compaction_run(&mut self, running: bool, now: Instant, fx: &mut Vec<Effect>) {
        self.compaction_run = running;
        if running {
            self.compacting = true;
            return;
        }
        self.compacting = false;
        self.status.compacting_since = None;
        if self.agent_busy() {
            return;
        }
        if self.has_held_command() && self.send_next.is_none() {
            self.run_held(now, fx);
            return;
        }
        let held = self
            .pending
            .take_where(|p| p.state == PendingState::Deferred);
        if !held.is_empty() || self.send_next.is_some() {
            self.next_after_turn(held, false, true, now, fx);
        }
    }

    /// Remove transcript blocks and forget the ones the app held (every
    /// other id stays valid).
    pub(super) fn drop_blocks(&mut self, blocks: impl IntoIterator<Item = BlockId>) {
        for block in blocks {
            if self.transcript.remove_block(block).is_some() {
                if self.panel_selected == Some(block) {
                    self.panel_selected = None;
                }
                if self
                    .helper_view
                    .as_ref()
                    .is_some_and(|view| view.header == block)
                {
                    self.helper_view = None;
                }
            }
        }
    }

    /// Take pending items out of the queue and the transcript, returning their
    /// texts oldest first.
    pub(super) fn withdraw(&mut self, ids: &[PendingId]) -> Vec<String> {
        let taken = self.pending.take_where(|p| ids.contains(&p.id));
        for item in &taken {
            self.deferred.remove(&item.id);
        }
        self.drop_blocks(taken.iter().map(|p| p.block));
        taken.into_iter().map(|p| p.text).collect()
    }

    /// Put recalled texts (oldest first) before the editor contents.
    pub(super) fn prepend_to_editor(&mut self, texts: Vec<String>) {
        if texts.is_empty() {
            return;
        }
        let mut text = texts.join("\n");
        let current = self.input.text();
        if !text.is_empty() && !current.is_empty() {
            text.push('\n');
        }
        text.push_str(&current);
        self.input.clear();
        self.input.set_text(&text);
    }

    pub(super) fn steer_ack(
        &mut self,
        id: PendingId,
        result: Result<(), SteerFailure>,
        now: Instant,
        fx: &mut Vec<Effect>,
    ) {
        let Some(text) = self.pending.get(id).map(|p| p.text.clone()) else {
            return;
        };
        match result {
            Ok(()) => {
                self.pending
                    .set_state(&mut self.transcript, id, PendingState::Admitted);
            }
            // The turn ended before the message reached the runtime: hold it
            // for the turn's end (the runner's `TurnFinished` follows).
            Err(SteerFailure::NotRunning) if self.turn_or_shell() => {
                self.pending
                    .set_state(&mut self.transcript, id, PendingState::Deferred);
            }
            Err(SteerFailure::NotRunning) => {
                self.pending.send_now(&mut self.transcript, id);
                self.begin_turn(text, false, now, fx);
            }
            Err(cause @ SteerFailure::Rejected(_)) => {
                let texts = self.withdraw(&[id]);
                self.prepend_to_editor(texts);
                self.transcript.push_notice(
                    NoticeLevel::Warn,
                    format!(
                        "couldn't send {}: {cause}; it is back in the editor",
                        quote_short(&text)
                    ),
                );
            }
        }
    }

    /// Messages handed to the runner (admitted, or not acknowledged yet)
    /// that no recall has found with the agent.
    pub(super) fn has_admitted_recallable(&self) -> bool {
        self.pending.items().iter().any(|p| {
            matches!(p.state, PendingState::Queued | PendingState::Admitted)
                && !self.reached.contains(&p.id)
        })
    }

    /// [`Msg::Recalled`](crate::ui::Msg::Recalled): the recalled messages and every held one go back
    /// to the editor (oldest first, before the draft); the admitted ones
    /// left already reached the agent.
    pub(super) fn recalled(&mut self, recalled: &[PendingId], errors: &[String]) {
        self.recalling = false;
        let back: Vec<PendingId> = self
            .pending
            .items()
            .iter()
            .filter(|p| recalled.contains(&p.id) || p.state.recallable())
            .map(|p| p.id)
            .collect();
        let texts = self.withdraw(&back);
        let returned = texts.len();
        self.prepend_to_editor(texts);
        let left: Vec<PendingId> = self
            .pending
            .items()
            .iter()
            .filter(|p| matches!(p.state, PendingState::Queued | PendingState::Admitted))
            .map(|p| p.id)
            .collect();
        let reached = left.len();
        self.reached.extend(left);
        if !errors.is_empty() {
            self.transcript.push_notice(
                NoticeLevel::Warn,
                format!("could not recall: {}", errors.join("; ")),
            );
        }
        let plural = crate::text::plural::<usize>;
        let text = match (returned, reached) {
            (0, 0) => return,
            (0, _) if self.running => {
                "your messages already reached the agent; Esc interrupts".to_string()
            }
            (0, _) => return,
            (n, 0) => format!("{n} message{} returned to the editor", plural(n)),
            (n, m) if self.running => format!(
                "{n} message{} returned to the editor; {m} already reached the agent (Esc interrupts)",
                plural(n)
            ),
            (n, _) => format!("{n} message{} returned to the editor", plural(n)),
        };
        self.transcript.push_notice(NoticeLevel::Info, text);
        self.scroll.bottom();
    }

    /// [`Msg::NotDelivered`](crate::ui::Msg::NotDelivered): the runtime
    /// ended the message `id` without writing it into a run; it returns to
    /// the editor (before the draft), as a recalled one does, with one
    /// notice saying so and why.
    pub(super) fn not_delivered(&mut self, id: PendingId, why: Option<&str>) {
        let Some(text) = self.pending.get(id).map(|p| p.text.clone()) else {
            return;
        };
        self.reached.retain(|reached| *reached != id);
        let texts = self.withdraw(&[id]);
        self.prepend_to_editor(texts);
        let why = why.map(|why| format!(" ({why})")).unwrap_or_default();
        self.transcript.push_notice(
            NoticeLevel::Warn,
            format!(
                "the agent did not take {}{why}; it is back in the editor",
                quote_short(&text)
            ),
        );
        self.scroll.bottom();
    }

    /// [`Msg::Returned`](crate::ui::Msg::Returned): the interrupt took
    /// these messages back. During the turn they leave the queue and wait
    /// for its end, which puts them in the editor with the others and
    /// counts them in its one line; after it they go to the editor at once
    /// with a line of their own.
    pub(super) fn returned(&mut self, ids: &[PendingId]) {
        let mut taken: Vec<(PendingId, String)> = Vec::new();
        for id in ids {
            if let Some(text) = self.pending.get(*id).map(|p| p.text.clone()) {
                self.withdraw(&[*id]);
                taken.push((*id, text));
            }
        }
        self.reached.retain(|reached| !ids.contains(reached));
        if taken.is_empty() {
            return;
        }
        if self.running {
            self.returned.extend(taken);
            return;
        }
        taken.sort_by_key(|(id, _)| *id);
        let count = taken.len();
        self.prepend_to_editor(taken.into_iter().map(|(_, text)| text).collect());
        if let Some(text) = back_text(count) {
            self.transcript.push_notice(NoticeLevel::Info, text);
        }
        self.scroll.bottom();
    }

    /// ↑ on an empty editor with queued messages: while the runtime holds
    /// messages no recall has found with the agent (during a turn, or
    /// after one that ended before the agent read them), ask the runner to
    /// take them back (the answer also brings back the held ones, so the
    /// editor keeps their order); otherwise pull back the newest held one.
    /// While a recall is on its way, wait for it.
    pub(super) fn take_back_queued(&mut self, fx: &mut Vec<Effect>) {
        if self.recalling {
            return;
        }
        if self.has_admitted_recallable() {
            self.recalling = true;
            fx.push(Effect::RecallQueued);
        } else {
            self.recall_newest();
        }
    }

    /// ↑ on an empty editor with held messages: pull back the newest one.
    pub(super) fn recall_newest(&mut self) {
        let Some(id) = self.pending.newest_recallable().map(|p| p.id) else {
            return;
        };
        let texts = self.withdraw(&[id]);
        self.prepend_to_editor(texts);
    }

    pub(super) fn turn_finished(
        &mut self,
        result: Result<TurnSummary, TurnFailure>,
        now: Instant,
        fx: &mut Vec<Effect>,
    ) {
        self.running = false;
        self.recalling = false;
        self.reached.clear();
        self.status.running_since = None;
        self.status.model_since = None;
        self.status.classifying = false;
        self.status.classifying_since = None;
        self.status.open_tools.clear();
        // An unconfirmed interrupt goes on as an interrupted turn does (the
        // queue goes next; it waits behind a run that is still going).
        let interrupted = matches!(
            &result,
            Err(TurnFailure::Interrupted | TurnFailure::Unconfirmed)
        );
        let succeeded = result.is_ok();
        let answered = matches!(&result, Ok(summary) if !summary.text.trim().is_empty());
        // An interrupt returns every message the agent did not read to the
        // editor, in the order typed: the ones it took back from the agent
        // and the ones the UI still held; nothing typed during the
        // interrupted turn starts a run by itself. Ctrl+Enter's text
        // (`send_next`) is sent, with them, as the user asked.
        let mut unsent = self.pending.take_unsent();
        let returned = std::mem::take(&mut self.returned);
        let mut back = 0;
        if interrupted && self.send_next.is_none() {
            self.drop_blocks(unsent.iter().map(|p| p.block));
            let mut texts: Vec<(PendingId, String)> = returned;
            texts.extend(unsent.drain(..).map(|p| (p.id, p.text)));
            texts.sort_by_key(|(id, _)| *id);
            back = texts.len();
            self.prepend_to_editor(texts.into_iter().map(|(_, text)| text).collect());
        } else if !returned.is_empty() {
            // Sent first with Ctrl+Enter's text; else (the run ended on its
            // own as the interrupt came) back in the editor.
            let texts: Vec<String> = returned.into_iter().map(|(_, text)| text).collect();
            back = texts.len();
            if let Some(next) = self.send_next.take() {
                self.send_next = Some(format!("{}\n\n{next}", texts.join("\n\n")));
                back = 0;
            } else {
                self.prepend_to_editor(texts);
            }
        }
        match result {
            Ok(summary) => {
                if !self.streamed_text && !summary.text.is_empty() {
                    self.transcript.push(Block::Assistant { md: summary.text });
                }
            }
            // Said once: the run's end said it already ([`super::failures`]).
            Err(TurnFailure::Failed(_)) if self.failures.said => {}
            Err(err @ TurnFailure::Failed(_)) => {
                self.transcript
                    .push_notice(NoticeLevel::Error, format!("turn failed: {err}"));
            }
            Err(TurnFailure::Interrupted) => {
                self.transcript
                    .push_notice(NoticeLevel::Warn, interrupted_line(back));
            }
            Err(TurnFailure::Unconfirmed) => {
                self.transcript.push_notice(
                    NoticeLevel::Warn,
                    format!(
                        "the daemon did not confirm the interrupt, not even after a hard cancel: the run may still be going and shows here again when it reports; what you send next waits behind it{}",
                        back_text(back).map(|text| format!("; {text}")).unwrap_or_default()
                    ),
                );
            }
        }
        if !interrupted && let Some(text) = back_text(back) {
            self.transcript.push_notice(
                NoticeLevel::Info,
                format!("{text} (the run had ended as you interrupted it)"),
            );
        }
        self.streamed_text = false;
        // `!` commands typed during the turn run first; what would start the
        // next turn waits for them (a failed turn's messages still go back
        // to the editor). Commands held for the turn's end then go, in
        // order with the messages typed after them.
        if self.shell_busy() && (self.send_next.is_some() || succeeded || interrupted) {
            for item in unsent {
                self.pending.hold_again(&mut self.transcript, item);
            }
        } else if self.has_held_command() && self.send_next.is_none() && (succeeded || interrupted)
        {
            for item in unsent {
                self.pending.hold_again(&mut self.transcript, item);
            }
            self.run_held(now, fx);
        } else {
            self.next_after_turn(unsent, interrupted, succeeded, now, fx);
            self.run_held(now, fx);
        }
        if self.shell.running.is_none() && !self.running {
            self.run_next_shell(now, fx);
        }
        self.offer_plan(answered);
        self.check_context();
    }

    /// After a turn: let the context gauge warn (warn mode; in auto mode
    /// the engine compacts on its own).
    pub(super) fn check_context(&mut self) {
        let Some(used) = self.status.context_used else {
            return;
        };
        if let Some(text) = self.gauge.after_turn(used, self.status.context_window) {
            self.transcript.push_notice(NoticeLevel::Warn, text);
        }
    }

    /// Decide what to send after a turn, in priority order: the Ctrl-X Ctrl-S
    /// prompt (with anything unsent), unsent messages back to the editor
    /// after a turn that ended without a result (failed, unknown, pending),
    /// unsent messages as a new turn (after a completed or an interrupted
    /// turn: Esc interrupts and the queue goes next, as in Claude Code).
    pub(super) fn next_after_turn(
        &mut self,
        unsent: Vec<Pending>,
        interrupted: bool,
        succeeded: bool,
        now: Instant,
        fx: &mut Vec<Effect>,
    ) {
        if let Some(next) = self.send_next.take() {
            let mut texts = self.unsent_as_user(unsent);
            self.transcript.push(Block::User { text: next.clone() });
            texts.push(next);
            self.begin_turn(texts.join("\n\n"), false, now, fx);
        } else if !succeeded && !interrupted {
            if !unsent.is_empty() {
                self.transcript.push_notice(
                    NoticeLevel::Warn,
                    format!(
                        "{} undelivered message(s) returned to the editor",
                        unsent.len()
                    ),
                );
            }
            self.drop_blocks(unsent.iter().map(|p| p.block));
            self.prepend_to_editor(unsent.into_iter().map(|p| p.text).collect());
        } else if !unsent.is_empty() {
            self.send_unsent(unsent, now, fx);
        }
    }

    /// Start one turn with unsent messages (joined by blank lines), showing
    /// them as user blocks in place.
    pub(super) fn send_unsent(&mut self, unsent: Vec<Pending>, now: Instant, fx: &mut Vec<Effect>) {
        let texts = self.unsent_as_user(unsent);
        self.begin_turn(texts.join("\n\n"), false, now, fx);
    }

    /// Show unsent messages as user blocks (they are about to be sent) and
    /// return their texts.
    pub(super) fn unsent_as_user(&mut self, unsent: Vec<Pending>) -> Vec<String> {
        unsent
            .into_iter()
            .map(|p| {
                self.transcript.replace_block(
                    p.block,
                    Block::User {
                        text: p.text.clone(),
                    },
                );
                p.text
            })
            .collect()
    }
}

/// The one line an interrupted turn ends with: the agent is idle, and how
/// many typed messages are back in the editor.
#[must_use]
pub fn interrupted_line(back: usize) -> String {
    match back_text(back) {
        Some(text) => format!("turn interrupted; the agent is idle, and {text}"),
        None => "turn interrupted; the agent is idle".to_string(),
    }
}

/// How many of the user's typed messages an interrupt put back in the
/// editor, in words (`None`: none).
fn back_text(back: usize) -> Option<String> {
    match back {
        0 => None,
        1 => Some("your typed message is back in the input".to_string()),
        n => Some(format!(
            "your {n} typed messages are back in the input, in the order you typed them"
        )),
    }
}
