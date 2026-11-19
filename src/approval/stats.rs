//! The review numbers `/auto stats` shows: how often the classifier
//! reviews, flags and blocks (by rule), how often auto mode pauses, how
//! often the user overrides a block with `/permissions`, how often the user
//! allows what is put to them, and what the classifier costs in tokens and
//! time, since the daemon started and per session. They are there to tell
//! whether the policy fits this user's work: a rule that blocks often and
//! is overridden as often asks for a statement or a rule, a high flag rate
//! for a cheaper first stage.

use std::collections::{BTreeMap, HashMap};

use meerkat_core::SessionId;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::audit::ReviewRecord;

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
    /// Blocked calls the user approved with `/permissions` (once, for the
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
    /// Runs that reached their checkpoint and were decided
    /// ([`super::checkpoint`]).
    #[serde(default)]
    pub checkpoints: u64,
    /// Of those, the ones that continued automatically.
    #[serde(default)]
    pub checkpoint_continues: u64,
    /// And the ones that stopped.
    #[serde(default)]
    pub checkpoint_stops: u64,
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
        let checkpoints = if self.checkpoints == 0 {
            String::new()
        } else {
            format!(
                " · checkpoints {}: continued {}, stopped {}",
                self.checkpoints, self.checkpoint_continues, self.checkpoint_stops
            )
        };
        format!(
            "reviews {} · flagged by stage 1 {} ({}) · blocked {blocked}{rules} · failed {} · pauses {} · approved in /permissions {} ({} of blocks) · asked you {}, allowed {} ({}) · classifier {} in / {} out tokens, {:.1} s{checkpoints}",
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

/// The numbers since the daemon started, and per session: the engine's
/// (`crate::approval::ApprovalEngine::stats`). Its lock is taken alone.
#[derive(Debug, Default)]
pub struct Stats {
    book: Mutex<Book>,
}

#[derive(Debug, Default)]
struct Book {
    /// Since the daemon started.
    total: ReviewStats,
    /// Per session (an agent's, its helpers' calls included).
    by_session: HashMap<SessionId, ReviewStats>,
}

impl Stats {
    /// Apply `change` to the daemon's numbers and to `session`'s.
    pub(super) fn add(&self, session: Option<&SessionId>, change: impl Fn(&mut ReviewStats)) {
        let mut book = self.book.lock();
        change(&mut book.total);
        if let Some(session) = session {
            change(book.by_session.entry(session.clone()).or_default());
        }
    }

    /// Count one classifier review of a call of `session`.
    pub(super) fn review(&self, session: &SessionId, record: &ReviewRecord) {
        self.add(Some(session), |stats| {
            stats.reviews += 1;
            if record.stage == 2 {
                stats.flagged += 1;
            }
            match record.verdict.as_str() {
                "block" => {
                    *stats
                        .blocks
                        .entry(record.rule.clone().unwrap_or_else(|| "none".into()))
                        .or_insert(0) += 1;
                }
                "error" => stats.errors += 1,
                _ => {}
            }
            stats.input_tokens += record.input_tokens;
            stats.output_tokens += record.output_tokens;
            stats.review_ms += record.latency_ms;
        });
    }

    /// The numbers since the daemon started, and `session`'s.
    #[must_use]
    pub fn of(&self, session: Option<&SessionId>) -> (ReviewStats, ReviewStats) {
        let book = self.book.lock();
        let mine = session
            .and_then(|s| book.by_session.get(s).cloned())
            .unwrap_or_default();
        (book.total.clone(), mine)
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
            "approved in /permissions 1 (50% of blocks)",
            "asked you 4, allowed 3 (75%)",
            "1200 in / 80 out tokens, 2.5 s",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }
        assert!(!text.contains("checkpoints"), "{text}");
        assert!(
            ReviewStats::default()
                .describe()
                .contains("flagged by stage 1 0 (-)")
        );
        stats.checkpoints = 3;
        stats.checkpoint_continues = 2;
        stats.checkpoint_stops = 1;
        assert!(
            stats
                .describe()
                .ends_with(" · checkpoints 3: continued 2, stopped 1"),
            "{}",
            stats.describe()
        );
    }
}
