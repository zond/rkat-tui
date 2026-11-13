//! Pasted text held as one chip in the input, as Claude Code does.
//!
//! A bracketed paste of more than one line, or of one line longer than
//! [`CHIP_MIN_CHARS`] characters, is not inserted as text: the editor gets
//! one character of Unicode's supplementary private use area that stands
//! for it, shown as `[Pasted text #N, C lines]` (or `C chars` for one long
//! line). Being one character, the chip is one unit for every edit: the
//! cursor crosses it in one step, Backspace, Delete and the kill commands
//! remove it whole, and undo and yank bring it back. Its text is kept here
//! for the life of the process, and [`expand`] puts it back in place
//! wherever the input's text leaves the editor (a message, a steer, a `!`
//! line, a command's argument, the history, the unsent store, the external
//! editor), so nothing stored ever holds a chip.
//!
//! `N` counts the chips of the session (this process) from 1.

use parking_lot::Mutex;

/// The longest single line a paste inserts as text.
pub const CHIP_MIN_CHARS: usize = 80;

/// The first chip character (Supplementary Private Use Area-A).
const BASE: u32 = 0xF_0000;

/// The pasted texts, the chip `BASE + i` standing for the `i`-th.
static CHIPS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Whether a paste of `text` becomes a chip.
#[must_use]
pub fn wants_chip(text: &str) -> bool {
    text.contains('\n') || text.chars().count() > CHIP_MIN_CHARS
}

/// A new chip standing for `text` (`\r\n` and `\r` already newlines).
#[must_use]
pub fn chip(text: &str) -> char {
    let mut chips = CHIPS.lock();
    let index = u32::try_from(chips.len()).unwrap_or(u32::MAX);
    match char::from_u32(BASE + index).filter(|_| index < 0xFFFE) {
        Some(ch) => {
            chips.push(text.to_string());
            ch
        }
        // Past the area's 65 534 chips: a placeholder that is plain text.
        None => '\u{FFFD}',
    }
}

/// The chip's index, when `ch` is one this process made.
fn index(ch: char) -> Option<usize> {
    let code = u32::from(ch).checked_sub(BASE)?;
    let index = usize::try_from(code).ok()?;
    (index < CHIPS.lock().len()).then_some(index)
}

/// Whether `ch` stands for a paste.
#[must_use]
pub fn is_chip(ch: char) -> bool {
    index(ch).is_some()
}

/// What the input shows for chip `ch`: `[Pasted text #N, C lines]`, or
/// `[Pasted text #N, C chars]` for one long line.
#[must_use]
pub fn label(ch: char) -> Option<String> {
    let index = index(ch)?;
    let chips = CHIPS.lock();
    let text = chips.get(index)?;
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
pub fn expand(text: &str) -> String {
    if !text.chars().any(is_chip) {
        return text.to_string();
    }
    let chips = CHIPS.lock();
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        let code = u32::from(ch)
            .checked_sub(BASE)
            .and_then(|c| usize::try_from(c).ok());
        match code.and_then(|i| chips.get(i)) {
            Some(pasted) => out.push_str(pasted),
            None => out.push(ch),
        }
    }
    out
}

/// `text` with every chip shown as its label (what the input draws; tests).
#[must_use]
#[cfg(any(test, feature = "test-support"))]
pub fn shown(text: &str) -> String {
    text.chars()
        .map(|ch| label(ch).unwrap_or_else(|| ch.to_string()))
        .collect()
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
        let many = chip("a\nb\nc");
        let long = chip(&"y".repeat(120));
        assert!(is_chip(many) && is_chip(long) && !is_chip('a'));
        let n = |label: String| {
            label
                .split('#')
                .nth(1)
                .unwrap()
                .split(',')
                .next()
                .unwrap()
                .parse::<usize>()
                .unwrap()
        };
        let first = label(many).unwrap();
        assert!(first.ends_with(", 3 lines]"), "{first}");
        let second = label(long).unwrap();
        assert!(second.ends_with(", 120 chars]"), "{second}");
        assert_eq!(n(second), n(first) + 1, "numbered in order");
        let typed = format!("see {many} and {long}!");
        assert_eq!(
            expand(&typed),
            format!("see a\nb\nc and {}!", "y".repeat(120))
        );
        assert!(shown(&typed).starts_with("see [Pasted text #"));
        assert_eq!(expand("plain"), "plain");
    }
}
