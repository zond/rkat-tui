//! The review numbers `/auto stats` shows (the research's change 9): how
//! often the classifier reviews, flags and blocks (by rule), how often
//! auto mode pauses, how often the user overrides a block with `/approve`,
//! how often the user allows what is put to them, and what the classifier
//! costs in tokens and time, since the daemon started and per session.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One set of review numbers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewStats {
    /// Classifier reviews (a cached allow is none).
    pub reviews: u64,
    /// Reviews the quick first stage flagged (stage 2 ran).
    pub flagged: u64,
    /// Blocks, by the rule the classifier named (`none` when it named
    /// none).
    pub blocks: BTreeMap<String, u64>,
    /// Reviews that could not be completed.
    pub errors: u64,
    /// Times auto mode paused after blocks.
    pub pauses: u64,
    /// Blocked calls the user approved with `/approve` (once, for the
    /// session or always).
    pub approvals: u64,
    /// Questions the user answered.
    pub asks: u64,
    /// Of those, the ones they allowed.
    pub asks_allowed: u64,
    /// The classifier's input tokens.
    pub input_tokens: u64,
    /// Its output tokens.
    pub output_tokens: u64,
    /// The time its reviews took, in milliseconds.
    pub review_ms: u64,
}

impl ReviewStats {
    /// Blocks in all.
    #[must_use]
    pub fn blocked(&self) -> u64 {
        self.blocks.values().sum()
    }

    /// The numbers as `/auto stats` lists them.
    #[must_use]
    pub fn describe(&self) -> String {
        let pct = |part: u64, whole: u64| {
            if whole == 0 {
                "-".to_string()
            } else {
                #[allow(clippy::cast_precision_loss)]
                let pct = part as f64 * 100.0 / whole as f64;
                format!("{pct:.0}%")
            }
        };
        let rules = if self.blocks.is_empty() {
            String::new()
        } else {
            let listed: Vec<String> = self
                .blocks
                .iter()
                .map(|(rule, n)| format!("{rule} {n}"))
                .collect();
            format!(" ({})", listed.join(", "))
        };
        let blocked = self.blocked();
        format!(
            "reviews {} · flagged by stage 1 {} ({}) · blocked {blocked}{rules} · failed {} · pauses {} · /approve overrides {} ({} of blocks) · asked you {}, allowed {} ({}) · classifier {} in / {} out tokens, {:.1} s",
            self.reviews,
            self.flagged,
            pct(self.flagged, self.reviews),
            self.errors,
            self.pauses,
            self.approvals,
            pct(self.approvals, blocked),
            self.asks,
            self.asks_allowed,
            pct(self.asks_allowed, self.asks),
            self.input_tokens,
            self.output_tokens,
            {
                #[allow(clippy::cast_precision_loss)]
                let secs = self.review_ms as f64 / 1000.0;
                secs
            },
        )
    }
}

/// The numbers since the daemon started, and per session.
#[derive(Debug, Default)]
pub(crate) struct StatsBook {
    /// Since the daemon started.
    pub total: ReviewStats,
    /// Per session (an agent's, its helpers' calls included).
    pub by_session: std::collections::HashMap<meerkat_core::SessionId, ReviewStats>,
}

impl StatsBook {
    /// Apply `change` to the daemon's numbers and to `session`'s.
    pub fn add(
        &mut self,
        session: Option<&meerkat_core::SessionId>,
        change: impl Fn(&mut ReviewStats),
    ) {
        change(&mut self.total);
        if let Some(session) = session {
            change(self.by_session.entry(session.clone()).or_default());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_numbers_read_as_rates() {
        let mut stats = ReviewStats {
            reviews: 10,
            flagged: 4,
            approvals: 1,
            asks: 4,
            asks_allowed: 3,
            input_tokens: 1200,
            output_tokens: 80,
            review_ms: 2500,
            ..ReviewStats::default()
        };
        stats.blocks.insert("T3".into(), 2);
        let text = stats.describe();
        for part in [
            "reviews 10",
            "flagged by stage 1 4 (40%)",
            "blocked 2 (T3 2)",
            "/approve overrides 1 (50% of blocks)",
            "asked you 4, allowed 3 (75%)",
            "1200 in / 80 out tokens, 2.5 s",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }
        assert!(
            ReviewStats::default()
                .describe()
                .contains("flagged by stage 1 0 (-)")
        );
    }
}
