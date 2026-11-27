//! Token totals by model and their price (pure).
//!
//! The daemon counts them (`crate::daemon::usage`): every model call of a
//! conversation's agent under the model the provider accounted it to,
//! every helper's call to its model *and* to the helper's own totals
//! ([`SessionCost::helpers`], the `/cost` breakdown per helper), every
//! classifier call and every compaction summary. A client prices the
//! totals with a [`PriceTable`] whenever the footer is drawn or `/cost` is
//! asked for.
//!
//! The providers report a call's input in two conventions, which the
//! engine's provider accounting names (`ProviderTokenAccounting`): the
//! Anthropic client's `input_tokens` is the uncached input alone, beside
//! disjoint cache-read and cache-write counts; the OpenAI (Responses and
//! compatible) and Gemini clients' `input_tokens` is the whole prompt,
//! of which the cache counts are a part. [`UsageTotals`] keeps the
//! uncached input apart, from the engine's normalized total (everything
//! presented to the model, [`meerkat_core::TurnUsage::presented_tokens`])
//! less the cache counts, so every provider's call is priced once:
//!
//! ```text
//! ( input       × price.input
//! + cache_read  × (price.cached_input or price.input)
//! + cache_write × (price.cache_write  or price.input)
//! + output      × price.output ) / 1M
//! ```
//!
//! A compaction's summary call is counted from the usage its provider
//! reported (`crate::daemon::compaction_call`) when the daemon could tell
//! which call it was; otherwise from the engine's estimate of the history
//! it summarized, as input, and the summary, as output
//! ([`SessionCost::compactions`] keeps the two apart).

use std::fmt::Write as _;

use indexmap::IndexMap;
use meerkat_core::{TurnUsage, Usage};
use serde::{Deserialize, Serialize};

use crate::pricing::{ModelPrice, PriceSource, PriceTable, TOKENS_PER_UNIT};

/// Cumulative tokens of one model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageTotals {
    /// Uncached input tokens.
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    /// Input tokens read from the prompt cache.
    #[serde(default)]
    pub cache_read: u64,
    /// Input tokens written to the prompt cache.
    #[serde(default)]
    pub cache_write: u64,
}

impl UsageTotals {
    /// Totals of one call's [`Usage`]: by its provider accounting when it
    /// carries one (see the module docs), else with `input_tokens` taken
    /// as the uncached input (a usage made by hand, which names no
    /// provider's convention).
    #[must_use]
    pub fn from_usage(usage: &Usage) -> Self {
        Self::split(usage, usage.presented_tokens())
    }

    /// Totals of one call's [`TurnUsage`], by its provider accounting.
    #[must_use]
    #[cfg(test)]
    pub fn from_turn(turn: &TurnUsage) -> Self {
        Self::split(turn, Some(turn.presented_tokens()))
    }

    /// `usage` with the uncached input apart: `presented` (everything the
    /// model was given) less the cache counts, each clamped to what is
    /// left as the engine clamps them.
    fn split(usage: &Usage, presented: Option<u64>) -> Self {
        let read = usage.cache_read_tokens.unwrap_or(0);
        let write = usage.cache_creation_tokens.unwrap_or(0);
        let (input, cache_read, cache_write) = match presented {
            Some(presented) => {
                let read = read.min(presented);
                let write = write.min(presented - read);
                (presented - read - write, read, write)
            }
            None => (usage.input_tokens, read, write),
        };
        Self {
            input,
            output: usage.output_tokens,
            cache_read,
            cache_write,
        }
    }

    /// Every input token of the call, cached or not.
    #[must_use]
    pub fn presented(&self) -> u64 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }

    /// Add `other`.
    pub fn add(&mut self, other: &Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
    }

    #[must_use]
    pub fn is_zero(&self) -> bool {
        *self == Self::default()
    }

    /// Cost in USD at `price` (see the module docs).
    #[must_use]
    pub fn cost(&self, price: &ModelPrice) -> f64 {
        let cached = price.cached_input.unwrap_or(price.input);
        let write = price.cache_write.unwrap_or(price.input);
        (self.input as f64 * price.input
            + self.cache_read as f64 * cached
            + self.cache_write as f64 * write
            + self.output as f64 * price.output)
            / TOKENS_PER_UNIT
    }
}

/// Token totals by model id, in first-use order.
pub type ModelTotals = IndexMap<String, UsageTotals>;

/// Token totals of a session by model, in first-use order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCost {
    /// Totals by model id (the session's own calls and its helpers').
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub per_model: ModelTotals,
    /// The helpers' share of `per_model`, by helper label.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub helpers: IndexMap<String, ModelTotals>,
    /// The compaction summaries' share of `per_model` (counted since this
    /// table was kept: a record from before it has its summaries in
    /// `per_model` only).
    #[serde(default, skip_serializing_if = "Compactions::is_empty")]
    pub compactions: Compactions,
}

/// The compaction summary calls' tokens, by model, measured or estimated.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compactions {
    /// What the provider reported for the summary calls.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub measured: ModelTotals,
    /// The summary calls the daemon could not tell apart: the engine's
    /// history estimate as their input, the summary as their output.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub estimated: ModelTotals,
}

impl Compactions {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.measured.values().all(UsageTotals::is_zero)
            && self.estimated.values().all(UsageTotals::is_zero)
    }
}

/// The footer's view of a [`SessionCost`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CostReadout {
    /// Nothing used yet: no segment.
    Unused,
    /// Tokens were used but no model is priced: `$?`.
    Unknown,
    /// The cost, `~` when approximate.
    Usd {
        /// US dollars.
        usd: f64,
        /// Some model is unpriced, or a price is a stale estimate.
        approx: bool,
    },
}

impl SessionCost {
    /// Add `totals` to `model`.
    pub fn add(&mut self, model: &str, totals: &UsageTotals) {
        add_to(&mut self.per_model, model, totals);
    }

    /// Add one model call of the helper `label` (on `model`): to the
    /// session's totals and to the helper's own.
    pub fn record_helper(&mut self, label: &str, model: &str, usage: &Usage) {
        let totals = UsageTotals::from_usage(usage);
        if totals.is_zero() {
            return;
        }
        self.add(model, &totals);
        add_to(
            self.helpers.entry(label.to_string()).or_default(),
            model,
            &totals,
        );
    }

    /// Add one model call's usage to `model`.
    pub fn record(&mut self, model: &str, usage: &Usage) {
        self.add(model, &UsageTotals::from_usage(usage));
    }

    /// Count a compaction's summary call on `model` as its provider
    /// reported it.
    pub fn record_summary_call(&mut self, model: &str, totals: &UsageTotals) {
        if totals.is_zero() {
            return;
        }
        self.add(model, totals);
        add_to(&mut self.compactions.measured, model, totals);
    }

    /// Count a compaction's summary call on `model` from the engine's
    /// estimate: `input` is the history estimate the compaction reported,
    /// `output` the summary size.
    pub fn record_compaction(&mut self, model: &str, input: u64, output: u64) {
        let totals = UsageTotals {
            input,
            output,
            ..UsageTotals::default()
        };
        if totals.is_zero() {
            return;
        }
        self.add(model, &totals);
        add_to(&mut self.compactions.estimated, model, &totals);
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.per_model.values().all(UsageTotals::is_zero)
    }

    /// The cost of the priced models in USD, and whether every model was
    /// priced. Unpriced models are left out.
    #[must_use]
    pub fn cost_usd(&self, prices: &PriceTable) -> (f64, bool) {
        let mut usd = 0.0;
        let mut all = true;
        for (model, totals) in &self.per_model {
            match prices.lookup(model) {
                Some((price, _)) => usd += totals.cost(&price),
                None => all = false,
            }
        }
        (usd, all)
    }

    /// The footer readout.
    #[must_use]
    pub fn readout(&self, prices: &PriceTable) -> CostReadout {
        if self.is_empty() {
            return CostReadout::Unused;
        }
        let mut usd = 0.0;
        let mut priced = 0_usize;
        let mut approx = false;
        for (model, totals) in self.per_model.iter().filter(|(_, t)| !t.is_zero()) {
            match prices.lookup(model) {
                Some((price, source)) => {
                    usd += totals.cost(&price);
                    priced += 1;
                    approx |= source.is_estimate();
                }
                None => approx = true,
            }
        }
        if priced == 0 {
            CostReadout::Unknown
        } else {
            CostReadout::Usd { usd, approx }
        }
    }

    /// The `/cost` breakdown: tokens and USD per model with the price source.
    #[must_use]
    pub fn breakdown(&self, prices: &PriceTable) -> String {
        if self.is_empty() {
            return "cost: no tokens used in this session yet".to_string();
        }
        let mut out = String::from("Session cost by model:");
        for (model, totals) in self.per_model.iter().filter(|(_, t)| !t.is_zero()) {
            let usd = match prices.lookup(model) {
                Some((price, source)) => {
                    format!(
                        "{} ({})",
                        fmt_usd(totals.cost(&price)),
                        source_label(source)
                    )
                }
                None => "$? (no price; set [pricing.\"<id>\"] in tui.toml)".to_string(),
            };
            let _ = write!(
                out,
                "\n  {model}: in {} · out {} · cache read {} · cache write {} · {usd}",
                totals.input, totals.output, totals.cache_read, totals.cache_write
            );
        }
        self.write_helpers(&mut out, prices);
        self.write_compactions(&mut out, prices);
        let total = match self.readout(prices) {
            CostReadout::Usd { usd, approx } => {
                format!("{}{}", if approx { "~" } else { "" }, fmt_usd(usd))
            }
            CostReadout::Unknown | CostReadout::Unused => "$?".to_string(),
        };
        let _ = write!(out, "\n  total: {total}");
        out
    }
}

impl SessionCost {
    /// The helpers' lines of the `/cost` breakdown (their tokens are
    /// already in the per-model lines above them).
    fn write_helpers(&self, out: &mut String, prices: &PriceTable) {
        let helpers: Vec<_> = self
            .helpers
            .iter()
            .flat_map(|(label, models)| models.iter().map(move |(model, t)| (label, model, t)))
            .filter(|(_, _, totals)| !totals.is_zero())
            .collect();
        if helpers.is_empty() {
            return;
        }
        out.push_str("\n  of which helpers:");
        for (label, model, totals) in helpers {
            write_share(out, &format!("{label} ({model})"), model, totals, prices);
        }
    }

    /// The compaction summaries' lines of the `/cost` breakdown (their
    /// tokens are in the per-model lines too).
    fn write_compactions(&self, out: &mut String, prices: &PriceTable) {
        if self.compactions.is_empty() {
            return;
        }
        out.push_str("\n  of which compaction summaries:");
        let Compactions {
            measured,
            estimated,
        } = &self.compactions;
        for (model, totals) in measured.iter().filter(|(_, t)| !t.is_zero()) {
            write_share(out, model, model, totals, prices);
        }
        for (model, totals) in estimated.iter().filter(|(_, t)| !t.is_zero()) {
            write_share(out, &format!("{model} (estimated)"), model, totals, prices);
        }
    }
}

/// One line of a share of the `/cost` breakdown: `label`'s tokens and
/// their price on `model`.
fn write_share(
    out: &mut String,
    label: &str,
    model: &str,
    totals: &UsageTotals,
    prices: &PriceTable,
) {
    let usd = prices.lookup(model).map_or_else(
        || "$?".to_string(),
        |(price, _)| fmt_usd(totals.cost(&price)),
    );
    let _ = write!(
        out,
        "\n    {label}: in {} · out {} · cache read {} · cache write {} · {usd}",
        totals.input, totals.output, totals.cache_read, totals.cache_write
    );
}

/// Add `totals` to `model` in `per_model`.
fn add_to(per_model: &mut ModelTotals, model: &str, totals: &UsageTotals) {
    if totals.is_zero() {
        return;
    }
    match per_model.get_mut(model) {
        Some(existing) => existing.add(totals),
        None => {
            per_model.insert(model.to_string(), *totals);
        }
    }
}

fn source_label(source: PriceSource) -> String {
    format!("price: {source}")
}

/// Tokens in the context after a model call: everything presented to the
/// model (the provider's normalized count: the cache counts are in it
/// once, whichever convention the provider reports in) plus the reply.
#[must_use]
pub fn context_tokens(usage: &TurnUsage) -> u64 {
    usage.normalized_total_tokens()
}

/// Dollars with two decimals; `$12.3k` from 10 000 on.
#[must_use]
pub fn fmt_usd(usd: f64) -> String {
    if usd >= 10_000.0 {
        format!("${:.1}k", usd / 1_000.0)
    } else {
        format!("${usd:.2}")
    }
}

/// The footer segment text: `$0.42`, `~$0.42`, `$?`; `None` before any use.
#[must_use]
pub fn cost_segment_text(cost: &SessionCost, prices: &PriceTable) -> Option<String> {
    match cost.readout(prices) {
        CostReadout::Unused => None,
        CostReadout::Unknown => Some("$?".to_string()),
        CostReadout::Usd { usd, approx } => {
            Some(format!("{}{}", if approx { "~" } else { "" }, fmt_usd(usd)))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::config::PricingSection;
    use std::collections::BTreeMap;

    fn price(input: f64, output: f64, cached: Option<f64>, write: Option<f64>) -> PricingSection {
        PricingSection {
            input: Some(input),
            output: Some(output),
            cached_input: cached,
            cache_write: write,
        }
    }

    fn table(entries: &[(&str, PricingSection)]) -> PriceTable {
        PriceTable::new(
            entries
                .iter()
                .map(|(id, p)| ((*id).to_string(), *p))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    fn usage(input: u64, output: u64, read: Option<u64>, write: Option<u64>) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_creation_tokens: write,
            cache_read_tokens: read,
            ..Usage::default()
        }
    }

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn arithmetic_with_cache_tokens() {
        let prices = table(&[("opus", price(15.0, 75.0, Some(1.5), Some(18.75)))]);
        let mut cost = SessionCost::default();
        cost.record(
            "opus",
            &usage(1_000_000, 100_000, Some(2_000_000), Some(400_000)),
        );
        // 1M × 15 + 2M × 1.5 + 0.4M × 18.75 + 0.1M × 75 = 15 + 3 + 7.5 + 7.5
        close(cost.cost_usd(&prices).0, 33.0);
        assert!(cost.cost_usd(&prices).1);
        // Missing cache prices fall back to the input price.
        let prices = table(&[("opus", price(15.0, 75.0, None, None))]);
        // 1M × 15 + 2M × 15 + 0.4M × 15 + 0.1M × 75 = 15 + 30 + 6 + 7.5
        close(cost.cost_usd(&prices).0, 58.5);
    }

    #[test]
    fn multi_model_accumulation_in_first_use_order() {
        let prices = table(&[
            ("a", price(1.0, 2.0, None, None)),
            ("b", price(10.0, 20.0, None, None)),
        ]);
        let mut cost = SessionCost::default();
        cost.record("a", &usage(500_000, 0, None, None));
        cost.record("b", &usage(100_000, 50_000, None, None));
        cost.record("a", &usage(500_000, 1_000_000, None, None));
        cost.record("b", &usage(0, 50_000, None, None));
        cost.record("a", &usage(0, 0, None, None));
        let order: Vec<&str> = cost.per_model.keys().map(String::as_str).collect();
        assert_eq!(order, vec!["a", "b"]);
        assert_eq!(cost.per_model["a"].input, 1_000_000);
        assert_eq!(cost.per_model["b"].output, 100_000);
        // a: 1 + 2 = 3; b: 1 + 2 = 3
        close(cost.cost_usd(&prices).0, 6.0);
        assert_eq!(cost_segment_text(&cost, &prices).as_deref(), Some("$6.00"));
    }

    #[test]
    fn unknown_model_is_excluded_and_marks_approx() {
        let prices = table(&[("a", price(1.0, 2.0, None, None))]);
        let mut cost = SessionCost::default();
        cost.record("a", &usage(420_000, 0, None, None));
        cost.record("mystery", &usage(1_000_000, 1_000_000, None, None));
        let (usd, all) = cost.cost_usd(&prices);
        close(usd, 0.42);
        assert!(!all);
        assert_eq!(cost_segment_text(&cost, &prices).as_deref(), Some("~$0.42"));
    }

    #[test]
    fn nothing_priced_is_unknown_and_nothing_used_is_hidden() {
        let prices = PriceTable::default();
        let mut cost = SessionCost::default();
        assert_eq!(cost.readout(&prices), CostReadout::Unused);
        assert_eq!(cost_segment_text(&cost, &prices), None);
        cost.record("mystery", &usage(10, 10, None, None));
        assert_eq!(cost.readout(&prices), CostReadout::Unknown);
        assert_eq!(cost_segment_text(&cost, &prices).as_deref(), Some("$?"));
    }

    #[test]
    fn helper_usage_rolls_up_and_is_broken_down() {
        let prices = table(&[
            ("a", price(1.0, 2.0, None, None)),
            ("b", price(10.0, 20.0, None, None)),
        ]);
        let mut cost = SessionCost::default();
        cost.record("a", &usage(1_000_000, 0, None, None));
        cost.record_helper("scout", "a", &usage(500_000, 0, None, None));
        cost.record_helper("scout", "b", &usage(100_000, 0, None, None));
        cost.record_helper("idle", "b", &usage(0, 0, None, None));
        // The footer total includes the helpers: 1.5 × 1 + 0.1 × 10.
        close(cost.cost_usd(&prices).0, 2.5);
        assert_eq!(cost.per_model["a"].input, 1_500_000);
        assert_eq!(cost.helpers["scout"]["b"].input, 100_000);
        assert!(!cost.helpers.contains_key("idle"));
        let text = cost.breakdown(&prices);
        assert!(text.contains("of which helpers:"), "{text}");
        assert!(text.contains("scout (a): in 500000"), "{text}");
        assert!(
            text.contains("scout (b): in 100000 · out 0 · cache read 0 · cache write 0 · $1.00"),
            "{text}"
        );
        assert!(text.contains("total: $2.50"), "{text}");
        // No helpers, no helper section.
        let mut plain = SessionCost::default();
        plain.record("a", &usage(1, 1, None, None));
        assert!(!plain.breakdown(&prices).contains("helpers"));
    }

    #[test]
    fn a_compaction_counts_its_estimate() {
        let mut compacted = SessionCost::default();
        compacted.record_compaction("a", 1_000_000, 10);
        assert_eq!(compacted.per_model["a"].input, 1_000_000);
        assert_eq!(compacted.per_model["a"].output, 10);
        assert_eq!(compacted.compactions.estimated["a"].input, 1_000_000);
        assert!(compacted.compactions.measured.is_empty());
        compacted.record_compaction("a", 0, 0);
        assert_eq!(compacted.per_model["a"].output, 10);
    }

    /// A measured summary call is in the model's totals and in the
    /// summaries' share, which `/cost` lists on lines of their own, the
    /// estimated ones marked so.
    #[test]
    fn a_measured_summary_call_is_its_own_share_of_the_breakdown() {
        let prices = table(&[("a", price(1.0, 2.0, Some(0.1), Some(1.25)))]);
        let mut cost = SessionCost::default();
        cost.record("a", &usage(1_000_000, 0, None, None));
        let summary = UsageTotals {
            input: 1_000_000,
            output: 500_000,
            cache_read: 0,
            cache_write: 0,
        };
        cost.record_summary_call("a", &summary);
        cost.record_summary_call("a", &UsageTotals::default());
        cost.record_compaction("a", 2_000, 10);
        assert_eq!(cost.per_model["a"].input, 2_002_000);
        assert_eq!(cost.compactions.measured["a"], summary);
        let text = cost.breakdown(&prices);
        assert!(text.contains("of which compaction summaries:"), "{text}");
        assert!(
            text.contains(
                "\n    a: in 1000000 · out 500000 · cache read 0 · cache write 0 · $2.00"
            ),
            "{text}"
        );
        assert!(
            text.contains("\n    a (estimated): in 2000 · out 10 · cache read 0 · cache write 0"),
            "{text}"
        );
        let mut plain = SessionCost::default();
        plain.record("a", &usage(1, 1, None, None));
        assert!(!plain.breakdown(&prices).contains("compaction"));
    }

    /// A record written before the summaries' table was kept (no
    /// `compactions`) still reads, and a record without summaries writes
    /// none.
    #[test]
    fn the_summaries_table_is_optional_in_the_stored_record() {
        let old: SessionCost = toml::from_str("[per_model.a]\ninput = 5\noutput = 1\n").unwrap();
        assert!(old.compactions.is_empty());
        assert_eq!(old.per_model["a"].input, 5);
        assert!(!toml::to_string(&old).unwrap().contains("compactions"));
        let mut new = old;
        new.record_compaction("a", 7, 1);
        let text = toml::to_string(&new).unwrap();
        assert!(text.contains("compactions"), "{text}");
        assert_eq!(toml::from_str::<SessionCost>(&text).unwrap(), new);
    }

    fn turn(usage: Usage, accounting: meerkat_core::ProviderTokenAccounting) -> TurnUsage {
        TurnUsage::new(usage, accounting)
    }

    /// Each provider's convention is priced and gauged once. Anthropic
    /// reports the uncached input apart from disjoint cache counts; the
    /// OpenAI Responses, OpenAI-compatible and Gemini clients report the
    /// whole prompt as input, the cache counts a part of it. The same
    /// call (1000 tokens presented: 600 read from the cache, 100 written
    /// to it, 300 uncached; 50 out) comes out the same from each.
    #[test]
    fn every_provider_s_cached_tokens_are_counted_once() {
        use meerkat_core::ProviderTokenAccounting as Accounting;
        let expected = UsageTotals {
            input: 300,
            output: 50,
            cache_read: 600,
            cache_write: 100,
        };
        let anthropic = turn(
            usage(300, 50, Some(600), Some(100)),
            Accounting::anthropic("claude", 300, 100, 600),
        );
        let inclusive = usage(1_000, 50, Some(600), Some(100));
        let openai = turn(inclusive.clone(), Accounting::openai("gpt", 1_000));
        let compatible = turn(inclusive, Accounting::openai_compatible("proxied", 1_000));
        let gemini_usage = usage(1_000, 50, Some(600), None);
        let gemini = turn(gemini_usage, Accounting::gemini("gemini", 1_000));
        for (name, call) in [
            ("anthropic", &anthropic),
            ("openai", &openai),
            ("compatible", &compatible),
        ] {
            assert_eq!(UsageTotals::from_turn(call), expected, "{name}");
            // The same through a `Usage` that keeps the accounting.
            assert_eq!(
                UsageTotals::from_usage(&call.clone().into_inner()),
                expected,
                "{name}"
            );
            assert_eq!(context_tokens(call), 1_050, "{name}");
        }
        assert_eq!(
            UsageTotals::from_turn(&gemini),
            UsageTotals {
                input: 400,
                cache_write: 0,
                ..expected
            }
        );
        assert_eq!(context_tokens(&gemini), 1_050);
        // Priced once: 300 × 1 + 600 × 0.1 + 100 × 1.25 + 50 × 2 (per 1M).
        let prices = table(&[("m", price(1.0, 2.0, Some(0.1), Some(1.25)))]);
        let (price, _) = prices.lookup("m").unwrap();
        for call in [&anthropic, &openai, &compatible] {
            close(
                UsageTotals::from_turn(call).cost(&price),
                (300.0 + 60.0 + 125.0 + 100.0) / 1_000_000.0,
            );
        }
        // A cache count larger than the prompt is clamped to it.
        let odd = turn(
            usage(100, 0, Some(150), Some(10)),
            Accounting::openai("gpt", 100),
        );
        assert_eq!(
            UsageTotals::from_turn(&odd),
            UsageTotals {
                input: 0,
                output: 0,
                cache_read: 100,
                cache_write: 0,
            }
        );
        // A usage with no accounting is taken as Anthropic's.
        assert_eq!(
            UsageTotals::from_usage(&usage(300, 50, Some(600), Some(100))),
            expected
        );
        assert_eq!(expected.presented(), 1_000);
    }

    #[test]
    fn usd_formatting() {
        assert_eq!(fmt_usd(0.0), "$0.00");
        assert_eq!(fmt_usd(0.424), "$0.42");
        assert_eq!(fmt_usd(9_999.994), "$9999.99");
        assert_eq!(fmt_usd(12_345.0), "$12.3k");
    }

    #[test]
    fn breakdown_lists_tokens_usd_and_source() {
        let prices = table(&[("a", price(1.0, 2.0, None, None))]);
        let mut cost = SessionCost::default();
        cost.record("a", &usage(1_000_000, 500_000, Some(7), Some(3)));
        cost.record("mystery", &usage(1, 1, None, None));
        let text = cost.breakdown(&prices);
        assert!(
            text.contains("a: in 1000000 · out 500000 · cache read 7 · cache write 3"),
            "{text}"
        );
        assert!(text.contains("(price: tui.toml)"), "{text}");
        assert!(text.contains("mystery: in 1 · out 1"), "{text}");
        assert!(text.contains("$? (no price"), "{text}");
        assert!(text.contains("total: ~$2.00"), "{text}");
        assert!(
            SessionCost::default()
                .breakdown(&prices)
                .contains("no tokens used")
        );
    }
}
