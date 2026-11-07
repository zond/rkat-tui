//! The completion menu above the input: `/` commands while the first word of
//! a one-line input is a slash command, `@` workspace paths while the cursor
//! ends a word that starts with `@`.
//!
//! The menu is a pure function of the input text and cursor
//! ([`word_at`] finds the word it completes, [`candidates`] what it offers);
//! the app keeps the open menu, its highlighted row and the word Esc closed
//! it for.

use super::commands;
use crate::mentions::{self, PathEntry};

/// Rows the menu shows at most (a longer list scrolls with the highlight).
pub const MAX_ROWS: usize = 8;

/// What a menu completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    /// A slash command (the first word of a one-line input).
    Command,
    /// A workspace path after `@`.
    Path,
}

/// The word under the cursor a menu completes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    /// What it completes.
    pub kind: CompletionKind,
    /// Input line.
    pub row: usize,
    /// Char column of its first char (the `/` or `@`).
    pub start: usize,
    /// Char column just past it.
    pub end: usize,
    /// The word without its leading `/` or `@`.
    pub text: String,
}

/// One menu row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The text that replaces the word (its `/` or `@` included).
    pub insert: String,
    /// The row's left part (`/name usage`).
    pub label: String,
    /// The row's dim right part (the summary).
    pub detail: String,
    /// Completing it keeps the menu open, for what is below it (a
    /// directory: no space follows it).
    pub keep_open: bool,
}

/// An open menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    /// The word it completes.
    pub word: Word,
    /// Its rows (never empty).
    pub items: Vec<Candidate>,
    /// The highlighted row.
    pub selected: usize,
}

impl Menu {
    /// A menu over `items` for `word`, or `None` when nothing matches.
    #[must_use]
    pub fn open(word: Word, items: Vec<Candidate>) -> Option<Self> {
        (!items.is_empty()).then_some(Self {
            word,
            items,
            selected: 0,
        })
    }

    /// Move the highlight down (`forward`) or up, wrapping around.
    pub fn step(&mut self, forward: bool) {
        let len = self.items.len().max(1);
        self.selected = if forward {
            (self.selected + 1) % len
        } else {
            (self.selected + len - 1) % len
        };
    }

    /// The highlighted row.
    #[must_use]
    pub fn current(&self) -> Option<&Candidate> {
        self.items.get(self.selected)
    }

    /// Whether the typed word is complete already, so Enter sends instead
    /// of completing: it names a command exactly, or it is the highlighted
    /// file's path.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        match self.word.kind {
            CompletionKind::Command => commands::spec(&self.word.text).is_some(),
            CompletionKind::Path => self.current().is_some_and(|item| {
                !item.keep_open && item.insert.strip_prefix('@') == Some(self.word.text.as_str())
            }),
        }
    }

    /// The first row drawn and how many rows are drawn: up to [`MAX_ROWS`],
    /// scrolled so the highlight stays visible.
    #[must_use]
    pub fn window(&self) -> (usize, usize) {
        let shown = self.items.len().min(MAX_ROWS);
        let first = self
            .selected
            .saturating_sub(shown.saturating_sub(1))
            .min(self.items.len() - shown);
        (first, shown)
    }
}

/// The word under the cursor that a menu would complete: the first word of
/// a one-line input that starts with `/` (not `//`), while the cursor is in
/// it; else a word that starts with `@` and ends at the cursor (anywhere in
/// the input; `a@b` is not one).
#[must_use]
pub fn word_at(lines: &[String], cursor: (usize, usize)) -> Option<Word> {
    command_word(lines, cursor).or_else(|| mention_word(lines, cursor))
}

fn mention_word(lines: &[String], (row, col): (usize, usize)) -> Option<Word> {
    let chars: Vec<char> = lines.get(row)?.chars().collect();
    if chars.get(col).is_some_and(|c| !c.is_whitespace()) {
        return None;
    }
    let before = chars.get(..col)?;
    let start = before
        .iter()
        .rposition(|c| c.is_whitespace())
        .map_or(0, |i| i + 1);
    (before.get(start) == Some(&'@')).then(|| Word {
        kind: CompletionKind::Path,
        row,
        start,
        end: col,
        text: before.get(start + 1..).unwrap_or_default().iter().collect(),
    })
}

fn command_word(lines: &[String], cursor: (usize, usize)) -> Option<Word> {
    let (row, col) = cursor;
    let [line] = lines else {
        return None;
    };
    let body = line.strip_prefix('/')?;
    if body.starts_with('/') {
        return None;
    }
    let len = 1 + body.chars().take_while(|c| !c.is_whitespace()).count();
    (row == 0 && col <= len).then(|| Word {
        kind: CompletionKind::Command,
        row,
        start: 0,
        end: len,
        text: line.chars().skip(1).take(len - 1).collect(),
    })
}

/// The rows a menu offers for `word`; `paths` is the workspace index (see
/// [`mentions::workspace_paths`]) a path word is matched against.
#[must_use]
pub fn candidates(word: &Word, paths: &[PathEntry]) -> Vec<Candidate> {
    match word.kind {
        CompletionKind::Path => mentions::match_paths(paths, &word.text, MAX_ROWS)
            .into_iter()
            .map(|entry| Candidate {
                insert: format!("@{}", entry.path),
                label: entry.path.clone(),
                detail: String::new(),
                keep_open: entry.is_dir(),
            })
            .collect(),
        CompletionKind::Command => commands::completions(&word.text)
            .map(|(spec, name)| Candidate {
                insert: format!("/{name}"),
                label: if spec.usage.is_empty() {
                    format!("/{name}")
                } else {
                    format!("/{name} {}", spec.usage)
                },
                detail: spec.summary.to_string(),
                keep_open: false,
            })
            .collect(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn line(s: &str) -> Vec<String> {
        vec![s.to_string()]
    }

    #[test]
    fn the_command_word_is_the_first_word_of_a_one_line_slash_input() {
        let word = word_at(&line("/he"), (0, 3)).unwrap();
        assert_eq!((word.start, word.end, word.text.as_str()), (0, 3, "he"));
        // The cursor anywhere in the first word, args after it.
        let word = word_at(&line("/mod gpt"), (0, 2)).unwrap();
        assert_eq!((word.end, word.text.as_str()), (4, "mod"));
        assert!(word_at(&line("/model gpt"), (0, 7)).is_none());
        assert_eq!(word_at(&line("/"), (0, 1)).unwrap().text, "");
        for (text, cursor) in [("//lit", (0, 3)), ("hi /x", (0, 5)), (" /x", (0, 3))] {
            assert!(word_at(&line(text), cursor).is_none(), "{text}");
        }
        let two = vec!["/he".to_string(), "x".to_string()];
        assert!(word_at(&two, (0, 3)).is_none());
    }

    #[test]
    fn a_path_word_starts_with_at_and_ends_at_the_cursor() {
        let word = word_at(&line("read @src/ma please"), (0, 12)).unwrap();
        assert_eq!(word.kind, CompletionKind::Path);
        assert_eq!((word.start, word.end, word.text.as_str()), (5, 12, "src/ma"));
        assert_eq!(word_at(&line("@"), (0, 1)).unwrap().text, "");
        // In a later line, and after a command's first word.
        let lines = vec!["one".to_string(), "two @x".to_string()];
        assert_eq!(word_at(&lines, (1, 6)).unwrap().text, "x");
        assert_eq!(word_at(&line("/model @x"), (0, 9)).unwrap().kind, CompletionKind::Path);
        // Not inside a word, not after a non-blank, not for a@b.
        assert!(word_at(&line("@src/ma"), (0, 4)).is_none());
        assert!(word_at(&line("mail a@b"), (0, 8)).is_none());
        assert!(word_at(&line("@x y"), (0, 4)).is_none());
    }

    #[test]
    fn path_candidates_keep_the_menu_open_for_directories() {
        let index: Vec<PathEntry> = ["src/", "src/main.rs", "src/ui/"]
            .into_iter()
            .map(|path| PathEntry { path: path.into() })
            .collect();
        let word = word_at(&line("@src/"), (0, 5)).unwrap();
        let mut menu = Menu::open(word.clone(), candidates(&word, &index)).unwrap();
        let rows: Vec<(&str, bool)> = menu
            .items
            .iter()
            .map(|item| (item.insert.as_str(), item.keep_open))
            .collect();
        assert_eq!(rows, [("@src/main.rs", false), ("@src/ui/", true)]);
        assert!(!menu.is_complete());
        let typed = word_at(&line("@src/main.rs"), (0, 12)).unwrap();
        assert!(Menu::open(typed.clone(), candidates(&typed, &index)).unwrap().is_complete());
        menu.step(true);
        assert!(!menu.is_complete());
    }

    #[test]
    fn candidates_show_usage_and_summary() {
        let word = word_at(&line("/mode"), (0, 5)).unwrap();
        let items = candidates(&word, &[]);
        assert_eq!(items.len(), 2, "/mode and /model");
        assert_eq!(items[0].insert, "/mode");
        assert_eq!(items[0].label, "/mode [default | accept-edits | plan | auto]");
        assert_eq!(items[0].detail, "show or set the permission mode");
        assert_eq!(items[1].insert, "/model");
        assert_eq!(items[1].label, "/model [[endpoint:]id | refresh]");
        assert_eq!(items[1].detail, "pick or switch the model");
        let none = word_at(&line("/zz"), (0, 3)).unwrap();
        assert!(Menu::open(none.clone(), candidates(&none, &[])).is_none());
    }

    #[test]
    fn the_highlight_wraps_and_the_window_follows_it() {
        let word = word_at(&line("/"), (0, 1)).unwrap();
        let mut menu = Menu::open(word.clone(), candidates(&word, &[])).unwrap();
        let len = menu.items.len();
        assert!(len > MAX_ROWS);
        assert_eq!(menu.window(), (0, MAX_ROWS));
        menu.step(false);
        assert_eq!(menu.selected, len - 1);
        assert_eq!(menu.window(), (len - MAX_ROWS, MAX_ROWS));
        menu.step(true);
        assert_eq!(menu.selected, 0);
        for _ in 0..MAX_ROWS {
            menu.step(true);
        }
        assert_eq!(menu.window(), (1, MAX_ROWS));
        assert!(!menu.is_complete());
        let exact = word_at(&line("/help"), (0, 5)).unwrap();
        assert!(Menu::open(exact.clone(), candidates(&exact, &[])).unwrap().is_complete());
    }
}
