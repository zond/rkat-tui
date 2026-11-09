//! `/model` across endpoints: switching and the context-gauge window.
//!
//! The lists and the picker are the workbench's
//! ([`crate::workbench::models`]); the in-process runner fetches them in the
//! background at start-up, unless start-up already fetched them to resolve
//! a bare `--model` id.
//!
//! A switch goes through the runtime's session-LLM reconfigure host
//! ([`crate::session::SessionHandle::switch_model`]) between turns, after
//! making sure the engine has a capability profile for the target
//! ([`crate::bootstrap::Runtime::ensure_profile`]); the session's metadata
//! then records the model and binding the endpoint is derived from.
//!
//! The gauge's (and the compaction policy's) window is the engine's model
//! profile's: the catalog, or the realm's `[models.<id>]` entry, which a
//! fetched listing keeps current (Anthropic and Gemini list windows,
//! LiteLLM's `/model/info` reports them); else [`DEFAULT_CONTEXT_WINDOW`],
//! shown as an estimate. A fetch finishing in the background refreshes the
//! current model's profile and the gauge, but never rebuilds the session:
//! a changed compaction trigger applies at the next switch the user makes.

use meerkat_core::Provider;

use super::Runner;
use super::context::RebuildReason;
use crate::bootstrap::Runtime;
use crate::endpoints::registry_context_window;
use crate::endpoints::{EndpointRef, ModelRefError, parse_model_ref};
use crate::models::{FetchResults, ModelCache};
use crate::ui::{Msg, NoticeLevel, StatusUpdate};
use crate::workbench::models::Current;

/// The context window assumed when neither the endpoint nor the engine's
/// model registry knows the model's (shown as `~`, an estimate).
pub const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

/// The current endpoint and model of a session on `endpoint`, as the
/// picker lists them first.
pub(super) fn current<'a>(
    endpoint: Option<&'a crate::endpoints::EndpointName>,
    identity: &'a crate::session::SessionIdentity,
) -> Current<'a> {
    Current {
        endpoint: endpoint.map_or("", crate::endpoints::EndpointName::as_str),
        model: &identity.model,
    }
}

impl Runner {
    /// The current endpoint and model, when an endpoint serves the session.
    pub(super) fn current_choice(&self) -> Option<EndpointRef> {
        self.current_endpoint
            .as_ref()
            .map(|endpoint| EndpointRef::new(endpoint.as_str(), self.identity.model.clone()))
    }

    /// The current endpoint's name, or `""` when none serves the session.
    fn current_endpoint_name(&self) -> &str {
        self.current_endpoint
            .as_ref()
            .map_or("", crate::endpoints::EndpointName::as_str)
    }

    /// The background fetch finished: the workbench caches the lists and
    /// their prices and opens a waiting picker; here the current model's
    /// profile is refreshed from its listing, the gauge follows, and a
    /// `/model <id>` waiting for the lists is finished. The session is not
    /// rebuilt here (see the module docs).
    pub(super) async fn model_fetch_finished(
        &mut self,
        joined: Result<FetchResults, tokio::task::JoinError>,
    ) {
        let current = current(self.current_endpoint.as_ref(), &self.identity);
        let pending = self.bench.lists_arrived(joined, current);
        let listed = self
            .bench
            .models()
            .and_then(|cache| cache.entry(self.current_endpoint_name(), &self.identity.model))
            .cloned();
        if let Some(listed) = listed
            && let Err(err) = self
                .runtime
                .ensure_profile(self.identity.provider, &self.identity.model, Some(&listed))
                .await
        {
            tracing::warn!(error = %err, "cannot refresh the model profile from its listing");
        }
        self.push_context_window();
        if let Some(reference) = pending {
            if self.turn.is_active() {
                self.notice(
                    NoticeLevel::Warn,
                    format!("/model {reference} dropped: a turn started meanwhile"),
                );
            } else {
                self.set_model(reference).await;
            }
        }
    }

    /// Resolve `/model <reference>` against the cached lists.
    fn resolve_ref(&self, reference: &str) -> Result<EndpointRef, ModelRefError> {
        let index = self
            .bench
            .models()
            .map(ModelCache::index)
            .unwrap_or_default();
        parse_model_ref(
            reference,
            self.runtime.endpoints.as_slice(),
            &index,
            self.current_endpoint
                .as_ref()
                .map(crate::endpoints::EndpointName::as_str),
        )
    }

    /// `/model <reference>` or a picker selection: switch the live session's
    /// model between turns.
    ///
    /// A bare id is looked up in the cached lists (fetched first when the
    /// cache is empty and several endpoints could serve it).
    pub(super) async fn set_model(&mut self, reference: String) {
        if self.turn.is_active() {
            self.notice(
                NoticeLevel::Warn,
                "a turn is running; switch models once it ends (or interrupt it)",
            );
            return;
        }
        if self.bench.defer_until_listed(&reference) {
            return;
        }
        let failed = |err: String| format!("model not changed: {err}");
        let target = match self.resolve_ref(&reference) {
            Ok(target) => target,
            Err(err) => {
                self.notice(NoticeLevel::Error, failed(err.to_string()));
                return;
            }
        };
        let choice = match self.runtime.choice_for(&target) {
            Ok(choice) => choice,
            Err(err) => {
                self.notice(NoticeLevel::Error, failed(err.to_string()));
                return;
            }
        };
        let listed = self
            .bench
            .models()
            .and_then(|m| m.entry(choice.endpoint.as_str(), &choice.model));
        if let Err(err) = self
            .runtime
            .ensure_profile(choice.provider, &choice.model, listed)
            .await
        {
            self.notice(NoticeLevel::Error, failed(err.to_string()));
            return;
        }
        // The live agent's model registry predates the profile: rebuild the
        // agent from the persisted session first.
        if self.session.profiles() < self.runtime.profiles()
            && let Err(err) = self
                .rebuild_session_workaround(RebuildReason::ModelProfile, self.session.compaction())
                .await
        {
            self.notice(NoticeLevel::Error, failed(err.to_string()));
            return;
        }
        match self.session.switch_model(&choice).await {
            Ok(()) => {
                self.refresh_identity().await;
                self.send(Msg::Status(StatusUpdate::Model {
                    provider: self.current_endpoint_name().to_string(),
                    model: self.identity.model.clone(),
                }));
                self.push_context_window();
                // The new model's window moves an auto-mode trigger.
                self.ensure_trigger().await;
            }
            Err(err) => self.notice(NoticeLevel::Error, failed(err.to_string())),
        }
    }

    /// The current model's context window: known, or
    /// [`DEFAULT_CONTEXT_WINDOW`] as an estimate (`true`).
    pub(super) fn context_window(&self) -> (u64, bool) {
        window_of(&self.runtime, self.identity.provider, &self.identity.model)
    }

    /// Push the current model's context window (see the module docs).
    pub(super) fn push_context_window(&self) {
        let update = match self.context_window() {
            (window, false) => StatusUpdate::ContextWindow(Some(window)),
            (window, true) => StatusUpdate::ApproxContextWindow(window),
        };
        self.send(Msg::Status(update));
    }
}

/// The context window of `model` on `provider`: the engine's model
/// profile's (see the module docs), else [`DEFAULT_CONTEXT_WINDOW`] as an
/// estimate (`true`).
pub(crate) fn window_of(runtime: &Runtime, provider: Provider, model: &str) -> (u64, bool) {
    registry_context_window(&runtime.effective.read(), provider, model)
        .map_or((DEFAULT_CONTEXT_WINDOW, true), |window| (window, false))
}
