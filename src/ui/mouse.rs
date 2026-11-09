//! The mouse: clicks on blocks and helper headers, drag selection with
//! word and line multi-clicks, and copying to the clipboard (`/copy`).

use std::time::Instant;

use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::text::Line;

use super::{
    App, Block, BlockId, CellPos, LastDown, MULTI_CLICK_WINDOW, NoticeLevel, Press, QueuedCopy,
    SelectMode, Selection, keymap, selection,
};

impl App {
    /// Record where the transcript was drawn: its screen area, the logical
    /// line of its first row and the block shown on each row (set by the
    /// renderer; row `i` shows logical line `first_line + i`).
    pub fn set_transcript_rows(&mut self, area: Rect, first_line: usize, rows: Vec<BlockId>) {
        self.transcript_area = area;
        self.transcript_rows = rows
            .into_iter()
            .enumerate()
            .map(|(i, id)| (id, first_line + i))
            .collect();
    }

    /// The transcript block drawn at screen row `row` in the last frame.
    pub fn block_at_row(&self, row: u16) -> Option<BlockId> {
        let offset = row.checked_sub(self.transcript_area.y)?;
        self.transcript_rows
            .get(usize::from(offset))
            .map(|(id, _)| *id)
    }

    /// The transcript cell (logical line, display column) at a screen
    /// position in the last frame.
    pub fn cell_at(&self, row: u16, column: u16) -> Option<CellPos> {
        let offset = row.checked_sub(self.transcript_area.y)?;
        let col = column.checked_sub(self.transcript_area.x)?;
        if col >= self.transcript_area.width {
            return None;
        }
        let (_, line) = self.transcript_rows.get(usize::from(offset))?;
        Some(CellPos::new(*line, usize::from(col)))
    }

    /// The current mouse selection.
    pub fn selection(&self) -> Option<&Selection> {
        self.selection.as_ref()
    }

    /// The copy queued since the last call.
    #[cfg(any(test, feature = "test-support"))]
    pub fn take_clipboard(&mut self) -> Option<QueuedCopy> {
        self.clipboard.take()
    }

    /// The text of [`Self::take_clipboard`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn take_clipboard_text(&mut self) -> Option<String> {
        self.take_clipboard().map(|copy| copy.text)
    }

    /// Bring the layout up to date at the last drawn width (so logical line
    /// indices match what is on screen) and return line `idx`.
    pub(super) fn layout_line(&mut self, idx: usize) -> Option<Line<'static>> {
        if self.transcript_area.width > 0 {
            self.transcript.ensure_layout(self.transcript_area.width);
        }
        self.transcript.line(idx).cloned()
    }

    pub(super) fn mouse(&mut self, m: MouseEvent, now: Instant) -> bool {
        match m.kind {
            MouseEventKind::ScrollUp => self.active_scroll().up(keymap::LINE_SCROLL),
            MouseEventKind::ScrollDown => self.active_scroll().down(keymap::LINE_SCROLL),
            MouseEventKind::Down(MouseButton::Left) => {
                return self.mouse_down(m.row, m.column, now);
            }
            MouseEventKind::Drag(MouseButton::Left) => return self.mouse_drag(m.row, m.column),
            MouseEventKind::Up(MouseButton::Left) => return self.mouse_up(),
            _ => return false,
        }
        true
    }

    /// Left press: a single click clears the selection (and may become a
    /// click or a drag); a double click selects a word, a triple click a line.
    pub(super) fn mouse_down(&mut self, row: u16, column: u16, now: Instant) -> bool {
        if self.modal.is_some() {
            return false;
        }
        let had_selection = self.selection.take().is_some();
        let Some(pos) = self.cell_at(row, column) else {
            self.press = None;
            self.last_down = None;
            return had_selection;
        };
        let clicks = match self.last_down {
            Some(d)
                if d.row == row
                    && d.column == column
                    && now.saturating_duration_since(d.at) < MULTI_CLICK_WINDOW =>
            {
                d.clicks % 3 + 1
            }
            _ => 1,
        };
        self.last_down = Some(LastDown {
            at: now,
            row,
            column,
            clicks,
        });
        self.press = Some(Press {
            pos,
            row,
            clicks,
            dragged: false,
        });
        self.selection = match clicks {
            1 => None,
            2 => {
                let line = self.layout_line(pos.line);
                Some(Selection::new(pos, SelectMode::Word, line.as_ref()))
            }
            _ => Some(Selection::new(pos, SelectMode::Line, None)),
        };
        had_selection || clicks > 1
    }

    /// Drag with the left button: extend the selection, scrolling when the
    /// pointer is past the top or bottom edge of the transcript.
    pub(super) fn mouse_drag(&mut self, row: u16, column: u16) -> bool {
        if self.modal.is_some() {
            return false;
        }
        let Some(press) = self.press.as_mut() else {
            return false;
        };
        press.dragged = true;
        let start = press.pos;
        let Some(pos) = self.drag_target(row, column) else {
            return false;
        };
        let line = self.layout_line(pos.line);
        let sel = self
            .selection
            .get_or_insert_with(|| Selection::new(start, SelectMode::Char, None));
        sel.extend(pos, line.as_ref());
        true
    }

    /// The cell a drag at a screen position selects to. Past the top edge
    /// (or below the transcript while scrolled back) it scrolls one line and
    /// targets the line that scrolls in; below the last line it targets the
    /// end of the last line.
    pub(super) fn drag_target(&mut self, row: u16, column: u16) -> Option<CellPos> {
        let first = self.transcript_rows.first()?.1;
        let last = self.transcript_rows.last()?.1;
        let area = self.transcript_area;
        let col = usize::from(column.saturating_sub(area.x));
        if row < area.y {
            if first > 0 {
                self.scroll.up(1);
            }
            return Some(CellPos::new(first.saturating_sub(1), col));
        }
        if let Some((_, line)) = self.transcript_rows.get(usize::from(row - area.y)) {
            return Some(CellPos::new(*line, col));
        }
        if row >= area.bottom() && self.scroll.offset_from_bottom > 0 {
            self.scroll.down(1);
            let max = self.transcript.line_count().saturating_sub(1);
            return Some(CellPos::new((last + 1).min(max), col));
        }
        Some(CellPos::new(last, usize::MAX))
    }

    /// Left release: a press without drag is a click (expand/collapse a tool
    /// block); after a drag or a double/triple click the selection is copied.
    pub(super) fn mouse_up(&mut self) -> bool {
        let Some(press) = self.press.take() else {
            return false;
        };
        if self.modal.is_some() {
            return false;
        }
        if !press.dragged && press.clicks == 1 {
            return self.click(press.row);
        }
        self.copy_selection();
        true
    }

    /// Queue the selected text for the clipboard.
    pub(super) fn copy_selection(&mut self) {
        let Some(sel) = self.selection else {
            return;
        };
        let (start, end) = sel.range();
        if self.transcript_area.width > 0 {
            self.transcript.ensure_layout(self.transcript_area.width);
        }
        let lines = self
            .transcript
            .visible_lines(start.line, end.line - start.line + 1);
        let text = sel.extract_window(start.line, &lines);
        self.copy_text(&text);
    }

    /// `/copy [N]`: copy the Nth latest assistant answer (1 = the latest).
    pub(super) fn copy_answer(&mut self, n: usize) {
        let answers: Vec<String> = self
            .transcript
            .blocks()
            .iter()
            .rev()
            .filter_map(|b| match b {
                Block::Assistant { md } if !md.trim().is_empty() => Some(md.clone()),
                _ => None,
            })
            .take(n)
            .collect();
        match answers.get(n.saturating_sub(1)) {
            Some(text) => self.copy_text(text),
            None if answers.is_empty() => self.copy_text(""),
            None => {
                let n = answers.len();
                let (verb, plural) = if n == 1 { ("is", "") } else { ("are", "s") };
                self.transcript.push_notice(
                    NoticeLevel::Warn,
                    format!("there {verb} only {n} answer{plural} to copy"),
                );
            }
        }
    }

    /// Queue `text` (capped) for the clipboard; the runtime reports how the
    /// copy went.
    pub(super) fn copy_text(&mut self, text: &str) {
        if text.trim().is_empty() {
            self.transcript
                .push_notice(NoticeLevel::Info, "nothing to copy");
            return;
        }
        let (capped, truncated) = selection::cap_clipboard(text);
        self.clipboard = Some(QueuedCopy {
            text: capped.to_string(),
            truncated,
        });
    }

    /// Clear the selection and any press in progress.
    pub(super) fn clear_selection(&mut self) {
        self.selection = None;
        self.press = None;
    }

    /// A left click: expand or collapse the tool block under it. Expanding
    /// stops following new output so the clicked block stays where it is.
    /// Returns whether anything changed.
    pub(super) fn click(&mut self, row: u16) -> bool {
        if self.modal.is_some() {
            return false;
        }
        let Some(id) = self.block_at_row(row) else {
            return false;
        };
        if matches!(self.transcript.block(id), Some(Block::Helper(_))) {
            return self.toggle_helper(id);
        }
        match self.transcript.toggle_tool(id) {
            Some(collapsed) => {
                if !collapsed {
                    self.scroll.follow = false;
                }
                true
            }
            None => false,
        }
    }
}
