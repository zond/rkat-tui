//! `endpoints.<name>.models` against the engine's names of a model: which
//! endpoint an engine identity runs on, and whether that endpoint's list
//! allows its model. The daemon's checks of a model switch, `/btw`'s side
//! call and the agents' client decorator (the backstop every agent's and
//! helper's model request passes, `crate::daemon::schema_compat`) ask here.

use super::*;

impl EndpointRegistry {
    /// Whether the engine identity (`model` as the engine names it,
    /// `provider`, the self-hosted `server`, the auth `binding`) may run:
    /// the endpoint it runs on ([`Self::route_of`]) allows its model. An
    /// identity on no endpoint of this process runs on none with a list.
    ///
    /// # Errors
    ///
    /// [`ModelNotAllowed`] naming the endpoint's setting.
    pub fn check_route(
        &self,
        model: &str,
        provider: Provider,
        server: Option<&str>,
        binding: Option<&AuthBindingRef>,
    ) -> Result<(), ModelNotAllowed> {
        match self.route_of(model, provider, server, binding) {
            Some(endpoint) => endpoint.check_model(endpoint.own_model(model)),
            None => Ok(()),
        }
    }

    /// [`Self::check_route`] of a session identity.
    ///
    /// # Errors
    ///
    /// As [`Self::check_route`].
    pub fn check_identity(
        &self,
        identity: &meerkat_core::SessionLlmIdentity,
    ) -> Result<(), ModelNotAllowed> {
        self.check_route(
            &identity.model,
            identity.provider,
            identity.self_hosted_server_id.as_deref(),
            identity.auth_binding.as_ref(),
        )
    }

    /// The endpoint an engine identity runs on: the configured endpoint its
    /// binding into [`ENDPOINT_REALM`] names; else, on the self-hosted
    /// provider, the server (or the alias's `<name>:` prefix); else the
    /// built-in endpoint of the provider's dialect (a binding of another
    /// realm resolves that provider's credentials, still the built-in
    /// endpoint's model ids).
    #[must_use]
    pub fn route_of(
        &self,
        model: &str,
        provider: Provider,
        server: Option<&str>,
        binding: Option<&AuthBindingRef>,
    ) -> Option<&Endpoint> {
        if let Some(binding) = binding.filter(|b| is_endpoint_realm(b.realm.as_str())) {
            return self.get(binding.binding.as_str());
        }
        if provider == Provider::SelfHosted {
            let name = server.or_else(|| model.split_once(':').map(|(name, _)| name))?;
            return self.get(name);
        }
        self.get(WireDialect::from_provider(provider)?.as_str())
    }

    /// For a client that does not say its route (no auth binding known):
    /// `model` of `provider` may run on every endpoint it could be on. On
    /// the self-hosted provider the alias names its endpoint; on a
    /// dialect's provider it could be the built-in endpoint or any
    /// configured gateway of that dialect, and each one's list must allow
    /// it (a refusal of an unknown route errs toward the list).
    ///
    /// # Errors
    ///
    /// The first [`ModelNotAllowed`].
    pub fn check_unrouted(&self, model: &str, provider: Provider) -> Result<(), ModelNotAllowed> {
        if provider == Provider::SelfHosted {
            return self.check_route(model, provider, None, None);
        }
        let Some(dialect) = WireDialect::from_provider(provider) else {
            return Ok(());
        };
        self.iter()
            .filter(|e| e.dialect == dialect && !e.is_self_hosted())
            .try_for_each(|e| e.check_model(model))
    }
}
