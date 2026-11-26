//! The endpoints' model lists and the `/model` picker.
//!
//! Every endpoint's list is fetched in parallel (each request with its own
//! timeout) in the background and cached for the process; `/model` opens
//! the picker from the cache (fetching when it is empty), `/model refresh`
//! refetches. The lists come from the endpoints only, never from the
//! `meerkat-models` catalog. The cache also resolves bare ids (see
//! [`crate::endpoints::parse_model_ref`]); what a switch then does is the
//! runner's own.

use std::sync::Arc;

use tokio::task::{JoinError, JoinHandle};

use super::Workbench;
use crate::endpoints::needs_lookup;
use crate::models::{
    FetchResults, ListModelsError, ModelCache, ModelGroup, fetch_warning, with_current,
};
use crate::ui::{Msg, NoticeLevel};

/// A fetch of every endpoint's list running in the background.
pub(super) type ModelFetch = JoinHandle<FetchResults>;

/// The endpoint and model the session runs on now (`""`: none), which the
/// picker lists first.
#[derive(Debug, Clone, Copy, Default)]
pub struct Current<'a> {
    pub endpoint: &'a str,
    pub model: &'a str,
}

/// A model picker asked for before the lists were fetched: which one, and
/// the model it marks as current (the agent's, or the classifier's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Waiting {
    /// The classifier's picker (`/auto model`).
    pub(crate) classifier: bool,
    /// The endpoint of the model it marks.
    pub(crate) endpoint: String,
    /// The model it marks.
    pub(crate) model: String,
}

impl Workbench {
    /// The fetched lists, once there are any.
    #[must_use]
    pub fn models(&self) -> Option<&ModelCache> {
        self.models.as_ref()
    }

    /// This process's endpoint called `name`.
    #[must_use]
    pub fn endpoint(&self, name: &str) -> Option<&crate::endpoints::Endpoint> {
        self.endpoints.get(name)
    }

    /// Whether `endpoint` is a configured (not a built-in) endpoint of this
    /// process: its own listing decides its models' facts.
    #[must_use]
    pub fn is_configured_endpoint(&self, endpoint: &str) -> bool {
        self.endpoints
            .get(endpoint)
            .is_some_and(|e| !e.is_builtin())
    }

    /// Start the background fetch unless one is running.
    pub fn ensure_fetch(&mut self, announce: bool) {
        if self.model_fetch.is_some() {
            return;
        }
        let endpoints = self
            .endpoints
            .iter()
            .filter(|e| e.has_key())
            .cloned()
            .collect();
        let lister = Arc::clone(&self.lister);
        self.model_fetch = Some(tokio::spawn(async move {
            crate::models::fetch_lists(&lister, endpoints).await
        }));
        if announce {
            self.notice(NoticeLevel::Info, "fetching the model lists…");
        }
    }

    /// `/model <reference>` before the lists are known: when `reference`
    /// needs them (a bare id several endpoints could serve), keep it and
    /// fetch; [`Workbench::lists_arrived`] hands it back. Whether it was
    /// kept.
    pub fn defer_until_listed(&mut self, reference: &str) -> bool {
        if self.models.is_some() || !needs_lookup(reference, self.endpoints.as_slice()) {
            return false;
        }
        self.pending_set = Some(reference.to_string());
        self.ensure_fetch(true);
        true
    }

    /// Answer [`crate::ui::Effect::RequestModelPicker`] (`classifier`:
    /// [`crate::ui::Effect::RequestClassifierPicker`], `current` the
    /// classifier's model).
    pub fn request_model_picker(&mut self, refresh: bool, current: Current<'_>, classifier: bool) {
        if refresh {
            self.models = None;
        }
        if self.models.is_some() || self.endpoints.is_empty() {
            self.show_picker(current, classifier);
            return;
        }
        // What waits is one value: which picker, and the model it marks.
        self.waiting = Some(Waiting {
            classifier,
            endpoint: current.endpoint.to_string(),
            model: current.model.to_string(),
        });
        self.ensure_fetch(true);
    }

    /// Open the picker that was asked for (the agent's, or with
    /// `classifier` the classifier's), `current` marked.
    fn show_picker(&self, current: Current<'_>, classifier: bool) {
        let groups = self.picker_groups(current);
        self.send(if classifier {
            Msg::ShowClassifierPicker(groups)
        } else {
            Msg::ShowModelPicker(groups)
        });
    }

    /// The background fetch finished ([`Workbench::background`]): cache the
    /// lists and their prices and open the picker when one waits, with the
    /// model it was asked for marked (the classifier's for `/auto model`).
    /// Returns the `/model <id>` that waited for the lists, for the runner
    /// to switch.
    pub fn lists_arrived(&mut self, joined: Result<FetchResults, JoinError>) -> Option<String> {
        let cache = match joined {
            Ok(results) => {
                let (cache, prices) = ModelCache::from_results(results);
                self.set_endpoint_prices(prices);
                cache
            }
            Err(err) => {
                tracing::warn!(error = %err, "list-models task failed");
                ModelCache::failed(
                    self.endpoints.names(),
                    &Arc::new(ListModelsError::Task(err)),
                )
            }
        };
        self.models = Some(cache);
        if let Some(waiting) = self.waiting.take() {
            self.show_picker(
                Current {
                    endpoint: &waiting.endpoint,
                    model: &waiting.model,
                },
                waiting.classifier,
            );
        }
        self.pending_set.take()
    }

    /// The picker: one group per endpoint, the current endpoint first with
    /// the current model first.
    #[must_use]
    pub fn picker_groups(&self, current: Current<'_>) -> Vec<ModelGroup> {
        let endpoints = &self.endpoints;
        let first = endpoints.iter().filter(|e| e.name == current.endpoint);
        let others = endpoints.iter().filter(|e| e.name != current.endpoint);
        first
            .chain(others)
            .map(|endpoint| {
                let listed = self.models.as_ref().and_then(|m| m.get(&endpoint.name));
                let (models, warning) = match listed {
                    Some(Ok(list)) => (list.clone(), None),
                    Some(Err(err)) => (Vec::new(), Some(fetch_warning(err))),
                    None if !endpoint.has_key() => (
                        Vec::new(),
                        Some("model listing needs an API key the TUI can read".to_string()),
                    ),
                    None => (Vec::new(), None),
                };
                let model = if endpoint.name == current.endpoint {
                    current.model
                } else {
                    ""
                };
                ModelGroup {
                    endpoint: endpoint.name.clone(),
                    dialect: endpoint.dialect,
                    base_url: endpoint.base_url.clone(),
                    models: with_current(model, models),
                    warning,
                }
            })
            .collect()
    }
}
