//! The small formatters every message shares: a count with its noun
//! (`1 tool`, `3 tools`), a length of time (`5m6s`, `5m`) and a
//! conversation's short id, a token count (`84k`), a text's prefix cut on
//! a character boundary.

use std::fmt::{Display, Write as _};

/// `n` and `thing`, plural unless `n` is one: `count(3, "tool")` is
/// `3 tools`.
#[must_use]
pub fn count<N: Copy + PartialEq + From<u8> + Display>(n: N, thing: &str) -> String {
    format!("{n} {thing}{}", plural(n))
}

/// The plural ending for `n` things: `""` for one, else `"s"`.
#[must_use]
pub fn plural<N: PartialEq + From<u8>>(n: N) -> &'static str {
    if n == N::from(1) { "" } else { "s" }
}

/// How many characters of a conversation's id [`short_id`] shows.
pub const SHORT_ID_LEN: usize = 8;

/// The short form of a conversation's (session's) id, the one every place
/// that shows an id short uses: its last [`SHORT_ID_LEN`] characters.
/// Session ids are UUID v7, whose first characters are a timestamp, so
/// conversations made within a minute or so share their first eight; the
/// last ones are random. `/resume` takes this form as well as a prefix.
#[must_use]
pub fn short_id(id: &str) -> &str {
    let start = id.len().saturating_sub(SHORT_ID_LEN);
    id.get(start..).unwrap_or(id)
}

/// `seconds` in the largest unit that is not zero (days, hours, minutes,
/// seconds) and the `units - 1` units after it, down to seconds:
/// `span(3_725, 2)` is `1h2m`, `span(3_725, 1)` is `1h`, `span(7, 2)` is
/// `7s`.
#[must_use]
pub fn span(seconds: u64, units: usize) -> String {
    const UNITS: [(u64, char); 4] = [(86_400, 'd'), (3_600, 'h'), (60, 'm'), (1, 's')];
    let first = UNITS
        .iter()
        .position(|(size, _)| seconds >= *size)
        .unwrap_or(UNITS.len() - 1);
    let mut rest = seconds;
    let mut out = String::new();
    for (size, name) in UNITS.iter().skip(first).take(units.max(1)) {
        let _ = write!(out, "{}{name}", rest / size);
        rest %= size;
    }
    out
}

/// The longest prefix of `s` that is at most `max` bytes and ends on a
/// character boundary: `prefix("héllo", 2)` is `h`.
#[must_use]
pub fn prefix(s: &str, max: usize) -> &str {
    s.get(..s.floor_char_boundary(max)).unwrap_or_default()
}

/// [`prefix`], and whether anything was cut.
#[must_use]
pub fn head_bytes(s: &str, max: usize) -> (&str, bool) {
    let head = prefix(s, max);
    (head, head.len() < s.len())
}

/// The longest suffix of `s` that is at most `max` bytes and starts on a
/// character boundary: `tail_bytes("héllo", 4)` is `llo`.
#[must_use]
pub fn tail_bytes(s: &str, max: usize) -> &str {
    let from = s.ceil_char_boundary(s.len().saturating_sub(max));
    s.get(from..).unwrap_or_default()
}

/// The first `max` characters of `s`, and whether anything was cut.
#[must_use]
pub fn head_chars(s: &str, max: usize) -> (&str, bool) {
    match s.char_indices().nth(max) {
        Some((byte, _)) => (s.get(..byte).unwrap_or_default(), true),
        None => (s, false),
    }
}

/// `s` cut to at most `max_chars` characters, `...` after a cut.
#[must_use]
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    match head_chars(s, max_chars) {
        (head, true) => format!("{head}..."),
        (whole, false) => whole.to_string(),
    }
}

/// `text` on one line: every run of whitespace (line breaks included) one
/// space, none at either end.
#[must_use]
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Short token count: `950`, `8.5k`, `84k`, `1M`, `1.2M`; with `decimal`
/// always one decimal (`12.3k`, `1.0k`, `2.0M`). The count is rounded
/// first and the unit picked from the rounded value, so `999_960` is
/// `1M` (`1.0M`), never `1000k`.
#[must_use]
pub fn tokens_with(n: u64, decimal: bool) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    let thousands = n as f64 / 1_000.0;
    let in_k = if decimal || n < 10_000 {
        format!("{thousands:.1}")
    } else {
        format!("{thousands:.0}")
    };
    // The unit from the value as rounded: `999.96` rounds to `1000.0`.
    let (value, unit) = if in_k.parse::<f64>().is_ok_and(|v| v < 1_000.0) {
        (in_k, "k")
    } else {
        (format!("{:.1}", n as f64 / 1_000_000.0), "M")
    };
    if decimal {
        return format!("{value}{unit}");
    }
    let value = value.strip_suffix(".0").unwrap_or(&value);
    format!("{value}{unit}")
}

/// [`tokens_with`] as the context gauge shows it (no tenth from 10k).
#[must_use]
pub fn tokens(n: u64) -> String {
    tokens_with(n, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_spans() {
        assert_eq!(count(1usize, "tool"), "1 tool");
        assert_eq!(count(0u32, "tool"), "0 tools");
        assert_eq!(count(3u64, "tool"), "3 tools");
        assert_eq!(span(0, 2), "0s");
        assert_eq!(span(7, 2), "7s");
        assert_eq!(span(5 * 60 + 6, 2), "5m6s");
        assert_eq!(span(60, 2), "1m0s");
        assert_eq!(span(3 * 3600 + 4 * 60 + 5, 2), "3h4m");
        assert_eq!(span(86_400 + 2 * 3600, 2), "1d2h");
        assert_eq!(span(59, 1), "59s");
        assert_eq!(span(3_599, 1), "59m");
        assert_eq!(span(86_399, 1), "23h");
        assert_eq!(span(3 * 86_400, 1), "3d");
    }

    #[test]
    fn a_prefix_ends_on_a_character_boundary() {
        assert_eq!(prefix("héllo", 0), "");
        assert_eq!(prefix("héllo", 1), "h");
        assert_eq!(prefix("héllo", 2), "h");
        assert_eq!(prefix("héllo", 3), "hé");
        assert_eq!(prefix("héllo", 6), "héllo");
        assert_eq!(prefix("héllo", 100), "héllo");
        assert_eq!(head_bytes("héllo", 2), ("h", true));
        assert_eq!(head_bytes("héllo", 6), ("héllo", false));
        assert_eq!(tail_bytes("héllo", 4), "llo");
        assert_eq!(tail_bytes("héllo", 5), "éllo");
        assert_eq!(tail_bytes("héllo", 0), "");
        assert_eq!(tail_bytes("héllo", 99), "héllo");
        assert_eq!(head_chars("héllo", 2), ("hé", true));
        assert_eq!(head_chars("héllo", 5), ("héllo", false));
        assert_eq!(truncate_chars("héllo", 2), "hé...");
        assert_eq!(truncate_chars("hi", 2), "hi");
    }

    #[test]
    fn one_line_makes_every_whitespace_run_one_space() {
        assert_eq!(one_line("  a\n\tb  c\r\n"), "a b c");
        assert_eq!(one_line(""), "");
    }

    /// Rounded first, then the unit: no `1000k`, in either form.
    #[test]
    fn token_counts_round_before_they_pick_a_unit() {
        let gauge = [
            (950, "950"),
            (8_500, "8.5k"),
            (9_960, "10k"),
            (84_000, "84k"),
            (999_499, "999k"),
            (999_960, "1M"),
            (1_000_000, "1M"),
            (1_240_000, "1.2M"),
        ];
        for (n, shown) in gauge {
            assert_eq!(tokens(n), shown, "{n}");
        }
        let decimal = [
            (950, "950"),
            (1_000, "1.0k"),
            (12_340, "12.3k"),
            (999_940, "999.9k"),
            (999_960, "1.0M"),
            (2_000_000, "2.0M"),
        ];
        for (n, shown) in decimal {
            assert_eq!(tokens_with(n, true), shown, "{n}");
        }
    }
}
