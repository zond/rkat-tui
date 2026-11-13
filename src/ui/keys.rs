//! Keys outside the session's work: the main key map, the completion
//! menu, history search, the editor, dialogs' keys, Esc, Ctrl-C, Ctrl-D
//! and quitting.

use std::time::Instant;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::{
    Action, AgentPick, App, CHORD_WINDOW, Command, DOUBLE_CTRL_C_WINDOW, DOUBLE_ESC_WINDOW,
    Departure, EditAction, EditorClosed, EditorTarget, Effect, HistorySearch, InputMode,
    LocalEffect, Modal, ModalKey, ModelCommand, NoticeLevel, PathIndex, PendingId,
    QUIT_CONFIRM_WINDOW, QuitChoice, QuitDecision, QuitDialog, SearchOutcome, StashOutcome,
    complete, keymap, search,
};

impl App {
    /// The active history search and its corpus, if searching.
    pub fn search(&self) -> Option<(&HistorySearch, &[String])> {
        match &self.mode {
            InputMode::Search(s) => Some((s, &self.search_corpus)),
            InputMode::Normal => None,
        }
    }

    /// Whether a Ctrl-X chord is pending at `now` (used for the input hint).
    pub fn chord_pending(&self, now: Instant) -> bool {
        self.chord
            .is_some_and(|t| now.saturating_duration_since(t) < CHORD_WINDOW)
    }

    /// Whether a second Ctrl-C at `now` would quit (used for the input hint).
    pub fn quit_armed(&self, now: Instant) -> bool {
        self.quit
            .last_ctrl_c
            .is_some_and(|t| now.saturating_duration_since(t) < DOUBLE_CTRL_C_WINDOW)
    }

    /// Whether a second Esc at `now` would clear the input (used for the
    /// input hint).
    pub fn esc_clear_armed(&self, now: Instant) -> bool {
        self.last_esc
            .is_some_and(|t| now.saturating_duration_since(t) < DOUBLE_ESC_WINDOW)
    }

    /// Whether quitting now would interrupt a turn or leave messages
    /// undelivered (a quit then asks again).
    pub fn quit_is_busy(&self) -> bool {
        self.running
            || !self.pending.is_empty()
            || self.send_next.is_some()
            || self.shell.running.is_some()
    }

    /// Whether a second Ctrl-D (or, while busy, `/quit`) at `now` would quit
    /// (used for the input hint).
    pub fn quit_confirm_armed(&self, now: Instant) -> bool {
        self.quit
            .confirm
            .is_some_and(|t| now.saturating_duration_since(t) < QUIT_CONFIRM_WINDOW)
    }

    /// The open completion menu, if any.
    pub fn menu(&self) -> Option<&complete::Menu> {
        self.completion.menu.as_ref()
    }

    /// Open, keep or close the completion menu for the word under the
    /// cursor. The same word keeps the menu (and its highlight); a new word
    /// rebuilds it; no word, a dismissed word, search mode or no matches
    /// close it.
    pub(super) fn refresh_menu(&mut self) {
        let word = match self.mode {
            InputMode::Normal => complete::word_at(self.input.lines(), self.input.cursor()),
            InputMode::Search(_) => None,
        };
        let word = word
            .filter(|word| word.kind != complete::CompletionKind::Path || self.workspace.is_some());
        if word
            .as_ref()
            .is_none_or(|word| word.kind != complete::CompletionKind::Path)
        {
            self.completion.path_index = PathIndex::Dropped;
        }
        let Some(word) = word else {
            self.completion.menu = None;
            self.completion.dismissed = None;
            return;
        };
        if self.completion.dismissed.as_ref() == Some(&word) {
            self.completion.menu = None;
            return;
        }
        self.completion.dismissed = None;
        if self
            .completion
            .menu
            .as_ref()
            .is_some_and(|menu| menu.word == word)
        {
            return;
        }
        // The same word in the same index found nothing a moment ago.
        if self.completion.menu.is_none()
            && self
                .completion
                .missed
                .as_ref()
                .is_some_and(|(missed, index)| *missed == word && *index == self.index_key())
        {
            return;
        }
        let paths: &[crate::mentions::PathEntry] = match (&word.kind, &self.workspace) {
            (complete::CompletionKind::Path, Some(root)) => match &self.completion.path_index {
                PathIndex::Ready(paths) => paths,
                PathIndex::Indexing(_) => &[],
                PathIndex::Dropped => {
                    self.completion.generation += 1;
                    let generation = self.completion.generation;
                    self.completion.path_index = PathIndex::Indexing(generation);
                    self.local.push(LocalEffect::IndexPaths {
                        generation,
                        root: root.clone(),
                    });
                    &[]
                }
            },
            _ => &[],
        };
        let items = complete::candidates(&word, paths);
        #[cfg(test)]
        {
            self.completion.lookups += 1;
        }
        self.completion.missed = None;
        let missed = word.clone();
        self.completion.menu = complete::Menu::open(word, items);
        if self.completion.menu.is_none() {
            self.completion.missed = Some((missed, self.index_key()));
        }
    }

    /// Which path index a lookup ran against: its generation and whether
    /// its paths had arrived.
    fn index_key(&self) -> (u64, bool) {
        (
            self.completion.generation,
            matches!(self.completion.path_index, PathIndex::Ready(_)),
        )
    }

    /// A key while the completion menu is open: ↑/↓ move the highlight, Tab
    /// completes, Enter completes a partial word (a complete one sends as
    /// usual), Esc closes the menu for this word. Returns whether the key
    /// was the menu's.
    pub(super) fn menu_key(&mut self, key: &KeyEvent) -> bool {
        if self.modal.is_some() || !key.modifiers.is_empty() {
            return false;
        }
        let Some(menu) = &mut self.completion.menu else {
            return false;
        };
        match key.code {
            KeyCode::Up => menu.step(false),
            KeyCode::Down => menu.step(true),
            KeyCode::Tab => self.accept_completion(),
            KeyCode::Enter if !menu.is_complete() => self.accept_completion(),
            KeyCode::Esc => {
                self.completion.dismissed = Some(menu.word.clone());
                self.completion.menu = None;
            }
            _ => return false,
        }
        true
    }

    /// Put the highlighted row in place of the word, followed by a space
    /// unless one follows already.
    pub(super) fn accept_completion(&mut self) {
        let Some(menu) = self.completion.menu.take() else {
            return;
        };
        let Some(item) = menu.current() else {
            return;
        };
        let word = &menu.word;
        if item.keep_open {
            // A directory: the menu goes on with what is in it.
            self.input
                .replace_in_line(word.row, word.start, word.end, &item.insert);
            return;
        }
        let spaced = self
            .input
            .lines()
            .get(word.row)
            .and_then(|line| line.chars().nth(word.end))
            .is_some_and(char::is_whitespace);
        let text = if spaced {
            item.insert.clone()
        } else {
            format!("{} ", item.insert)
        };
        self.input
            .replace_in_line(word.row, word.start, word.end, &text);
        if spaced {
            // Step over the space that is there.
            self.input.right();
        }
    }

    pub(super) fn main_key(&mut self, key: &KeyEvent, now: Instant, fx: &mut Vec<Effect>) {
        if let InputMode::Search(search) = &mut self.mode {
            self.quit.last_ctrl_c = None;
            match search.key(key, &self.search_corpus) {
                SearchOutcome::Continue => {}
                SearchOutcome::Accept(text) | SearchOutcome::Cancel(text) => {
                    self.end_search(&text);
                }
            }
            return;
        }
        if let Some(started) = self.chord.take()
            && now.saturating_duration_since(started) < CHORD_WINDOW
        {
            match key.code {
                KeyCode::Esc => {
                    self.quit.last_ctrl_c = None;
                    return;
                }
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.quit.last_ctrl_c = None;
                    self.interrupt_and_send(now, fx);
                    return;
                }
                KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.quit.last_ctrl_c = None;
                    self.edit_draft_externally();
                    return;
                }
                // Anything else cancels the chord and is processed normally.
                _ => {}
            }
        }
        if self.menu_key(key) {
            self.quit.last_ctrl_c = None;
            self.last_esc = None;
            return;
        }
        if key.code == KeyCode::Char('?')
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && self.input.is_empty()
        {
            // `?` on an empty input opens the shortcut panel; with text it
            // is just typed.
            self.quit.last_ctrl_c = None;
            self.last_esc = None;
            self.modal = Some(Modal::Keys);
            return;
        }
        let action = keymap::map_key(key, self.input.is_empty());
        if action != Action::CtrlC {
            self.quit.last_ctrl_c = None;
        }
        let esc_armed = self.esc_clear_armed(now);
        self.last_esc = None;
        let page = self.viewport_height.saturating_sub(2).max(1);
        let view = self.helper_view();
        match action {
            // With an empty input, Enter acts on the panel selection; with
            // text it submits as always.
            Action::Submit => match self.panel_selection() {
                Some(id) if self.input.is_empty() => self.toggle_helper_view(id),
                // `\`+Enter at the end of a line inserts a newline.
                _ if self.input.backslash_newline() => {}
                _ => self.submit(now, fx),
            },
            Action::SendNow => self.interrupt_and_send(now, fx),
            // Esc closes the helper view first, then drops the selection.
            Action::Escape if view.is_some() => {
                self.helper_view = None;
                self.clear_selection();
            }
            Action::Escape if self.panel_selection().is_some() => self.panel_selected = None,
            Action::HelperPrev => self.move_panel_selection(false),
            Action::HelperNext => self.move_panel_selection(true),
            Action::Newline => self.input.newline(),
            Action::CtrlC => self.ctrl_c(now, fx),
            Action::CtrlD if !self.input.is_empty() => self.input.delete(),
            Action::CtrlD => self.ctrl_d(now, fx),
            Action::Escape => self.escape(esc_armed, now, fx),
            Action::Edit(EditAction::Up) if self.input.is_empty() && self.can_take_back() => {
                self.take_back_queued(fx);
            }
            Action::ToggleDetail => {
                self.transcript.toggle_last_detail_in(view);
            }
            Action::ToggleChecklist => self.toggle_checklist(fx),
            Action::Stash => {
                if let StashOutcome::Stashed {
                    displaced: Some(text),
                } = self.input.toggle_stash()
                {
                    // Keep the replaced stash reachable with ↑.
                    self.input.remember(&text);
                    self.submitted.push(text);
                    self.transcript.push_notice(
                        NoticeLevel::Info,
                        "stashed; the earlier stash went to the prompt history (↑)",
                    );
                }
            }
            Action::Redraw => self.local.push(LocalEffect::Redraw),
            Action::Suspend => self.local.push(LocalEffect::Suspend),
            Action::ExternalEditor => self.edit_draft_externally(),
            // The draft stays in the editor behind the picker.
            Action::ModelPicker => {
                self.run_command(Command::Model(ModelCommand::Pick), now, fx);
            }
            Action::CycleMode => {
                let next = self.status.mode.unwrap_or_default().next();
                fx.push(Effect::SetMode(next));
            }
            Action::HistorySearch => self.start_search(),
            Action::ChordPrefix => self.chord = Some(now),
            Action::ScrollUp(n) => self.active_scroll().up(n),
            Action::ScrollDown(n) => self.active_scroll().down(n),
            Action::PageUp => self.active_scroll().up(page),
            Action::PageDown => self.active_scroll().down(page),
            Action::ScrollTop => self.active_scroll().top(),
            Action::ScrollBottom => self.active_scroll().bottom(),
            Action::Edit(edit) => self.edit(edit),
            Action::None => {}
        }
    }

    pub(super) fn start_search(&mut self) {
        self.search_corpus = search::build_corpus(self.input.history(), &self.global_history);
        self.mode = InputMode::Search(HistorySearch::start(self.input.text(), &self.search_corpus));
    }

    /// Leave search mode with `text` in the editor (cursor at the end).
    pub(super) fn end_search(&mut self, text: &str) {
        self.mode = InputMode::Normal;
        self.search_corpus = Vec::new();
        self.input.clear();
        self.input.set_text(text);
    }

    /// Ctrl+Enter or Ctrl-X Ctrl-S: when idle, the same as Enter; when running, interrupt and
    /// send the editor contents next (plain interrupt when the editor is empty).
    /// Ctrl-G, Ctrl-X Ctrl-E: open the draft in the user's editor (idle or
    /// running: it only edits the input).
    pub(super) fn edit_draft_externally(&mut self) {
        self.local
            .push(LocalEffect::ExternalEditor(EditorTarget::Draft(
                self.input.text(),
            )));
    }

    pub(super) fn interrupt_and_send(&mut self, now: Instant, fx: &mut Vec<Effect>) {
        if !self.running {
            self.submit(now, fx);
        } else if self.input.text().trim().is_empty() {
            self.interrupt(fx);
        } else {
            let text = self.input.take();
            self.submitted.push(text.clone());
            self.send_next = Some(match self.send_next.take() {
                Some(prev) => format!("{prev}\n\n{text}"),
                None => text,
            });
            fx.push(Effect::Interrupt);
        }
    }

    pub(super) fn edit(&mut self, edit: EditAction) {
        let e = &mut self.input;
        match edit {
            EditAction::Insert(c) => e.insert_char(c),
            EditAction::Backspace => e.backspace(),
            EditAction::Delete => e.delete(),
            EditAction::Left => e.left(),
            EditAction::Right => e.right(),
            EditAction::Home => e.home(),
            EditAction::End => e.end(),
            EditAction::WordLeft => e.word_left(),
            EditAction::WordRight => e.word_right(),
            EditAction::Up => e.up(),
            EditAction::Down => e.down(),
            EditAction::DeleteWordBack => e.delete_word_back(),
            EditAction::KillToLineStart => e.kill_to_line_start(),
            EditAction::KillToLineEnd => e.kill_to_line_end(),
            EditAction::KillWordForward => e.kill_word_forward(),
            EditAction::Yank => {
                e.yank();
            }
            EditAction::YankPop => {
                e.yank_pop();
            }
            EditAction::Undo => {
                e.undo();
            }
        }
    }

    pub(super) fn modal_key(&mut self, key: &KeyEvent, now: Instant, fx: &mut Vec<Effect>) {
        match &mut self.modal {
            Some(Modal::Escalation { id, dialog, .. }) => {
                let id = *id;
                match dialog.key(key) {
                    ModalKey::Stay => {}
                    ModalKey::Close => self.stop_all_escalations(fx),
                    ModalKey::Choose(answer) => {
                        fx.push(Effect::RespondEscalation(id, answer));
                        self.advance_escalation();
                    }
                }
            }
            Some(Modal::Resume(picker)) => match picker.key(key) {
                ModalKey::Stay => {}
                ModalKey::Close => self.modal = None,
                ModalKey::Choose(reference) => {
                    self.modal = None;
                    fx.push(Effect::ResumeSession(reference));
                }
            },
            Some(Modal::Agents(picker)) => match picker.key(key) {
                ModalKey::Stay => {}
                ModalKey::Close => self.modal = None,
                // A retire keeps the picker open: the list comes again
                // without the agent ([`Msg::RefreshAgentPicker`]), or the
                // refusal shows on it.
                ModalKey::Choose(AgentPick::Retire(name)) => fx.push(Effect::RetireAgent(name)),
                ModalKey::Choose(AgentPick::Switch(name)) => {
                    self.modal = None;
                    fx.push(Effect::Agents(Some(name)));
                }
                ModalKey::Choose(AgentPick::New) => {
                    self.modal = None;
                    fx.push(Effect::NewAgent);
                }
            },
            Some(Modal::List(picker)) => match picker.key(key) {
                ModalKey::Stay => {}
                ModalKey::Close => self.modal = None,
                ModalKey::Choose(act) => self.picked(act, fx),
            },
            Some(Modal::Model(picker)) => match picker.key(key) {
                ModalKey::Stay => {}
                ModalKey::Close => self.modal = None,
                ModalKey::Choose(model) => {
                    self.modal = None;
                    let current = format!("{}:{}", self.status.endpoint, self.status.model);
                    if model != current {
                        self.switch_model(model, fx);
                    }
                }
            },
            Some(Modal::Quit(dialog)) => match dialog.key(key) {
                ModalKey::Stay => {}
                ModalKey::Close => self.modal = None,
                ModalKey::Choose(choice) => {
                    self.modal = None;
                    let departure = match choice {
                        QuitChoice::Detach => Departure::Detach,
                        QuitChoice::Terminate => Departure::Terminate,
                    };
                    self.leave(Some(departure), fx);
                }
            },
            Some(Modal::PlanReady(dialog)) => match dialog.key(key) {
                ModalKey::Stay => {}
                ModalKey::Close => self.modal = None,
                ModalKey::Choose(choice) => {
                    self.modal = None;
                    self.plan_chosen(choice, now, fx);
                }
            },
            // Any key closes the panel and is not typed; a held `?` does
            // not close what it just opened.
            Some(Modal::Keys) if key.kind != KeyEventKind::Repeat => self.modal = None,
            Some(Modal::Keys) | None => {}
        }
    }

    /// The external editor closed: an edited instruction file goes to the
    /// client runner to reload, an edited draft replaces the input (Ctrl-_
    /// brings the old one back); a failure keeps everything and notices.
    pub(super) fn editor_closed(&mut self, closed: EditorClosed, fx: &mut Vec<Effect>) {
        match closed {
            EditorClosed::Memory {
                target,
                path,
                result: Ok(()),
            } => fx.push(Effect::MemoryEdited { target, path }),
            EditorClosed::Memory {
                path,
                result: Err(err),
                ..
            } => {
                self.transcript.push_notice(
                    NoticeLevel::Warn,
                    format!("{} may not be saved: {err}", path.display()),
                );
                self.scroll.bottom();
            }
            EditorClosed::Draft(Ok(text)) => {
                if text != self.input.text() {
                    self.input.set_text(&text);
                }
            }
            EditorClosed::Draft(Err(err)) => {
                self.transcript
                    .push_notice(NoticeLevel::Warn, format!("the input is unchanged: {err}"));
                self.scroll.bottom();
            }
        }
    }

    /// Esc: while a turn runs, interrupt it (as Claude Code does); the
    /// messages queued behind it are sent as the next turn once it ends
    /// (see [`Self::next_after_turn`]). ↑ on an empty input takes them back
    /// instead.
    ///
    /// Idle, Esc twice within [`DOUBLE_ESC_WINDOW`] on an input with text
    /// clears it and saves it to the prompt history (`esc_armed`: the
    /// previous key was such an Esc); on an empty input it does nothing.
    /// One Esc only arms it.
    pub(super) fn escape(&mut self, esc_armed: bool, now: Instant, fx: &mut Vec<Effect>) {
        if self.running {
            self.interrupt(fx);
        } else if self.shell.running.is_some() {
            fx.push(Effect::CancelShell);
        } else if self.input.is_empty() {
            // Nothing to clear.
        } else if esc_armed {
            let text = self.input.take();
            if !text.trim().is_empty() {
                self.submitted.push(text);
            }
        } else {
            self.last_esc = Some(now);
        }
    }

    pub(super) fn ctrl_c(&mut self, now: Instant, fx: &mut Vec<Effect>) {
        if self.running {
            self.quit.last_ctrl_c = None;
            self.interrupt(fx);
        } else if self.shell.running.is_some() {
            self.quit.last_ctrl_c = None;
            fx.push(Effect::CancelShell);
        } else if !self.input.is_empty() {
            self.quit.last_ctrl_c = None;
            self.input.clear();
        } else if self.quit_armed(now) {
            self.quit.last_ctrl_c = None;
            self.request_quit(now, fx);
        } else {
            self.quit.last_ctrl_c = Some(now);
        }
    }

    /// Interrupt the running turn. The runner also cancels every steered
    /// message not yet delivered; at the turn's end the UI takes them back
    /// ([`PendingQueue::take_unsent`](super::delivery::PendingQueue::take_unsent))
    /// and they start the next turn.
    pub(super) fn interrupt(&mut self, fx: &mut Vec<Effect>) {
        fx.push(Effect::Interrupt);
    }

    /// Ctrl-D on an empty editor: when idle, the first press only arms
    /// [`QUIT_CONFIRM_WINDOW`] (the editor title says to press it again)
    /// and a second one quits; while busy, the same as `/quit`.
    pub(super) fn ctrl_d(&mut self, now: Instant, fx: &mut Vec<Effect>) {
        if self.quit_is_busy() || self.quit_confirm_armed(now) {
            self.request_quit(now, fx);
        } else {
            self.quit.confirm = Some(now);
        }
    }

    /// `/quit` (and a confirmed or busy Ctrl-D): quit right away when idle;
    /// otherwise warn and quit only on a second request within
    /// [`QUIT_CONFIRM_WINDOW`]. Quitting hands every held message to the
    /// client runner ([`Effect::SaveUnsent`]) so undelivered ones are saved.
    pub(super) fn request_quit(&mut self, now: Instant, fx: &mut Vec<Effect>) {
        let queued = self.pending.len() + usize::from(self.send_next.is_some());
        let busy = self.running || queued > 0;
        if busy && !self.quit_confirm_armed(now) {
            self.quit.confirm = Some(now);
            let mut what = Vec::new();
            if self.running {
                what.push("a turn is running".to_string());
            }
            if queued > 0 {
                what.push(format!("{} queued", crate::text::count(queued, "message")));
            }
            // The daemon decides next whether the agent retires and the
            // daemon stops.
            let after = if self.status.reached_daemon() {
                "quitting retires this agent and interrupts its turn unless other clients work with it, /detach keeps it running; messages the daemon has not received are saved"
            } else {
                "the turn is interrupted, undelivered messages are saved"
            };
            self.transcript.push_notice(
                NoticeLevel::Warn,
                format!("{}; press again to quit anyway ({after})", what.join(" / ")),
            );
            self.scroll.bottom();
            return;
        }
        self.quit.confirm = None;
        // Attached: the runner asks the daemon whether this client stops
        // it, leaves it to other clients, or asks (detach or terminate). A
        // second `/quit` while it asks leaves at once, as a detach.
        if self.status.reached_daemon() && !std::mem::take(&mut self.quit.deciding) {
            self.quit.deciding = true;
            fx.push(Effect::DecideQuit);
            return;
        }
        self.leave(None, fx);
    }

    /// `/detach`: attached, leave and keep the agent running (the daemon
    /// records the detach); not attached (yet), there is nothing to detach
    /// from.
    pub(super) fn request_detach(&mut self, fx: &mut Vec<Effect>) {
        if self.status.reached_daemon() {
            self.leave(Some(Departure::Detach), fx);
        } else {
            self.transcript.push_notice(
                NoticeLevel::Info,
                "nothing to detach from: not attached to a daemon (/quit leaves)",
            );
            self.scroll.bottom();
        }
    }

    /// The runner's answer to [`Effect::DecideQuit`].
    pub(super) fn quit_decided(&mut self, decision: QuitDecision, fx: &mut Vec<Effect>) {
        if !std::mem::take(&mut self.quit.deciding) {
            return;
        }
        match decision {
            // The daemon did it already (`rkat/quit`): just leave.
            QuitDecision::Terminate | QuitDecision::Leave | QuitDecision::Retire => {
                self.leave(None, fx);
            }
            QuitDecision::Ask { reason, running } => {
                if matches!(self.modal, Some(Modal::Escalation { .. })) {
                    self.transcript.push_notice(
                        NoticeLevel::Warn,
                        "answer the approval request first, then /quit again",
                    );
                    self.scroll.bottom();
                } else {
                    self.modal = Some(Modal::Quit(QuitDialog::new(reason, running)));
                }
            }
        }
    }

    /// Leave now: hand every held message to the client runner
    /// ([`Effect::SaveUnsent`]), say what leaving does to an attached
    /// daemon ([`Effect::Leave`]), then [`Effect::Quit`].
    pub(super) fn leave(&mut self, departure: Option<Departure>, fx: &mut Vec<Effect>) {
        self.quit.deciding = false;
        // Delivered (settled) messages are in the conversation already.
        let mut held: Vec<(Option<PendingId>, String)> = self
            .pending
            .items()
            .iter()
            .map(|p| (Some(p.id), p.text.clone()))
            .collect();
        if let Some(next) = &self.send_next {
            held.push((None, next.clone()));
        }
        if !held.is_empty() {
            fx.push(Effect::SaveUnsent(held));
        }
        if let Some(departure) = departure {
            fx.push(Effect::Leave(departure));
        }
        fx.push(Effect::Quit);
    }
}

/// Ctrl-C, Ctrl-D and `/quit` while work is in flight.
#[derive(Debug, Default)]
pub(super) struct QuitState {
    /// When a Ctrl-D / `/quit` was refused because work is in flight; a
    /// second one within [`super::QUIT_CONFIRM_WINDOW`] quits anyway.
    pub(super) confirm: Option<Instant>,
    /// An [`Effect::DecideQuit`] waits for its [`super::Msg::QuitDecided`]
    /// (another `/quit` meanwhile leaves at once).
    pub(super) deciding: bool,
    /// When Ctrl-C was last pressed on an idle input.
    pub(super) last_ctrl_c: Option<Instant>,
}

/// The completion menu above the input and the `@` path index it reads.
#[derive(Debug)]
pub(super) struct Completion {
    /// The menu, while open.
    pub(super) menu: Option<complete::Menu>,
    /// The word Esc closed the menu for: it stays closed until the word
    /// changes.
    pub(super) dismissed: Option<complete::Word>,
    /// The workspace paths, asked for when a path menu opens and dropped
    /// when it closes.
    pub(super) path_index: PathIndex,
    /// The last [`super::LocalEffect::IndexPaths`] generation.
    pub(super) generation: u64,
    /// The word last looked up that opened no menu, with the path index
    /// it was looked up in ([`App::index_key`]): the next refresh for the
    /// same word and index does not scan the index again.
    pub(super) missed: Option<(complete::Word, (u64, bool))>,
    /// How many times the candidates were computed (tests).
    #[cfg(test)]
    pub(super) lookups: usize,
}

impl Default for Completion {
    fn default() -> Self {
        Self {
            menu: None,
            dismissed: None,
            path_index: PathIndex::Dropped,
            generation: 0,
            missed: None,
            #[cfg(test)]
            lookups: 0,
        }
    }
}
