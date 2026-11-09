//! Modal dialogs: the escalation (approval) prompt, the resume picker, the
//! model picker, the shortcut panel and plan mode's "Plan ready" dialog.
//!
//! Modals are driven by plain data so the UI stays decoupled from the
//! session modules. The escalation dialog shows the approval engine's
//! [`RecordedPrompt`] and answers with its [`EscalationAnswer`], the same
//! shapes the daemon's decisions carry to a client.

use ratatui::Frame;
use std::ops::Range;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as BorderBlock, Borders, Clear, Paragraph};

use super::markdown::{truncate_to_width, wrap_text};
use super::render::fmt_ctx_tokens;
pub use crate::approval::desk::EscalationAnswer;
use crate::approval::escalation::RecordedPrompt;
use crate::endpoints::describe_endpoint;
use crate::models::{ModelEntry, ModelGroup, ModelSource};

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

/// The rows of a list to draw in `rows` lines so `selected` stays
/// visible (the list scrolls once the selection passes the last line).
#[must_use]
pub fn windowed(selected: usize, rows: usize) -> Range<usize> {
    let rows = rows.max(1);
    let start = selected.saturating_sub(rows - 1);
    start..start + rows
}

/// Ctrl-C.
fn is_ctrl_c(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
}

/// A held key (auto-repeat): it may move, edit and type, never answer,
/// select or close.
fn is_held(key: &KeyEvent) -> bool {
    key.kind == KeyEventKind::Repeat
}

/// Ctrl or Alt held (not typed into a filter or comment).
fn has_ctrl_or_alt(key: &KeyEvent) -> bool {
    key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

/// Maximum lines of the args preview shown in the escalation dialog.
const MAX_ARGS_LINES: usize = 10;

/// Identifier of a pending escalation, assigned by the approval engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EscalationId(pub u64);

/// How many numbered choices the escalation dialog lists.
const ESCALATION_CHOICES: usize = 3;

/// Index of the "No, and tell the agent what to do instead" choice.
const DENY_CHOICE: usize = 2;

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
}

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
        if has_ctrl_or_alt(key) {
            return ModalKey::Stay;
        }
        self.answer(key.code)
            .map_or(ModalKey::Stay, ModalKey::Choose)
    }

    /// Whether a held (repeating) `code` may act: it moves the cursor or
    /// edits the comment, never answers.
    fn accepts_repeat(&self, code: KeyCode) -> bool {
        if self.comment.is_some() {
            matches!(code, KeyCode::Char(_) | KeyCode::Backspace)
        } else {
            matches!(code, KeyCode::Up | KeyCode::Down | KeyCode::Char('j' | 'k'))
        }
    }

    /// Apply a key: arrows (and `j`/`k`) move the cursor, Enter picks the
    /// highlighted choice, `1`-`3` pick directly, and the accelerators
    /// `y`, `a`, `n`/Esc and `s` answer at once. The deny choice opens the
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
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor = (self.cursor + 1).min(ESCALATION_CHOICES - 1);
                None
            }
            KeyCode::Enter => self.pick(self.cursor),
            KeyCode::Char(c @ '1'..='3') => self.pick(usize::from(c as u8 - b'1')),
            KeyCode::Char('y' | 'Y') => self.pick(0),
            KeyCode::Char('a' | 'A') => self.pick(1),
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
        let room = MAX_COMMENT_CHARS.saturating_sub(comment.chars().count());
        comment.extend(
            text.chars()
                .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
                .filter(|c| !c.is_control())
                .take(room),
        );
    }

    fn pick(&mut self, index: usize) -> Option<EscalationAnswer> {
        match index {
            0 => Some(EscalationAnswer::AllowOnce),
            1 => Some(EscalationAnswer::AllowAndAddRule),
            _ => {
                self.cursor = DENY_CHOICE;
                self.comment = Some(String::new());
                None
            }
        }
    }
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
}

impl PlanDialog {
    /// A key: arrows (and `j`/`k`) move, Enter picks the highlighted row,
    /// `1`-`3` pick directly, Esc keeps planning and Ctrl-C closes. A held
    /// key only moves; other Ctrl/Alt keys do nothing.
    pub fn key(&mut self, key: &KeyEvent) -> ModalKey<PlanChoice> {
        if is_ctrl_c(key) && !is_held(key) {
            return ModalKey::Close;
        }
        let moves = matches!(
            key.code,
            KeyCode::Up | KeyCode::Down | KeyCode::Char('j' | 'k')
        );
        if (is_held(key) && !moves) || has_ctrl_or_alt(key) {
            return ModalKey::Stay;
        }
        self.choice(key.code)
            .map_or(ModalKey::Stay, ModalKey::Choose)
    }

    fn choice(&mut self, code: KeyCode) -> Option<PlanChoice> {
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor = (self.cursor + 1).min(PLAN_CHOICES.len() - 1);
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
            "Terminate: stop the daemon (interrupts the running turn)"
        } else {
            "Terminate: stop the daemon"
        };
        [
            (
                QuitChoice::Detach,
                "Detach: keep the agent running (rkat-tui reattaches)".to_string(),
            ),
            (QuitChoice::Terminate, terminate.to_string()),
        ]
    }

    /// A key: arrows (and `j`/`k`) move, Enter picks the highlighted row,
    /// `1`/`d` detach, `2`/`t` terminate, Esc and Ctrl-C cancel the quit.
    /// A held key only moves; other Ctrl/Alt keys do nothing.
    pub fn key(&mut self, key: &KeyEvent) -> ModalKey<QuitChoice> {
        if is_ctrl_c(key) && !is_held(key) {
            return ModalKey::Close;
        }
        let moves = matches!(
            key.code,
            KeyCode::Up | KeyCode::Down | KeyCode::Char('j' | 'k')
        );
        if (is_held(key) && !moves) || has_ctrl_or_alt(key) {
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
        let held = is_held(key);
        if is_ctrl_c(key) {
            return if held {
                ModalKey::Stay
            } else {
                ModalKey::Close
            };
        }
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
}

impl ModelPicker {
    /// A picker over `groups` with nothing filtered and the first model selected.
    pub fn new(groups: Vec<ModelGroup>) -> Self {
        Self {
            groups,
            filter: String::new(),
            cursor: ListCursor::default(),
        }
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
        let held = is_held(key);
        if is_ctrl_c(key) {
            return if held {
                ModalKey::Stay
            } else {
                ModalKey::Close
            };
        }
        match key.code {
            KeyCode::Enter | KeyCode::Esc if held => ModalKey::Stay,
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
    /// The `?` shortcut panel: the keys part of the help.
    Keys,
    /// A turn ended in plan mode with an answer: proceed or keep planning.
    PlanReady(PlanDialog),
    /// Attach mode: `/quit` asks whether to detach or stop the daemon.
    Quit(QuitDialog),
}

/// Center a `width`×`height` rectangle inside `area` (clamped to fit).
pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// Draw `modal` centered over `area`.
pub fn draw_modal(frame: &mut Frame<'_>, area: Rect, modal: &Modal) {
    let width = area.width.saturating_sub(4).clamp(20, 80);
    let inner_w = usize::from(width.saturating_sub(2));
    let (title, lines) = match modal {
        Modal::Escalation {
            prompt,
            queued_behind,
            dialog,
            ..
        } => {
            let title = if *queued_behind > 0 {
                format!(" Approval needed (+{queued_behind} queued) ")
            } else {
                " Approval needed ".to_string()
            };
            (title, escalation_lines(prompt, dialog, inner_w))
        }
        Modal::Resume(picker) => (
            " Resume session ".to_string(),
            picker_lines(picker, inner_w, usize::from(area.height.saturating_sub(4))),
        ),
        Modal::Model(picker) => (
            " Select model ".to_string(),
            model_picker_lines(picker, inner_w, usize::from(area.height.saturating_sub(4))),
        ),
        Modal::PlanReady(dialog) => (" Plan ready ".to_string(), plan_lines(*dialog, inner_w)),
        Modal::Quit(dialog) => (" Quit ".to_string(), quit_lines(dialog, inner_w)),
        Modal::Keys => {
            // Wider than the other dialogs: the help's columns.
            let width = area.width.saturating_sub(2).clamp(20, 104);
            let inner_w = usize::from(width.saturating_sub(2));
            let lines = keys_lines(inner_w, usize::from(area.height.saturating_sub(2)));
            return draw_box(frame, area, width, " Keys (any key closes) ", lines);
        }
    };
    draw_box(frame, area, width, &title, lines);
}

/// A bordered box of `lines`, `width` wide, centered over `area`.
fn draw_box(frame: &mut Frame<'_>, area: Rect, width: u16, title: &str, lines: Vec<Line<'static>>) {
    let height = (lines.len() as u16).saturating_add(2);
    let rect = centered(area, width, height);
    frame.render_widget(Clear, rect);
    let block = BorderBlock::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(title.to_string());
    frame.render_widget(Paragraph::new(lines).block(block), rect);
}

/// Column where the help's descriptions start.
const HELP_DESC_COL: usize = 33;

/// The keys help, wrapped to `width` (a description too long for its
/// column continues under it) and cut to `max_lines` with a pointer to
/// `/help`.
fn keys_lines(width: usize, max_lines: usize) -> Vec<Line<'static>> {
    let heading = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut lines = Vec::new();
    // The box's title names the panel: skip the help's own "Keys:" heading.
    for raw in super::commands::keys_help().lines().skip(1) {
        if !raw.starts_with(' ') {
            lines.push(Line::styled(raw.to_string(), heading));
            continue;
        }
        let split = raw
            .char_indices()
            .nth(HELP_DESC_COL)
            .map(|(i, _)| i)
            .filter(|_| width > HELP_DESC_COL + 20);
        match split.map(|i| raw.split_at(i)) {
            Some((key, desc)) => {
                let mut pad = key.to_string();
                for (n, part) in wrap_text(desc, width - HELP_DESC_COL)
                    .into_iter()
                    .enumerate()
                {
                    if n > 0 {
                        pad = " ".repeat(HELP_DESC_COL);
                    }
                    lines.push(Line::from(format!("{pad}{part}")));
                }
            }
            None => lines.extend(wrap_text(raw, width).into_iter().map(Line::from)),
        }
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines.saturating_sub(1));
        lines.push(Line::styled("… more in /help", dim));
    }
    lines
}

fn escalation_lines(
    prompt: &RecordedPrompt,
    dialog: &EscalationDialog,
    width: usize,
) -> Vec<Line<'static>> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut lines = Vec::new();
    if let Some(agent) = &prompt.agent {
        lines.push(Line::from(vec![
            Span::styled("Helper: ", dim),
            Span::styled(
                agent.clone(),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled("Tool: ", dim),
        Span::styled(
            prompt.tool.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]));
    let args = wrap_text(&prompt.args_preview, width);
    let extra = args.len().saturating_sub(MAX_ARGS_LINES);
    lines.extend(args.into_iter().take(MAX_ARGS_LINES).map(Line::from));
    if extra > 0 {
        lines.push(Line::styled(format!("… ({extra} more lines)"), dim));
    }
    if let Some(reason) = &prompt.policy_reason {
        lines.push(Line::default());
        lines.extend(
            wrap_text(&format!("Policy: {reason}"), width)
                .into_iter()
                .map(|l| Line::styled(l, Style::default().fg(Color::Yellow))),
        );
    }
    if let Some(hint) = &prompt.classifier_hint {
        lines.push(Line::default());
        lines.extend(
            wrap_text(&format!("Classifier: {hint}"), width)
                .into_iter()
                .map(|l| Line::styled(l, Style::default().fg(Color::Yellow))),
        );
    }
    lines.push(Line::default());
    let rule = match prompt.rule() {
        Some(rule) => format!("Yes, allow and add a rule ({rule})"),
        None => "Yes, allow (no rule fits this action)".to_string(),
    };
    let choices = [
        "Yes, allow once".to_string(),
        rule,
        "No, and tell the agent what to do instead (Esc)".to_string(),
    ];
    let highlight = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    for (i, choice) in choices.iter().enumerate() {
        let (mark, style) = if i == dialog.cursor {
            ("❯ ", highlight)
        } else {
            ("  ", Style::default())
        };
        lines.push(Line::styled(
            truncate_to_width(&format!("{mark}{}. {choice}", i + 1), width),
            style,
        ));
    }
    lines.push(Line::default());
    let Some(comment) = &dialog.comment else {
        lines.push(Line::styled(
            "Enter select · s stop the turn · Ctrl+C stop all",
            dim,
        ));
        return lines;
    };
    lines.push(Line::from("Tell the agent what to do instead:"));
    // One line: the end of the text stays in view, with a block cursor.
    let room = width.saturating_sub(3);
    let skip = comment.chars().count().saturating_sub(room);
    let shown: String = comment.chars().skip(skip).collect();
    lines.push(Line::from(vec![
        Span::styled("› ", highlight),
        Span::raw(shown),
        Span::styled(" ", Style::default().add_modifier(Modifier::REVERSED)),
    ]));
    lines.extend(
        wrap_text(
            "Enter sends · empty Enter denies without a comment · Esc denies · Ctrl+C stop all",
            width,
        )
        .into_iter()
        .map(|l| Line::styled(l, dim)),
    );
    lines
}

fn quit_lines(dialog: &QuitDialog, width: usize) -> Vec<Line<'static>> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let highlight = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line<'static>> = wrap_text(
        &format!(
            "The agent runs in this workspace's daemon ({}). Keep it running, or stop it?",
            dialog.reason
        ),
        width,
    )
    .into_iter()
    .map(Line::from)
    .collect();
    lines.push(Line::default());
    for (i, (_, row)) in dialog.rows().iter().enumerate() {
        let (mark, style) = if i == dialog.cursor {
            ("❯ ", highlight)
        } else {
            ("  ", Style::default())
        };
        lines.push(Line::styled(
            truncate_to_width(&format!("{mark}{}. {row}", i + 1), width),
            style,
        ));
    }
    lines.push(Line::default());
    lines.push(Line::styled(
        "Enter select · d/1 detach · t/2 terminate · Esc stay",
        dim,
    ));
    lines
}

fn plan_lines(dialog: PlanDialog, width: usize) -> Vec<Line<'static>> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let highlight = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line<'static>> = wrap_text(
        "The agent answered in plan mode. Proceed with its plan?",
        width,
    )
    .into_iter()
    .map(Line::from)
    .collect();
    lines.push(Line::default());
    for (i, (_, choice)) in PLAN_CHOICES.iter().enumerate() {
        let (mark, style) = if i == dialog.cursor {
            ("❯ ", highlight)
        } else {
            ("  ", Style::default())
        };
        lines.push(Line::styled(
            truncate_to_width(&format!("{mark}{}. {choice}", i + 1), width),
            style,
        ));
    }
    lines.push(Line::default());
    lines.push(Line::styled(
        "Enter select · 1-3 pick · Esc keep planning",
        dim,
    ));
    lines
}

fn picker_lines(picker: &ResumePicker, width: usize, max_rows: usize) -> Vec<Line<'static>> {
    if picker.items.is_empty() {
        return vec![Line::styled(
            "no sessions found (Esc to close)",
            Style::default().add_modifier(Modifier::DIM),
        )];
    }
    let selected = picker.cursor.index();
    let window = windowed(selected, max_rows);
    picker
        .items
        .iter()
        .enumerate()
        .skip(window.start)
        .take(window.len())
        .map(|(i, (id, label))| {
            let text = truncate_to_width(&format!("{id}  {label}"), width);
            if i == selected {
                Line::styled(text, Style::default().add_modifier(Modifier::REVERSED))
            } else {
                Line::from(text)
            }
        })
        .collect()
}

/// Filter line, then per endpoint a header, its warning and its models,
/// then a key hint. At most `max_lines` lines; the list scrolls to keep the
/// selection visible.
fn model_picker_lines(picker: &ModelPicker, width: usize, max_lines: usize) -> Vec<Line<'static>> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut head = vec![if picker.filter.is_empty() {
        Line::styled("filter: (type to filter)", dim)
    } else {
        Line::from(vec![
            Span::styled("filter: ", dim),
            Span::raw(truncate_to_width(&picker.filter, width.saturating_sub(8))),
        ])
    }];
    let footer = Line::styled("↑/↓ move · Enter select · Esc close", dim);
    let needle = picker.filter.to_lowercase();
    let id_width = picker
        .filtered()
        .map(|(_, entry)| entry.id.chars().count())
        .max()
        .unwrap_or(0)
        .min(width / 2);
    let mut body: Vec<Line<'static>> = Vec::new();
    let mut selected_line = None;
    let mut index = 0;
    for group in &picker.groups {
        let matching: Vec<&ModelEntry> = group
            .models
            .iter()
            .filter(|entry| ModelPicker::matches(&needle, group, entry))
            .collect();
        if !needle.is_empty() && matching.is_empty() {
            continue;
        }
        body.push(Line::styled(
            truncate_to_width(&group_header(group), width),
            Style::default().fg(Color::Cyan),
        ));
        if let Some(warning) = &group.warning {
            body.extend(
                wrap_text(warning, width)
                    .into_iter()
                    .map(|l| Line::styled(l, Style::default().fg(Color::Yellow))),
            );
        } else if matching.is_empty() {
            body.push(Line::styled("no models", dim));
        }
        for entry in matching {
            let text = truncate_to_width(&model_row(entry, id_width), width);
            if index == picker.cursor.index() {
                selected_line = Some(body.len());
                body.push(Line::styled(
                    text,
                    Style::default().add_modifier(Modifier::REVERSED),
                ));
            } else if entry.source == ModelSource::Current {
                body.push(Line::styled(
                    text,
                    Style::default().add_modifier(Modifier::BOLD),
                ));
            } else {
                body.push(Line::from(text));
            }
            index += 1;
        }
    }
    if index == 0 && !needle.is_empty() {
        body.push(Line::styled("no models match", dim));
    }
    let window = windowed(
        selected_line.unwrap_or(0),
        max_lines.saturating_sub(head.len() + 1),
    );
    head.extend(body.into_iter().skip(window.start).take(window.len()));
    head.push(footer);
    head
}

/// `── litellm (openai dialect, http://localhost:4000) ──`.
fn group_header(group: &ModelGroup) -> String {
    format!(
        "── {} ({}) ──",
        group.endpoint,
        describe_endpoint(group.dialect, group.base_url.as_deref())
    )
}

/// `id  Display Name  200k ctx  current`, with the id padded to `id_width`.
fn model_row(entry: &ModelEntry, id_width: usize) -> String {
    let mut row = format!("{:<id_width$}", entry.id);
    if let Some(name) = &entry.display_name {
        row.push_str("  ");
        row.push_str(name);
    }
    if let Some(window) = entry.context_window {
        row.push_str("  ");
        row.push_str(&fmt_ctx_tokens(window));
        row.push_str(" ctx");
    }
    if entry.source == ModelSource::Current {
        row.push_str("  current");
    }
    row.trim_end().to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

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

    #[test]
    fn windowed_keeps_the_selection_on_the_last_line() {
        assert_eq!(windowed(0, 3), 0..3);
        assert_eq!(windowed(2, 3), 0..3);
        assert_eq!(windowed(3, 3), 1..4);
        assert_eq!(windowed(9, 3), 7..10);
        assert_eq!(windowed(4, 0), 4..5, "at least one row");
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

    fn model(id: &str, name: Option<&str>, source: ModelSource) -> ModelEntry {
        ModelEntry {
            id: id.into(),
            display_name: name.map(Into::into),
            source,
            context_window: None,
            max_output_tokens: None,
        }
    }

    fn group(endpoint: &str, models: Vec<ModelEntry>) -> ModelGroup {
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
    fn model_row_shows_name_window_and_current() {
        let mut entry = model(
            "gemini-3.1-pro-preview",
            Some("Gemini 3.1 Pro"),
            ModelSource::Current,
        );
        entry.context_window = Some(1_048_576);
        assert_eq!(
            model_row(&entry, 24),
            "gemini-3.1-pro-preview    Gemini 3.1 Pro  1M ctx  current"
        );
        let bare = model("gpt-5.4", None, ModelSource::Provider);
        assert_eq!(model_row(&bare, 7), "gpt-5.4");
    }

    #[test]
    fn group_header_names_endpoint_dialect_and_url() {
        let mut g = group("litellm", Vec::new());
        g.dialect = crate::endpoints::WireDialect::OpenAi;
        g.base_url = Some("http://localhost:4000".into());
        assert_eq!(
            group_header(&g),
            "── litellm (openai dialect, http://localhost:4000) ──"
        );
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
        assert_eq!(d.cursor, 2);
        assert_eq!(d.answer(KeyCode::Char('k')), None);
        assert_eq!(
            d.answer(KeyCode::Enter),
            Some(EscalationAnswer::AllowAndAddRule)
        );
        for (c, answer) in [
            ('1', EscalationAnswer::AllowOnce),
            ('2', EscalationAnswer::AllowAndAddRule),
            ('y', EscalationAnswer::AllowOnce),
            ('a', EscalationAnswer::AllowAndAddRule),
            ('s', EscalationAnswer::Stop),
        ] {
            assert_eq!(d.answer(KeyCode::Char(c)), Some(answer), "{c}");
        }
        assert_eq!(d.answer(KeyCode::Char('4')), None);
        assert_eq!(d.answer(KeyCode::Char('x')), None);
        assert_eq!(d.comment, None);
        // `3`, `n`, Esc and Enter on row 3 open the comment field; there
        // Esc denies without a comment.
        for open in [
            KeyCode::Char('3'),
            KeyCode::Char('n'),
            KeyCode::Char('N'),
            KeyCode::Esc,
        ] {
            let mut d = EscalationDialog::default();
            assert_eq!(d.answer(open), None, "{open:?}");
            assert_eq!(d.cursor, 2);
            assert_eq!(d.comment.as_deref(), Some(""));
            assert_eq!(d.answer(KeyCode::Esc), Some(deny(None)));
        }
        let mut d = EscalationDialog {
            cursor: 2,
            comment: None,
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
        d.answer(KeyCode::Char('3'));
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
        assert_eq!(d.cursor, 2);
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

    #[test]
    fn centered_rect_fits() {
        let r = centered(Rect::new(0, 0, 10, 10), 20, 4);
        assert_eq!(r, Rect::new(0, 3, 10, 4));
    }
}
