//! The two small formatters every message shares: a count with its noun
//! (`1 tool`, `3 tools`) and a length of time (`5m6s`, `5m`).

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
}
