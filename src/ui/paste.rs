//! Pasted text held as one chip in the input, as Claude Code does.
//!
//! A bracketed paste of more than one line, or of one line longer than
//! [`CHIP_MIN_CHARS`] characters, is not inserted as text: the editor gets
//! one character of Unicode's supplementary private use area that stands
//! for it, shown as `[Pasted text #N, C lines]` (or `C chars` for one long
//! line). Being one character, the chip is one unit for every edit: the
//! cursor crosses it in one step, Backspace, Delete and the kill commands
//! remove it whole, and undo and yank bring it back. Its text is kept in
//! the editor's own [`Chips`], and [`Chips::expand`] puts it back in place
//! wherever the input's text leaves the editor (a message, a steer, a `!`
//! line, a command's argument, the history, the unsent store, the external
//! editor), so nothing stored ever holds a chip.
//!
//! `N` counts the chips of one message from 1: once the text is sent, the
//! editor keeps only the chips something can still bring back (a kill to
//! yank, the stash). Text that comes into the editor from anywhere else
//! goes through [`plain`] first, so a private-use character already in it
//! is never taken for a chip.

use std::borrow::Cow;

/// The longest single line a paste inserts as text.
pub const CHIP_MIN_CHARS: usize = 80;

/// The first chip character (Supplementary Private Use Area-A); every char
/// from it on (planes 15 and 16) is private use.
const BASE: u32 = 0xF_0000;

/// The most chips one editor holds (plane 15's private-use chars).
const MAX_CHIPS: usize = 0xFFFE;

/// Whether a paste of `text` becomes a chip.
#[must_use]
pub fn wants_chip(text: &str) -> bool {
    text.contains('\n') || text.chars().count() > CHIP_MIN_CHARS
}

/// Whether `ch` is a supplementary private-use char (planes 15 and 16),
/// where chips live.
fn private_use(ch: char) -> bool {
    u32::from(ch) >= BASE
}

/// `ch`, or U+FFFD for a supplementary private-use char: what a char from
/// outside the editor becomes, so that it is never taken for a chip.
#[must_use]
pub fn plain_char(ch: char) -> char {
    if private_use(ch) { '\u{FFFD}' } else { ch }
}

/// `text` with every supplementary private-use char replaced by U+FFFD
/// ([`plain_char`]).
#[must_use]
pub fn plain(text: &str) -> Cow<'_, str> {
    if text.chars().any(private_use) {
        Cow::Owned(text.chars().map(plain_char).collect())
    } else {
        Cow::Borrowed(text)
    }
}

/// The pasted texts of one editor, the chip `BASE + i` standing for the
/// `i`-th.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Chips(Vec<String>);

impl Chips {
    /// A new chip standing for `text` (`\r\n` and `\r` already newlines).
    pub fn chip(&mut self, text: &str) -> char {
        let index = u32::try_from(self.0.len()).unwrap_or(u32::MAX);
        match char::from_u32(BASE.saturating_add(index)).filter(|_| self.0.len() < MAX_CHIPS) {
            Some(ch) => {
                self.0.push(text.to_string());
                ch
            }
            // Past the area's 65 534 chips: a placeholder that is plain text.
            None => '\u{FFFD}',
        }
    }

    /// How many pasted texts are held.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no pasted text is held.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The chip's index, when `ch` is one of these.
    fn index(&self, ch: char) -> Option<usize> {
        let code = u32::from(ch).checked_sub(BASE)?;
        let index = usize::try_from(code).ok()?;
        (index < self.0.len()).then_some(index)
    }

    /// What the input shows for chip `ch`: `[Pasted text #N, C lines]`, or
    /// `[Pasted text #N, C chars]` for one long line.
    #[must_use]
    pub fn label(&self, ch: char) -> Option<String> {
        let index = self.index(ch)?;
        let text = self.0.get(index)?;
        let lines = text.lines().count();
        Some(if lines > 1 || text.ends_with('\n') {
            format!(
                "[Pasted text #{}, {}]",
                index + 1,
                crate::text::count(lines.max(1), "line")
            )
        } else {
            format!(
                "[Pasted text #{}, {}]",
                index + 1,
                crate::text::count(text.chars().count(), "char")
            )
        })
    }

    /// `text` with every chip replaced by the text it stands for.
    #[must_use]
    pub fn expand(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        for ch in text.chars() {
            match self.index(ch).and_then(|i| self.0.get(i)) {
                Some(pasted) => out.push_str(pasted),
                None => out.push(ch),
            }
        }
        out
    }

    /// Keep only the chips that occur in `texts`, numbered again from 1 in
    /// the order they occur, and rewrite `texts` to the new chips; the
    /// others' pasted texts go.
    pub fn retain_in<'a>(&mut self, texts: impl IntoIterator<Item = &'a mut String>) {
        let texts: Vec<&mut String> = texts.into_iter().collect();
        let mut kept = Vec::new();
        let mut renamed: Vec<Option<char>> = vec![None; self.0.len()];
        for text in &texts {
            for ch in text.chars() {
                if let Some(i) = self.index(ch)
                    && renamed[i].is_none()
                {
                    let to = u32::try_from(kept.len())
                        .ok()
                        .and_then(|n| char::from_u32(BASE + n))
                        .unwrap_or('\u{FFFD}');
                    renamed[i] = Some(to);
                    kept.push(std::mem::take(&mut self.0[i]));
                }
            }
        }
        for text in texts {
            if text.chars().any(|ch| self.index(ch).is_some()) {
                *text = text
                    .chars()
                    .map(|ch| self.index(ch).and_then(|i| renamed[i]).unwrap_or(ch))
                    .collect();
            }
        }
        self.0 = kept;
    }

    /// `text` with every chip shown as its label (what the input draws; tests).
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn shown(&self, text: &str) -> String {
        text.chars()
            .map(|ch| self.label(ch).unwrap_or_else(|| ch.to_string()))
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn chips_stand_for_their_text_and_say_what_it_is() {
        assert!(!wants_chip("short line"));
        assert!(!wants_chip(&"x".repeat(CHIP_MIN_CHARS)));
        assert!(wants_chip(&"x".repeat(CHIP_MIN_CHARS + 1)));
        assert!(wants_chip("two\nlines"));
        let mut chips = Chips::default();
        let many = chips.chip("a\nb\nc");
        let long = chips.chip(&"y".repeat(120));
        assert_eq!(chips.label('a'), None);
        assert_eq!(chips.label(many).unwrap(), "[Pasted text #1, 3 lines]");
        assert_eq!(chips.label(long).unwrap(), "[Pasted text #2, 120 chars]");
        let typed = format!("see {many} and {long}!");
        assert_eq!(
            chips.expand(&typed),
            format!("see a\nb\nc and {}!", "y".repeat(120))
        );
        assert!(chips.shown(&typed).starts_with("see [Pasted text #1"));
        assert_eq!(chips.expand("plain"), "plain");
        // Another editor's chips are its own.
        assert_eq!(Chips::default().expand(&typed), typed);
    }

    /// Keeping the chips some texts hold numbers them again from 1 and
    /// rewrites the texts; the rest go.
    #[test]
    fn retained_chips_are_numbered_again() {
        let mut chips = Chips::default();
        let first = chips.chip("one\n");
        let second = chips.chip("two\n");
        let mut kept = format!("x{second}");
        chips.retain_in([&mut kept]);
        assert_eq!(chips.len(), 1);
        assert_eq!(chips.expand(&kept), "xtwo\n");
        assert_eq!(
            chips.label(kept.chars().nth(1).unwrap()).unwrap(),
            "[Pasted text #1, 1 line]"
        );
        assert_eq!(chips.label(second), None);
        let _ = first;
        chips.retain_in(std::iter::empty());
        assert!(chips.is_empty());
    }

    /// Private-use chars of planes 15 and 16 become U+FFFD; others stay.
    #[test]
    fn plain_text_holds_no_chip_char() {
        assert_eq!(
            plain("a\u{F0000}b\u{10FFFD}c\u{E000}"),
            "a\u{FFFD}b\u{FFFD}c\u{E000}"
        );
        assert!(matches!(plain("as is"), Cow::Borrowed("as is")));
        assert_eq!(plain_char('\u{F0001}'), '\u{FFFD}');
        assert_eq!(plain_char('é'), 'é');
    }
}
