//! `/rewind` (and Esc Esc on an empty idle input): the menu of the
//! session's earlier user messages and what choosing one does.
//!
//! The runner lists the points ([`RewindPoint`], newest first) from the
//! session's committed history, the UI shows them in a [`RewindMenu`], and
//! choosing one asks the runner to fork the session just before that
//! message ([`super::Effect::Rewind`]). The runner answers with a
//! [`Rewound`]: the UI cuts its transcript before the message, clears the
//! helper selection, puts the message's text back in the editor and says
//! where the full conversation went.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::modal::{ListCursor, ModalKey};

use super::transcript::Block;

/// One user message the session can rewind to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindPoint {
    /// The message's index in the session's committed transcript: the fork
    /// keeps the messages before it.
    pub message_index: usize,
    /// The message as the user typed it (file attachments of `@` mentions
    /// left out).
    pub text: String,
}

impl RewindPoint {
    /// The menu's one-line label: the first non-empty line, with `…` when
    /// more follows.
    #[must_use]
    pub fn label(&self) -> String {
        let mut lines = self.text.lines().map(str::trim).filter(|l| !l.is_empty());
        let first = lines.next().unwrap_or_default();
        if lines.next().is_some() {
            format!("{first} …")
        } else {
            first.to_string()
        }
    }
}

/// The open rewind menu: the points newest first and the highlighted one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindMenu {
    /// The points, newest first.
    pub points: Vec<RewindPoint>,
    /// The highlighted point.
    pub cursor: ListCursor,
}

impl RewindMenu {
    /// A menu over `points` (newest first) with the newest highlighted.
    #[must_use]
    pub fn new(points: Vec<RewindPoint>) -> Self {
        Self {
            points,
            cursor: ListCursor::default(),
        }
    }

    /// ↑/↓ (or k/j) move, Enter rewinds to the highlighted point, Esc,
    /// `q` or Ctrl-C close. A held key only moves.
    pub fn key(&mut self, key: &KeyEvent) -> ModalKey<RewindPoint> {
        let held = key.kind == KeyEventKind::Repeat;
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
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
                self.cursor.down(self.points.len());
                ModalKey::Stay
            }
            KeyCode::Enter => self
                .points
                .get(self.cursor.index())
                .cloned()
                .map_or(ModalKey::Close, ModalKey::Choose),
            KeyCode::Esc | KeyCode::Char('q') => ModalKey::Close,
            _ => ModalKey::Stay,
        }
    }
}

/// The runner's answer to a [`super::Effect::Rewind`] that worked: the TUI
/// now runs on a fork of the session that ends before the chosen message.
#[derive(Debug, Clone, PartialEq)]
pub struct Rewound {
    /// How many of the session's user messages (the chosen one included)
    /// the fork left out: the transcript is cut before the
    /// `from_end`-th user block from its end.
    pub from_end: usize,
    /// The chosen message's text, for the editor.
    pub text: String,
    /// The fork's history as transcript blocks, shown instead when the
    /// transcript's user blocks do not line up with the session's (after a
    /// compaction, say).
    pub blocks: Vec<Block>,
    /// What happened and how to get back to the full conversation.
    pub notice: String,
}

/// The text of a session user message as the user typed it: the `@`
/// mention attachments ([`crate::mentions::expand_mentions`] appends them
/// after a blank line) left out.
#[must_use]
pub fn typed_text(sent: &str) -> &str {
    ["\n\n<file path=\"", "\n\n<directory path=\""]
        .iter()
        .filter_map(|marker| sent.find(marker))
        .min()
        .map_or(sent, |at| sent.get(..at).unwrap_or(sent))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(index: usize, text: &str) -> RewindPoint {
        RewindPoint {
            message_index: index,
            text: text.into(),
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    #[test]
    fn the_menu_moves_chooses_and_cancels() {
        let mut menu = RewindMenu::new(vec![
            point(5, "third"),
            point(3, "second"),
            point(1, "first"),
        ]);
        assert_eq!(menu.cursor.index(), 0);
        assert_eq!(menu.key(&press(KeyCode::Up)), ModalKey::Stay);
        assert_eq!(menu.cursor.index(), 0);
        assert_eq!(menu.key(&press(KeyCode::Down)), ModalKey::Stay);
        assert_eq!(menu.key(&press(KeyCode::Char('j'))), ModalKey::Stay);
        assert_eq!(menu.key(&press(KeyCode::Down)), ModalKey::Stay);
        assert_eq!(menu.cursor.index(), 2, "stops at the oldest");
        assert_eq!(menu.key(&press(KeyCode::Char('k'))), ModalKey::Stay);
        assert_eq!(menu.key(&press(KeyCode::Char('x'))), ModalKey::Stay);
        // A held Enter does not choose; a pressed one does.
        let mut held = press(KeyCode::Enter);
        held.kind = KeyEventKind::Repeat;
        assert_eq!(menu.key(&held), ModalKey::Stay);
        assert_eq!(
            menu.key(&press(KeyCode::Enter)),
            ModalKey::Choose(point(3, "second"))
        );
        assert_eq!(menu.key(&press(KeyCode::Esc)), ModalKey::Close);
        assert_eq!(menu.key(&press(KeyCode::Char('q'))), ModalKey::Close);
        assert_eq!(
            menu.key(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            ModalKey::Close
        );
        assert_eq!(
            RewindMenu::new(Vec::new()).key(&press(KeyCode::Enter)),
            ModalKey::Close
        );
    }

    #[test]
    fn labels_are_one_line() {
        assert_eq!(point(0, "fix the parser").label(), "fix the parser");
        assert_eq!(point(0, "\n  fix it\nand test\n").label(), "fix it …");
        assert_eq!(point(0, "").label(), "");
    }

    #[test]
    fn typed_text_leaves_the_attachments_out() {
        assert_eq!(typed_text("plain"), "plain");
        assert_eq!(
            typed_text("see @a.txt\n\n<file path=\"a.txt\">\nx\n</file>"),
            "see @a.txt"
        );
        assert_eq!(
            typed_text(
                "ls @src/\n\n<directory path=\"src/\">\nmain.rs\n</directory>\n\n<file path=\"b\">"
            ),
            "ls @src/"
        );
    }
}
