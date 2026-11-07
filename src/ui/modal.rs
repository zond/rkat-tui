//! Modal dialogs: the escalation (approval) prompt, the resume picker, the
//! model picker, the shortcut panel and plan mode's "Plan ready" dialog.
//!
//! Modals are driven by plain data so the UI stays decoupled from the approval
//! engine and session modules.

use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as BorderBlock, Borders, Clear, Paragraph};

use super::markdown::{truncate_to_width, wrap_text};
use super::render::fmt_ctx_tokens;
use crate::endpoints::describe_endpoint;
use crate::models::{ModelEntry, ModelGroup, ModelSource};

/// Maximum lines of the args preview shown in the escalation dialog.
const MAX_ARGS_LINES: usize = 10;

/// Identifier of a pending escalation, assigned by the approval engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EscalationId(pub u64);

/// The user's answer to an escalation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscalationAnswer {
    /// Allow this call only.
    AllowOnce,
    /// Allow and persist a narrow allow rule.
    AllowAndAddRule,
    /// Deny this call.
    Deny {
        /// What to tell the agent to do instead (the tool error carries it).
        comment: Option<String>,
    },
    /// Deny and stop the turn.
    Stop,
}

/// What the escalation dialog shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscalationPrompt {
    /// The helper making the call (`None`: the session's own agent).
    pub agent: Option<String>,
    /// Tool name.
    pub tool: String,
    /// Human-readable preview of the arguments.
    pub args_preview: String,
    /// Classifier explanation, if any.
    pub hint: Option<String>,
    /// Why the policy asks without the classifier (a protected path), if so.
    pub reason: Option<String>,
    /// The allow rule "Yes, allow and add a rule" would add (`shell ls -la`),
    /// `None` when the action has no rule subject (the choice then allows
    /// once).
    pub rule: Option<String>,
}

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
    /// Whether a held (repeating) `code` may act: it moves the cursor or
    /// edits the comment, never answers.
    #[must_use]
    pub fn accepts_repeat(&self, code: KeyCode) -> bool {
        if self.comment.is_some() {
            matches!(code, KeyCode::Char(_) | KeyCode::Backspace)
        } else {
            matches!(
                code,
                KeyCode::Up | KeyCode::Down | KeyCode::Char('j' | 'k')
            )
        }
    }

    /// Apply a key: arrows (and `j`/`k`) move the cursor, Enter picks the
    /// highlighted choice, `1`-`3` pick directly, and the accelerators
    /// `y`, `a`, `n`/Esc and `s` answer at once. The deny choice opens the
    /// comment field; there Enter denies with the comment (none when
    /// empty) and Esc denies without one. `None`: no answer yet.
    pub fn key(&mut self, code: KeyCode) -> Option<EscalationAnswer> {
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
    /// Apply a key: arrows (and `j`/`k`) move, Enter picks the highlighted
    /// row, `1`-`3` pick directly and Esc keeps planning. `None`: no choice
    /// yet.
    pub fn key(&mut self, code: KeyCode) -> Option<PlanChoice> {
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
            KeyCode::Char(c @ '1'..='3') => {
                PLAN_CHOICES.get(usize::from(c as u8 - b'1')).map(|(choice, _)| *choice)
            }
            KeyCode::Esc => Some(PlanChoice::KeepPlanning),
            _ => None,
        }
    }
}

/// Session picker for `/resume` without a reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumePicker {
    /// `(session ref, label)` pairs, newest first.
    pub items: Vec<(String, String)>,
    /// Selected index.
    pub selected: usize,
}

impl ResumePicker {
    /// Move the selection up.
    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the selection down.
    pub fn down(&mut self) {
        if self.selected + 1 < self.items.len() {
            self.selected += 1;
        }
    }

    /// The selected session ref.
    pub fn selected_ref(&self) -> Option<&str> {
        self.items.get(self.selected).map(|(r, _)| r.as_str())
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
    /// Selected index into the filtered models (across groups).
    pub selected: usize,
}

impl ModelPicker {
    /// A picker over `groups` with nothing filtered and the first model selected.
    pub fn new(groups: Vec<ModelGroup>) -> Self {
        Self {
            groups,
            filter: String::new(),
            selected: 0,
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
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the selection down.
    pub fn down(&mut self) {
        if self.selected + 1 < self.filtered().count() {
            self.selected += 1;
        }
    }

    /// Append a character to the filter and select the first match.
    pub fn push_filter(&mut self, c: char) {
        self.filter.push(c);
        self.selected = 0;
    }

    /// Remove the filter's last character and select the first match.
    pub fn pop_filter(&mut self) {
        self.filter.pop();
        self.selected = 0;
    }

    /// The selected model as `endpoint:id`, `None` when nothing matches.
    pub fn selected_ref(&self) -> Option<String> {
        self.filtered()
            .nth(self.selected)
            .map(|(group, entry)| format!("{}:{}", group.endpoint, entry.id))
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
        prompt: EscalationPrompt,
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
    prompt: &EscalationPrompt,
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
    if let Some(reason) = &prompt.reason {
        lines.push(Line::default());
        lines.extend(
            wrap_text(&format!("Policy: {reason}"), width)
                .into_iter()
                .map(|l| Line::styled(l, Style::default().fg(Color::Yellow))),
        );
    }
    if let Some(hint) = &prompt.hint {
        lines.push(Line::default());
        lines.extend(
            wrap_text(&format!("Classifier: {hint}"), width)
                .into_iter()
                .map(|l| Line::styled(l, Style::default().fg(Color::Yellow))),
        );
    }
    lines.push(Line::default());
    let rule = match &prompt.rule {
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
    lines.push(Line::styled("Enter select · 1-3 pick · Esc keep planning", dim));
    lines
}

fn picker_lines(picker: &ResumePicker, width: usize, max_rows: usize) -> Vec<Line<'static>> {
    if picker.items.is_empty() {
        return vec![Line::styled(
            "no sessions found (Esc to close)",
            Style::default().add_modifier(Modifier::DIM),
        )];
    }
    let rows = max_rows.max(1);
    let start = picker.selected.saturating_sub(rows.saturating_sub(1));
    picker
        .items
        .iter()
        .enumerate()
        .skip(start)
        .take(rows)
        .map(|(i, (id, label))| {
            let text = truncate_to_width(&format!("{id}  {label}"), width);
            if i == picker.selected {
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
            if index == picker.selected {
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
    let rows = max_lines.saturating_sub(head.len() + 1).max(1);
    let start = selected_line.map_or(0, |line| line.saturating_sub(rows.saturating_sub(1)));
    head.extend(body.into_iter().skip(start).take(rows));
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
        let mut p = ResumePicker {
            items: vec![("a".into(), "x".into()), ("b".into(), "y".into())],
            selected: 0,
        };
        p.up();
        assert_eq!(p.selected_ref(), Some("a"));
        p.down();
        p.down();
        assert_eq!(p.selected_ref(), Some("b"));
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
        assert_eq!(p.selected, 0);
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
        assert_eq!(p.selected, 0);
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
        assert_eq!(d.key(KeyCode::Up), None);
        assert_eq!(d.cursor, 0);
        assert_eq!(d.key(KeyCode::Enter), Some(EscalationAnswer::AllowOnce));
        assert_eq!(d.key(KeyCode::Char('k')), None);
        for _ in 0..5 {
            assert_eq!(d.key(KeyCode::Down), None);
        }
        assert_eq!(d.cursor, 2);
        assert_eq!(d.key(KeyCode::Char('k')), None);
        assert_eq!(
            d.key(KeyCode::Enter),
            Some(EscalationAnswer::AllowAndAddRule)
        );
        for (c, answer) in [
            ('1', EscalationAnswer::AllowOnce),
            ('2', EscalationAnswer::AllowAndAddRule),
            ('y', EscalationAnswer::AllowOnce),
            ('a', EscalationAnswer::AllowAndAddRule),
            ('s', EscalationAnswer::Stop),
        ] {
            assert_eq!(d.key(KeyCode::Char(c)), Some(answer), "{c}");
        }
        assert_eq!(d.key(KeyCode::Char('4')), None);
        assert_eq!(d.key(KeyCode::Char('x')), None);
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
            assert_eq!(d.key(open), None, "{open:?}");
            assert_eq!(d.cursor, 2);
            assert_eq!(d.comment.as_deref(), Some(""));
            assert_eq!(d.key(KeyCode::Esc), Some(deny(None)));
        }
        let mut d = EscalationDialog {
            cursor: 2,
            comment: None,
        };
        assert_eq!(d.key(KeyCode::Enter), None);
        // Empty (or blank) Enter denies without a comment.
        assert_eq!(d.key(KeyCode::Char(' ')), None);
        assert_eq!(d.key(KeyCode::Enter), Some(deny(None)));
    }

    #[test]
    fn the_comment_field_takes_text_and_denies_with_it() {
        let mut d = EscalationDialog::default();
        d.type_text("ignored while closed");
        assert_eq!(d.comment, None);
        d.key(KeyCode::Char('3'));
        // Letters that answer elsewhere are text here, `s` included.
        for c in "use git mvx".chars() {
            assert_eq!(d.key(KeyCode::Char(c)), None);
        }
        assert_eq!(d.key(KeyCode::Backspace), None);
        assert!(d.accepts_repeat(KeyCode::Char('s')));
        assert!(d.accepts_repeat(KeyCode::Backspace));
        assert!(!d.accepts_repeat(KeyCode::Enter));
        assert!(!d.accepts_repeat(KeyCode::Esc));
        // A paste lands on one line, control characters dropped.
        d.type_text("\n and\tkeep\u{1b} history");
        assert_eq!(d.comment.as_deref(), Some("use git mv  and keep history"));
        assert_eq!(d.key(KeyCode::Up), None, "arrows do not move in the field");
        assert_eq!(d.cursor, 2);
        assert_eq!(
            d.key(KeyCode::Enter),
            Some(EscalationAnswer::Deny {
                comment: Some("use git mv  and keep history".into())
            })
        );
        // The field is capped.
        let mut d = EscalationDialog::default();
        d.key(KeyCode::Esc);
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
