//! Drawing the modal dialogs: each dialog's lines from its state, in a
//! bordered box centered over the screen. The dialogs' state and keys are
//! the parent module's; nothing here changes them, except the expanded
//! escalation dialog's scroll bounds, which only a drawing knows.

use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as BorderBlock, Borders, Clear, Paragraph};

use super::{
    ENTER_NEEDS_A_ROW, EscalationDialog, HelpDialog, HelpView, MAX_ARGS_LINES, Modal, ModelPicker,
    PLAN_CHOICES, PlanDialog, QuitDialog, ResumePicker,
};
use crate::approval::escalation::RecordedPrompt;
use crate::endpoints::describe_endpoint;
use crate::models::{ModelEntry, ModelGroup, ModelSource};
use crate::ui::markdown::{sanitize, truncate_to_width, wrap_text};
use crate::ui::picker::list_picker_lines;
use crate::ui::render::fmt_ctx_tokens;

/// The rows of a list of one-line rows to draw in `rows` lines so
/// `selected` stays visible (the list scrolls once the selection passes
/// the last line). It only picks the range: the caller styles the rows,
/// and `selected` need not be a row (the model picker with no match).
#[must_use]
fn windowed(selected: usize, rows: usize) -> Range<usize> {
    let rows = rows.max(1);
    let start = selected.saturating_sub(rows - 1);
    start..start + rows
}

/// The rows (each one or more lines) that fit in `room` lines with row
/// `selected` among them and its first line highlighted: the list scrolls
/// once the selection passes the last line, as [`windowed`] does for rows
/// of one line, and the rows after the selection fill what is left.
pub(in crate::ui) fn window_lines(
    rows: Vec<Vec<Line<'static>>>,
    selected: usize,
    room: usize,
) -> Vec<Line<'static>> {
    let room = room.max(1);
    let height = |row: &Vec<Line<'static>>| row.len().max(1);
    let selected = selected.min(rows.len().saturating_sub(1));
    let mut start = selected;
    let mut used = rows.get(selected).map_or(0, height);
    while start > 0 && used + height(&rows[start - 1]) <= room {
        start -= 1;
        used += height(&rows[start]);
    }
    // Rows after the selection fill what is left.
    let mut lines = Vec::new();
    for (i, mut row) in rows.into_iter().enumerate().skip(start) {
        if lines.len() + row.len() > room && i != selected {
            break;
        }
        if i == selected
            && let Some(first) = row.first_mut()
        {
            first.style = first.style.add_modifier(Modifier::REVERSED);
        }
        lines.extend(row);
    }
    lines
}

/// Center a `width`×`height` rectangle inside `area` (clamped to fit).
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// Draw `modal` centered over `area` (an expanded escalation dialog
/// records how far its arguments can scroll at this size); `ignoring`: a
/// dialog that opened by itself dropped the keys typed into it
/// ([`crate::ui::Arming`]) and says so on its last line.
pub fn draw_modal(frame: &mut Frame<'_>, area: Rect, modal: &mut Modal, ignoring: bool) {
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
            let room = usize::from(area.height)
                .saturating_sub(16)
                .max(MAX_ARGS_LINES);
            (title, escalation_lines(prompt, dialog, inner_w, room))
        }
        Modal::Resume(picker) => (
            " Resume session ".to_string(),
            picker_lines(picker, inner_w, usize::from(area.height.saturating_sub(4))),
        ),
        Modal::Model(picker) => (
            if picker.classifier {
                " Classifier model ".to_string()
            } else {
                " Select model ".to_string()
            },
            model_picker_lines(picker, inner_w, usize::from(area.height.saturating_sub(4))),
        ),
        Modal::List(picker) => (
            picker.title(),
            list_picker_lines(picker, inner_w, usize::from(area.height.saturating_sub(4))),
        ),
        Modal::PlanReady(dialog) => (" Plan ready ".to_string(), plan_lines(*dialog, inner_w)),
        Modal::Quit(dialog) => (" Quit ".to_string(), quit_lines(dialog, inner_w)),
        Modal::Help(dialog) => {
            // Wider than the other dialogs: the help's columns.
            let width = area.width.saturating_sub(2).clamp(20, 104);
            let inner_w = usize::from(width.saturating_sub(2));
            let lines = help_lines(dialog, inner_w, usize::from(area.height.saturating_sub(2)));
            return draw_box(frame, area, width, " Help ", lines);
        }
    };
    let mut lines = lines;
    if ignoring {
        lines.push(Line::styled(
            truncate_to_width(crate::ui::KEYS_IGNORED, inner_w),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
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

/// The help dialog in `room` lines of `width`: the tab line, the window
/// of the view's wrapped text at the dialog's scroll, and the keys (with
/// where the window is when the text does not fit). It records how far the
/// text can scroll and a page's length at this size.
fn help_lines(dialog: &mut HelpDialog, width: usize, room: usize) -> Vec<Line<'static>> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let text = help_text_lines(dialog.view.text(), width);
    let total = text.len();
    let keys = "↑/↓ j/k PgUp/PgDn Home/End scroll · Tab switches · Esc, q or Ctrl-C closes";
    // A footer wrapped to the width, and the window of the rest; at least
    // one line of text however small the terminal.
    let footer_of = |position: Option<String>| {
        let footer = match position {
            Some(position) => format!("{position} · {keys}"),
            None => keys.to_string(),
        };
        wrap_text(&footer, width)
    };
    let mut footer = footer_of(None);
    let mut body = room.saturating_sub(1 + footer.len()).max(1);
    if total > body {
        // The position joins the footer, which may take a line more.
        let guess = footer_of(Some(format!("{total}/{total}"))).len();
        body = room.saturating_sub(1 + guess).max(1);
        dialog.scroll_max = total - body;
        dialog.scroll = dialog.scroll.min(dialog.scroll_max);
        let end = dialog.scroll + body;
        footer = footer_of(Some(format!("{}-{end}/{total}", dialog.scroll + 1)));
    } else {
        dialog.scroll_max = 0;
        dialog.scroll = 0;
    }
    dialog.page = body.saturating_sub(1).max(1);
    // A terminal too small for the whole footer keeps what fits of it.
    footer.truncate(room.saturating_sub(1 + body));
    let mut tabs = Vec::new();
    for view in [HelpView::Commands, HelpView::Keys] {
        let style = if view == dialog.view {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            dim
        };
        tabs.push(Span::styled(format!(" {} ", view.name()), style));
        tabs.push(Span::raw(" "));
    }
    let mut lines = vec![Line::from(tabs)];
    lines.extend(text.into_iter().skip(dialog.scroll).take(body));
    lines.extend(footer.into_iter().map(|l| Line::styled(l, dim)));
    lines
}

/// A help text wrapped to `width`: a heading (`Approval prompt:`) in
/// bold, a row's description too long for its column continued under it,
/// any other line wrapped whole. The text's first line is its own
/// heading, which the dialog's tab line already names.
fn help_text_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let heading = Style::default().add_modifier(Modifier::BOLD);
    let mut lines = Vec::new();
    for raw in text.lines().skip(1) {
        if raw.is_empty() {
            lines.push(Line::default());
            continue;
        }
        if !raw.starts_with(' ') {
            let style = if raw.ends_with(':') {
                heading
            } else {
                Style::default()
            };
            lines.extend(
                wrap_text(raw, width)
                    .into_iter()
                    .map(|l| Line::styled(l, style)),
            );
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
    lines
}

fn escalation_lines(
    prompt: &RecordedPrompt,
    dialog: &mut EscalationDialog,
    width: usize,
    room: usize,
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
    if let Some(call) = call_line(prompt) {
        lines.push(Line::from(vec![
            Span::styled("Call: ", dim),
            Span::raw(truncate_to_width(&sanitize(&call), width.saturating_sub(6))),
        ]));
    }
    if dialog.expanded {
        // Every line is reachable: a window of `room` lines at `scroll`.
        let text = prompt.args_full.as_deref().unwrap_or(&prompt.args_preview);
        let args = wrap_text(text, width);
        dialog.scroll_max = args.len().saturating_sub(room);
        dialog.scroll = dialog.scroll.min(dialog.scroll_max);
        let total = args.len();
        let end = (dialog.scroll + room).min(total);
        if dialog.scroll_max > 0 {
            lines.push(Line::styled(
                format!(
                    "(lines {}-{end} of {total}; PgUp/PgDn/Home/End scroll)",
                    dialog.scroll + 1,
                ),
                dim,
            ));
        }
        lines.extend(
            args.into_iter()
                .skip(dialog.scroll)
                .take(room)
                .map(Line::from),
        );
        if end < total {
            lines.push(Line::styled(
                format!("… ({} more lines; PgDn/End to see them)", total - end),
                dim,
            ));
        }
    } else {
        let args = wrap_text(&prompt.args_preview, width);
        let extra = args.len().saturating_sub(MAX_ARGS_LINES);
        lines.extend(args.into_iter().take(MAX_ARGS_LINES).map(Line::from));
        if extra > 0 {
            lines.push(Line::styled(
                format!("… ({extra} more lines; Ctrl+O shows all)"),
                dim,
            ));
        } else if prompt.args_full.is_some() {
            lines.push(Line::styled("(abbreviated; Ctrl+O shows all)", dim));
        }
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
    let session = match &prompt.session_grant {
        Some(grant) => format!("Yes, for this session ({grant})"),
        None => "Yes, for this session (no grant fits)".to_string(),
    };
    let choices = [
        "Yes, allow once".to_string(),
        session,
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
        if dialog.enter_unmoved {
            lines.extend(
                wrap_text(ENTER_NEEDS_A_ROW, width)
                    .into_iter()
                    .map(|l| Line::styled(l, Style::default().fg(Color::Yellow))),
            );
        }
        let hint = match (prompt.args_full.is_some(), prompt.agent.is_some()) {
            (true, _) => "↑/↓ then Enter · s stop · Ctrl+O all arguments · Ctrl+C stop all",
            // A helper's call: Stop stops that helper, not the turn.
            (false, true) => "↑/↓ then Enter · s stop this helper · Ctrl+C stop all",
            (false, false) => "↑/↓ then Enter · s stop the turn · Ctrl+C stop all",
        };
        lines.extend(
            wrap_text(hint, width)
                .into_iter()
                .map(|l| Line::styled(l, dim)),
        );
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
    if dialog.enter_unmoved {
        lines.extend(
            wrap_text(ENTER_NEEDS_A_ROW, width)
                .into_iter()
                .map(|l| Line::styled(l, Style::default().fg(Color::Yellow))),
        );
    }
    lines.push(Line::styled(
        "↑/↓ then Enter · 1-3 pick · Esc keep planning",
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

/// The call an approval dialog asks about as the transcript's line
/// shows it (`edit src/a.rs`), from the arguments the prompt carries as
/// JSON (`None` when they are not: a shell call's command, which the
/// dialog shows as it is, or a schedule's summary).
fn call_line(prompt: &RecordedPrompt) -> Option<String> {
    let text = prompt.args_full.as_deref().unwrap_or(&prompt.args_preview);
    let args = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .filter(serde_json::Value::is_object)?;
    let paths = crate::ui::transcript::tool_line::PathView::default();
    Some(crate::ui::transcript::tool_line::head(&prompt.tool, &args, &paths).text())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::tests::{group, model};
    use super::*;

    #[test]
    fn windowed_keeps_the_selection_on_the_last_line() {
        assert_eq!(windowed(0, 3), 0..3);
        assert_eq!(windowed(2, 3), 0..3);
        assert_eq!(windowed(3, 3), 1..4);
        assert_eq!(windowed(9, 3), 7..10);
        assert_eq!(windowed(4, 0), 4..5, "at least one row");
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
    fn centered_rect_fits() {
        let r = centered(Rect::new(0, 0, 10, 10), 20, 4);
        assert_eq!(r, Rect::new(0, 3, 10, 4));
    }
}
