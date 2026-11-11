//! The cost readout: the price table (overrides, endpoint prices, the
//! community list refreshed in the background) and the per-session token
//! totals by model, persisted in the session's cost record
//! ([`super::cost_store`]).
//!
//! The UI counts every `TurnCompleted` usage under the model the provider
//! accounted it to (the identity `TurnUsage` carries), every classifier
//! call under the classifier model and every compaction (estimated input
//! plus the summary), and sends the totals as
//! [`crate::ui::Effect::CostSnapshot`]; the runner stores them next to the
//! session.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::task::{JoinError, JoinHandle};

use super::Workbench;
use super::cost_store::{load_cost, save_cost};
use crate::pricing::{CommunityPrices, FETCH_TIMEOUT, ModelPrice, PricingError, refresh_community};
use crate::ui::{Msg, SessionCost, SessionCostSnapshot, StatusUpdate};

/// A community price-list refresh running in the background.
pub(super) type PriceFetch = JoinHandle<Result<CommunityPrices, PricingError>>;

/// A cost record: its realm and session.
type RecordKey = (PathBuf, String);

/// The cost records this client writes, off its loop (each one is a
/// temporary file, an fsync and a rename, and a snapshot comes with every
/// model call): the newest snapshot of a session waits here until it is
/// written, and a read meanwhile is answered from here.
#[derive(Debug, Default)]
pub(super) struct CostWriter {
    /// Not written yet: the newest snapshot of each record and its order.
    pending: parking_lot::Mutex<std::collections::HashMap<RecordKey, (u64, SessionCost)>>,
    next: std::sync::atomic::AtomicU64,
    /// One record write at a time.
    writing: parking_lot::Mutex<()>,
}

impl CostWriter {
    /// `cost` is `key`'s newest snapshot.
    fn put(&self, key: RecordKey, cost: SessionCost) {
        let order = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.pending.lock().insert(key, (order, cost));
    }

    /// Write `key`'s newest snapshot, if one waits (blocking file I/O).
    fn write(&self, key: &RecordKey) {
        let _one = self.writing.lock();
        let Some((order, cost)) = self.pending.lock().get(key).cloned() else {
            return;
        };
        if let Err(err) = save_cost(&key.0, &key.1, &cost) {
            tracing::warn!(error = %err, "could not record the session's cost");
        }
        let mut pending = self.pending.lock();
        if pending.get(key).is_some_and(|(newest, _)| *newest == order) {
            pending.remove(key);
        }
    }

    /// `key`'s snapshot not written yet, if any.
    fn waiting(&self, key: &RecordKey) -> Option<SessionCost> {
        self.pending.lock().get(key).map(|(_, cost)| cost.clone())
    }
}

/// Start refreshing the community list from `url` (cached at `cache`).
pub(super) fn spawn_price_fetch(url: String, cache: Option<PathBuf>) -> PriceFetch {
    tokio::spawn(async move { refresh_community(&url, cache.as_deref(), FETCH_TIMEOUT).await })
}

impl Workbench {
    /// Send the current price table to the UI.
    pub fn push_prices(&self) {
        self.send(Msg::Status(StatusUpdate::Prices(Arc::clone(&self.prices))));
    }

    /// Replace the endpoint (LiteLLM `/model/info`) prices and tell the UI.
    pub(super) fn set_endpoint_prices(&mut self, prices: Vec<(String, ModelPrice)>) {
        if prices.is_empty() && self.prices.is_empty() {
            return;
        }
        Arc::make_mut(&mut self.prices).set_endpoint_prices(prices);
        self.push_prices();
    }

    /// The community refresh finished: use the new list (the task wrote the
    /// cache) or keep the old one.
    pub(super) fn price_fetch_finished(
        &mut self,
        joined: Result<Result<CommunityPrices, PricingError>, JoinError>,
    ) {
        match joined {
            Ok(Ok(community)) => {
                tracing::debug!(models = community.len(), "community price list refreshed");
                Arc::make_mut(&mut self.prices).set_community(community);
                self.push_prices();
            }
            Ok(Err(err)) => {
                tracing::warn!(error = %err, "could not fetch the community price list");
            }
            Err(err) => tracing::warn!(error = %err, "price-list task failed"),
        }
    }

    /// Answer [`crate::ui::Effect::CostSnapshot`]: store the totals next to
    /// `session`, written by a blocking task (on a runtime) so the loop
    /// that must take Esc never waits for a file system sync.
    pub fn save_cost(&self, session: &str, snapshot: &SessionCostSnapshot) {
        let key = (self.scope.realm_root(), session.to_string());
        self.costs.put(key.clone(), snapshot.clone());
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                let costs = Arc::clone(&self.costs);
                drop(runtime.spawn_blocking(move || costs.write(&key)));
            }
            Err(_) => self.costs.write(&key),
        }
    }

    /// The cost totals recorded for `session`, if any (a snapshot not
    /// written yet included).
    #[must_use]
    pub fn recorded_cost(&self, session: &str) -> Option<SessionCost> {
        let key = (self.scope.realm_root(), session.to_string());
        self.costs
            .waiting(&key)
            .or_else(|| load_cost(&key.0, &key.1))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A snapshot waits until it is written and answers reads meanwhile;
    /// the write takes the newest one, and an older write never lands
    /// over it.
    #[test]
    fn the_newest_snapshot_is_read_back_and_written() {
        let dir = tempfile::tempdir().unwrap();
        let key = (dir.path().to_path_buf(), "s1".to_string());
        let writer = CostWriter::default();
        let usage = |input| meerkat_core::Usage {
            input_tokens: input,
            ..meerkat_core::Usage::default()
        };
        let mut first = SessionCost::default();
        first.record("m", &usage(10));
        let mut second = first.clone();
        second.record("m", &usage(5));
        writer.put(key.clone(), first);
        writer.put(key.clone(), second.clone());
        assert_eq!(writer.waiting(&key), Some(second.clone()));
        assert_eq!(load_cost(&key.0, &key.1), None, "nothing written yet");
        writer.write(&key);
        assert_eq!(writer.waiting(&key), None);
        assert_eq!(load_cost(&key.0, &key.1), Some(second.clone()));
        // A second write with nothing waiting changes nothing.
        writer.write(&key);
        assert_eq!(load_cost(&key.0, &key.1), Some(second));
    }
}
