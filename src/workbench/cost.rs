//! The cost readout's prices: the price table (overrides, endpoint
//! prices, the community list refreshed in the background). The token
//! totals it prices are the daemon's (`rkat/usage`, `rkat/cost`).

use std::path::PathBuf;
use std::sync::Arc;

use tokio::task::{JoinError, JoinHandle};

use super::Workbench;
use crate::pricing::{CommunityPrices, FETCH_TIMEOUT, ModelPrice, PricingError, refresh_community};
use crate::ui::{Msg, StatusUpdate};

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
}
