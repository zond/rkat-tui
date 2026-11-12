//! The list pickers of `/schedules`, `/permissions`, `/helpers`, `/mode`
//! and `/memory`: one [`ListPicker`] of rows, each carrying the keys that
//! act on it (an act, an act to confirm with `y`, or why it cannot), with
//! a selection, a scrolling window and an optional line under the rows.
//!
//! The rows are built where their data lives (the runner for the daemon's
//! schedules and rules, the workbench for the instruction files, the UI
//! for the mode and the helpers); the picker only moves, asks and answers
//! with a [`PickAct`], which [`App::picked`] turns into what the typed
//! command does.

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;

use super::markdown::{truncate_to_width, wrap_text};
use super::modal::{ListCursor, ModalKey, has_ctrl_or_alt, is_ctrl_c, is_held, window_lines};
use super::{App, Effect, Modal};
use crate::daemon::protocol::ScheduleOp;

/// Which command's picker a [`ListPicker`] is (a refresh replaces only a
/// picker of its own kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    /// `/schedules`.
    Schedules,
}

impl PickerKind {
    /// The picker box's title.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Schedules => " Schedules ",
        }
    }
}

/// What a picker's key does; each is what a typed command does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickAct {
    /// `/schedules pause|resume|delete <id>`, then the list again.
    Schedule(ScheduleOp, String),
}

impl PickAct {
    /// Whether the picker stays open after it (a refreshed list replaces
    /// it; the others leave it for the editor, a view or the new mode).
    fn keeps_picker(&self) -> bool {
        matches!(self, Self::Schedule(..))
    }
}

/// One key of a row: what it does there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickKey {
    /// The key.
    pub code: KeyCode,
    /// Its act, or why it does nothing on this row.
    pub act: Result<PickAct, String>,
    /// The question `y` answers before the act runs.
    pub confirm: Option<String>,
}

impl PickKey {
    /// `code` does `act` at once.
    #[must_use]
    pub fn now(code: KeyCode, act: PickAct) -> Self {
        Self {
            code,
            act: Ok(act),
            confirm: None,
        }
    }

    /// `code` asks `question`; `y` then does `act`.
    #[must_use]
    pub fn confirmed(code: KeyCode, act: PickAct, question: impl Into<String>) -> Self {
        Self {
            code,
            act: Ok(act),
            confirm: Some(question.into()),
        }
    }

    /// `code` says `why` it does nothing here.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn refused(code: KeyCode, why: impl Into<String>) -> Self {
        Self {
            code,
            act: Err(why.into()),
            confirm: None,
        }
    }
}

/// One row: its line, an optional dim line under it, and its keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickRow {
    /// The row as shown.
    pub text: String,
    /// A second, dim line (an explanation, a path).
    pub note: Option<String>,
    /// What the keys do on it.
    pub keys: Vec<PickKey>,
}

impl PickRow {
    /// A row of `text` with `keys`.
    #[must_use]
    pub fn new(text: impl Into<String>, keys: Vec<PickKey>) -> Self {
        Self {
            text: text.into(),
            note: None,
            keys,
        }
    }

    /// The row with `note` under it.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn noted(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// What `code` does on this row.
    fn key(&self, code: KeyCode) -> Option<&PickKey> {
        self.keys.iter().find(|key| key.code == code)
    }
}

/// The line under a picker's rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerLine {
    /// A key that asks first: `y` does `act`, any other key keeps things.
    Confirm {
        /// What is asked.
        question: String,
        /// What `y` does.
        act: PickAct,
    },
    /// Why the key pressed does nothing on this row.
    Refused(String),
}

/// A picker over rows ([`PickerKind`] says whose).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListPicker {
    /// Whose picker.
    pub kind: PickerKind,
    /// The rows, in order.
    pub rows: Vec<PickRow>,
    /// The highlighted row.
    pub cursor: ListCursor,
    /// A question being asked, or why a key did nothing.
    pub line: Option<PickerLine>,
    /// What shows when there are no rows.
    pub empty: String,
    /// The key hint at the bottom.
    pub footer: String,
}

impl ListPicker {
    /// A picker of `kind` over `rows`, the first highlighted; `empty`
    /// shows when there are none, `footer` is the key hint.
    #[must_use]
    pub fn new(
        kind: PickerKind,
        rows: Vec<PickRow>,
        empty: impl Into<String>,
        footer: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            rows,
            cursor: ListCursor::default(),
            line: None,
            empty: empty.into(),
            footer: footer.into(),
        }
    }

    /// The picker with row `index` highlighted (the last when past it).
    #[must_use]
    pub fn at(mut self, index: usize) -> Self {
        self.cursor = ListCursor::at(index.min(self.rows.len().saturating_sub(1)));
        self
    }

    /// The highlighted row.
    #[must_use]
    pub fn selected(&self) -> Option<&PickRow> {
        self.rows.get(self.cursor.index())
    }

    /// ↑/↓ (or k/j) move; a row's own key acts, asks first (`y` then acts,
    /// any other key keeps things and is not acted on) or says why it does
    /// nothing; Esc, `q` or Ctrl-C close. A held key only moves.
    pub fn key(&mut self, key: &KeyEvent) -> ModalKey<PickAct> {
        let held = is_held(key);
        if is_ctrl_c(key) {
            return if held {
                ModalKey::Stay
            } else {
                ModalKey::Close
            };
        }
        let moves = matches!(
            key.code,
            KeyCode::Up | KeyCode::Down | KeyCode::Char('j' | 'k')
        );
        if (held && !moves) || has_ctrl_or_alt(key) {
            return ModalKey::Stay;
        }
        if let Some(PickerLine::Confirm { act, .. }) = self.line.take() {
            return match key.code {
                KeyCode::Char('y') => ModalKey::Choose(act),
                _ => ModalKey::Stay,
            };
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.cursor.up(),
            KeyCode::Down | KeyCode::Char('j') => self.cursor.down(self.rows.len()),
            KeyCode::Esc | KeyCode::Char('q') => return ModalKey::Close,
            code => match self.selected().and_then(|row| row.key(code)).cloned() {
                Some(PickKey {
                    act: Ok(act),
                    confirm: None,
                    ..
                }) => return ModalKey::Choose(act),
                Some(PickKey {
                    act: Ok(act),
                    confirm: Some(question),
                    ..
                }) => self.line = Some(PickerLine::Confirm { question, act }),
                Some(PickKey { act: Err(why), .. }) => self.line = Some(PickerLine::Refused(why)),
                None => {}
            },
        }
        ModalKey::Stay
    }

    /// `fresh` in place of this picker, on the same row where it still is.
    #[must_use]
    pub fn refreshed(&self, fresh: Self) -> Self {
        fresh.at(self.cursor.index())
    }
}

/// The rows (each with its note), else the empty line; then the question
/// or refusal; then the key hint. At most `max_lines` lines: the rows
/// scroll to keep the selection visible.
pub(super) fn list_picker_lines(
    picker: &ListPicker,
    width: usize,
    max_lines: usize,
) -> Vec<Line<'static>> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let warn = Style::default().fg(Color::Yellow);
    let mut tail = Vec::new();
    match &picker.line {
        Some(PickerLine::Confirm { question, .. }) => tail.extend(
            wrap_text(&format!("{question} (y yes, any other key no)"), width)
                .into_iter()
                .map(|line| Line::styled(line, warn)),
        ),
        Some(PickerLine::Refused(why)) => tail.extend(
            wrap_text(why, width)
                .into_iter()
                .map(|line| Line::styled(line, warn)),
        ),
        None => {}
    }
    tail.extend(
        wrap_text(&picker.footer, width)
            .into_iter()
            .map(|line| Line::styled(line, dim)),
    );
    if picker.rows.is_empty() {
        let mut lines: Vec<Line<'static>> = wrap_text(&picker.empty, width)
            .into_iter()
            .map(Line::from)
            .collect();
        lines.extend(tail);
        return lines;
    }
    let rows: Vec<Vec<Line<'static>>> = picker
        .rows
        .iter()
        .map(|row| {
            let mut lines = vec![Line::from(truncate_to_width(&row.text, width))];
            if let Some(note) = &row.note {
                lines.push(Line::styled(
                    truncate_to_width(&format!("    {note}"), width),
                    dim,
                ));
            }
            lines
        })
        .collect();
    let room = max_lines.saturating_sub(tail.len());
    let mut lines = window_lines(rows, picker.cursor.index(), room);
    lines.extend(tail);
    lines
}

impl App {
    /// Open a picker `modal` over nothing (or over the keys panel); behind
    /// an approval dialog it waits until the dialogs are answered
    /// (replacing a picker waiting there); over any other dialog it is
    /// dropped.
    pub(super) fn show_modal(&mut self, modal: Modal) {
        match self.modal {
            None | Some(Modal::Keys) => self.modal = Some(modal),
            Some(Modal::Escalation { .. }) => self.parked = Some(modal),
            Some(_) => {}
        }
    }

    /// Show `picker`: over nothing (or the keys panel) it opens; behind an
    /// approval dialog it waits until the dialogs are answered. A
    /// `refresh` (the list again after a picker's own act) only replaces
    /// a picker of its kind that is open or waiting, on the same row.
    pub(super) fn show_picker(&mut self, picker: ListPicker, refresh: bool) {
        let same = |modal: &Option<Modal>| matches!(modal, Some(Modal::List(open)) if open.kind == picker.kind);
        if same(&self.modal) {
            if let Some(Modal::List(open)) = &self.modal {
                self.modal = Some(Modal::List(open.refreshed(picker)));
            }
            return;
        }
        if same(&self.parked) {
            if let Some(Modal::List(open)) = &self.parked {
                self.parked = Some(Modal::List(open.refreshed(picker)));
            }
            return;
        }
        if refresh {
            return;
        }
        self.show_modal(Modal::List(picker));
    }

    /// A picker's answer: what the typed command does; the schedules, the
    /// rules and the helpers stay open (their list comes again).
    pub(super) fn picked(&mut self, act: PickAct, fx: &mut Vec<Effect>) {
        if !act.keeps_picker() {
            self.modal = None;
        }
        match act {
            PickAct::Schedule(op, id) => fx.push(Effect::PickedSchedule(op, id)),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyEventKind, KeyModifiers};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn picker() -> ListPicker {
        ListPicker::new(
            PickerKind::Schedules,
            vec![
                PickRow::new(
                    "a",
                    vec![
                        PickKey::now(
                            KeyCode::Enter,
                            PickAct::Schedule(ScheduleOp::Pause, "1".into()),
                        ),
                        PickKey::confirmed(
                            KeyCode::Char('d'),
                            PickAct::Schedule(ScheduleOp::Delete, "1".into()),
                            "delete a?",
                        ),
                    ],
                ),
                PickRow::new("b", vec![PickKey::refused(KeyCode::Char('d'), "b stays")]),
            ],
            "none",
            "keys",
        )
    }

    /// Rows act at once, ask first (only `y` then acts, any other key
    /// keeps things and is not acted on) or say why not; a key a row does
    /// not have does nothing; ↑/↓ and j/k move within the rows; Esc, `q`
    /// and Ctrl-C close; a held key only moves.
    #[test]
    fn a_list_picker_acts_asks_refuses_moves_and_closes() {
        let mut p = picker();
        assert_eq!(
            p.key(&press(KeyCode::Enter)),
            ModalKey::Choose(PickAct::Schedule(ScheduleOp::Pause, "1".into()))
        );
        assert_eq!(p.key(&press(KeyCode::Char('d'))), ModalKey::Stay);
        assert!(
            matches!(&p.line, Some(PickerLine::Confirm { question, .. }) if question == "delete a?")
        );
        // `n` keeps it; Esc during the question keeps the picker open.
        assert_eq!(p.key(&press(KeyCode::Char('n'))), ModalKey::Stay);
        assert_eq!(p.line, None);
        p.key(&press(KeyCode::Char('d')));
        assert_eq!(p.key(&press(KeyCode::Esc)), ModalKey::Stay);
        p.key(&press(KeyCode::Char('d')));
        assert_eq!(
            p.key(&press(KeyCode::Char('y'))),
            ModalKey::Choose(PickAct::Schedule(ScheduleOp::Delete, "1".into()))
        );
        // The second row refuses with its reason; Enter there does nothing.
        p.key(&press(KeyCode::Char('j')));
        p.key(&press(KeyCode::Down));
        assert_eq!(p.cursor.index(), 1);
        p.key(&press(KeyCode::Char('d')));
        assert_eq!(p.line, Some(PickerLine::Refused("b stays".into())));
        assert_eq!(p.key(&press(KeyCode::Enter)), ModalKey::Stay);
        assert_eq!(p.line, None);
        p.key(&press(KeyCode::Char('k')));
        assert_eq!(p.cursor.index(), 0);
        let mut held = press(KeyCode::Enter);
        held.kind = KeyEventKind::Repeat;
        assert_eq!(p.key(&held), ModalKey::Stay);
        let mut held_down = press(KeyCode::Down);
        held_down.kind = KeyEventKind::Repeat;
        p.key(&held_down);
        assert_eq!(p.cursor.index(), 1);
        assert_eq!(p.key(&press(KeyCode::Char('q'))), ModalKey::Close);
        assert_eq!(p.key(&press(KeyCode::Esc)), ModalKey::Close);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(p.key(&ctrl_c), ModalKey::Close);
        // An empty picker moves nowhere and acts on nothing.
        let mut empty = ListPicker::new(PickerKind::Schedules, vec![], "none", "keys");
        assert_eq!(empty.key(&press(KeyCode::Enter)), ModalKey::Stay);
        assert_eq!(empty.key(&press(KeyCode::Down)), ModalKey::Stay);
        assert_eq!(empty.selected(), None);
    }

    /// A refreshed picker keeps the row the user was on, the last when the
    /// list got shorter.
    #[test]
    fn a_refresh_keeps_the_row() {
        let open = picker().at(1);
        let fresh = open.refreshed(picker());
        assert_eq!(fresh.cursor.index(), 1);
        let shorter = ListPicker::new(
            PickerKind::Schedules,
            vec![PickRow::new("a", vec![])],
            "none",
            "keys",
        );
        assert_eq!(open.refreshed(shorter).cursor.index(), 0);
    }
}
