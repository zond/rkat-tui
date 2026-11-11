//! Which endpoints this process has: the built-in providers with a key,
//! and every `[endpoints.<name>]` of the layered `tui.toml`.

use super::*;

/// The endpoints available to this process.
#[derive(Debug, Clone, Default)]
pub struct EndpointRegistry {
    endpoints: Vec<Endpoint>,
}

impl EndpointRegistry {
    /// A registry over exactly these endpoints (tests, or a caller that
    /// resolved them itself).
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_endpoints(endpoints: Vec<Endpoint>) -> Self {
        Self { endpoints }
    }

    /// The built-in endpoints that resolve (a binding for the provider on
    /// `realm`'s chain, else the provider's environment variable; see
    /// [`resolve_builtin`]), then the layered `[endpoints.<name>]` of
    /// `tui.toml`.
    ///
    /// Invalid configured endpoints (unknown or missing dialect, no key,
    /// invalid name, a name clashing with a built-in) are skipped with a
    /// [`Notice`].
    ///
    /// # Errors
    ///
    /// [`EndpointError::InlineApiKey`] when a configured endpoint sets an
    /// inline `api_key`: keys come from the environment only.
    pub fn discover(
        config: &Config,
        realm: Option<&RealmId>,
        configured: &BTreeMap<String, LayeredEndpoint>,
    ) -> Result<(Self, Vec<Notice>), EndpointError> {
        Self::discover_with(config, realm, configured, |var| std::env::var(var).ok())
    }

    /// [`Self::discover`] with an explicit environment lookup.
    ///
    /// # Errors
    ///
    /// See [`Self::discover`].
    pub fn discover_with(
        config: &Config,
        realm: Option<&RealmId>,
        configured: &BTreeMap<String, LayeredEndpoint>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<(Self, Vec<Notice>), EndpointError> {
        if let Some(name) = configured
            .iter()
            .find_map(|(name, layered)| layered.section.api_key.is_some().then_some(name))
        {
            return Err(EndpointError::InlineApiKey { name: name.clone() });
        }
        let mut endpoints = Vec::new();
        for dialect in WireDialect::ALL {
            if let Some(creds) = resolve_builtin(config, dialect.into(), realm, &env) {
                endpoints.push(Endpoint::builtin(
                    dialect,
                    creds.base_url,
                    creds.api_key.unwrap_or_default(),
                ));
            }
        }
        let mut notices = Vec::new();
        for (name, layered) in configured {
            match configured_endpoint(name, layered, &env) {
                Ok(endpoint) => endpoints.push(endpoint),
                Err(reason) => notices.push(Notice::EndpointSkipped {
                    name: name.clone(),
                    reason: reason.to_string(),
                }),
            }
        }
        Ok((Self { endpoints }, notices))
    }

    /// Add each built-in endpoint missing from the registry, keyless (a host
    /// that serves every session with its own client, such as the tests:
    /// sessions on them never resolve credentials, and their lists are not
    /// fetched).
    pub fn add_keyless_builtins(&mut self) {
        for (index, dialect) in WireDialect::ALL.into_iter().enumerate() {
            if self.get(dialect.as_str()).is_none() {
                let at = index.min(self.endpoints.len());
                self.endpoints.insert(
                    at,
                    Endpoint::builtin(dialect, None, SecretString::default()),
                );
            }
        }
    }

    /// The endpoint called `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Endpoint> {
        self.endpoints.iter().find(|e| e.name == name)
    }

    /// Every endpoint: built-ins first, then configured ones by name.
    pub fn iter(&self) -> impl Iterator<Item = &Endpoint> {
        self.endpoints.iter()
    }

    /// The endpoints as a slice (for [`parse_model_ref`]).
    #[must_use]
    pub fn as_slice(&self) -> &[Endpoint] {
        &self.endpoints
    }

    /// Whether no endpoint is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    /// Endpoint names, in [`Self::iter`] order.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        names(&self.endpoints)
    }
}

/// Why one `[endpoints.<name>]` entry is skipped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EndpointSkip {
    /// The name is not a valid endpoint name.
    #[error("names may only use letters, digits, `_` and `-`")]
    InvalidName,
    /// The name is a built-in endpoint's.
    #[error(
        "`{0}` is a built-in endpoint; configure its key and base URL in the realm config, or pick another name"
    )]
    BuiltinName(String),
    /// No `dialect`.
    #[error("`dialect` is missing (anthropic, openai or gemini)")]
    NoDialect,
    /// An unknown `dialect`.
    #[error("unknown dialect `{0}` (expected anthropic, openai or gemini)")]
    UnknownDialect(String),
    /// A project file may not name environment variables.
    #[error(
        "a project tui.toml may not read keys from the environment (`api_key_env = \"{0}\"`); define the endpoint in your own tui.toml"
    )]
    ProjectEnvKey(String),
    /// `api_key_env` names an unset variable.
    #[error("environment variable `{0}` (api_key_env) is not set")]
    UnsetEnv(String),
    /// No key at all.
    #[error("no key: set `api_key_env` to the environment variable that holds it")]
    NoKey,
    /// `api_style` on an endpoint that is not of the `openai` dialect.
    #[error("`api_style` applies to the openai dialect only, not {0}")]
    ApiStyleDialect(WireDialect),
}

/// Validate one `[endpoints.<name>]` entry (an inline `api_key` is refused
/// before, by [`EndpointRegistry::discover_with`]).
pub(super) fn configured_endpoint(
    name: &str,
    layered: &LayeredEndpoint,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<Endpoint, EndpointSkip> {
    let EndpointSection {
        dialect,
        base_url,
        api_key_env,
        api_key: _,
        priority,
        api_style,
    } = &layered.section;
    if !is_valid_endpoint_name(name) {
        return Err(EndpointSkip::InvalidName);
    }
    if BUILTIN_NAMES.contains(&name) {
        return Err(EndpointSkip::BuiltinName(name.to_string()));
    }
    let dialect = match dialect.as_deref() {
        None => return Err(EndpointSkip::NoDialect),
        Some(raw) => {
            WireDialect::parse(raw).ok_or_else(|| EndpointSkip::UnknownDialect(raw.to_string()))?
        }
    };
    if api_style.is_some() && dialect != WireDialect::OpenAi {
        return Err(EndpointSkip::ApiStyleDialect(dialect));
    }
    let Some(var) = api_key_env else {
        return Err(EndpointSkip::NoKey);
    };
    if layered.origin == RuleOrigin::Project {
        return Err(EndpointSkip::ProjectEnvKey(var.clone()));
    }
    let key = env(var)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| EndpointSkip::UnsetEnv(var.clone()))?;
    Ok(Endpoint::configured(
        name,
        dialect,
        base_url.clone(),
        SecretString::new(key),
        var.clone(),
        priority.unwrap_or(0),
        api_style.unwrap_or_default(),
    ))
}
