//! Multi-line prompt editor with history.
//!
//! The cursor column is a char index into the current line; display columns are
//! computed with `unicode-width` when rendering. Logical lines soft-wrap at the
//! field width ([`wrap_layout`]); wrapping is layout only, the stored lines are
//! never split.

use std::collections::VecDeque;

use super::markdown::{display_width, sanitize};

/// Entries the kill ring keeps (the oldest is dropped beyond this).
pub const KILL_RING_CAP: usize = 16;

/// Input states the undo stack keeps (the oldest is dropped beyond this).
pub const UNDO_DEPTH: usize = 64;

/// The input text and cursor, as the undo stack keeps them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

/// The previous editing command, as far as the next one cares: consecutive
/// kills join one kill-ring entry, Alt+Y works only right after a yank, and
/// a run of typing (up to a word boundary) or of deleting is one undo step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastEdit {
    /// Anything else (moving, a newline, a paste, a history recall).
    Other,
    /// Typed a character.
    Typing,
    /// Deleted a character (Backspace, Delete).
    Deleting,
    /// A kill that took text.
    Kill,
    /// A yank put kill-ring entry `age` (0 = the newest) between `start`
    /// and the cursor.
    Yank { start: (usize, usize), age: usize },
}

/// What Ctrl+S did ([`InputEditor::toggle_stash`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StashOutcome {
    /// The input was stashed and cleared; `displaced` is the text of an
    /// older stash it replaced, if any.
    Stashed {
        /// The replaced stash's text.
        displaced: Option<String>,
    },
    /// The empty input got the stash back.
    Restored,
    /// Empty input and nothing stashed.
    Nothing,
}

/// A multi-line text editor with prompt history and a kill ring.
#[derive(Debug, Clone)]
pub struct InputEditor {
    lines: Vec<String>,
    row: usize,
    col: usize,
    history: Vec<String>,
    history_pos: Option<usize>,
    draft: Option<String>,
    /// Columns a visual row holds (set by the renderer; 0 = do not wrap).
    wrap_width: usize,
    /// Killed text, the newest last (Ctrl+K/U/W, Alt+D, Ctrl/Alt+Backspace);
    /// Ctrl+Y yanks the newest, Alt+Y cycles to older ones.
    kill_ring: VecDeque<String>,
    last: LastEdit,
    /// Earlier input states, the newest last (Ctrl+_ restores them). Pushed
    /// at word boundaries while typing, at the start of a run of deletes,
    /// and before kills, yanks, pastes, newlines and clears.
    undo: VecDeque<Snapshot>,
    /// A prompt put aside with Ctrl+S (text and cursor).
    stash: Option<Snapshot>,
}

impl Default for InputEditor {
    fn default() -> Self {
        Self::new()
    }
}

fn byte_index(line: &str, col: usize) -> usize {
    line.char_indices().nth(col).map_or(line.len(), |(b, _)| b)
}

fn char_len(line: &str) -> usize {
    line.chars().count()
}

/// Display columns one char of a stored line takes once sanitized for the
/// screen (a tab expands, a control char becomes a replacement glyph).
fn cell_width(ch: char) -> usize {
    // A pasted-text chip is as wide as its label.
    if let Some(label) = super::paste::label(ch) {
        return display_width(&label);
    }
    let mut buf = [0u8; 4];
    display_width(&sanitize(ch.encode_utf8(&mut buf)))
}

/// One visual row of the wrapped input: chars `start..end` of logical line
/// `line`, `width` display columns wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualRow {
    pub line: usize,
    pub start: usize,
    pub end: usize,
    pub width: usize,
}

/// The input's logical lines soft-wrapped into visual rows (see [`wrap_layout`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapLayout {
    rows: Vec<VisualRow>,
}

/// Wrap `lines` at `width` display columns, at char boundaries (no word wrap).
///
/// A wide char never splits: it starts a new row when it does not fit. A
/// zero-width char stays on the row of the char before it. A line of width 0
/// still takes one row, and a line whose last row is exactly full gets an
/// empty row after it, where the cursor at its end sits. `width == 0` means
/// no wrapping (one row per line).
pub fn wrap_layout(lines: &[String], width: usize) -> WrapLayout {
    let limit = if width == 0 { usize::MAX } else { width };
    let mut rows = Vec::with_capacity(lines.len());
    for (line, text) in lines.iter().enumerate() {
        let (mut start, mut used, mut idx) = (0, 0usize, 0);
        for ch in text.chars() {
            let w = cell_width(ch);
            if w > 0 && used > 0 && used.saturating_add(w) > limit {
                rows.push(VisualRow {
                    line,
                    start,
                    end: idx,
                    width: used,
                });
                start = idx;
                used = 0;
            }
            used += w;
            idx += 1;
        }
        rows.push(VisualRow {
            line,
            start,
            end: idx,
            width: used,
        });
        if idx > start && used >= limit {
            rows.push(VisualRow {
                line,
                start: idx,
                end: idx,
                width: 0,
            });
        }
    }
    if rows.is_empty() {
        rows.push(VisualRow {
            line: 0,
            start: 0,
            end: 0,
            width: 0,
        });
    }
    WrapLayout { rows }
}

impl WrapLayout {
    /// The visual rows, top to bottom.
    pub fn rows(&self) -> &[VisualRow] {
        &self.rows
    }

    /// Number of visual rows (at least 1).
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Always false: a layout has at least one row.
    #[cfg(any(test, feature = "test-support"))]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The visual `(row, display column)` of logical `(row, char column)`.
    /// A position at a row boundary belongs to the later row.
    pub fn to_visual(&self, lines: &[String], row: usize, col: usize) -> (usize, usize) {
        let mut found = None;
        for (i, r) in self.rows.iter().enumerate() {
            if r.line > row {
                break;
            }
            if r.line == row && r.start <= col {
                found = Some(i);
            }
        }
        let Some(i) = found else {
            return (0, 0);
        };
        let start = self.rows.get(i).map_or(0, |r| r.start);
        let text = lines.get(row).map_or("", String::as_str);
        let vcol = text
            .chars()
            .skip(start)
            .take(col.saturating_sub(start))
            .map(cell_width)
            .sum();
        (i, vcol)
    }

    /// The logical `(row, char column)` nearest visual `(vrow, vcol)`: the
    /// last char boundary of the row at or left of `vcol`. Only the last row
    /// of a line can hold the position after its last char (elsewhere that
    /// position is the next row's start). `None` past the last row.
    pub fn logical_at(&self, lines: &[String], vrow: usize, vcol: usize) -> Option<(usize, usize)> {
        let r = *self.rows.get(vrow)?;
        let text = lines.get(r.line).map_or("", String::as_str);
        let (mut col, mut used) = (r.start, 0usize);
        for ch in text.chars().skip(r.start).take(r.end - r.start) {
            used += cell_width(ch);
            if used > vcol {
                break;
            }
            col += 1;
        }
        let last_of_line = self.rows.get(vrow + 1).is_none_or(|n| n.line != r.line);
        if !last_of_line && col >= r.end {
            col = r.end.saturating_sub(1).max(r.start);
        }
        Some((r.line, col))
    }
}

impl InputEditor {
    /// An empty editor with no history.
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
            history: Vec::new(),
            history_pos: None,
            draft: None,
            wrap_width: 0,
            kill_ring: VecDeque::new(),
            last: LastEdit::Other,
            undo: VecDeque::new(),
            stash: None,
        }
    }

    /// Set the columns a visual row holds (0 = do not wrap). The renderer
    /// sets it from the field width each frame; Up/Down move by visual rows.
    pub fn set_wrap_width(&mut self, width: usize) {
        self.wrap_width = width;
    }

    /// The current lines wrapped at the wrap width.
    pub fn layout(&self) -> WrapLayout {
        wrap_layout(&self.lines, self.wrap_width)
    }

    /// The cursor's visual `(row, display column)` in `layout`.
    pub fn cursor_visual(&self, layout: &WrapLayout) -> (usize, usize) {
        layout.to_visual(&self.lines, self.row, self.col)
    }

    /// Seed the history (oldest first).
    /// Replace the recall history (oldest first), leaving the editor's
    /// text alone.
    pub fn set_history(&mut self, history: Vec<String>) {
        self.history = history;
        self.history_pos = None;
    }

    pub fn with_history(mut self, history: Vec<String>) -> Self {
        self.history = history;
        self
    }

    /// Submitted prompts, oldest first.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Current lines.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Cursor as `(row, char column)`.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    /// Full text joined with `\n`, every pasted-text chip expanded to the
    /// text it stands for ([`super::paste`]).
    pub fn text(&self) -> String {
        super::paste::expand(&self.raw_text())
    }

    /// Full text joined with `\n`, chips as they are.
    fn raw_text(&self) -> String {
        self.lines.join("\n")
    }

    /// Whether the editor holds no text at all.
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines.first().is_none_or(String::is_empty)
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            lines: self.lines.clone(),
            row: self.row,
            col: self.col,
        }
    }

    /// Remember the current state for Ctrl+_ (unless it is the newest one
    /// already).
    fn push_undo(&mut self) {
        let snapshot = self.snapshot();
        if self.undo.back() != Some(&snapshot) {
            self.undo.push_back(snapshot);
            if self.undo.len() > UNDO_DEPTH {
                self.undo.pop_front();
            }
        }
    }

    /// Ctrl+_: restore the newest remembered state that differs from the
    /// current one. Returns whether there was one.
    pub fn undo(&mut self) -> bool {
        let current = self.snapshot();
        while let Some(snapshot) = self.undo.pop_back() {
            if snapshot != current {
                let Snapshot { lines, row, col } = snapshot;
                self.lines = lines;
                (self.row, self.col) = (row, col);
                self.last = LastEdit::Other;
                return true;
            }
        }
        false
    }

    /// Ctrl+S: with text, put it (and the cursor) aside and clear the
    /// input; on an empty input, bring the stash back.
    pub fn toggle_stash(&mut self) -> StashOutcome {
        if !self.is_empty() {
            self.push_undo();
            let displaced = self
                .stash
                .replace(self.snapshot())
                // It goes to the history: with its chips expanded.
                .map(|old| super::paste::expand(&old.lines.join("\n")));
            self.last = LastEdit::Other;
            self.reset();
            return StashOutcome::Stashed { displaced };
        }
        let Some(Snapshot { lines, row, col }) = self.stash.take() else {
            return StashOutcome::Nothing;
        };
        self.push_undo();
        self.last = LastEdit::Other;
        self.lines = lines;
        (self.row, self.col) = (row, col);
        StashOutcome::Restored
    }

    /// Whether a prompt is stashed (Ctrl+S on an empty input restores it).
    pub fn has_stash(&self) -> bool {
        self.stash.is_some()
    }

    /// Remove all text (history is kept); Ctrl+_ brings it back.
    pub fn clear(&mut self) {
        self.push_undo();
        self.last = LastEdit::Other;
        self.reset();
    }

    fn reset(&mut self) {
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
        self.history_pos = None;
        self.draft = None;
    }

    /// Replace the text and put the cursor at the end.
    pub fn set_text(&mut self, text: &str) {
        self.push_undo();
        self.last = LastEdit::Other;
        self.lines = text.split('\n').map(str::to_string).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.lines.len() - 1;
        self.col = char_len(self.current());
    }

    /// Replace the chars `start..end` of line `row` with `text` (no
    /// newlines) and put the cursor after it: a completion. Ctrl-_ undoes it.
    pub fn replace_in_line(&mut self, row: usize, start: usize, end: usize, text: &str) {
        let Some(line) = self.lines.get(row) else {
            return;
        };
        let (a, b) = (byte_index(line, start), byte_index(line, end.max(start)));
        self.push_undo();
        self.last = LastEdit::Other;
        if let Some(line) = self.lines.get_mut(row) {
            line.replace_range(a..b, text);
        }
        self.row = row;
        self.col = start + char_len(text);
    }

    /// Add non-blank `text` to the prompt history (unless it is the newest
    /// entry already).
    pub fn remember(&mut self, text: &str) {
        if !text.trim().is_empty() && self.history.last().is_none_or(|last| last != text) {
            self.history.push(text.to_string());
        }
    }

    /// Take the text for submission: clears the editor and records non-blank text in history.
    pub fn take(&mut self) -> String {
        let text = self.text();
        self.remember(&text);
        // A new prompt starts with no undo states.
        self.reset();
        self.last = LastEdit::Other;
        self.undo.clear();
        text
    }

    fn current(&self) -> &str {
        self.lines.get(self.row).map_or("", String::as_str)
    }

    fn current_mut(&mut self) -> &mut String {
        if self.row >= self.lines.len() {
            self.lines.resize(self.row + 1, String::new());
        }
        &mut self.lines[self.row]
    }

    /// Insert one character at the cursor (`\n` splits the line).
    pub fn insert_char(&mut self, ch: char) {
        if ch == '\n' {
            self.newline();
            return;
        }
        let after_word = self
            .current()
            .chars()
            .nth(self.col.wrapping_sub(1))
            .is_some_and(|c| !c.is_whitespace());
        if self.last != LastEdit::Typing || (ch.is_whitespace() && after_word) {
            self.push_undo();
        }
        self.put_char(ch);
        self.last = LastEdit::Typing;
    }

    fn put_char(&mut self, ch: char) {
        let col = self.col;
        let line = self.current_mut();
        let at = byte_index(line, col);
        line.insert(at, ch);
        self.col += 1;
    }

    /// Insert text verbatim. `\r\n` and `\r` become newlines.
    pub fn insert_str(&mut self, text: &str) {
        self.push_undo();
        self.last = LastEdit::Other;
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.put_str(&normalized);
    }

    /// A bracketed paste: more than one line, or one line longer than
    /// [`super::paste::CHIP_MIN_CHARS`], goes in as one chip
    /// (`[Pasted text #N, C lines]`) that edits as a unit and expands on
    /// send; anything shorter is inserted as text.
    pub fn paste(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        if !super::paste::wants_chip(&normalized) {
            self.insert_str(&normalized);
            return;
        }
        self.push_undo();
        self.last = LastEdit::Other;
        self.put_char(super::paste::chip(&normalized));
    }

    /// Split the line at the cursor.
    pub fn newline(&mut self) {
        self.push_undo();
        self.last = LastEdit::Other;
        self.split_line();
    }

    fn split_line(&mut self) {
        let col = self.col;
        let line = self.current_mut();
        let at = byte_index(line, col);
        let rest = line.split_off(at);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    /// `\`+Enter: when the char before the cursor is a backslash at the end
    /// of the line, replace it with a newline and return true; otherwise
    /// leave the text alone and return false (Enter then submits).
    pub fn backslash_newline(&mut self) -> bool {
        let line = self.current();
        let len = char_len(line);
        if self.col == 0 || self.col != len || !line.ends_with('\\') {
            return false;
        }
        self.push_undo();
        self.remove_before();
        self.split_line();
        self.last = LastEdit::Other;
        true
    }

    /// Delete the character before the cursor (joining lines at column 0).
    pub fn backspace(&mut self) {
        if self.last != LastEdit::Deleting {
            self.push_undo();
        }
        self.last = LastEdit::Deleting;
        self.remove_before();
    }

    fn remove_before(&mut self) {
        if self.col > 0 {
            let col = self.col;
            let line = self.current_mut();
            let at = byte_index(line, col - 1);
            line.remove(at);
            self.col -= 1;
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.col = char_len(self.current());
            self.current_mut().push_str(&line);
        }
    }

    /// Delete the character under the cursor (joining lines at end of line).
    pub fn delete(&mut self) {
        if self.last != LastEdit::Deleting {
            self.push_undo();
        }
        self.last = LastEdit::Deleting;
        let col = self.col;
        if col < char_len(self.current()) {
            let line = self.current_mut();
            let at = byte_index(line, col);
            line.remove(at);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.current_mut().push_str(&next);
        }
    }

    /// Move left one char (wrapping to the previous line).
    pub fn left(&mut self) {
        self.last = LastEdit::Other;
        self.move_left();
    }

    fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = char_len(self.current());
        }
    }

    /// Move right one char (wrapping to the next line).
    pub fn right(&mut self) {
        self.last = LastEdit::Other;
        self.move_right();
    }

    fn move_right(&mut self) {
        if self.col < char_len(self.current()) {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    /// Start of the current line.
    pub fn home(&mut self) {
        self.last = LastEdit::Other;
        self.col = 0;
    }

    /// End of the current line.
    pub fn end(&mut self) {
        self.last = LastEdit::Other;
        self.col = char_len(self.current());
    }

    /// Jump to the start of the previous word.
    pub fn word_left(&mut self) {
        self.last = LastEdit::Other;
        self.move_word_left();
    }

    fn move_word_left(&mut self) {
        if self.col == 0 {
            self.move_left();
            return;
        }
        let chars: Vec<char> = self.current().chars().collect();
        let mut c = self.col;
        while c > 0 && chars.get(c - 1).is_some_and(|ch| ch.is_whitespace()) {
            c -= 1;
        }
        while c > 0 && chars.get(c - 1).is_some_and(|ch| !ch.is_whitespace()) {
            c -= 1;
        }
        self.col = c;
    }

    /// Jump past the end of the next word.
    pub fn word_right(&mut self) {
        self.last = LastEdit::Other;
        self.move_word_right();
    }

    fn move_word_right(&mut self) {
        let chars: Vec<char> = self.current().chars().collect();
        if self.col >= chars.len() {
            self.move_right();
            return;
        }
        let mut c = self.col;
        while c < chars.len() && chars[c].is_whitespace() {
            c += 1;
        }
        while c < chars.len() && !chars[c].is_whitespace() {
            c += 1;
        }
        self.col = c;
    }

    /// Where a cursor move would land, without moving.
    fn probe(&mut self, mv: fn(&mut Self)) -> (usize, usize) {
        let at = self.cursor();
        mv(self);
        let to = self.cursor();
        (self.row, self.col) = at;
        to
    }

    /// Remove the text from `from` to `to` (`from` first), leave the cursor
    /// at `from` and return the text (`\n` between lines).
    fn remove_range(&mut self, from: (usize, usize), to: (usize, usize)) -> String {
        let ((r1, c1), (r2, c2)) = (from, to);
        if r2 >= self.lines.len() || from >= to {
            return String::new();
        }
        let a = byte_index(&self.lines[r1], c1);
        let b = byte_index(&self.lines[r2], c2);
        let removed = if r1 == r2 {
            let removed = self.lines[r1].get(a..b).unwrap_or_default().to_string();
            self.lines[r1].replace_range(a..b, "");
            removed
        } else {
            let tail = self.lines[r2].split_off(b);
            let mut removed = self.lines[r1].split_off(a);
            for line in self.lines.drain(r1 + 1..=r2) {
                removed.push('\n');
                removed.push_str(&line);
            }
            self.lines[r1].push_str(&tail);
            removed
        };
        (self.row, self.col) = from;
        removed
    }

    /// Put killed text on the kill ring: right after another kill it joins
    /// that entry (after it for a forward kill, before it for a backward
    /// one), as in readline.
    fn record_kill(&mut self, text: String, forward: bool) {
        if text.is_empty() {
            return;
        }
        match self.kill_ring.back_mut() {
            Some(entry) if self.last == LastEdit::Kill => {
                if forward {
                    entry.push_str(&text);
                } else {
                    entry.insert_str(0, &text);
                }
            }
            _ => {
                self.kill_ring.push_back(text);
                if self.kill_ring.len() > KILL_RING_CAP {
                    self.kill_ring.pop_front();
                }
            }
        }
        self.last = LastEdit::Kill;
    }

    /// Kill from `from` to the cursor (a backward kill).
    fn kill_back_to(&mut self, from: (usize, usize)) {
        if from < self.cursor() {
            self.push_undo();
        }
        let text = self.remove_range(from, self.cursor());
        self.record_kill(text, false);
    }

    /// Kill from the cursor to `to` (a forward kill).
    fn kill_forward_to(&mut self, to: (usize, usize)) {
        if self.cursor() < to {
            self.push_undo();
        }
        let text = self.remove_range(self.cursor(), to);
        self.record_kill(text, true);
    }

    /// Kill the word before the cursor (Ctrl-W, Ctrl/Alt+Backspace); at a
    /// line start, the newline before it.
    pub fn delete_word_back(&mut self) {
        let from = self.probe(Self::move_word_left);
        self.kill_back_to(from);
    }

    /// Kill the word after the cursor (Alt+D); at a line end, the newline.
    pub fn kill_word_forward(&mut self) {
        let to = self.probe(Self::move_word_right);
        self.kill_forward_to(to);
    }

    /// Kill from the line start to the cursor (Ctrl-U).
    pub fn kill_to_line_start(&mut self) {
        self.kill_back_to((self.row, 0));
    }

    /// Kill from the cursor to the line end (Ctrl-K); at a line end, the
    /// newline (joining the next line).
    pub fn kill_to_line_end(&mut self) {
        let len = char_len(self.current());
        let to = if self.col < len {
            (self.row, len)
        } else if self.row + 1 < self.lines.len() {
            (self.row + 1, 0)
        } else {
            return;
        };
        self.kill_forward_to(to);
    }

    /// Insert text at the cursor without touching the editing state.
    fn put_str(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                self.split_line();
            } else {
                self.put_char(ch);
            }
        }
    }

    /// Ctrl-Y: insert the newest kill-ring entry at the cursor. Returns
    /// whether the ring held one.
    pub fn yank(&mut self) -> bool {
        let Some(text) = self.kill_ring.back().cloned() else {
            return false;
        };
        self.push_undo();
        let start = self.cursor();
        self.put_str(&text);
        self.last = LastEdit::Yank { start, age: 0 };
        true
    }

    /// Alt-Y right after a yank: replace the yanked text with the next
    /// older kill-ring entry (wrapping to the newest). Returns whether it
    /// did anything.
    pub fn yank_pop(&mut self) -> bool {
        let LastEdit::Yank { start, age } = self.last else {
            return false;
        };
        let n = self.kill_ring.len();
        if n == 0 {
            return false;
        }
        let age = (age + 1) % n;
        let Some(text) = self.kill_ring.get(n - 1 - age).cloned() else {
            return false;
        };
        self.push_undo();
        let end = self.cursor();
        self.remove_range(start, end);
        self.put_str(&text);
        self.last = LastEdit::Yank { start, age };
        true
    }

    /// Move the cursor to visual row `vrow` at display column `vcol` (clamped).
    fn move_to_visual(&mut self, layout: &WrapLayout, vrow: usize, vcol: usize) {
        if let Some((row, col)) = layout.logical_at(&self.lines, vrow, vcol) {
            self.row = row;
            self.col = col;
        }
    }

    /// Up arrow: move up a visual row, or recall older history from the
    /// first visual row.
    pub fn up(&mut self) {
        self.last = LastEdit::Other;
        let layout = self.layout();
        let (vrow, vcol) = self.cursor_visual(&layout);
        if vrow > 0 {
            self.move_to_visual(&layout, vrow - 1, vcol);
            return;
        }
        let next = match self.history_pos {
            None if self.history.is_empty() => return,
            None => {
                self.draft = Some(self.raw_text());
                self.history.len() - 1
            }
            Some(0) => return,
            Some(p) => p - 1,
        };
        self.history_pos = Some(next);
        let entry = self.history.get(next).cloned().unwrap_or_default();
        self.set_text(&entry);
    }

    /// Down arrow: move down a visual row, or recall newer history from the
    /// last visual row.
    pub fn down(&mut self) {
        self.last = LastEdit::Other;
        let layout = self.layout();
        let (vrow, vcol) = self.cursor_visual(&layout);
        if vrow + 1 < layout.len() {
            self.move_to_visual(&layout, vrow + 1, vcol);
            return;
        }
        let Some(p) = self.history_pos else {
            return;
        };
        if p + 1 < self.history.len() {
            self.history_pos = Some(p + 1);
            let entry = self.history.get(p + 1).cloned().unwrap_or_default();
            self.set_text(&entry);
        } else {
            self.history_pos = None;
            let draft = self.draft.take().unwrap_or_default();
            self.set_text(&draft);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn typed(s: &str) -> InputEditor {
        let mut e = InputEditor::new();
        for ch in s.chars() {
            e.insert_char(ch);
        }
        e
    }

    #[test]
    fn insert_and_backspace_unicode() {
        let mut e = typed("héllo");
        e.left();
        e.left();
        e.backspace();
        assert_eq!(e.text(), "hélo");
        assert_eq!(e.cursor(), (0, 2));
        e.delete();
        assert_eq!(e.text(), "héo");
    }

    #[test]
    fn newline_splits_and_backspace_joins() {
        let mut e = typed("abcd");
        e.left();
        e.left();
        e.newline();
        assert_eq!(e.lines(), &["ab".to_string(), "cd".to_string()]);
        assert_eq!(e.cursor(), (1, 0));
        e.backspace();
        assert_eq!(e.text(), "abcd");
        assert_eq!(e.cursor(), (0, 2));
    }

    #[test]
    fn paste_is_verbatim_with_crlf_normalized() {
        let mut e = InputEditor::new();
        e.insert_str("a\r\nb\tc");
        assert_eq!(e.text(), "a\nb\tc");
        assert_eq!(e.cursor(), (1, 3));
    }

    #[test]
    fn word_jumps_and_delete_word() {
        let mut e = typed("foo bar  baz");
        e.word_left();
        assert_eq!(e.cursor(), (0, 9));
        e.word_left();
        assert_eq!(e.cursor(), (0, 4));
        e.word_right();
        assert_eq!(e.cursor(), (0, 7));
        e.end();
        e.delete_word_back();
        assert_eq!(e.text(), "foo bar  ");
        e.home();
        assert_eq!(e.cursor(), (0, 0));
    }

    #[test]
    fn history_navigation_restores_draft() {
        let mut e = InputEditor::new().with_history(vec!["one".into(), "two".into()]);
        e.insert_str("dra");
        e.up();
        assert_eq!(e.text(), "two");
        e.up();
        assert_eq!(e.text(), "one");
        e.up();
        assert_eq!(e.text(), "one");
        e.down();
        assert_eq!(e.text(), "two");
        e.down();
        assert_eq!(e.text(), "dra");
    }

    #[test]
    fn up_moves_within_multiline_before_history() {
        let mut e = InputEditor::new().with_history(vec!["old".into()]);
        e.insert_str("a\nb");
        e.up();
        assert_eq!(e.cursor(), (0, 1));
        assert_eq!(e.text(), "a\nb");
        e.up();
        assert_eq!(e.text(), "old");
    }

    #[test]
    fn a_trailing_backslash_becomes_a_newline() {
        let mut e = typed("ab\\");
        assert!(e.backslash_newline());
        assert_eq!(e.lines(), &["ab".to_string(), String::new()]);
        assert_eq!(e.cursor(), (1, 0));
        // Not at the end of the line, or no backslash: nothing happens.
        let mut e = typed("a\\b");
        assert!(!e.backslash_newline());
        e.left();
        assert!(!e.backslash_newline());
        assert_eq!(e.text(), "a\\b");
        assert!(!InputEditor::new().backslash_newline());
    }

    #[test]
    fn kills_feed_the_ring_and_yank_puts_the_newest_back() {
        let mut e = typed("one two three");
        e.delete_word_back();
        assert_eq!(e.text(), "one two ");
        assert!(e.yank());
        assert_eq!(e.text(), "one two three");
        // Ctrl-K to the line end, then (at the end) the newline.
        let mut e = typed("ab\ncd");
        e.up();
        e.home();
        e.right();
        e.kill_to_line_end();
        assert_eq!(e.text(), "a\ncd");
        e.left();
        e.right();
        e.kill_to_line_end();
        assert_eq!(e.text(), "acd");
        // Nothing left to kill at the very end: no new entry.
        e.end();
        e.kill_to_line_end();
        assert_eq!(e.kill_ring.len(), 2);
        // Ctrl-U kills to the line start; Alt-D kills the word after.
        let mut e = typed("foo bar baz");
        e.word_left();
        e.kill_to_line_start();
        assert_eq!(e.text(), "baz");
        e.end();
        e.home();
        e.kill_word_forward();
        assert_eq!(e.text(), "");
        assert_eq!(
            e.kill_ring.iter().map(String::as_str).collect::<Vec<_>>(),
            ["foo bar ", "baz"]
        );
    }

    #[test]
    fn consecutive_kills_join_one_entry() {
        let mut e = typed("alpha beta gamma");
        e.delete_word_back();
        e.delete_word_back();
        assert_eq!(e.text(), "alpha ");
        assert_eq!(e.kill_ring.len(), 1);
        assert_eq!(e.kill_ring.back().map(String::as_str), Some("beta gamma"));
        // Forward kills append; a move in between starts a new entry.
        let mut e = typed("a b c");
        e.home();
        e.kill_word_forward();
        e.kill_word_forward();
        assert_eq!(e.kill_ring.back().map(String::as_str), Some("a b"));
        e.end();
        e.delete_word_back();
        assert_eq!(e.kill_ring.len(), 2);
        // Ctrl-W at a line start kills the newline.
        let mut e = typed("x\ny");
        e.home();
        e.delete_word_back();
        assert_eq!(e.text(), "xy");
        assert_eq!(e.kill_ring.back().map(String::as_str), Some("\n"));
    }

    #[test]
    fn alt_y_cycles_older_entries_only_right_after_a_yank() {
        let mut e = InputEditor::new();
        assert!(!e.yank());
        for word in ["one", "two", "three"] {
            e.insert_str(word);
            e.home();
            e.kill_to_line_end();
            e.insert_char(' ');
            e.backspace();
        }
        assert_eq!(e.kill_ring.len(), 3);
        assert!(!e.yank_pop());
        e.insert_str("> ");
        assert!(e.yank());
        assert_eq!(e.text(), "> three");
        assert!(e.yank_pop());
        assert_eq!(e.text(), "> two");
        assert!(e.yank_pop());
        assert_eq!(e.text(), "> one");
        assert!(e.yank_pop());
        assert_eq!(e.text(), "> three");
        e.insert_char('!');
        assert!(!e.yank_pop());
        // A multi-line entry is replaced whole.
        let mut e = typed("l1\nl2");
        e.up();
        e.home();
        e.kill_to_line_end();
        e.kill_to_line_end();
        e.kill_to_line_end();
        assert_eq!(e.text(), "");
        e.insert_str("x");
        e.home();
        e.kill_to_line_end();
        e.insert_char('>');
        e.yank();
        assert_eq!(e.text(), ">x");
        assert!(e.yank_pop());
        assert_eq!(e.text(), ">l1\nl2");
        assert!(e.yank_pop());
        assert_eq!(e.text(), ">x");
    }

    #[test]
    fn the_kill_ring_keeps_the_newest_entries() {
        let mut e = InputEditor::new();
        for i in 0..KILL_RING_CAP + 3 {
            e.insert_str(&i.to_string());
            e.home();
            e.kill_to_line_end();
            e.right();
        }
        assert_eq!(e.kill_ring.len(), KILL_RING_CAP);
        assert_eq!(e.kill_ring.front().map(String::as_str), Some("3"));
    }

    #[test]
    fn undo_steps_back_by_words_and_edits() {
        let mut e = InputEditor::new();
        assert!(!e.undo());
        for ch in "hello big world".chars() {
            e.insert_char(ch);
        }
        assert!(e.undo());
        assert_eq!(e.text(), "hello big");
        assert!(e.undo());
        assert_eq!(e.text(), "hello");
        // A run of backspaces is one step.
        e.end();
        e.backspace();
        e.backspace();
        assert_eq!(e.text(), "hel");
        assert!(e.undo());
        assert_eq!(e.text(), "hello");
        assert_eq!(e.cursor(), (0, 5));
        // Kills, yanks, pastes and clears are steps of their own.
        e.delete_word_back();
        e.yank();
        e.insert_str(" pasted");
        e.clear();
        assert!(e.undo());
        assert_eq!(e.text(), "hello pasted");
        assert!(e.undo());
        assert_eq!(e.text(), "hello");
        assert!(e.undo());
        assert_eq!(e.text(), "");
        assert!(e.undo());
        assert_eq!(e.text(), "hello");
        assert!(e.undo());
        assert_eq!(e.text(), "");
        assert!(!e.undo());
        // A sent prompt leaves nothing to undo.
        e.insert_str("sent");
        e.take();
        assert!(!e.undo());
    }

    #[test]
    fn the_undo_stack_keeps_the_newest_states() {
        let mut e = InputEditor::new();
        for i in 0..UNDO_DEPTH + 10 {
            e.insert_str(&format!("{i} "));
        }
        let mut steps = 0;
        while e.undo() {
            steps += 1;
        }
        assert_eq!(steps, UNDO_DEPTH);
        assert!(!e.is_empty());
    }

    #[test]
    fn stash_puts_the_prompt_aside_and_brings_it_back() {
        let mut e = typed("one\ntwo");
        e.left();
        assert_eq!(e.toggle_stash(), StashOutcome::Stashed { displaced: None });
        assert!(e.is_empty() && e.has_stash());
        e.insert_str("other");
        assert_eq!(e.take(), "other");
        assert_eq!(e.toggle_stash(), StashOutcome::Restored);
        assert_eq!(e.text(), "one\ntwo");
        assert_eq!(e.cursor(), (1, 2));
        assert!(!e.has_stash());
        assert_eq!(InputEditor::new().toggle_stash(), StashOutcome::Nothing);
        // Stashing over a stash hands the older one back.
        e.toggle_stash();
        e.insert_str("newer");
        assert_eq!(
            e.toggle_stash(),
            StashOutcome::Stashed {
                displaced: Some("one\ntwo".into())
            }
        );
        e.toggle_stash();
        assert_eq!(e.text(), "newer");
    }

    #[test]
    fn take_records_history() {
        let mut e = typed("hi");
        assert_eq!(e.take(), "hi");
        assert!(e.is_empty());
        assert_eq!(e.history(), &["hi".to_string()]);
        e.insert_str("   ");
        e.take();
        assert_eq!(e.history().len(), 1);
    }

    fn lines(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|t| (*t).to_string()).collect()
    }

    fn spans(layout: &WrapLayout) -> Vec<(usize, usize, usize)> {
        layout
            .rows()
            .iter()
            .map(|r| (r.line, r.start, r.end))
            .collect()
    }

    #[test]
    fn wrap_layout_splits_a_long_line_at_the_width() {
        let ls = lines(&["abcdefghijklmnopqrstuvwxy"]);
        let layout = wrap_layout(&ls, 10);
        assert_eq!(spans(&layout), vec![(0, 0, 10), (0, 10, 20), (0, 20, 25)]);
        assert_eq!(
            layout.rows().iter().map(|r| r.width).collect::<Vec<_>>(),
            vec![10, 10, 5]
        );
        assert_eq!(layout.to_visual(&ls, 0, 25), (2, 5));
        assert_eq!(layout.to_visual(&ls, 0, 3), (0, 3));
    }

    #[test]
    fn wrap_layout_never_splits_a_wide_char() {
        // 4 narrow + 3 wide (2 columns each) = 10 columns at width 5.
        let ls = lines(&["abcd日本語"]);
        let layout = wrap_layout(&ls, 5);
        assert_eq!(spans(&layout), vec![(0, 0, 4), (0, 4, 6), (0, 6, 7)]);
        assert!(layout.rows().iter().all(|r| r.width <= 5));
        assert_eq!(layout.to_visual(&ls, 0, 5), (1, 2));
        // A zero-width joiner stays on the row of the char before it.
        let ls = lines(&["abcde\u{200D}f"]);
        assert_eq!(spans(&wrap_layout(&ls, 5)), vec![(0, 0, 6), (0, 6, 7)]);
    }

    #[test]
    fn wrap_layout_gives_an_empty_line_one_row() {
        let ls = lines(&["", "ab", ""]);
        let layout = wrap_layout(&ls, 10);
        assert_eq!(spans(&layout), vec![(0, 0, 0), (1, 0, 2), (2, 0, 0)]);
        assert_eq!(layout.to_visual(&ls, 2, 0), (2, 0));
        assert_eq!(wrap_layout(&[], 10).len(), 1);
    }

    #[test]
    fn wrap_layout_maps_row_ends_to_the_next_row() {
        let ls = lines(&["abcdefghijklmnopqrstuvwxy"]);
        let layout = wrap_layout(&ls, 10);
        // The boundary position belongs to the later row.
        assert_eq!(layout.to_visual(&ls, 0, 10), (1, 0));
        assert_eq!(layout.to_visual(&ls, 0, 20), (2, 0));
        // A column past a row's end clamps to its last char, except on the
        // line's last row.
        assert_eq!(layout.logical_at(&ls, 0, 99), Some((0, 9)));
        assert_eq!(layout.logical_at(&ls, 2, 99), Some((0, 25)));
        assert_eq!(layout.logical_at(&ls, 1, 4), Some((0, 14)));
        assert_eq!(layout.logical_at(&ls, 3, 0), None);
    }

    #[test]
    fn wrap_layout_puts_the_cursor_after_an_exactly_full_line_on_the_next_row() {
        let ls = lines(&["abcdefghij", "k"]);
        let layout = wrap_layout(&ls, 10);
        assert_eq!(spans(&layout), vec![(0, 0, 10), (0, 10, 10), (1, 0, 1)]);
        assert_eq!(layout.to_visual(&ls, 0, 10), (1, 0));
        assert_eq!(layout.logical_at(&ls, 1, 0), Some((0, 10)));
        assert_eq!(layout.logical_at(&ls, 0, 99), Some((0, 9)));
    }

    #[test]
    fn wrap_width_zero_does_not_wrap() {
        let ls = lines(&["abcdefghijklmnopqrstuvwxy"]);
        assert_eq!(spans(&wrap_layout(&ls, 0)), vec![(0, 0, 25)]);
    }

    #[test]
    fn up_and_down_move_by_visual_rows() {
        let mut e = InputEditor::new().with_history(vec!["old".into()]);
        e.set_wrap_width(10);
        e.insert_str("abcdefghijklmnopqrstuvwxy");
        e.left();
        e.left();
        assert_eq!(e.cursor(), (0, 23));
        e.up();
        assert_eq!(e.cursor(), (0, 13));
        e.up();
        assert_eq!(e.cursor(), (0, 3));
        e.down();
        assert_eq!(e.cursor(), (0, 13));
        e.up();
        e.up();
        assert_eq!(e.text(), "old");
    }
}
