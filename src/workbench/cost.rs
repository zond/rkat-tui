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
    /// `session`.
    pub fn save_cost(&self, session: &str, snapshot: &SessionCostSnapshot) {
        if let Err(err) = save_cost(&self.scope.realm_root(), session, snapshot) {
            tracing::warn!(error = %err, "could not record the session's cost");
        }
    }

    /// The cost totals recorded for `session`, if any.
    #[must_use]
    pub fn recorded_cost(&self, session: &str) -> Option<SessionCost> {
        load_cost(&self.scope.realm_root(), session)
    }
}
