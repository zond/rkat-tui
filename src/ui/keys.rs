//! Keys outside the session's work: the main key map, the completion
//! menu, history search, the editor, dialogs' keys, Esc, Ctrl-C, Ctrl-D
//! and quitting.

use std::time::Instant;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{
    Action, App, Command, Departure, EditAction, EditorClosed, EditorTarget, Effect, HelpDialog,
    HelpView, HistorySearch, INTERRUPT_REPEAT, InputMode, LocalEffect, Modal, ModalKey,
    ModelCommand, NoticeLevel, PendingId, PendingState, Pressed, QUIT_CONFIRM_WINDOW, QuitChoice,
    QuitDecision, QuitDialog, SearchOutcome, StashOutcome, complete, keymap, search,
};

/// The `@` menu's workspace paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PathIndex {
    /// None held (no path menu open).
    Dropped,
    /// Asked for with this generation ([`LocalEffect::IndexPaths`]).
    Indexing(u64),
    /// The paths.
    Ready(Vec<crate::mentions::PathEntry>),
}

impl App {
    /// The active history search and its corpus, if searching.
    pub fn search(&self) -> Option<(&HistorySearch, &[String])> {
        self.mode
            .searching()
            .map(|searching| (&searching.search, searching.corpus.as_slice()))
    }

    /// Whether a Ctrl-X chord is pending at `now` (used for the input hint).
    pub fn chord_pending(&self, now: Instant) -> bool {
        self.mode.chord_pending(now)
    }

    /// Whether a second Ctrl-C at `now` would quit (used for the input hint).
    pub fn quit_armed(&self, now: Instant) -> bool {
        self.mode.ctrl_c_armed(now)
    }

    /// Whether a second Esc at `now` would clear the input (used for the
    /// input hint).
    pub fn esc_clear_armed(&self, now: Instant) -> bool {
        self.mode.esc_armed(now)
    }

    /// Whether quitting now would interrupt a turn or leave messages
    /// undelivered (a quit then asks again).
    pub fn quit_is_busy(&self) -> bool {
        self.turn.is_running()
            || !self.pending.is_empty()
            || self.delivery.has_next()
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
    /// close it. Over an entry ↑/↓ recalled from the history and not
    /// edited since, no menu opens: ↑/↓ go on walking the history (a
    /// recalled `/models` or `… @src/main.rs` would otherwise take them for
    /// the menu's highlight, and the walk would stop there); the first
    /// edit opens it.
    pub(super) fn refresh_menu(&mut self) {
        let word = match self.mode {
            InputMode::Search(_) => None,
            _ if self.input.is_unedited_recall() => None,
            _ => complete::word_at(self.input.lines(), self.input.cursor()),
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
        // One mode at a time ([`InputMode`]): any key ends an armed Esc or
        // Ctrl-C and a pending chord, and says what they made of it (the
        // Esc that clears, the Ctrl-C that quits, the chord's second key).
        let pressed = self.mode.key_pressed(now);
        if self.stop_confirmation().is_some() {
            self.stop_confirm_key(key, fx);
            return;
        }
        if let InputMode::Search(searching) = &mut self.mode {
            match searching.search.key(key, &searching.corpus) {
                SearchOutcome::Continue => {}
                SearchOutcome::Accept(text) | SearchOutcome::Cancel(text) => {
                    self.end_search(&text);
                }
            }
            return;
        }
        if pressed == Pressed::Chord {
            match key.code {
                KeyCode::Esc => return,
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.interrupt_and_send(now, fx);
                    return;
                }
                KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.edit_draft_externally();
                    return;
                }
                // Anything else cancels the chord and is processed normally.
                _ => {}
            }
        }
        if self.menu_key(key) {
            return;
        }
        if key.code == KeyCode::Char('?')
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && self.input.is_empty()
        {
            // `?` on an empty input opens the help on its keys; with text
            // it is just typed.
            self.show_modal(Modal::Help(HelpDialog::new(HelpView::Keys)));
            return;
        }
        let action = keymap::map_key(key, self.input.is_empty());
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
            // Esc in a helper's view goes back to the transcript at once
            // (a pending `/btw` takes it first); it neither interrupts the
            // agent nor arms Esc-Esc. The next Esc drops the selection.
            Action::Escape if view.is_some() => {
                if self.btw_pending() {
                    self.cancel_btw(fx);
                } else {
                    self.helper_view = None;
                    self.clear_selection();
                }
            }
            // Ctrl-C in a helper's view asks whether to stop a running one
            // (or says it is not running); it never reaches the agent's
            // interrupt or the quit ladder.
            Action::CtrlC if view.is_some() => self.ask_stop_in_view(),
            Action::Escape if self.panel_selection().is_some() => self.panel_selected = None,
            Action::HelperPrev => self.move_panel_selection(false),
            Action::HelperNext => self.move_panel_selection(true),
            Action::Newline => self.input.newline(),
            Action::CtrlC => self.ctrl_c(pressed == Pressed::CtrlCArmed, now, fx),
            Action::CtrlD if !self.input.is_empty() => self.input.delete(),
            Action::CtrlD => self.ctrl_d(now, fx),
            Action::Escape => self.escape(pressed == Pressed::EscArmed, now, fx),
            Action::Edit(EditAction::Up) if self.input.is_empty() && self.can_take_back() => {
                self.take_back_queued(fx);
            }
            Action::ToggleDetail => self.toggle_detail(view),
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
            Action::ChordPrefix => self.mode.start_chord(now),
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
        let corpus = search::build_corpus(self.input.history(), &self.global_history);
        let search = HistorySearch::start(self.input.text(), &corpus);
        self.mode.start_search(search, corpus);
    }

    /// Leave search mode with `text` in the editor (cursor at the end).
    pub(super) fn end_search(&mut self, text: &str) {
        self.mode.end_search();
        self.input.clear();
        self.input.set_text(text);
    }

    /// Ctrl-G, Ctrl-X Ctrl-E: open the draft in the user's editor (idle or
    /// running: it only edits the input).
    pub(super) fn edit_draft_externally(&mut self) {
        self.local
            .push(LocalEffect::ExternalEditor(EditorTarget::Draft(
                self.input.text(),
            )));
    }

    /// Ctrl-X Ctrl-S, send now: when idle, the same as Enter. While a turn
    /// runs it interrupts the turn and sends, as the next run, the user's
    /// messages still waiting for the agent (queued or delivering, in the
    /// order typed) and then the editor's text: the interrupt's answer
    /// hands the unread ones back ([`super::Msg::Returned`]) and the
    /// turn's end sends them instead of putting them in the editor
    /// ([`Self::turn_finished`]). With nothing waiting and an empty editor
    /// a plain interrupt, idle too: the client then asks the daemon
    /// whether the agent runs a turn this UI does not show.
    pub(super) fn interrupt_and_send(&mut self, now: Instant, fx: &mut Vec<Effect>) {
        let empty = self.input.text().trim().is_empty();
        let running = self.turn.is_running();
        let waiting = running && self.has_waiting_messages();
        if !running && self.compaction.run_going() && self.holds_messages() {
            // The messages wait for the run that lets the agent compact:
            // it is stopped, and they go as soon as it ended.
            if !empty {
                self.submit(now, fx);
            }
            self.transcript.push_notice(
                NoticeLevel::Info,
                "stopping the run that lets the agent compact; the messages it held go when it ends",
            );
            self.interrupt(now, fx);
        } else if empty && !waiting {
            self.interrupt(now, fx);
        } else if !running {
            self.submit(now, fx);
        } else {
            if !empty {
                let text = self.input.take();
                self.submitted.push(text.clone());
                self.delivery.send_after(text);
            }
            self.turn.push_waiting();
            fx.push(Effect::Interrupt);
        }
    }

    /// Whether one of the user's messages steered into the running turn
    /// still waits for the agent (queued or delivering, and no recall found
    /// it read already).
    pub(super) fn has_waiting_messages(&self) -> bool {
        self.has_admitted_recallable() || self.holds_messages()
    }

    /// Whether the UI holds a message of the user's (one that waits for
    /// the turn's end, a `!` command or a compaction).
    pub(super) fn holds_messages(&self) -> bool {
        self.pending
            .items()
            .iter()
            .any(|p| p.state == PendingState::Deferred)
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

    /// Whether a dialog the application opened by itself and has not
    /// armed yet ([`super::Arming`]) drops `key` at `now`: it does, unless
    /// the key only shows more of it (Ctrl+O, and PgUp/PgDn/Home/End over
    /// the whole arguments), and the wait starts again.
    fn unarmed_drops(&mut self, key: &KeyEvent, now: Instant) -> bool {
        let Some(arming) = &mut self.arming else {
            return false;
        };
        if arming.armed(now) {
            self.arming = None;
            return false;
        }
        let shows_more = match &self.modal {
            Some(Modal::Escalation { dialog, .. }) => {
                (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o'))
                    || (dialog.expanded
                        && key.modifiers.is_empty()
                        && matches!(
                            key.code,
                            KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End
                        ))
            }
            _ => false,
        };
        if shows_more {
            return false;
        }
        arming.quiet_since = now;
        arming.dropped = true;
        true
    }

    pub(super) fn modal_key(&mut self, key: &KeyEvent, now: Instant, fx: &mut Vec<Effect>) {
        if self.unarmed_drops(key, now) {
            return;
        }
        match &mut self.modal {
            Some(Modal::Escalation { id, dialog, .. }) => {
                let id = *id;
                match dialog.key(key) {
                    ModalKey::Stay => {}
                    ModalKey::Close => self.stop_escalations_of_shown(now, fx),
                    ModalKey::Choose(answer) => {
                        fx.push(Effect::RespondEscalation(id, answer));
                        self.advance_escalation(now);
                    }
                }
            }
            Some(Modal::Resume(picker)) => match picker.key(key) {
                ModalKey::Stay => {}
                ModalKey::Close => self.modal = None,
                ModalKey::Choose(reference) => {
                    self.modal = None;
                    self.resume(reference, fx);
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
                    let classifier = picker.classifier;
                    self.modal = None;
                    let current = format!("{}:{}", self.status.endpoint, self.status.model);
                    if classifier {
                        fx.push(Effect::SetClassifierModel(model, false));
                    } else if model != current {
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
            Some(Modal::Help(dialog)) => match dialog.key(key) {
                ModalKey::Stay | ModalKey::Choose(()) => {}
                ModalKey::Close => self.modal = None,
            },
            None => {}
        }
    }

    /// The external editor closed: an edited instruction file goes to the
    /// client runner to reload, an edited draft replaces the input (Ctrl-_
    /// brings the old one back); a failure keeps everything and notices.
    pub(super) fn editor_closed(&mut self, closed: EditorClosed, fx: &mut Vec<Effect>) {
        let memory_picker = [&self.modal, &self.parked].into_iter().any(|slot| {
            matches!(slot, Some(Modal::List(open)) if open.kind == super::PickerKind::Memory)
        });
        // The `/memory` picker the file was opened from stays: its rows
        // again after the edit, with the file's new size.
        let refresh = memory_picker && matches!(closed, EditorClosed::Memory { .. });
        self.editor_closed_as(closed, fx);
        if refresh {
            fx.push(Effect::RequestMemoryPicker);
        }
    }

    fn editor_closed_as(&mut self, closed: EditorClosed, fx: &mut Vec<Effect>) {
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
    /// messages typed during it that the agent has not read come back to
    /// the editor when it ends (see [`Self::turn_finished`]).
    ///
    /// A pending `/btw` takes the first Esc: it cancels that question
    /// only, never the agent's run; the next Esc is the usual one.
    ///
    /// Presses within [`INTERRUPT_REPEAT`] of an interrupting one belong to
    /// that interrupt, also once its turn has ended: they go to the runner
    /// (which forces the interrupt, or says the turn is already stopped)
    /// and never clear the editor the returned messages are in.
    ///
    /// Idle, Esc twice within [`super::DOUBLE_ESC_WINDOW`] on an input with text
    /// clears it and saves it to the prompt history (`esc_armed`: the
    /// previous key was such an Esc); one Esc only arms it. On an empty
    /// input it interrupts all the same: the agent may run a turn this UI
    /// does not show, and the client asks the daemon (it says when nothing
    /// runs).
    pub(super) fn escape(&mut self, esc_armed: bool, now: Instant, fx: &mut Vec<Effect>) {
        // A pending `/btw` takes the first Esc, and only it is cancelled;
        // the next Esc is the usual one.
        if self.btw_pending() {
            self.cancel_btw(fx);
        } else if self.turn.is_running() || self.interrupt_repeats(now) {
            self.interrupt(now, fx);
        } else if self.shell.running.is_some() {
            fx.push(Effect::CancelShell);
        } else if self.input.is_empty() {
            self.interrupt(now, fx);
        } else if esc_armed {
            let text = self.input.take();
            if !text.trim().is_empty() {
                self.submitted.push(text);
            }
        } else {
            self.mode.arm_esc(now);
        }
    }

    /// Ctrl-C: interrupt a running turn, cancel a `!` command, clear the
    /// input; on an idle empty input the first press arms the quit and a
    /// second (`armed`: the previous key was such a press, within
    /// [`super::DOUBLE_CTRL_C_WINDOW`]) quits.
    pub(super) fn ctrl_c(&mut self, armed: bool, now: Instant, fx: &mut Vec<Effect>) {
        if self.turn.is_running() || self.interrupt_repeats(now) {
            self.interrupt(now, fx);
        } else if self.shell.running.is_some() {
            fx.push(Effect::CancelShell);
        } else if !self.input.is_empty() {
            self.input.clear();
        } else if armed {
            self.request_quit(now, fx);
        } else {
            self.mode.arm_ctrl_c(now);
        }
    }

    /// Interrupt the running turn. The runner takes back every message of
    /// this client's the agent has not read ([`Msg::Returned`]); at the
    /// turn's end they and the ones the UI held go back to the editor
    /// ([`Self::turn_finished`]).
    ///
    /// [`Msg::Returned`]: super::Msg::Returned
    pub(super) fn interrupt(&mut self, now: Instant, fx: &mut Vec<Effect>) {
        self.last_interrupt = Some(now);
        fx.push(Effect::Interrupt);
    }

    /// Whether a press at `now` belongs to the last interrupt
    /// ([`INTERRUPT_REPEAT`] after its last press).
    fn interrupt_repeats(&self, now: Instant) -> bool {
        self.last_interrupt
            .is_some_and(|last| now.saturating_duration_since(last) < INTERRUPT_REPEAT)
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
        let commands = self
            .pending
            .items()
            .iter()
            .filter(|p| p.state == PendingState::Command)
            .count();
        let queued = self.pending.len() - commands + usize::from(self.delivery.has_next());
        let running = self.turn.is_running();
        let shell = self.shell.running.is_some();
        // The one notion of busy ([`Self::quit_is_busy`]), told apart.
        if self.quit_is_busy() && !self.quit_confirm_armed(now) {
            self.quit.confirm = Some(now);
            let mut what = Vec::new();
            if running {
                what.push("a turn is running".to_string());
            }
            if shell {
                what.push("a ! command is running (quitting stops it)".to_string());
            }
            if queued > 0 {
                what.push(format!("{} queued", crate::text::count(queued, "message")));
            }
            if commands > 0 {
                what.push(format!(
                    "{} waiting for the turn's end (dropped on quit)",
                    crate::text::count(commands, "command")
                ));
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
        // Commands held for the turn's end would run on this client: they
        // are dropped, and said so.
        let dropped = self.drop_held_commands();
        if !dropped.is_empty() {
            self.transcript.push_notice(
                NoticeLevel::Warn,
                format!(
                    "dropped what waited for the turn's end: {}",
                    dropped.join(", ")
                ),
            );
            fx.push(Effect::DroppedCommands(dropped));
        }
        // Delivered (settled) messages are in the conversation already.
        let mut held: Vec<(Option<PendingId>, String)> = self
            .pending
            .items()
            .iter()
            .map(|p| (Some(p.id), p.text.clone()))
            .collect();
        if let Some(next) = self.delivery.next() {
            held.push((None, next.to_string()));
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
