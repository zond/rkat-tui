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
use crate::pricing::PriceTable;
use crate::ui::markdown::{display_width, sanitize, truncate_to_width, wrap_text};
use crate::ui::picker::list_picker_lines_padded;
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
/// ([`crate::ui::Arming`]) and says so on its last line. The model
/// pickers show each model's price from `prices`.
pub fn draw_modal(
    frame: &mut Frame<'_>,
    area: Rect,
    modal: &mut Modal,
    ignoring: bool,
    prices: &PriceTable,
) {
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
            model_picker_lines(
                picker,
                prices,
                inner_w,
                usize::from(area.height.saturating_sub(4)),
            ),
        ),
        Modal::List(picker) => {
            let room = usize::from(area.height.saturating_sub(4));
            let steady = picker.steady_lines(inner_w, room);
            (
                picker.title(),
                list_picker_lines_padded(picker, inner_w, room, steady),
            )
        }
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
    // The box keeps the height of the tallest view: Tab leaves its
    // outline, and the footer its row, where they are.
    let tallest = [HelpView::Commands, HelpView::Keys]
        .into_iter()
        .map(|view| help_text_lines(view.text(), width).len())
        .max()
        .unwrap_or(total);
    let steady = (1 + tallest + footer_of(None).len()).min(room);
    let blank = steady.saturating_sub(lines.len() + footer.len());
    lines.extend(std::iter::repeat_n(Line::default(), blank));
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

/// What a model row says of its price: none known.
const NO_PRICE: &str = "no price";

/// A price per million tokens as short as it reads: `$3`, `$0.25`,
/// `$0.075` (at most six decimals, trailing zeros dropped).
fn fmt_per_mtok(usd: f64) -> String {
    let digits = if usd >= 0.01 { 4 } else { 6 };
    let text = format!("{usd:.digits$}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    format!("${text}")
}

/// The price column of a model on `endpoint`: `$3 / $15` (input / output
/// per million tokens), `~` before it when the price is an estimate (a
/// stale community list, as the cost readout marks it); `None` when no
/// source knows the model.
fn price_text(prices: &PriceTable, endpoint: &str, id: &str) -> Option<String> {
    let (price, source) = prices.lookup(&format!("{endpoint}:{id}"))?;
    Some(format!(
        "{}{} / {}",
        if source.is_estimate() { "~" } else { "" },
        fmt_per_mtok(price.input),
        fmt_per_mtok(price.output)
    ))
}

/// Every model of the picker, filtered or not (the price column stays
/// where it is while the filter is typed).
fn all_models(picker: &ModelPicker) -> impl Iterator<Item = (&ModelGroup, &ModelEntry)> {
    picker
        .groups
        .iter()
        .flat_map(|group| group.models.iter().map(move |entry| (group, entry)))
}

/// The width of the price column: `None` when no source prices any of
/// the picker's models (the column is left out rather than filled with
/// "no price").
fn price_width(picker: &ModelPicker, prices: &PriceTable) -> Option<usize> {
    all_models(picker)
        .filter_map(|(group, entry)| price_text(prices, &group.endpoint, &entry.id))
        .map(|text| display_width(&text))
        .max()
        .map(|width| width.max(NO_PRICE.len()))
}

/// Filter line, then per endpoint a header, its warning and its models
/// (each with its price from `prices` in a column of its own, when any
/// is known), then a key hint. At most `max_lines` lines; the list
/// scrolls to keep the selection visible.
fn model_picker_lines(
    picker: &ModelPicker,
    prices: &PriceTable,
    width: usize,
    max_lines: usize,
) -> Vec<Line<'static>> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut head = vec![if picker.filter.is_empty() {
        Line::styled("filter: (type to filter)", dim)
    } else {
        Line::from(vec![
            Span::styled("filter: ", dim),
            Span::raw(truncate_to_width(&picker.filter, width.saturating_sub(8))),
        ])
    }];
    let needle = picker.filter.to_lowercase();
    let id_width = all_models(picker)
        .map(|(_, entry)| entry.id.chars().count())
        .max()
        .unwrap_or(0)
        .min(width / 2);
    // The price column starts after the longest row that fits beside it;
    // a row longer than that keeps its text and loses its price, and a
    // box too narrow for an id beside the column has none.
    let column = price_width(picker, prices).and_then(|price_w| {
        let longest = all_models(picker)
            .map(|(_, entry)| display_width(&model_row(entry, id_width)))
            .max()
            .unwrap_or(0);
        let start = longest.min(width.checked_sub(price_w + 2)?);
        (start >= id_width).then_some((start, price_w))
    });
    let footer = Line::styled(
        if column.is_some() {
            "↑/↓ move · Enter select · Esc close · price: input / output per Mtok"
        } else {
            "↑/↓ move · Enter select · Esc close"
        },
        dim,
    );
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
            let row = model_row(entry, id_width);
            let mut line = match column {
                Some((start, price_w)) if display_width(&row) <= start => {
                    let pad = " ".repeat(start - display_width(&row) + 2);
                    let price = match price_text(prices, &group.endpoint, &entry.id) {
                        Some(price) => Span::raw(format!("{price:>price_w$}")),
                        None => Span::styled(format!("{NO_PRICE:>price_w$}"), dim),
                    };
                    Line::from(vec![Span::raw(format!("{row}{pad}")), price])
                }
                _ => Line::from(truncate_to_width(&row, width)),
            };
            if index == picker.cursor.index() {
                selected_line = Some(body.len());
                line.style = Style::default().add_modifier(Modifier::REVERSED);
            } else if entry.source == ModelSource::Current {
                line.style = Style::default().add_modifier(Modifier::BOLD);
            }
            body.push(line);
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

    fn text(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    /// `entries` priced in `tui.toml` (input, output per Mtok).
    fn table(entries: &[(&str, f64, f64)]) -> PriceTable {
        PriceTable::new(
            entries
                .iter()
                .map(|(id, input, output)| {
                    (
                        (*id).to_string(),
                        crate::config::PricingSection {
                            input: Some(*input),
                            output: Some(*output),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
        )
    }

    fn priced_picker() -> ModelPicker {
        ModelPicker::new(vec![
            group(
                "anthropic",
                vec![
                    model(
                        "claude-sonnet-4-6",
                        Some("Sonnet 4.6"),
                        ModelSource::Current,
                    ),
                    model("claude-mystery", None, ModelSource::Provider),
                ],
            ),
            group(
                "litellm",
                vec![model("gpt-5.4-mini", None, ModelSource::Provider)],
            ),
        ])
    }

    #[test]
    fn prices_per_mtok_read_as_short_as_they_can() {
        assert_eq!(fmt_per_mtok(3.0), "$3");
        assert_eq!(fmt_per_mtok(15.0), "$15");
        assert_eq!(fmt_per_mtok(0.25), "$0.25");
        assert_eq!(fmt_per_mtok(1.25), "$1.25");
        assert_eq!(fmt_per_mtok(0.075), "$0.075");
        assert_eq!(fmt_per_mtok(0.0375), "$0.0375");
        assert_eq!(fmt_per_mtok(0.000_15), "$0.00015");
        assert_eq!(fmt_per_mtok(0.0), "$0");
    }

    /// A priced row shows input / output per Mtok in a column of its
    /// own, right-aligned; a model no source prices says "no price"
    /// (dim); a LiteLLM endpoint's `/model/info` price counts; the footer
    /// names the unit once.
    #[test]
    fn model_rows_show_their_price_in_a_column() {
        let mut prices = table(&[("claude-sonnet-4-6", 3.0, 15.0)]);
        prices.set_endpoint_prices([(
            "gpt-5.4-mini".to_string(),
            crate::pricing::ModelPrice {
                input: 0.25,
                output: 2.0,
                cached_input: Some(0.025),
                cache_write: None,
            },
        )]);
        let picker = priced_picker();
        let lines = model_picker_lines(&picker, &prices, 70, 20);
        assert_eq!(
            text(&lines),
            vec![
                "filter: (type to filter)",
                "── anthropic (anthropic dialect) ──",
                "claude-sonnet-4-6  Sonnet 4.6  current    $3 / $15",
                "claude-mystery                            no price",
                "── litellm (anthropic dialect) ──",
                "gpt-5.4-mini                            $0.25 / $2",
                "↑/↓ move · Enter select · Esc close · price: input / output per Mtok",
            ]
        );
        let no_price = &lines[3].spans[1];
        assert_eq!(no_price.content.trim(), NO_PRICE);
        assert!(no_price.style.add_modifier.contains(Modifier::DIM));
        assert!(lines[2].style.add_modifier.contains(Modifier::REVERSED));
    }

    /// With no price known for any model the column and the unit are left
    /// out, and the rows are as they were.
    #[test]
    fn no_known_price_leaves_the_column_out() {
        let lines = model_picker_lines(&priced_picker(), &PriceTable::default(), 70, 20);
        assert_eq!(
            text(&lines)[2..4],
            ["claude-sonnet-4-6  Sonnet 4.6  current", "claude-mystery"]
        );
        assert_eq!(
            text(&lines).last().unwrap(),
            "↑/↓ move · Enter select · Esc close"
        );
    }

    /// A stale community list's price is marked an estimate, as the cost
    /// readout marks it.
    #[test]
    fn a_stale_price_is_marked_an_estimate() {
        let mut prices = PriceTable::default();
        let models = indexmap::IndexMap::from([(
            "claude-mystery".to_string(),
            crate::pricing::ModelPrice {
                input: 1.0,
                output: 5.0,
                cached_input: None,
                cache_write: None,
            },
        )]);
        prices.set_community(crate::pricing::CommunityPrices::new(models, 0, 10_000_000));
        let lines = text(&model_picker_lines(&priced_picker(), &prices, 70, 20));
        assert!(lines[3].ends_with("~$1 / $5"), "{lines:#?}");
    }

    /// Narrow, the price goes before any of the model's name: a row too
    /// long to have its price beside it loses the price, and a box too
    /// narrow for the id and the column has no column.
    #[test]
    fn the_price_is_what_goes_when_the_box_is_narrow() {
        let prices = table(&[
            ("claude-sonnet-4-6", 3.0, 15.0),
            ("claude-mystery", 1.0, 5.0),
        ]);
        let picker = priced_picker();
        let lines = text(&model_picker_lines(&picker, &prices, 40, 20));
        assert_eq!(lines[2], "claude-sonnet-4-6  Sonnet 4.6  current");
        assert_eq!(lines[3], "claude-mystery                   $1 / $5");
        // The longer row is cut as it was without prices, the shorter
        // keeps its price.
        let lines = text(&model_picker_lines(&picker, &prices, 24, 20));
        assert_eq!(lines[2], "claude-sonnet-4-6  Sonn…");
        assert_eq!(lines[3], "claude-mystery   $1 / $5");
        // No room for an id beside the column: no column.
        let lines = text(&model_picker_lines(&picker, &prices, 20, 20));
        assert!(lines[2].starts_with("claude-sonnet-4-6"), "{lines:#?}");
        assert_eq!(lines[3], "claude-mystery");
        assert!(lines.iter().all(|l| !l.contains('$')), "{lines:#?}");
        assert!(
            lines[2..4].iter().all(|l| display_width(l) <= 20),
            "{lines:#?}"
        );
    }

    /// The filter matches the endpoint and the model's names, never its
    /// price.
    #[test]
    fn the_filter_does_not_match_prices() {
        let prices = table(&[("claude-sonnet-4-6", 3.0, 15.0)]);
        let mut picker = priced_picker();
        for c in "$15".chars() {
            picker.push_filter(c);
        }
        assert_eq!(picker.filtered().count(), 0);
        let lines = text(&model_picker_lines(&picker, &prices, 70, 20));
        assert!(lines.contains(&"no models match".to_string()), "{lines:#?}");
        // The column stays where it is for the rows the filter keeps.
        picker.filter = "mini".into();
        let lines = text(&model_picker_lines(&picker, &prices, 70, 20));
        assert_eq!(lines[2], "gpt-5.4-mini                            no price");
    }

    #[test]
    fn centered_rect_fits() {
        let r = centered(Rect::new(0, 0, 10, 10), 20, 4);
        assert_eq!(r, Rect::new(0, 3, 10, 4));
    }
}
