//! Modal dialogs: the escalation (approval) prompt, the resume picker, the
//! model picker, the list pickers ([`super::picker`], the agent picker
//! among them), the shortcut panel and plan mode's "Plan ready"
//! dialog.
//!
//! Modals are driven by plain data so the UI stays decoupled from the
//! session modules. The escalation dialog shows the approval engine's
//! [`RecordedPrompt`] and answers with its [`EscalationAnswer`], the same
//! shapes the daemon's decisions carry to a client.
//!
//! This module holds the dialogs' state and keys; [`draw`] draws them.

mod draw;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub use self::draw::draw_modal;
pub(super) use self::draw::window_lines;
use super::picker::ListPicker;
pub use crate::approval::desk::EscalationAnswer;
use crate::approval::escalation::RecordedPrompt;
use crate::models::{ModelEntry, ModelGroup};

/// What a key did in a dialog ([`Modal`]): every dialog owns its keys,
/// held-key (repeat) policy included, and answers with one of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalKey<T> {
    /// Nothing to act on yet (a move, an edit, or an ignored key).
    Stay,
    /// The dialog's answer; it closes.
    Choose(T),
    /// Close without an answer (Esc, or Ctrl-C).
    Close,
}

/// The highlighted row of a list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListCursor(usize);

impl ListCursor {
    /// The cursor on row `index`.
    #[must_use]
    pub fn at(index: usize) -> Self {
        Self(index)
    }

    /// The highlighted row's index.
    #[must_use]
    pub fn index(self) -> usize {
        self.0
    }

    /// Move up one row (stops at the first).
    pub fn up(&mut self) {
        self.0 = self.0.saturating_sub(1);
    }

    /// Move down one row of `len` (stops at the last).
    pub fn down(&mut self, len: usize) {
        if self.0 + 1 < len {
            self.0 += 1;
        }
    }
}

/// Ctrl-C.
pub(super) fn is_ctrl_c(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
}

/// A held key (auto-repeat): it may move, edit and type, never answer,
/// select or close.
pub(super) fn is_held(key: &KeyEvent) -> bool {
    key.kind == KeyEventKind::Repeat
}

/// Ctrl or Alt held (not typed into a filter or comment).
pub(super) fn has_ctrl_or_alt(key: &KeyEvent) -> bool {
    key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

/// The list dialogs' first check: Ctrl-C closes, a held Ctrl-C does
/// nothing, and any other key is `None` (the dialog's own to handle).
pub(super) fn ctrl_c_answer<T>(key: &KeyEvent) -> Option<ModalKey<T>> {
    is_ctrl_c(key).then(|| {
        if is_held(key) {
            ModalKey::Stay
        } else {
            ModalKey::Close
        }
    })
}

/// A key a list dialog leaves alone: held but not a move (↑/↓, `j`/`k`),
/// or with Ctrl or Alt.
pub(super) fn ignored_key(key: &KeyEvent) -> bool {
    let moves = matches!(
        key.code,
        KeyCode::Up | KeyCode::Down | KeyCode::Char('j' | 'k')
    );
    (is_held(key) && !moves) || has_ctrl_or_alt(key)
}

/// Maximum lines of the args preview shown in the escalation dialog.
const MAX_ARGS_LINES: usize = 10;

/// Argument lines one PgUp/PgDn moves in the expanded dialog (a page of
/// the smallest room, so no line is skipped).
const ARGS_PAGE: usize = MAX_ARGS_LINES - 1;

/// Identifier of a pending escalation, assigned by the approval engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EscalationId(pub u64);

/// How many numbered choices the escalation dialog lists.
const ESCALATION_CHOICES: usize = 4;

/// Index of the "No, and tell the agent what to do instead" choice.
const DENY_CHOICE: usize = 3;

/// Longest deny comment, in characters.
const MAX_COMMENT_CHARS: usize = 2000;

/// The escalation dialog's own state: the highlighted choice and, once
/// "No" was chosen, the comment being written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EscalationDialog {
    /// Index into the numbered choices (0: allow once).
    pub cursor: usize,
    /// The comment field: `Some` once the deny choice was picked.
    pub comment: Option<String>,
    /// Ctrl+O: the whole arguments instead of the preview.
    pub expanded: bool,
    /// The first argument line shown while expanded (PgUp/PgDn/Home/End).
    pub scroll: usize,
    /// The last useful [`Self::scroll`]: argument lines past the room the
    /// terminal has, recorded by the renderer (it knows the width).
    pub scroll_max: usize,
    /// The cursor was moved with ↑/↓: Enter answers the row it is on.
    /// Until then Enter answers nothing (an Enter meant to send a message
    /// must not allow a call), and says so ([`Self::enter_unmoved`]).
    pub moved: bool,
    /// Enter was pressed before the cursor was moved.
    pub enter_unmoved: bool,
}

/// What a dialog says when Enter was pressed before ↑/↓ chose a row.
pub const ENTER_NEEDS_A_ROW: &str =
    "Enter answers the row you move to with ↑/↓; or press a row's number or letter";

impl EscalationDialog {
    /// A key: Ctrl-C closes (the caller stops every escalation); a held
    /// key only moves the cursor or edits the comment; other Ctrl/Alt
    /// keys do nothing; the rest go to [`Self::answer`].
    pub fn key(&mut self, key: &KeyEvent) -> ModalKey<EscalationAnswer> {
        if is_held(key) && !self.accepts_repeat(key.code) {
            return ModalKey::Stay;
        }
        if is_ctrl_c(key) {
            return ModalKey::Close;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
            self.expanded = !self.expanded;
            self.scroll = 0;
            return ModalKey::Stay;
        }
        if has_ctrl_or_alt(key) {
            return ModalKey::Stay;
        }
        if self.scroll_args(key.code) {
            return ModalKey::Stay;
        }
        self.answer(key.code)
            .map_or(ModalKey::Stay, ModalKey::Choose)
    }

    /// While expanded, PgUp/PgDn move the arguments by a page and
    /// Home/End go to their start and end (none of them answers or moves
    /// the cursor). Whether `code` was such a key.
    fn scroll_args(&mut self, code: KeyCode) -> bool {
        if !self.expanded {
            return false;
        }
        self.scroll = match code {
            KeyCode::PageUp => self.scroll.saturating_sub(ARGS_PAGE),
            KeyCode::PageDown => self.scroll.saturating_add(ARGS_PAGE),
            KeyCode::Home => 0,
            KeyCode::End => self.scroll_max,
            _ => return false,
        }
        .min(self.scroll_max);
        true
    }

    /// Whether a held (repeating) `code` may act: it moves the cursor,
    /// scrolls the arguments or edits the comment, never answers.
    fn accepts_repeat(&self, code: KeyCode) -> bool {
        if self.expanded && matches!(code, KeyCode::PageUp | KeyCode::PageDown) {
            return true;
        }
        if self.comment.is_some() {
            matches!(code, KeyCode::Char(_) | KeyCode::Backspace)
        } else {
            matches!(code, KeyCode::Up | KeyCode::Down | KeyCode::Char('j' | 'k'))
        }
    }

    /// Apply a key: arrows (and `j`/`k`) move the cursor, Enter picks the
    /// highlighted choice once the arrows moved it ([`Self::moved`]: `j`/`k`
    /// are letters of a sentence too), `1`-`4` pick directly, and the accelerators
    /// `y`, `t` (this session), `a`, `n`/Esc and `s` answer at once. The deny choice opens the
    /// comment field; there Enter denies with the comment (none when
    /// empty) and Esc denies without one. `None`: no answer yet.
    fn answer(&mut self, code: KeyCode) -> Option<EscalationAnswer> {
        if let Some(comment) = &mut self.comment {
            return match code {
                KeyCode::Enter => {
                    let text = comment.trim();
                    let comment = (!text.is_empty()).then(|| text.to_string());
                    Some(EscalationAnswer::Deny { comment })
                }
                KeyCode::Esc => Some(EscalationAnswer::Deny { comment: None }),
                KeyCode::Backspace => {
                    comment.pop();
                    None
                }
                KeyCode::Char(c) => {
                    let mut buf = [0; 4];
                    self.type_text(c.encode_utf8(&mut buf));
                    None
                }
                _ => None,
            };
        }
        self.enter_unmoved = false;
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.moved |= code == KeyCode::Up;
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.moved |= code == KeyCode::Down;
                self.cursor = (self.cursor + 1).min(ESCALATION_CHOICES - 1);
                None
            }
            KeyCode::Enter if !self.moved => {
                self.enter_unmoved = true;
                None
            }
            KeyCode::Enter => self.pick(self.cursor),
            KeyCode::Char(c @ '1'..='4') => self.pick(usize::from(c as u8 - b'1')),
            KeyCode::Char('y' | 'Y') => self.pick(0),
            KeyCode::Char('t' | 'T') => self.pick(1),
            KeyCode::Char('a' | 'A') => self.pick(2),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => self.pick(DENY_CHOICE),
            KeyCode::Char('s' | 'S') => Some(EscalationAnswer::Stop),
            _ => None,
        }
    }

    /// Type (or paste) `text` into an open comment field, on one line and
    /// without control characters. Does nothing while the field is closed.
    pub fn type_text(&mut self, text: &str) {
        let Some(comment) = &mut self.comment else {
            return;
        };
        push_line_text(comment, text, MAX_COMMENT_CHARS);
    }

    fn pick(&mut self, index: usize) -> Option<EscalationAnswer> {
        match index {
            0 => Some(EscalationAnswer::AllowOnce),
            1 => Some(EscalationAnswer::AllowForSession),
            2 => Some(EscalationAnswer::AllowAndAddRule),
            _ => {
                self.cursor = DENY_CHOICE;
                self.comment = Some(String::new());
                None
            }
        }
    }
}

/// Type (or paste) `text` into a one-line field `buf` of at most `max`
/// characters: a line break or tab is a space, control characters go,
/// what does not fit is dropped (the deny comment, the `/permissions`
/// statement field).
pub(super) fn push_line_text(buf: &mut String, text: &str, max: usize) {
    let room = max.saturating_sub(buf.chars().count());
    buf.extend(
        text.chars()
            .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
            .filter(|c| !c.is_control())
            .take(room),
    );
}

/// What the user chose in the "Plan ready" dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanChoice {
    /// Switch to accept-edits mode and tell the agent to proceed.
    Proceed,
    /// Stay in plan mode (the dialog closes).
    KeepPlanning,
    /// Switch to manual mode and tell the agent to proceed.
    ProceedManual,
}

/// The rows of the "Plan ready" dialog, in order.
pub const PLAN_CHOICES: [(PlanChoice, &str); 3] = [
    (
        PlanChoice::Proceed,
        "Proceed (switch to accept edits and send \"Proceed with the plan\")",
    ),
    (PlanChoice::KeepPlanning, "Keep planning (Esc)"),
    (PlanChoice::ProceedManual, "Proceed in manual mode"),
];

/// The "Plan ready" dialog, shown when a turn ends in plan mode with an
/// answer: the highlighted row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanDialog {
    /// Index into [`PLAN_CHOICES`].
    pub cursor: usize,
    /// The cursor was moved with ↑/↓: Enter answers the row it is on
    /// (until then Enter answers nothing, as in the approval dialog).
    pub moved: bool,
    /// Enter was pressed before the cursor was moved.
    pub enter_unmoved: bool,
}

impl PlanDialog {
    /// A key: arrows (and `j`/`k`) move, Enter picks the highlighted row,
    /// `1`-`3` pick directly, Esc keeps planning and Ctrl-C closes. A held
    /// key only moves; other Ctrl/Alt keys do nothing.
    pub fn key(&mut self, key: &KeyEvent) -> ModalKey<PlanChoice> {
        if let Some(answer) = ctrl_c_answer(key) {
            return answer;
        }
        if ignored_key(key) {
            return ModalKey::Stay;
        }
        self.choice(key.code)
            .map_or(ModalKey::Stay, ModalKey::Choose)
    }

    fn choice(&mut self, code: KeyCode) -> Option<PlanChoice> {
        self.enter_unmoved = false;
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.moved |= code == KeyCode::Up;
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.moved |= code == KeyCode::Down;
                self.cursor = (self.cursor + 1).min(PLAN_CHOICES.len() - 1);
                None
            }
            KeyCode::Enter if !self.moved => {
                self.enter_unmoved = true;
                None
            }
            KeyCode::Enter => PLAN_CHOICES.get(self.cursor).map(|(choice, _)| *choice),
            KeyCode::Char(c @ '1'..='3') => PLAN_CHOICES
                .get(usize::from(c as u8 - b'1'))
                .map(|(choice, _)| *choice),
            KeyCode::Esc => Some(PlanChoice::KeepPlanning),
            _ => None,
        }
    }
}

/// What the user chose in the attached `/quit` dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitChoice {
    /// Leave the agent running (the default).
    Detach,
    /// Stop the daemon.
    Terminate,
}

/// The attached `/quit` dialog: detach or terminate, detach highlighted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuitDialog {
    /// Why it asks.
    pub reason: String,
    /// A turn is running (terminating interrupts it).
    pub running: bool,
    /// 0: detach, 1: terminate.
    pub cursor: usize,
}

impl QuitDialog {
    /// A dialog with detach highlighted.
    #[must_use]
    pub fn new(reason: String, running: bool) -> Self {
        Self {
            reason,
            running,
            cursor: 0,
        }
    }

    /// The rows, in order.
    fn rows(&self) -> [(QuitChoice, String); 2] {
        let terminate = if self.running {
            "Terminate: stop the daemon and retire every agent (interrupts the running turn; conversations stay)"
        } else {
            "Terminate: stop the daemon and retire every agent (conversations stay)"
        };
        [
            (
                QuitChoice::Detach,
                "Detach: keep this agent running (rkat-tui -c reattaches)".to_string(),
            ),
            (QuitChoice::Terminate, terminate.to_string()),
        ]
    }

    /// A key: arrows (and `j`/`k`) move, Enter picks the highlighted row,
    /// `1`/`d` detach, `2`/`t` terminate, Esc and Ctrl-C cancel the quit.
    /// A held key only moves; other Ctrl/Alt keys do nothing.
    pub fn key(&mut self, key: &KeyEvent) -> ModalKey<QuitChoice> {
        if let Some(answer) = ctrl_c_answer(key) {
            return answer;
        }
        if ignored_key(key) {
            return ModalKey::Stay;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.cursor = 0;
                ModalKey::Stay
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor = 1;
                ModalKey::Stay
            }
            KeyCode::Enter => ModalKey::Choose(self.rows()[self.cursor.min(1)].0),
            KeyCode::Char('1' | 'd') => ModalKey::Choose(QuitChoice::Detach),
            KeyCode::Char('2' | 't') => ModalKey::Choose(QuitChoice::Terminate),
            KeyCode::Esc => ModalKey::Close,
            _ => ModalKey::Stay,
        }
    }
}

/// Session picker for `/resume` without a reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumePicker {
    /// `(session ref, label)` pairs, newest first.
    pub items: Vec<(String, String)>,
    /// The highlighted session.
    pub cursor: ListCursor,
}

impl ResumePicker {
    /// A picker over `items` (newest first) with the newest highlighted.
    #[must_use]
    pub fn new(items: Vec<(String, String)>) -> Self {
        Self {
            items,
            cursor: ListCursor::default(),
        }
    }

    /// The selected session ref.
    pub fn selected_ref(&self) -> Option<&str> {
        self.items.get(self.cursor.index()).map(|(r, _)| r.as_str())
    }

    /// ↑/↓ (or k/j) move, Enter resumes the highlighted session, Esc, `q`
    /// or Ctrl-C close. A held key only moves.
    pub fn key(&mut self, key: &KeyEvent) -> ModalKey<String> {
        if let Some(answer) = ctrl_c_answer(key) {
            return answer;
        }
        let held = is_held(key);
        match key.code {
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') if held => ModalKey::Stay,
            KeyCode::Up | KeyCode::Char('k') => {
                self.cursor.up();
                ModalKey::Stay
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor.down(self.items.len());
                ModalKey::Stay
            }
            KeyCode::Enter => self
                .selected_ref()
                .map_or(ModalKey::Close, |r| ModalKey::Choose(r.to_string())),
            KeyCode::Esc | KeyCode::Char('q') => ModalKey::Close,
            _ => ModalKey::Stay,
        }
    }
}

/// Model picker for `/model` without an id: one group per endpoint (the
/// current endpoint and model first), narrowed by a type-to-filter query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPicker {
    /// Every endpoint's models, in display order.
    pub groups: Vec<ModelGroup>,
    /// Case-insensitive substring filter over `endpoint:id` and display name.
    pub filter: String,
    /// The highlighted model among the filtered ones (across groups).
    pub cursor: ListCursor,
    /// It picks the classifier's model (`/auto model`), not the agent's.
    pub classifier: bool,
}

impl ModelPicker {
    /// A picker over `groups` with nothing filtered and the first model selected.
    pub fn new(groups: Vec<ModelGroup>) -> Self {
        Self {
            groups,
            filter: String::new(),
            cursor: ListCursor::default(),
            classifier: false,
        }
    }

    /// The same picker for the classifier's model.
    #[must_use]
    pub fn for_classifier(mut self) -> Self {
        self.classifier = true;
        self
    }

    fn matches(needle: &str, group: &ModelGroup, entry: &ModelEntry) -> bool {
        needle.is_empty()
            || format!("{}:{}", group.endpoint, entry.id)
                .to_lowercase()
                .contains(needle)
            || entry
                .display_name
                .as_deref()
                .is_some_and(|name| name.to_lowercase().contains(needle))
    }

    /// The models matching the filter with their group, in display order.
    pub fn filtered(&self) -> impl Iterator<Item = (&ModelGroup, &ModelEntry)> {
        let needle = self.filter.to_lowercase();
        self.groups.iter().flat_map(move |group| {
            let needle = needle.clone();
            group
                .models
                .iter()
                .filter(move |entry| Self::matches(&needle, group, entry))
                .map(move |entry| (group, entry))
        })
    }

    /// Move the selection up.
    pub fn up(&mut self) {
        self.cursor.up();
    }

    /// Move the selection down.
    pub fn down(&mut self) {
        let len = self.filtered().count();
        self.cursor.down(len);
    }

    /// Append a character to the filter and select the first match.
    pub fn push_filter(&mut self, c: char) {
        self.filter.push(c);
        self.cursor = ListCursor::default();
    }

    /// Remove the filter's last character and select the first match.
    pub fn pop_filter(&mut self) {
        self.filter.pop();
        self.cursor = ListCursor::default();
    }

    /// The selected model as `endpoint:id`, `None` when nothing matches.
    pub fn selected_ref(&self) -> Option<String> {
        self.filtered()
            .nth(self.cursor.index())
            .map(|(group, entry)| format!("{}:{}", group.endpoint, entry.id))
    }

    /// ↑/↓ move, typing filters (Backspace deletes), Enter picks the
    /// highlighted model (nothing when none matches), Esc or Ctrl-C close.
    /// A held key only moves and edits the filter.
    pub fn key(&mut self, key: &KeyEvent) -> ModalKey<String> {
        if let Some(answer) = ctrl_c_answer(key) {
            return answer;
        }
        match key.code {
            KeyCode::Enter | KeyCode::Esc if is_held(key) => ModalKey::Stay,
            KeyCode::Esc => ModalKey::Close,
            KeyCode::Up => {
                self.up();
                ModalKey::Stay
            }
            KeyCode::Down => {
                self.down();
                ModalKey::Stay
            }
            KeyCode::Backspace => {
                self.pop_filter();
                ModalKey::Stay
            }
            KeyCode::Enter => self.selected_ref().map_or(ModalKey::Stay, ModalKey::Choose),
            KeyCode::Char(c) if !has_ctrl_or_alt(key) => {
                self.push_filter(c);
                ModalKey::Stay
            }
            _ => ModalKey::Stay,
        }
    }

    /// Warnings of every group (shown as notices when the picker cannot open).
    pub fn warnings(groups: &[ModelGroup]) -> impl Iterator<Item = String> + '_ {
        groups.iter().filter_map(|group| {
            group
                .warning
                .as_ref()
                .map(|warning| format!("{}: {warning}", group.endpoint))
        })
    }
}

/// An open modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    /// A tool call awaiting a human decision.
    Escalation {
        /// Escalation id to answer.
        id: EscalationId,
        /// What to show.
        prompt: RecordedPrompt,
        /// How many more escalations are queued behind this one.
        queued_behind: usize,
        /// The highlighted choice.
        dialog: EscalationDialog,
    },
    /// Session picker.
    Resume(ResumePicker),
    /// Model picker.
    Model(ModelPicker),
    /// A list picker (`/schedules`, `/permissions`, `/helpers`, `/mode`,
    /// `/memory`, `/wire`, `/unwire`, `/agents`).
    List(ListPicker),
    /// The `?` shortcut panel: the keys part of the help.
    Keys,
    /// A turn ended in plan mode with an answer: proceed or keep planning.
    PlanReady(PlanDialog),
    /// `/quit` asks whether to detach or stop the daemon.
    Quit(QuitDialog),
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::models::ModelSource;

    #[test]
    fn picker_navigation_is_clamped() {
        let mut p = ResumePicker::new(vec![("a".into(), "x".into()), ("b".into(), "y".into())]);
        let press = |code| KeyEvent::from(code);
        assert_eq!(p.key(&press(KeyCode::Up)), ModalKey::Stay);
        assert_eq!(p.selected_ref(), Some("a"));
        p.key(&press(KeyCode::Down));
        p.key(&press(KeyCode::Char('j')));
        assert_eq!(p.selected_ref(), Some("b"));
        assert_eq!(p.key(&press(KeyCode::Enter)), ModalKey::Choose("b".into()));
        assert_eq!(p.key(&press(KeyCode::Char('q'))), ModalKey::Close);
        assert_eq!(
            ResumePicker::new(Vec::new()).key(&press(KeyCode::Enter)),
            ModalKey::Close
        );
    }

    /// Every dialog closes on Ctrl-C and ignores a held one; the model
    /// picker's Enter with nothing matching stays open.
    #[test]
    fn dialogs_own_ctrl_c_and_held_keys() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let mut held_ctrl_c = ctrl_c;
        held_ctrl_c.kind = KeyEventKind::Repeat;
        let mut resume = ResumePicker::new(vec![("a".into(), "x".into())]);
        assert_eq!(resume.key(&held_ctrl_c), ModalKey::Stay);
        assert_eq!(resume.key(&ctrl_c), ModalKey::Close);
        let mut models = sample_models();
        assert_eq!(models.key(&held_ctrl_c), ModalKey::Stay);
        assert_eq!(models.key(&ctrl_c), ModalKey::Close);
        for c in "zzz".chars() {
            models.key(&KeyEvent::from(KeyCode::Char(c)));
        }
        assert_eq!(models.key(&KeyEvent::from(KeyCode::Enter)), ModalKey::Stay);
        let mut plan = PlanDialog::default();
        assert_eq!(plan.key(&held_ctrl_c), ModalKey::Stay);
        assert_eq!(plan.key(&ctrl_c), ModalKey::Close);
        assert_eq!(
            plan.key(&KeyEvent::from(KeyCode::Esc)),
            ModalKey::Choose(PlanChoice::KeepPlanning)
        );
        let mut escalation = EscalationDialog::default();
        assert_eq!(escalation.key(&held_ctrl_c), ModalKey::Stay);
        assert_eq!(escalation.key(&ctrl_c), ModalKey::Close);
        let mut held_y = KeyEvent::from(KeyCode::Char('y'));
        held_y.kind = KeyEventKind::Repeat;
        assert_eq!(escalation.key(&held_y), ModalKey::Stay);
        assert_eq!(
            escalation.key(&KeyEvent::from(KeyCode::Char('y'))),
            ModalKey::Choose(EscalationAnswer::AllowOnce)
        );
    }

    pub(super) fn model(id: &str, name: Option<&str>, source: ModelSource) -> ModelEntry {
        ModelEntry {
            id: id.into(),
            display_name: name.map(Into::into),
            source,
            context_window: None,
            max_output_tokens: None,
            capabilities: Default::default(),
        }
    }

    pub(super) fn group(endpoint: &str, models: Vec<ModelEntry>) -> ModelGroup {
        ModelGroup {
            endpoint: endpoint.into(),
            dialect: crate::endpoints::WireDialect::Anthropic,
            base_url: None,
            models,
            warning: None,
        }
    }

    fn sample_models() -> ModelPicker {
        ModelPicker::new(vec![
            group(
                "anthropic",
                vec![
                    model(
                        "claude-sonnet-4-6",
                        Some("Claude Sonnet 4.6"),
                        ModelSource::Current,
                    ),
                    model(
                        "claude-opus-4-6",
                        Some("Claude Opus 4.6"),
                        ModelSource::Provider,
                    ),
                ],
            ),
            group(
                "litellm",
                vec![model("claude-haiku-4-5", None, ModelSource::Provider)],
            ),
        ])
    }

    #[test]
    fn model_picker_filters_by_endpoint_id_and_display_name() {
        let mut p = sample_models();
        for c in "OPUS".chars() {
            p.push_filter(c);
        }
        assert_eq!(
            p.selected_ref().as_deref(),
            Some("anthropic:claude-opus-4-6")
        );
        assert_eq!(p.filtered().count(), 1);
        p.filter.clear();
        for c in "4.6".chars() {
            p.push_filter(c);
        }
        let ids: Vec<&str> = p.filtered().map(|(_, e)| e.id.as_str()).collect();
        assert_eq!(ids, vec!["claude-sonnet-4-6", "claude-opus-4-6"]);
        p.filter.clear();
        for c in "litellm:".chars() {
            p.push_filter(c);
        }
        assert_eq!(
            p.selected_ref().as_deref(),
            Some("litellm:claude-haiku-4-5")
        );
    }

    #[test]
    fn model_picker_navigation_spans_groups_and_clamps() {
        let mut p = sample_models();
        p.down();
        p.down();
        p.down();
        assert_eq!(
            p.selected_ref().as_deref(),
            Some("litellm:claude-haiku-4-5")
        );
        p.push_filter('o');
        assert_eq!(p.cursor.index(), 0);
        p.down();
        assert_eq!(
            p.selected_ref().as_deref(),
            Some("anthropic:claude-opus-4-6")
        );
        p.push_filter('x');
        assert_eq!(p.selected_ref(), None);
        p.pop_filter();
        assert_eq!(
            p.selected_ref().as_deref(),
            Some("anthropic:claude-sonnet-4-6")
        );
        p.up();
        assert_eq!(p.cursor.index(), 0);
    }

    #[test]
    fn escalation_dialog_moves_clamped_and_picks() {
        let deny = |comment: Option<&str>| EscalationAnswer::Deny {
            comment: comment.map(str::to_string),
        };
        let mut d = EscalationDialog::default();
        assert_eq!(d.answer(KeyCode::Up), None);
        assert_eq!(d.cursor, 0);
        assert_eq!(d.answer(KeyCode::Enter), Some(EscalationAnswer::AllowOnce));
        assert_eq!(d.answer(KeyCode::Char('k')), None);
        for _ in 0..5 {
            assert_eq!(d.answer(KeyCode::Down), None);
        }
        assert_eq!(d.cursor, 3);
        assert_eq!(d.answer(KeyCode::Char('k')), None);
        assert_eq!(
            d.answer(KeyCode::Enter),
            Some(EscalationAnswer::AllowAndAddRule)
        );
        for (c, answer) in [
            ('1', EscalationAnswer::AllowOnce),
            ('2', EscalationAnswer::AllowForSession),
            ('3', EscalationAnswer::AllowAndAddRule),
            ('y', EscalationAnswer::AllowOnce),
            ('t', EscalationAnswer::AllowForSession),
            ('a', EscalationAnswer::AllowAndAddRule),
            ('s', EscalationAnswer::Stop),
        ] {
            assert_eq!(d.answer(KeyCode::Char(c)), Some(answer), "{c}");
        }
        assert_eq!(d.answer(KeyCode::Char('5')), None);
        assert_eq!(d.answer(KeyCode::Char('x')), None);
        assert_eq!(d.comment, None);
        // `4`, `n`, Esc and Enter on row 4 open the comment field; there
        // Esc denies without a comment.
        for open in [
            KeyCode::Char('4'),
            KeyCode::Char('n'),
            KeyCode::Char('N'),
            KeyCode::Esc,
        ] {
            let mut d = EscalationDialog::default();
            assert_eq!(d.answer(open), None, "{open:?}");
            assert_eq!(d.cursor, 3);
            assert_eq!(d.comment.as_deref(), Some(""));
            assert_eq!(d.answer(KeyCode::Esc), Some(deny(None)));
        }
        let mut d = EscalationDialog {
            cursor: 3,
            moved: true,
            ..EscalationDialog::default()
        };
        assert_eq!(d.answer(KeyCode::Enter), None);
        // Empty (or blank) Enter denies without a comment.
        assert_eq!(d.answer(KeyCode::Char(' ')), None);
        assert_eq!(d.answer(KeyCode::Enter), Some(deny(None)));
    }

    #[test]
    fn the_comment_field_takes_text_and_denies_with_it() {
        let mut d = EscalationDialog::default();
        d.type_text("ignored while closed");
        assert_eq!(d.comment, None);
        d.answer(KeyCode::Char('4'));
        // Letters that answer elsewhere are text here, `s` included.
        for c in "use git mvx".chars() {
            assert_eq!(d.answer(KeyCode::Char(c)), None);
        }
        assert_eq!(d.answer(KeyCode::Backspace), None);
        assert!(d.accepts_repeat(KeyCode::Char('s')));
        assert!(d.accepts_repeat(KeyCode::Backspace));
        assert!(!d.accepts_repeat(KeyCode::Enter));
        assert!(!d.accepts_repeat(KeyCode::Esc));
        // A paste lands on one line, control characters dropped.
        d.type_text("\n and\tkeep\u{1b} history");
        assert_eq!(d.comment.as_deref(), Some("use git mv  and keep history"));
        assert_eq!(
            d.answer(KeyCode::Up),
            None,
            "arrows do not move in the field"
        );
        assert_eq!(d.cursor, 3);
        assert_eq!(
            d.answer(KeyCode::Enter),
            Some(EscalationAnswer::Deny {
                comment: Some("use git mv  and keep history".into())
            })
        );
        // The field is capped.
        let mut d = EscalationDialog::default();
        d.answer(KeyCode::Esc);
        d.type_text(&"x".repeat(MAX_COMMENT_CHARS + 10));
        assert_eq!(
            d.comment.as_ref().map(|c| c.chars().count()),
            Some(MAX_COMMENT_CHARS)
        );
        // Closed, only moves repeat.
        let d = EscalationDialog::default();
        assert!(d.accepts_repeat(KeyCode::Down));
        assert!(!d.accepts_repeat(KeyCode::Char('y')));
    }
}
