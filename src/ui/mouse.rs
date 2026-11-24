//! The mouse: clicks on blocks and helper headers, drag selection with
//! word and line multi-clicks, and copying to the clipboard (`/copy`).

use std::time::Instant;

use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::text::Line;

use super::{
    App, Block, BlockId, CellPos, InputMode, LastDown, MULTI_CLICK_WINDOW, Modal, NoticeLevel,
    Press, QueuedCopy, SelectMode, Selection, keymap, selection,
};

impl App {
    /// Record where the transcript was drawn: its screen area, the logical
    /// line of its first row and the block shown on each row (set by the
    /// renderer; row `i` shows logical line `first_line + i`).
    pub fn set_transcript_rows(&mut self, area: Rect, first_line: usize, rows: Vec<BlockId>) {
        self.set_view_rows(area, first_line, rows, None);
    }

    /// [`Self::set_transcript_rows`] for the view the rows show: the
    /// transcript (`None`) or the full view of the helper `view`, whose
    /// line indices are its own ([`crate::ui::Transcript::helper_view_rows`]).
    /// A selection or press made in another view goes.
    pub fn set_view_rows(
        &mut self,
        area: Rect,
        first_line: usize,
        rows: Vec<BlockId>,
        view: Option<BlockId>,
    ) {
        if self.mouse.view != view {
            self.mouse.view = view;
            self.clear_selection();
        }
        self.mouse.transcript_area = area;
        self.mouse.transcript_rows = rows
            .into_iter()
            .enumerate()
            .map(|(i, id)| (id, first_line + i))
            .collect();
    }

    /// The laid-out lines `first..first + count` of the view the rows show
    /// (at the last drawn width).
    fn view_lines(&mut self, first: usize, count: usize) -> Vec<Line<'static>> {
        let width = self.mouse.transcript_area.width;
        match self.mouse.view {
            Some(header) => self
                .transcript
                .helper_view_rows(header, width)
                .0
                .into_iter()
                .skip(first)
                .take(count)
                .collect(),
            None => {
                if width > 0 {
                    self.transcript.ensure_layout(width);
                }
                self.transcript.visible_lines(first, count)
            }
        }
    }

    /// How many laid-out lines the view the rows show has.
    fn view_line_count(&mut self) -> usize {
        match self.mouse.view {
            Some(header) => {
                let width = self.mouse.transcript_area.width;
                self.transcript.helper_view_rows(header, width).0.len()
            }
            None => self.transcript.line_count(),
        }
    }

    /// The transcript block drawn at screen row `row` in the last frame.
    pub fn block_at_row(&self, row: u16) -> Option<BlockId> {
        let offset = row.checked_sub(self.mouse.transcript_area.y)?;
        self.mouse
            .transcript_rows
            .get(usize::from(offset))
            .map(|(id, _)| *id)
    }

    /// The transcript cell (logical line, display column) at a screen
    /// position in the last frame.
    pub fn cell_at(&self, row: u16, column: u16) -> Option<CellPos> {
        let offset = row.checked_sub(self.mouse.transcript_area.y)?;
        let col = column.checked_sub(self.mouse.transcript_area.x)?;
        if col >= self.mouse.transcript_area.width {
            return None;
        }
        let (_, line) = self.mouse.transcript_rows.get(usize::from(offset))?;
        Some(CellPos::new(*line, usize::from(col)))
    }

    /// The current mouse selection.
    pub fn selection(&self) -> Option<&Selection> {
        self.mouse.selection.as_ref()
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
        self.view_lines(idx, 1).pop()
    }

    pub(super) fn mouse(&mut self, m: MouseEvent, now: Instant) -> bool {
        match m.kind {
            // Over the help the wheel scrolls the help.
            MouseEventKind::ScrollUp => match &mut self.modal {
                Some(Modal::Help(help)) => help.up(keymap::LINE_SCROLL),
                _ => self.active_scroll().up(keymap::LINE_SCROLL),
            },
            MouseEventKind::ScrollDown => match &mut self.modal {
                Some(Modal::Help(help)) => help.down(keymap::LINE_SCROLL),
                _ => self.active_scroll().down(keymap::LINE_SCROLL),
            },
            MouseEventKind::Down(MouseButton::Left) => {
                return self.mouse_down(m.row, m.column, now);
            }
            MouseEventKind::Drag(MouseButton::Left) => return self.mouse_drag(m.row, m.column),
            MouseEventKind::Up(MouseButton::Left) => return self.mouse_up(),
            // With Shift held the terminal's own paste is wanted (most
            // terminals never report it then).
            MouseEventKind::Down(MouseButton::Middle)
                if !m.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.middle_paste();
            }
            _ => return false,
        }
        true
    }

    /// Middle-click: paste what this client copied last into the field
    /// that takes a paste now ([`Self::paste_text`]), as a paste from the
    /// terminal is; nothing copied yet says so. It starts no selection and
    /// clicks nothing.
    pub(super) fn middle_paste(&mut self) {
        match self.last_copied.clone() {
            Some(text) => self.paste_text(&text),
            None if self.modal.is_none() => {
                self.transcript.push_notice(
                    NoticeLevel::Info,
                    "nothing copied here yet; Shift+middle-click pastes the terminal's own selection",
                );
            }
            None => {}
        }
    }

    /// Paste `text` (the terminal's bracketed paste, or a middle-click):
    /// into a dialog's or picker's text field when one is open, into the
    /// history search while it runs, else into the input at its cursor
    /// (lines kept, nothing sent).
    pub(super) fn paste_text(&mut self, text: &str) {
        if let Some(Modal::Escalation { dialog, .. }) = &mut self.modal {
            dialog.type_text(text);
        } else if let Some(Modal::List(picker)) = &mut self.modal
            && picker.editing()
        {
            picker.type_text(text);
        } else if self.modal.is_none() {
            self.mode.end_chord();
            if let InputMode::Search(searching) = &mut self.mode {
                for c in text.chars().filter(|c| !c.is_control()) {
                    searching.search.push(c, &searching.corpus);
                }
            } else {
                self.input.paste(text);
            }
        }
    }

    /// Left press: a single click clears the selection (and may become a
    /// click or a drag); a double click selects a word, a triple click a line.
    pub(super) fn mouse_down(&mut self, row: u16, column: u16, now: Instant) -> bool {
        if self.modal.is_some() {
            return false;
        }
        let had_selection = self.mouse.selection.take().is_some();
        let Some(pos) = self.cell_at(row, column) else {
            self.mouse.press = None;
            self.mouse.last_down = None;
            return had_selection;
        };
        let clicks = match self.mouse.last_down {
            Some(d)
                if d.row == row
                    && d.column == column
                    && now.saturating_duration_since(d.at) < MULTI_CLICK_WINDOW =>
            {
                d.clicks % 3 + 1
            }
            _ => 1,
        };
        self.mouse.last_down = Some(LastDown {
            at: now,
            row,
            column,
            clicks,
        });
        self.mouse.press = Some(Press {
            pos,
            row,
            clicks,
            dragged: false,
        });
        self.mouse.selection = match clicks {
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
        let Some(press) = self.mouse.press.as_mut() else {
            return false;
        };
        press.dragged = true;
        let start = press.pos;
        let Some(pos) = self.drag_target(row, column) else {
            return false;
        };
        let line = self.layout_line(pos.line);
        let sel = self
            .mouse
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
        let first = self.mouse.transcript_rows.first()?.1;
        let last = self.mouse.transcript_rows.last()?.1;
        let area = self.mouse.transcript_area;
        let col = usize::from(column.saturating_sub(area.x));
        if row < area.y {
            if first > 0 {
                self.active_scroll().up(1);
            }
            return Some(CellPos::new(first.saturating_sub(1), col));
        }
        if let Some((_, line)) = self.mouse.transcript_rows.get(usize::from(row - area.y)) {
            return Some(CellPos::new(*line, col));
        }
        if row >= area.bottom() && self.active_scroll().offset_from_bottom > 0 {
            self.active_scroll().down(1);
            let max = self.view_line_count().saturating_sub(1);
            return Some(CellPos::new((last + 1).min(max), col));
        }
        Some(CellPos::new(last, usize::MAX))
    }

    /// Left release: a press without drag is a click (expand/collapse a tool
    /// block); after a drag or a double/triple click the selection is copied.
    pub(super) fn mouse_up(&mut self) -> bool {
        let Some(press) = self.mouse.press.take() else {
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
        let Some(sel) = self.mouse.selection else {
            return;
        };
        let (start, end) = sel.range();
        let lines = self.view_lines(start.line, end.line - start.line + 1);
        let text = sel.extract_window(start.line, &lines);
        self.copy_text(&text);
    }

    /// `/copy [N]`: copy the Nth latest assistant answer (1 = the latest).
    pub(super) fn copy_answer(&mut self, n: usize) {
        let answers: Vec<String> = self
            .transcript
            .iter()
            .rev()
            .filter_map(|(_, b)| match b {
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
        self.last_copied = Some(capped.to_string());
        self.clipboard = Some(QueuedCopy {
            text: capped.to_string(),
            truncated,
        });
    }

    /// The text under the selection moved `lines` laid-out lines (the
    /// renderer keeps a scrolled-up view on its text): the selection goes
    /// with it.
    pub(crate) fn shift_selection(&mut self, lines: isize) {
        if let Some(selection) = self.mouse.selection.as_mut() {
            selection.shift(lines);
        }
    }

    /// Clear the selection and any press in progress.
    pub(super) fn clear_selection(&mut self) {
        self.mouse.selection = None;
        self.mouse.press = None;
    }

    /// Which of the lines of block `id` screen row `row` showed in the last
    /// frame (0: its first).
    fn line_in_block(&mut self, row: u16, id: BlockId) -> Option<usize> {
        let offset = row.checked_sub(self.mouse.transcript_area.y)?;
        let (_, line) = *self.mouse.transcript_rows.get(usize::from(offset))?;
        let first = match self.mouse.view {
            None => self.transcript.start_line(id)?,
            Some(header) => {
                let width = self.mouse.transcript_area.width;
                self.transcript
                    .helper_view_rows(header, width)
                    .1
                    .iter()
                    .position(|block| *block == id)?
            }
        };
        Some(line.saturating_sub(first))
    }

    /// A left click: expand or collapse the tool block under it; the line
    /// at the bottom of the screen stays there ([`Self::keep_bottom`]).
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
        // The transcript keeps its bottom line; a helper's view keeps its
        // own scroll (its first line stays).
        if self.mouse.view.is_none() {
            self.keep_bottom(Some(id));
        }
        // A run's line opens or folds the run, not its first block.
        if let Some(line) = self.line_in_block(row, id)
            && self
                .transcript
                .toggle_run(id, line, self.mouse.view.is_some())
                .is_some()
        {
            return true;
        }
        if self.transcript.toggle_tool(id).is_some() {
            true
        } else {
            self.fold_anchor = None;
            false
        }
    }
}

/// The mouse: what the last draw put where, and the selection.
#[derive(Debug, Default)]
pub(super) struct MouseState {
    /// Screen area of the transcript as of the last draw.
    pub(super) transcript_area: Rect,
    /// The view the rows show: the transcript (`None`) or a helper's full
    /// view (its header), whose line indices are the view's own.
    pub(super) view: Option<BlockId>,
    /// Per drawn row, the block and the transcript logical line shown
    /// there, as of the last draw (hit-testing and selection).
    pub(super) transcript_rows: Vec<(BlockId, usize)>,
    /// The text selection (logical line coordinates).
    pub(super) selection: Option<Selection>,
    /// A left press not yet released.
    pub(super) press: Option<Press>,
    /// The last left press, for multi-click detection.
    pub(super) last_down: Option<LastDown>,
}
