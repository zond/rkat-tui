//! Turns and the messages typed during them: submitting, steering,
//! holding, recalling, the turn's end and what goes next, `/rewind`, and
//! the context gauge.

use std::time::Instant;

use super::{
    App, Block, BlockId, COMPACT_REQUESTED, Command, CompactFailure, Effect, NoticeLevel,
    OpenHelperView, Pending, PendingId, PendingState, SteerFailure, TurnFailure, TurnOutcome,
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
    /// holds, one the runtime holds while the turn runs (recalled through
    /// the runner), or a recall already on its way.
    pub fn can_take_back(&self) -> bool {
        self.has_recallable() || self.recalling || (self.running && self.has_admitted_recallable())
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
                if self.running {
                    self.enqueue(prompt, fx);
                } else if self.shell_busy() {
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
            let plural = if attached.len() == 1 { "" } else { "s" };
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
        self.run_first_block = self.transcript.len();
        self.streamed_text = false;
        if user_block {
            self.transcript.push(Block::User {
                text: prompt.clone(),
            });
        }
        self.running = true;
        self.status.running_since = Some(now);
        self.scroll.bottom();
        fx.push(Effect::StartTurn(prompt));
    }

    /// [`Msg::RemoteTurn`](crate::ui::Msg::RemoteTurn): a turn the daemon runs without this UI having
    /// started it; the UI is busy until its [`Msg::TurnFinished`](crate::ui::Msg::TurnFinished).
    pub(super) fn remote_turn(&mut self, prompt: Option<String>, now: Instant) {
        if self.running {
            return;
        }
        self.transcript.retire_finished_helpers();
        self.run_first_block = self.transcript.len();
        self.streamed_text = false;
        if let Some(text) = prompt {
            // A watch event the daemon announced before its run started
            // is shown labelled already.
            let announced = matches!(
                self.transcript.blocks().last(),
                Some(Block::Triggered { text: shown, .. }) if *shown == text
            );
            if !announced {
                self.transcript.push(Block::User { text });
            }
        }
        self.running = true;
        self.status.running_since = Some(now);
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
    pub(super) fn compact_requested(&mut self, result: Result<(), CompactFailure>) {
        self.compacting = false;
        match result {
            Ok(()) => {
                self.transcript
                    .push_notice(NoticeLevel::Info, COMPACT_REQUESTED);
            }
            Err(err) => {
                self.transcript
                    .push_notice(NoticeLevel::Error, format!("/compact failed: {err}"));
            }
        }
    }

    /// Remove transcript blocks (highest index first, so the remaining ids
    /// stay valid) and fix up every id the app holds.
    pub(super) fn drop_blocks(&mut self, blocks: impl IntoIterator<Item = BlockId>) {
        let mut blocks: Vec<BlockId> = blocks.into_iter().collect();
        blocks.sort_by_key(|b| std::cmp::Reverse(b.0));
        for block in blocks {
            if self.transcript.remove_block(block).is_some() {
                self.pending.block_removed(block);
                let shift = |id: BlockId| match id {
                    f if f == block => None,
                    f if f.0 > block.0 => Some(BlockId(f.0 - 1)),
                    f => Some(f),
                };
                self.panel_selected = self.panel_selected.and_then(shift);
                self.helper_view = self.helper_view.take().and_then(|view| {
                    shift(view.header).map(|header| OpenHelperView { header, ..view })
                });
                if let Some(run) = &mut self.shell_run
                    && let Some(moved) = shift(run.block)
                {
                    run.block = moved;
                }
                if self.run_first_block > block.0 {
                    self.run_first_block -= 1;
                }
            }
        }
    }

    /// Take pending items out of the queue and the transcript, returning their
    /// texts oldest first.
    pub(super) fn withdraw(&mut self, ids: &[PendingId]) -> Vec<String> {
        let taken = self.pending.take_where(|p| ids.contains(&p.id));
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
            Err(SteerFailure::NotRunning) if self.running || self.shell_busy() => {
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

    /// `/rewind` or Esc Esc on an empty input: ask the runner (which says
    /// why the daemon cannot rewind) when the session is idle with nothing
    /// queued, else say what is in the way.
    pub(super) fn request_rewind(&mut self, fx: &mut Vec<Effect>) {
        if self.idle_or_notice() {
            fx.push(Effect::RequestRewindMenu);
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
        let plural = |n: usize| if n == 1 { "" } else { "s" };
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

    /// ↑ on an empty editor with queued messages: while a turn runs and the
    /// runtime holds messages no recall has found with the agent, ask the
    /// runner to take them back (the answer also brings back the held ones,
    /// so the editor keeps their order); otherwise pull back the newest held
    /// one. While a recall is on its way, wait for it.
    pub(super) fn take_back_queued(&mut self, fx: &mut Vec<Effect>) {
        if self.recalling {
            return;
        }
        if self.running && self.has_admitted_recallable() {
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
        outcome: TurnOutcome,
        now: Instant,
        fx: &mut Vec<Effect>,
    ) {
        let TurnOutcome {
            result,
            undelivered,
        } = outcome;
        self.running = false;
        self.recalling = false;
        self.reached.clear();
        self.status.running_since = None;
        self.status.classifying = false;
        let interrupted = matches!(&result, Err(TurnFailure::Interrupted));
        let succeeded = result.is_ok();
        let answered = matches!(&result, Ok(summary) if !summary.text.trim().is_empty());
        match result {
            Ok(summary) => {
                if !self.streamed_text && !summary.text.is_empty() {
                    self.transcript.push(Block::Assistant { md: summary.text });
                }
            }
            Err(err @ TurnFailure::Failed(_)) => {
                self.transcript
                    .push_notice(NoticeLevel::Error, format!("turn failed: {err}"));
            }
            Err(TurnFailure::Interrupted) => {
                self.transcript
                    .push_notice(NoticeLevel::Warn, "turn interrupted");
            }
        }
        self.streamed_text = false;
        let unsent = self.pending.take_unsent(&undelivered);
        // `!` commands typed during the turn run first; what would start the
        // next turn waits for them (a failed turn's messages still go back
        // to the editor).
        if self.shell_busy() && (self.send_next.is_some() || succeeded || interrupted) {
            for item in unsent {
                self.pending.hold_again(&mut self.transcript, item);
            }
        } else {
            self.next_after_turn(unsent, interrupted, succeeded, now, fx);
        }
        if self.shell_run.is_none() && !self.running {
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
