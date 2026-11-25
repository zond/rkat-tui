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

/// Short token count for the context gauge: `950`, `8.5k`, `84k`, `1M`, `1.2M`.
#[must_use]
pub fn tokens(n: u64) -> String {
    let (value, unit) = match n {
        0..1_000 => return n.to_string(),
        1_000..10_000 => (format!("{:.1}", n as f64 / 1_000.0), "k"),
        10_000..1_000_000 => (format!("{:.0}", n as f64 / 1_000.0), "k"),
        _ => (format!("{:.1}", n as f64 / 1_000_000.0), "M"),
    };
    let value = value.strip_suffix(".0").unwrap_or(&value);
    format!("{value}{unit}")
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
    }
}
